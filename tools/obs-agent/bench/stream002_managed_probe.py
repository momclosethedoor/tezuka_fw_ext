"""Measure managed full-map VIEW, FIELD and standalone compact delivery."""
import json
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path

from egress_probe import HOST, Probe, Receiver, lease_loop
from stream002_sink import FIELD, FieldChunks, exact, field_frame


SCAN = dict(op="scan", planner_version=2, path_ack_version=1, sweep_geometry="27/9",
            xtg1_wait_ticks=256, catalog_on_boot=True, run=True, k=2,
            ranges=[dict(lo_hz=70_000_000, hi_hz=12_000_000_000, enabled=True)])
FIELD_TIMEOUT_MS = 90
ROI_PORT = 18011
TRANSMITTER_WAIT_S = 90
ARM_COUNTERS = """cat /proc/stat /proc/net/snmp /proc/net/dev
printf 'uptime '
cat /proc/uptime
for name in maia-delivery obs-ffm1-iio; do
    for pid in $(pidof "$name"); do
        cat "/proc/$pid/stat" 2>/dev/null || continue
        sed -n -e '/^VmRSS:/p' -e '/^VmHWM:/p' "/proc/$pid/status" 2>/dev/null || true
    done
done"""


class BenchReceiver(Receiver):
    field_mode = False
    fsb1 = False
    def __init__(self, folder):
        self.view_samples = {}
        super().__init__(folder)

    def sample_view(self, generation, ordinal, flags, k, view):
        records = self.view_samples.setdefault(generation, [])
        if len(records) < 512 or flags & 64 and len(records) < 528:
            records.append(dict(generation=generation, ordinal=ordinal, flags=flags,
                                actual_k=k, payload_hex=view.hex()))

    def real8(self, conn):
        if not self.field_mode:
            return super().real8(conn)
        chunks = FieldChunks() if self.fsb1 else None
        while not self.stop.is_set():
            try:
                header = exact(conn, FIELD.size, eof=True)
            except TimeoutError:
                continue
            if header is None:
                if chunks and chunks.active is not None:
                    self.protocol_errors.append("FSB1 TCP closed before END")
                return
            try:
                if chunks:
                    meta, length, payload = chunks.read(conn, header)
                else:
                    meta, length = field_frame(conn, header)
            except (ValueError, KeyError, EOFError) as error:
                self.protocol_errors.append(str(error))
                return
            self.field_stats["bytes"] += length
            if chunks:
                kind=meta["kind"]
                self.field_stats[kind] += 1
                self.field_stats["payload_bytes"] += payload
                if kind!="end":
                    continue
                if meta["qualified_field"]:
                    self.field_stats["qualified"] += 1
                    self.field_stats["successes"].append(meta)
                self.field_stats["error"] += int(bool(meta["error"]))
                code=str(meta["pl_outcome"])
            else:
                self.field_stats["end"] += 1
                code=str(meta["outcome"])
            outcomes = self.field_stats["outcomes"]
            outcomes[code] = outcomes.get(code, 0)+1


class BenchProbe(Probe):
    def save(self):
        # Probe.command runs while the receivers are active. Serialize evidence at STOP.
        pass

    def save_after_stop(self):
        super().save()


def launch_roi(log, full, sample=False):
    output=(log/"roi-sink.log").open("w",encoding="utf-8")
    try:
        command = [sys.executable,"-B","-u",str(Path(__file__).with_name("stream002_sink.py")),
                   "compact" if full else "roi",str(ROI_PORT),"--bind",HOST,"--seconds","240","--done-file",str(log/"roi-done")]
        if sample:
            command += ["--sample-file",str(log/("full-sample.jsonl" if full else "roi-sample.jsonl"))]
        return subprocess.Popen(command,
            stdout=output,stderr=subprocess.STDOUT)
    finally:
        output.close()


def install_lease(stand, args):
    remote = stand.REMOTE
    daemon = stand.member(args.reader, "goofy_lease.sh")
    stand.upload(daemon, remote, daemon.name)
    stand.ssh(f"""set -e
cd {remote}
test -e /etc/goofy/lease_enforce
test "$(cat /sys/bus/iio/devices/iio:device0/out_altvoltage1_TX_LO_powerdown)" = 1
chmod 700 goofy_lease.sh
old=$(cat /run/goofy/goofy_lease.pid)
mount -o bind "$PWD/goofy_lease.sh" /usr/sbin/goofy_lease.sh
kill -TERM "$old"
i=0
until [ -r /run/goofy/goofy_lease.pid ] && new=$(cat /run/goofy/goofy_lease.pid) && [ "$new" != "$old" ] &&
  kill -0 "$new" && [ -p /run/goofy/hb.fifo ]; do
  i=$((i+1)); test "$i" -lt 120; usleep 100000
done
""", 16)


def summary(status):
    reader = status["metrics"]["reader"]
    field_watch = status["actual"].get("field_watch") or {}
    return dict(phase=status["phase"], error=status["error"], field_error=field_watch.get("error"),
                generation=status["actual"]["generation"], progress=status["progress"], reader=reader)

def view_snapshot(receiver, generation):
    stats = dict(receiver.view_stats(generation))
    stats["actual_k_counts"] = dict(stats["actual_k_counts"])
    stats["hold_k_counts"] = dict(stats["hold_k_counts"])
    stats["pass_ticks"] = {name:dict(counts) for name,counts in stats["pass_ticks"].items()}
    return stats


def check_hold(stand, receiver, log, generation, args):
    before = view_snapshot(receiver, generation)["hold_k_counts"]
    held = stand.ssh(f'''set -e; trap 'devmem 0x7c460178 32 0' EXIT
devmem 0x7c460178 32 2
usleep 3000
a=$(devmem 0x7c460180)
usleep {args.hold_check_ms * 1000}
b=$(devmem 0x7c460180)
devmem 0x7c460178 32 0
usleep 12000
c=$(devmem 0x7c460180)
printf '%s %s %s\n' "$a" "$b" "$c"
''').split()
    time.sleep(.5)
    after = view_snapshot(receiver, generation)["hold_k_counts"]
    counts = {k:n-before.get(k,0) for k,n in after.items() if n>before.get(k,0)}
    good_epoch = len(held) == 3 and held[0] == held[1] and held[1] != held[2]
    good_groups = counts.get(args.hold_k,0)>0 and all(1<=k<=args.hold_k for k in counts)
    if args.hold_k>1:
        good_groups = good_groups and any(k<args.hold_k for k in counts)
    stand.write_json(log/"hold-check.json", dict(hold_k=args.hold_k, milliseconds=args.hold_check_ms,
                    epoch_readback=held, actual_k_counts=counts, pass_check=good_epoch and good_groups))
    if not good_epoch or not good_groups:
        raise RuntimeError("antenna hold did not produce the expected epoch and FFT groups")


def mode_regressions(stand, probe, receiver, log, analysis, obn5=False):
    scan = dict(SCAN, view_bins=512)
    analysis = dict(analysis, view_bins=1024)
    if obn5:
        scan.update(fft_bins=1024, view_bins=256)
        analysis.update(fft_bins=2048, view_bins=2048)
        before = stand.ssh("devmem 0x7c460398").strip()
        for invalid in (0, 1024, 1025, 2047, 1 << 32):
            probe.command(f"reject-hold-{invalid}", dict(scan, hold_k=invalid))
            if probe.cases[-1]["http"] != 400:
                raise RuntimeError(f"invalid hold threshold {invalid} reached the reader")
        if stand.ssh("devmem 0x7c460398").strip() != before:
            raise RuntimeError("a rejected hold threshold changed its CSR")
    video = dict(op="video", center_hz=5_865_000_000, width_hz=8_000_000)
    templates = {"scan":scan, "analysis":analysis, "video":video}
    results = []
    for start, middle, end in (("scan","analysis","scan"), ("scan","video","scan"),
                              ("analysis","video","analysis")):
        stages = []
        previous = None
        failure = None
        try:
            for index, mode in enumerate((start, middle, end)):
                command = {"op":"return"} if index == 2 and middle == "video" else templates[mode]
                active = probe.command(f"{start}-{middle}-{end}-{index}", command, seconds=45)
                gen = active.get("generation")
                if active["mode"] != mode or active["running"] is not True or gen == previous:
                    state = probe.cases[-1]["status"]
                    raise RuntimeError(f"{mode} did not apply: {state['error']} "
                                       f"(egress: {state['metrics']['owner'].get('error')})")
                deadline = time.monotonic()+5
                while time.monotonic() < deadline:
                    if mode == "video":
                        record = receiver.video_stats.get(gen, {})
                        count = record.get("packets", 0)
                    else:
                        count = receiver.view_stats(gen)["received"]
                    if count >= 2:
                        break
                    time.sleep(.05)
                else:
                    raise RuntimeError(f"{mode} has no fresh data in generation {gen}")
                stages.append(dict(mode=mode,generation=gen,records=count,view_bins=active.get("view_bins"),
                                   fft_bins=(active.get("spectrum") or {}).get("fft_bins"),
                                   tx=active["tx"],guard_tx=active.get("guard_tx")))
                previous = gen
        except RuntimeError as error:
            failure = str(error)
        probe.command(f"stop-{start}-{middle}-{end}", dict(SCAN, run=False), seconds=12)
        results.append(dict(sequence=[start,middle,end],stages=stages,error=failure))
        stand.write_json(log/"mode-regressions.json",results)


def run(stand, args, log, boot):
    remote = stand.REMOTE
    ddc = stand.ssh(f'''set -eu; cd {remote}; sh ddc4.sh
test "$(devmem 0x7c460354)" = 0x00000000
devmem 0x7c460284 32 0
test "$(devmem 0x7c460284)" = 0x00000000
''')
    (log / "ddc-readback.txt").write_text(ddc, encoding="utf-8")
    install_lease(stand, args)
    config = log / "owner.json"
    stopped = dict(SCAN, run=False)
    analysis = dict(op="analysis", center_hz=5_865_000_000, width_hz=24_000_000, k=700,
                    gain_db=35, agc="manual")
    control = dict(boot_id=boot, output=remote+"/control", scan=stopped,
        analysis=analysis, view=f"{HOST}:18010", route=f"{HOST}:18008", video=f"{HOST}:18009",
        bench_obn3_layout=args.bench_obn3_layout)
    if args.obn3_view_only:
        control["bench_obn3_view_only"] = True
    fields = getattr(args,"field_counts",(0,))
    targets = json.loads(args.targets.read_text(encoding="utf-8-sig")) if max(fields) else []
    if len(targets)<max(fields):
        raise ValueError("insufficient distinct targets for requested FIELD window")
    if targets:
        collector=stand.member(args.reader,"obs-ffm1-iio")
        stand.upload(collector,remote,"obs-ffm1-iio")
        stand.ssh(f"chmod 700 {remote}/obs-ffm1-iio")
        control["resident_field"]=dict(collector=remote+"/obs-ffm1-iio",timeout_ms=FIELD_TIMEOUT_MS)
    roi_center=getattr(args,"roi_center_hz",None)
    roi_range=getattr(args,"roi_range_hz",None)
    full=getattr(args,"full_compact_tcp",False)
    roi_enabled=full or roi_center is not None or roi_range is not None
    if roi_enabled:
        control["bench_roi"]=f"{HOST}:{ROI_PORT}"
        control["bench_full_compact_tcp"]=full
    stand.write_json(config,control)
    stand.upload(config, remote, config.name)
    receiver = BenchReceiver(log)
    receiver.obn5 = args.obn5
    receiver.record_views = args.obn5 and full
    receiver.fsb1 = args.fsb1 or args.obn5
    if receiver.fsb1:
        receiver.field_stats.update(payload_bytes=0,successes=[])
    roi_process = launch_roi(log,full,args.obn5 and (full or roi_range is not None)) if roi_enabled else None
    halted, ready = threading.Event(), threading.Event()
    lease = threading.Thread(target=lease_loop, args=(halted, ready, log, threading.Event()), daemon=True)
    lease.start()
    probe = BenchProbe(stand, receiver, log)
    rows = []
    transmitter = False
    gated=getattr(args,"gate_transmitter",False)
    try:
        if not ready.wait(8):
            raise RuntimeError("accepted scanner lease/will unavailable")
        if roi_process is not None:
            deadline=time.monotonic()+3
            while "READY" not in (log/"roi-sink.log").read_text(encoding="utf-8"):
                if roi_process.poll() is not None or time.monotonic()>=deadline:
                    raise RuntimeError("compact ROI receiver did not bind")
                time.sleep(.05)
        dmac_trace = "OBS_FSB1_DMAC_TRACE=1 " if args.fsb1_dmac_trace else ""
        iio_depth = f"OBS_FSB1_IIO_BUFFERS={args.fsb1_iio_buffers} " if args.fsb1_iio_buffers != 4 else ""
        capture = f"OBS_FSB1_CAPTURE_WORKER={args.fsb1_capture_worker} " if args.fsb1_capture_worker else ""
        header_filter = "OBS_ROI_HEADER_FILTER=1 " if args.roi_skip_unselected else ""
        stand.ssh(f"cd {remote}; (OBS_PHASE_TIMING=1 OBS_FFM1_TRACE=1 {dmac_trace}{iio_depth}{capture}{header_filter}timeout 270 ./maia-delivery --system-control owner.json "
                  "--listen 0.0.0.0:18080 </dev/null >probe-agent.log 2>&1 &)")
        time.sleep(.3)
        if not stand.ssh("pidof maia-delivery || true").strip():
            raise RuntimeError(stand.ssh(f"cat {remote}/probe-agent.log"))
        probe.until(lambda s: isinstance(s["actual"].get("catalog"), dict), 100)
        if getattr(args,"regression",False):
            mode_regressions(stand,probe,receiver,log,analysis,args.obn5)
            return
        combinations = [(width, num, k) for width in args.widths for num in fields for k in args.integrations]
        for index, (width, num, k) in enumerate(combinations):
            if gated and num and not transmitter:
                (log/"transmitter-needed").touch()
                print("TRANSMITTER_READY_REQUIRED",flush=True)
                deadline=time.monotonic()+(args.transmitter_wait_seconds or TRANSMITTER_WAIT_S)
                while not (log/"transmitter-ready").exists():
                    if time.monotonic()>=deadline:
                        raise RuntimeError("transmitter ready signal timed out")
                    time.sleep(.1)
                transmitter=True
            label = f"wide-{width}-field-{num}"
            if len(args.integrations) > 1 or k != SCAN["k"]:
                label += f"-k{k}"
            receiver.field_mode = bool(num)
            spectrum = dict(fft_bins=args.fft_bins, hold_k=args.hold_k) if args.obn5 else {}
            active = probe.command(label, dict(SCAN, k=k, view_bins=width, field_targets=targets[:num],
                                               **spectrum), seconds=40)
            if active["running"] is not True or active.get("view_bins") != width or active.get("k") != k:
                if num:
                    detail=stand.ssh(f"dir=$(ls -td {remote}/control/field-* | head -n 1); "
                                     'test -z "$dir" || cat "$dir/collector.log"')
                    (log/f"{label}-startup.log").write_text(detail,encoding="utf-8")
                raise RuntimeError(f"{label} did not start: {probe.cases[-1]['status']['error']}")
            stand.write_json(log / f"{label}-descriptor.json", active["spectrum"])
            if roi_center is not None:
                probe.request(dict(op="roi",id=uuid.uuid4().hex,center_hz=roi_center))
            elif roi_range is not None:
                probe.request(dict(op="roi",id=uuid.uuid4().hex,lo_hz=roi_range[0],hi_hz=roi_range[1],
                                   view_bins=args.roi_view_bins))
            time.sleep(1)
            generation = active["generation"]
            if args.hold_check_ms:
                check_hold(stand,receiver,log,generation,args)
            start = time.monotonic()
            snapshots = []
            abort_ether = False
            arm_begin = stand.ssh(ARM_COUNTERS)
            host_begin = view_snapshot(receiver, generation)
            while time.monotonic()-start < args.seconds:
                sample = probe.request()
                snapshots.append(summary(sample))
                if transmitter and (snapshots[-1]["field_error"] or sample["phase"]=="failed"):
                    abort_ether = True
                    break
                if sample["actual"]["running"] is not True or sample["actual"]["generation"] != generation:
                    break
                target = start+len(snapshots)
                time.sleep(max(0,target-time.monotonic()))
            host_end = view_snapshot(receiver, generation)
            elapsed = time.monotonic()-start
            arm_end = stand.ssh(ARM_COUNTERS)
            row = dict(view=width, field=num, k=k, seconds=elapsed, generation=generation, samples=snapshots,
                       fft_bins=active["spectrum"]["fft_bins"], hold_k=args.hold_k,
                       roi_range_hz=roi_range, roi_view_bins=args.roi_view_bins if roi_range else None,
                       host_begin=host_begin, host_end=host_end, arm_begin=arm_begin, arm_end=arm_end)
            rows.append(row)
            probe.command(f"stop-{label}", dict(stopped, k=k, view_bins=width, **spectrum), seconds=12)
            if transmitter and (abort_ether or index + 1 == len(combinations)):
                (log/"transmitter-done").touch()
                print("TRANSMITTER_CAN_STOP",flush=True)
                transmitter=False
            row["drained"]=summary(probe.request())
            if num:
                row["assignments"]=active.get("field_watch")
                directory=remote+f"/control/field-{generation}"
                for name in ("collector.log","receipts.jsonl"):
                    content=stand.ssh(f"cat {directory}/{name}")
                    (log/f"{label}-{name}").write_text(content,encoding="utf-8")
                if any(sample.get("field_error") for sample in snapshots):
                    kernel=stand.ssh("dmesg | tail -n 80")
                    (log/f"{label}-kernel.log").write_text(kernel,encoding="utf-8")
            stand.write_json(log / "windows.json", rows)
            print(json.dumps(dict(view=width, field=num, k=k, elapsed_s=elapsed, a=host_end["A"]-host_begin["A"],
                b=host_end["B"]-host_begin["B"])),flush=True)
            if abort_ether:
                break
    finally:
        if gated and (log/"transmitter-needed").exists() and not (log/"transmitter-done").exists():
            (log/"transmitter-done").touch()
            print("TRANSMITTER_CAN_STOP",flush=True)
        halted.set()
        lease.join(4)
        receiver.close()
        if receiver.record_views:
            with (log/"view-samples.jsonl").open("w",encoding="utf-8") as out:
                for samples in receiver.view_samples.values():
                    for sample in samples:
                        out.write(json.dumps(sample, separators=(",", ":")) + "\n")
        if roi_process is not None:
            (log/"roi-done").touch()
            try:
                roi_process.wait(5)
            except subprocess.TimeoutExpired:
                roi_process.terminate()
                roi_process.wait(5)
        probe.save_after_stop()
