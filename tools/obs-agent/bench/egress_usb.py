"""Exclusive RAM bench handoff to native libiio. No RF attributes are written."""
import ctypes
from contextlib import ExitStack
import os
from subprocess import TimeoutExpired
import threading
import time

from sdr.iio.discovery import scan_usb_contexts
from sdr.iio.lib import load

BLOCK_SAMPLES = 8192
BLOCK_COUNT = 4

# Match the continuous owner registers after confirmed analysis RF tune, STOP,
# and owner exit. A timed ARM-side trap restores the prior register values.
ARM_NATIVE = r"""
set -eu
cd "$1"
seconds=$2
for name in maia-delivery iio_readdev; do
    if pidof "$name" >/dev/null; then echo "Unexpected live $name"; exit 4; fi
done
test "$(devmem 0x7c460064)" = 0x00000010
test "$(devmem 0x7c4601e0)" = 0x00000000
test "$(devmem 0x7c460284)" = 0x00000000
test "$(cat /sys/bus/iio/devices/iio:device0/out_altvoltage1_TX_LO_powerdown)" = 1
node=
for d in /sys/bus/iio/devices/iio:device*; do
    test "$(cat "$d/name")" = cf-ad9361-lpc || continue
    for f in "$d"/buffer*/enable; do
        if test -f "$f"; then node=$f; break; fi
    done
done
test -n "$node"
test "$(cat "$node")" = 0
nco=$(devmem 0x7c460030)
mode=$(devmem 0x7c460038)
code=$(devmem 0x7c460040)
cleanup() {
    devmem 0x7c460284 32 0
    devmem 0x7c460038 32 0
    devmem 0x7c460030 32 "$nco"
    devmem 0x7c460040 32 "$code"
    devmem 0x7c460038 32 "$mode"
    echo NATIVE_STOP
}
trap cleanup EXIT
trap 'exit 2' INT TERM
devmem 0x7c460030 32 0
devmem 0x7c460038 32 1
devmem 0x7c460040 32 24
echo NATIVE_READY > native-ready
i=0
until test "$(cat "$node")" = 1; do
    test "$i" -lt 1000
    i=$((i+1))
    usleep 10000
done
devmem 0x7c460284 32 1
test "$(devmem 0x7c460284)" = 0x00000001
echo NATIVE_START
cat /proc/uptime
echo RF_FS
cat /sys/bus/iio/devices/iio:device0/out_altvoltage0_RX_LO_frequency
cat /sys/bus/iio/devices/iio:device0/in_voltage_sampling_frequency
for p in $(pidof iiod); do echo "IIOD_START $p"; cat "/proc/$p/stat"; done
i=0
while test "$i" -lt "$((10*(seconds+3)))" && test "$(cat "$node")" = 1; do
    if test "$i" -eq 20; then
        for p in $(pidof iiod); do
            echo "IIOD_RUNNING $p"
            grep -E 'VmRSS|Threads' "/proc/$p/status"
            ls "/proc/$p/fd" | wc -l
        done
    fi
    i=$((i+1))
    usleep 100000
done
cat /proc/uptime
for p in $(pidof iiod); do
    echo "IIOD_END $p"
    cat "/proc/$p/stat"
    grep -E 'VmRSS|Threads' "/proc/$p/status"
    ls "/proc/$p/fd" | wc -l
done
"""


def quantiles(values):
    if not values:
        return {}
    ordered = sorted(values)
    return {k: ordered[round((len(ordered)-1)*p)]/1e6 for k, p in (
        ("p50", .5), ("p95", .95), ("p99", .99), ("max", 1))}


def measure(board, folder, seconds, expected_lo, host_sample):
    """Call after the caller reaped the ARM owner and verified its STOP."""
    started = time.perf_counter_ns()
    lib = load()
    candidates = scan_usb_contexts(lib)
    if len(candidates) != 1:
        raise RuntimeError(f"native USB requires one context, found {len(candidates)}")
    script = folder/"native-real8.sh"
    script.write_bytes((ARM_NATIVE.strip()+"\n").encode("ascii"))
    remote = board.REMOTE
    board.ssh(f"test ! -f {remote}/native-ready || rm {remote}/native-ready")
    board.upload(script, remote, script.name)
    stopper = []
    def run_native():
        try:
            stopper.append(board.ssh(f"sh {remote}/{script.name} {remote} {seconds}", seconds+28))
        except (OSError, RuntimeError, TimeoutExpired) as exc:
            stopper.append(str(exc))
    agent = threading.Thread(target=run_native, daemon=True)
    agent.start()
    try:
        ready_deadline = time.monotonic() + 6
        while time.monotonic() < ready_deadline:
            if stopper:
                raise RuntimeError(stopper[0])
            if board.ssh(f"test ! -f {remote}/native-ready || cat {remote}/native-ready").strip() == "NATIVE_READY":
                break
        else:
            raise RuntimeError("native ARM handoff not armed")
        intervals, calls, window_intervals = [], [], []
        window_bytes = 0
        last, first = None, None
        bytes_received = 0
        clip = bytearray()
        source_errors = []
        resources = dict(before=host_sample())
        with ExitStack() as close:
            context = lib.create_context(candidates[0]["uri"], timeout_ms=1500)
            close.callback(lib.context_destroy, context)
            dev = lib.find_device(context, "cf-ad9361-lpc")
            mask = lib.create_mask(dev)
            close.callback(lib.mask_destroy, mask)
            for name in ("voltage0", "voltage1"):
                lib.channel_enable(lib.find_channel(dev, name, False), mask)
            word_bytes = lib.sample_size(dev, mask)
            if word_bytes != 4:
                raise RuntimeError(f"expected native real8 4-byte transport word, got {word_bytes}")
            block_bytes = word_bytes * BLOCK_SAMPLES
            stream = lib.buffer_open(lib.device_buffer(dev), mask)
            close.callback(lib.buffer_close, stream)
            blocks = []
            for _ in range(BLOCK_COUNT):
                block = lib.create_block(stream, block_bytes)
                blocks.append(block)
                close.callback(lib.block_destroy, block)
                lib.block_enqueue(block)
            lib.stream_start(stream)
            close.callback(lib.stream_stop, stream)
            stream_started = time.perf_counter_ns()
            cpu_begin = time.process_time()
            deadline = stream_started + (seconds+3)*1_000_000_000
            while time.perf_counter_ns() < deadline:
                block = blocks[bytes_received // block_bytes % len(blocks)]
                before = time.perf_counter_ns()
                rc = lib.block_dequeue_rc(block, False)
                now = time.perf_counter_ns()
                if rc < 0:
                    source_errors.append(rc)
                    break
                begin, end = lib.block_span(block)
                if end-begin != block_bytes:
                    raise RuntimeError(f"libiio block size {end-begin}, expected {block_bytes}")
                if last is None:
                    first = now
                    deadline = first + seconds*1_000_000_000
                else:
                    intervals.append(now-last)
                if now < deadline:
                    window_bytes += block_bytes
                    if last is not None:
                        window_intervals.append(now-last)
                last = now
                calls.append(now-before)
                bytes_received += block_bytes
                if len(clip) < 2*1024*1024:
                    clip.extend(ctypes.string_at(begin, block_bytes))
                lib.block_enqueue(block)
            resources.update(host_cpu_s=time.process_time() - cpu_begin, after=host_sample())
        if clip:
            (folder/"native-clip.real8").write_bytes(clip)
        result = dict(transport="native libiio USB USB-gadget", version=lib.version_str,
            block_samples=BLOCK_SAMPLES, blocks=BLOCK_COUNT, bytes_per_word=word_bytes, bytes=bytes_received,
            first_after_host_setup_ms=(first-started)/1e6 if first else None,
            first_after_stream_start_ms=(first-stream_started)/1e6 if first else None,
            interval_s=(last-first)/1e9 if first and last else None,
            interarrival_ms=quantiles(intervals), dequeue_wall_ms=quantiles(calls), source_errors=source_errors,
            fixed_window=(dict(seconds=seconds, bytes=window_bytes, frames=window_bytes//block_bytes,
                interarrival_ms=quantiles(window_intervals)) if first and last >= deadline else None),
            expected_rf_lo=expected_lo, effective_ddc_fs=15_360_000, process=resources,
            host_os=os.name, continuity_limit="IIO block size and cadence; no native source sequence")
        board.write_json(folder/"native-usb.json", result)
    finally:
        agent.join(6)
        if not stopper:
            raise RuntimeError("bounded native ARM helper still exiting")
        response = stopper[0]
        (folder/"native-board.txt").write_text(response, encoding="utf-8")
        if "NATIVE_STOP" not in response or "NATIVE_START" not in response:
            raise RuntimeError(f"native ARM handoff failed: {response[:300]}")
        if f"\n{expected_lo}\n61440000\n" not in response.replace("\r\n", "\n"):
            raise RuntimeError("native board RF or sample rate readback differs")
