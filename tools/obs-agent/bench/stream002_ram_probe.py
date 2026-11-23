"""RX-only OBN3 or OBN5 RAM check with the Pentium UART rollback."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import sys
import time


def file_digest(stream, algorithm):
    digest = hashlib.new(algorithm)
    for block in iter(lambda: stream.read(262144), b""):
        digest.update(block)
    return digest


def run_joint_cap2(stand, args, log, boot, profiles):
    pos = 1
    field_hz = 2_413_000_000
    field_nco = (1_000_000 << 28) // 61_440_000
    remote = stand.REMOTE
    stand.upload(args.cap2_probe, remote, "obn5-cap2-run")
    stand.upload(stand.member(args.reader, "obs-ffm1-iio"), remote, "obs-ffm1-iio")
    stand.ssh(f"cd {remote}; set -e; chmod 700 obn5-cap2-run obs-ffm1-iio; sh ddc4.sh; devmem 0x7c460284 32 0")
    for index, mode in enumerate(("cap2-first", "field-first", "field-cancel", "field-mismatch"), 1):
        name = f"joint-{mode}"
        case = log / name
        case.mkdir()
        dest = f"{remote}/{name}"
        stand.ssh(f"mkdir {dest}")
        version = 7800 + 2 * index
        job = 100 + index
        tagged = job + 1 if mode == "field-mismatch" else job
        mapping = bytearray(stand.make_map(profiles, tagged, version, flags=40, slot=pos))
        upper_at = 40 + 4096 + 32 * pos + 24
        upper, = struct.unpack_from("<Q", mapping, upper_at)
        struct.pack_into("<Q", mapping, upper_at, upper | field_nco << 36)
        (case / "map.bin").write_bytes(mapping)
        wat = bytearray(struct.pack("<4IQ2I", 0x31544157, 1, version, len(profiles), index, 1, 32))
        for row in range(len(profiles)):
            wat.extend(struct.pack("<IIQIIQ", 1, row, field_hz, field_nco, 24, index)
                       if row == pos else struct.pack("<II24x", 0, row))
        watch_name = f"WAT1-{version}.bin"
        (case / watch_name).write_bytes(wat)
        config = dict(setup=f"{dest}/map.bin", replay=None,
                      field=dict(collector=f"{remote}/obs-ffm1-iio", watch_directory=dest),
                      reader=dict(boot_id=boot, output=dest, command="rate", k=2, mode=0,
                                  stream_config=0, hold_k=1023, seconds=1.0, warmup=.1,
                                  scan=True, hot=True, settling=0, batch_limit=64, trace_limit=8000))
        stand.write_json(case / "config.json", config)
        for filename in ("map.bin", watch_name, "config.json"):
            stand.upload(case / filename, dest, filename)
        result = stand.ssh(f"cd {remote}; timeout 17 ./obn5-cap2-run {mode} 40960 15360000 {job} "
                           f"{dest}/config.json {dest}/iq.bin {pos} 2>&1; rc=$?; "
                           f"test ! -f reader.log || cp reader.log {dest}/reader.log; echo status=$rc", 24)
        (case / "cap2-result.txt").write_text(result, encoding="utf-8")
        stand.fetch(case, dest)
        if not result.strip().endswith("status=0"):
            raise RuntimeError(f"{mode} failed: {result}")
        field_epoch = int(re.search(r"\bfield_epoch=(\d+)", result)[1])
        receipts = [json.loads(line) for line in (case / "receipts.jsonl").read_text().splitlines()]
        matched = [r for r in receipts if r.get("kind") == "field" and r["epoch"] == field_epoch]
        report = json.loads((case / "agent-result.json").read_text())
        if (len(matched) != 1 or matched[0]["position"] != pos or not matched[0]["started"] or
                matched[0]["outcome"] not in (1, 2) or not report["field_stopped"] or
                "Ok" not in report["reader_outcome"] or report["guard_return"].get("state") != 3):
            raise RuntimeError(f"{mode} did not complete its matching FIELD and return RF: {matched}")
        print(f"{mode}: FIELD epoch={field_epoch} outcome={matched[0]['outcome']}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    parser.add_argument("--stand", type=Path, required=True)
    parser.add_argument("--modules", type=Path, required=True)
    parser.add_argument("--reader", type=Path, required=True)
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--capture-layout", action="store_true")
    parser.add_argument("--obn5-layout-check", type=Path, help="run the direct DMA layout checker before guard setup")
    parser.add_argument("--cap2-probe", type=Path, help="run the four joint OBN5 CAP2/FIELD cases")
    parser.add_argument("--view-mode", type=int, choices=range(5), default=1)
    parser.add_argument("--bench-obn3-layout", action="store_true")
    parser.add_argument("--managed", action="store_true")
    parser.add_argument("--widths", type=int, nargs="+", choices=(0, 128, 256, 512, 1024, 2048, 4096), default=(0,))
    parser.add_argument("--fft-bins", type=int, choices=(1024, 2048, 4096))
    parser.add_argument("--hold-k", type=int, choices=range(1, 1024), metavar="1..1023")
    parser.add_argument("--hold-check-ms", type=int, choices=(40, 350), help="pulse the antenna hold in a VIEW-only window")
    parser.add_argument("--integrations", type=int, nargs="+", choices=(2, 3), default=(2,),
                        help="spectral accumulation K for managed scan windows")
    parser.add_argument("--obn3-view-only", action="store_true",
                        help="select raw12 native and read only the OBN3 header and VIEW")
    parser.add_argument("--seconds", type=int, choices=range(15, 31), default=20)
    parser.add_argument("--regression", action="store_true")
    parser.add_argument("--field-counts", type=int, nargs="+", choices=range(11), default=(0,))
    parser.add_argument("--targets", type=Path)
    parser.add_argument("--roi-center-hz", type=int)
    parser.add_argument("--roi-range-hz", type=int, nargs=2, metavar=("LOW", "HIGH"))
    parser.add_argument("--roi-view-bins", type=int, choices=(512, 1024, 2048, 4096), default=4096)
    parser.add_argument("--roi-skip-unselected", action="store_true",
                        help="during packed ROI read only the headers of unselected DMA records")
    parser.add_argument("--full-compact-tcp", action="store_true")
    parser.add_argument("--gate-transmitter", action="store_true",
                        help="pause before FIELD windows for the operator's ready signal")
    parser.add_argument("--transmitter-wait-seconds", type=int,
                        help="wait 30 to 150 seconds for transmitter readiness (default 90)")
    parser.add_argument("--cached-dma", action="store_true", help="owned cacheable DMA with exact range sync")
    parser.add_argument("--fsb1", action="store_true", help="use the PL-FIELD-STREAM-001 RAM image and streaming sink")
    parser.add_argument("--obn5", action="store_true", help="use the OBN5 r1 RAM image and streaming FIELD sink")
    parser.add_argument("--fsb1-dmac-trace", action="store_true", help="save read-only ADI DMAC samples before a FIELD fault")
    parser.add_argument("--fsb1-iio-buffers", type=int, choices=(4, 8), default=4,
                        help="compare ADI IIO queue depths during the DMAC diagnostic")
    parser.add_argument("--fsb1-capture-worker", choices=("normal", "priority"),
                        help="separate IIO return from FIELD formatting and TCP")
    args = parser.parse_args()
    if args.obn5_layout_check and (not args.obn5 or args.managed or args.capture_layout):
        parser.error("--obn5-layout-check requires a standalone OBN5 run")
    if args.cap2_probe and (not args.obn5 or args.managed or args.capture_layout or args.obn5_layout_check):
        parser.error("--cap2-probe requires a standalone OBN5 run")
    if args.hold_check_ms and (not args.obn5 or not args.managed or max(args.field_counts) or not all(args.widths)):
        parser.error("--hold-check-ms requires an OBN5 managed VIEW-only window")
    if args.obn5 and (args.fsb1 or args.bench_obn3_layout or args.obn3_view_only):
        parser.error("OBN5 uses the native layout and its own RAM image")
    if args.obn5:
        args.fft_bins = args.fft_bins or 4096
        args.hold_k = args.hold_k or 1023
        widths = {0, args.fft_bins, args.fft_bins//2, args.fft_bins//4, args.fft_bins//8}
    else:
        if args.view_mode == 4 or args.fft_bins or args.hold_k:
            parser.error("VIEW /1, alternate FFT lengths and hold_k require OBN5")
        widths = {0, 512, 1024, 2048}
    if any(width not in widths for width in args.widths):
        parser.error("VIEW bins must be OFF or a supported division of FFT bins")
    if args.roi_skip_unselected and not args.roi_range_hz:
        parser.error("--roi-skip-unselected requires --roi-range-hz")
    if args.fsb1_dmac_trace and not ((args.fsb1 or args.obn5) and args.managed and max(args.field_counts)):
        parser.error("--fsb1-dmac-trace requires --fsb1, --managed and an active FIELD window")
    if args.fsb1_iio_buffers != 4 and not args.fsb1_dmac_trace:
        parser.error("--fsb1-iio-buffers 8 requires --fsb1-dmac-trace")
    if args.fsb1_capture_worker and not args.fsb1_dmac_trace:
        parser.error("--fsb1-capture-worker requires --fsb1-dmac-trace")
    compact_tcp = args.full_compact_tcp or args.roi_center_hz is not None
    if args.integrations != (2,) and (not args.managed or args.regression):
        parser.error("--integrations requires managed scan windows")
    if args.obn3_view_only and (not args.managed or args.regression or not all(args.widths) or
                               compact_tcp or args.roi_range_hz):
        parser.error("--obn3-view-only requires managed scan windows with VIEW and no compact delivery")
    if max(args.field_counts) and not args.targets:
        parser.error("FIELD windows require --targets")
    if sum((args.full_compact_tcp, args.roi_center_hz is not None, args.roi_range_hz is not None)) > 1:
        parser.error("choose one of full compact TCP, legacy ROI or packed ROI")
    if compact_tcp and (not args.managed or args.regression or
                        any(args.widths) and not (args.obn5 and args.full_compact_tcp)):
        parser.error("compact TCP requires a managed scan with VIEW OFF")
    if args.roi_range_hz and (not args.managed or args.regression or
                              not 70_000_000 <= args.roi_range_hz[0] < args.roi_range_hz[1] <= 13_000_000_000):
        parser.error("packed ROI requires a managed scan and increasing RF limits")
    if args.transmitter_wait_seconds is not None and (
        not args.gate_transmitter or not 30 <= args.transmitter_wait_seconds <= 150
    ):
        parser.error("--transmitter-wait-seconds requires --gate-transmitter and 30 to 150 seconds")
    if not hasattr(hashlib, "file_digest"):
        hashlib.file_digest = file_digest
    sys.path.insert(0, str(args.stand.resolve()))
    import run_live as stand

    image_hash = ("5515cc9fd0156e5cf41974499ea467b4904ed6ca8926f95462a657a857c8627f" if args.obn5 else
                  "0bd534f83e452fdce6e3c69ffb7acf82c10361000b107c25f9ec68a4ad140ce1" if args.fsb1 else
                  "830d6a8268b96d2661b985eb767b247f7398ae67ffcf208cd297f52b68bd6159")
    if stand.digest(args.image) != image_hash:
        raise ValueError("Expected the verified main RAM image for the selected ABI")
    log = args.log.resolve()
    log.mkdir()
    (log / "uart").mkdir()
    os.environ["OBS007F1_UART_LOG"] = str(log / "uart")
    before = stand.ssh(stand.BASELINE)
    boot = stand.ssh("cat /proc/sys/kernel/random/boot_id").strip()
    if "0xFD5F0000" not in before or not re.fullmatch(r"[a-f0-9-]{36}", boot):
        raise RuntimeError("Stock PL and a live boot ID are prerequisites")
    (log / "stock-before.txt").write_text(before, encoding="utf-8")
    stand.write_json(log / "stock-before.json", stand.api())
    stand.uart("probe")
    if boot not in (log / "uart/row99-probe.log").read_text():
        raise RuntimeError("UART and SSH did not identify the same boot")
    remote = stand.REMOTE
    stand.ssh(f"test \"$(cat /proc/sys/kernel/random/boot_id)\" = '{boot}' && mkdir {remote}")
    stand.stage(log, args.modules, args.reader, boot)
    loader = log / "loader/check-row99.sh"
    loader.write_bytes(loader.read_bytes().replace(
        b"7401ebc0de93033f6388c3c081561e61a5a9004f029c25fe0551caa719ecab78",
        image_hash.encode("ascii")) + b"\n" + b"\n".join([
            b' test "$(devmem 0x7c460328)" = 0x31475458',
            b' test "$(devmem 0x7c46033c)" = 0x00000000',
            b' test "$(devmem 0x7c460280)" = 0x32444C46',
            b' test "$(devmem 0x7c460350)" = 0x314D4646',
            *( [b' test "$(devmem 0x7c460388)" = 0x31425346'] if args.fsb1 or args.obn5 else [] ),
            b' echo STREAM002_ABI_AND_MODULES_READY',
        ]) + b"\n")
    if args.cached_dma:
        loader.write_bytes(loader.read_bytes().replace(b"insmod ./obs_dma.ko cached=0",b"insmod ./obs_dma.ko cached=1"))
    prepare = log / "loader/prepare-r3.sh"
    code = prepare.read_bytes()
    if args.obn5:
        code = code.replace(b"0x324E424F", b"0x354E424F")
        code = code.replace(b'0x7c4600e4)" = 0x00000003', b'0x7c4600e4)" = 0x00000001')
        code = code.replace(b'0x7c4601c8)" = 0x00000800', b'0x7c4601c8)" = 0x00001000')
        code = code.replace(b"OBS006C_R3_GEOMETRY_VERIFIED", b"OBN5_R1_GEOMETRY_VERIFIED")
        code += (b'\ntest "$(devmem 0x7c4600e0)" = 0x000001FF\n'
                 b'test "$(devmem 0x7c460398)" = 0x000003FF\n'
                 b'test "$(devmem 0x7c46039c)" = 0x000003FF\n')
    else:
        code = code.replace(b"0x324E424F", b"0x334E424F")
        code = code.replace(b'0x7c4600e4)" = 0x00000003', b'0x7c4600e4)" = 0x00000004')
        code = code.replace(b"OBS006C_R3_GEOMETRY_VERIFIED", b"OBN3_R4_GEOMETRY_VERIFIED")
    prepare.write_bytes(code)
    staged = json.loads((log / "staged.json").read_text())
    for name, path in [("row99.bit.bin", args.image), (loader.name, loader), (prepare.name, prepare)]:
        stand.upload(path, remote, name)
        staged[name] = stand.digest(path)
    stand.write_json(log / "staged.json", staged)
    try:
        stand.uart("load")
        if args.obn5_layout_check:
            stand.upload(args.obn5_layout_check, remote, "obn5-layout-check")
            result=stand.ssh(f"cd {remote}; chmod 700 obn5-layout-check && timeout 10 ./obn5-layout-check 2>&1; echo status=$?")
            (log / "direct-layout.log").write_text(result, encoding="utf-8")
            print(result, flush=True)
            if not result.strip().endswith("status=0"):
                raise RuntimeError("OBN5 direct FULL/VIEW verification failed")
            return
        stand.uart("scan-setup")
        if args.managed:
            from stream002_managed_probe import run
            run(stand, args, log, boot)
            return
        profiles = [bytes(map(int, row.split()[1].split(",")))
                    for row in stand.ssh(f"cat {remote}/profiles.txt").splitlines()]
        if args.cap2_probe:
            run_joint_cap2(stand, args, log, boot, profiles)
            return
        mapping = log / "map.bin"
        mapping.write_bytes(stand.make_map(profiles, 0, 7700))
        config = log / "reader.json"
        reader = dict(
            boot_id=boot, output=remote + "/probe", command="capture" if args.capture_layout else "rate",
            k=2, mode=args.view_mode, scan=True, hot=True, settling=0, seconds=.03 if args.capture_layout else 2.0,
            warmup=.5, batch_limit=64, trace_limit=50000)
        if args.bench_obn3_layout:
            reader["bench_obn3_layout"] = True
        if args.obn5:
            reader.update(stream_config={4096:0,2048:2,1024:4}[args.fft_bins], hold_k=args.hold_k)
        stand.write_json(config, dict(setup=remote + "/map.bin", replay=None, reader=reader))
        for path in (mapping, config):
            stand.upload(path, remote, path.name)
        stand.ssh(f"cd {remote}; mkdir probe; (OBS_PHASE_TIMING=1 timeout 25 ./maia-delivery "
                  "--system-observations reader.json --listen 0.0.0.0:18080 "
                  "</dev/null >probe-agent.log 2>&1 &)")
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            result = stand.ssh(f"cd {remote}; test ! -f probe/agent-result.json || cat probe/agent-result.json")
            if result.strip():
                break
            time.sleep(.25)
        else:
            raise RuntimeError("Reader did not report a terminal result")
        status = json.loads(result)
        (log / "agent-result.json").write_text(result, encoding="utf-8")
        if "Ok" not in status["reader_outcome"] or status["guard_return"].get("state") != 3:
            raise RuntimeError(f"Reader or RF return failed: {status}")
        if args.capture_layout:
            for name in ("agent-result.json", "memory.json", "unreleased.json"):
                content = stand.ssh(f"cat {remote}/probe/{name}")
                (log / name).write_text(content, encoding="utf-8")
        else:
            stand.fetch(log, remote + "/probe")
        if args.capture_layout:
            stride = 4096 if args.obn5 else 2048
            stand.ssh(f"cd {remote}/probe; set -e; dd if=full-ring.bin of=full-prefix.bin bs=32768 count=8; "
                      "dd if=full-ring.bin of=full-last.bin bs=32768 skip=1023 count=1; "
                      f"dd if=view-ring.bin of=view-prefix.bin bs={stride} count=8; "
                      f"dd if=view-ring.bin of=view-last.bin bs={stride} skip=1023 count=1")
            for name in ("full-prefix.bin", "full-last.bin", "view-prefix.bin", "view-last.bin"):
                stand.transfer(f"root@192.168.2.1:{remote}/probe/{name}", str(log / name))
            records = (log / "full-prefix.bin").read_bytes() + (log / "full-last.bin").read_bytes()
            if args.obn5:
                from stream002_sink import verify_obn5_pairs
                views = (log / "view-prefix.bin").read_bytes() + (log / "view-last.bin").read_bytes()
                stand.write_json(log / "layout.json", verify_obn5_pairs(records, views, args.fft_bins, args.view_mode))
                return
            observations = []
            for offset in range(0, len(records) - 64):
                if records[offset:offset+8] == b"OBN3\x04\x00\x40\x00":
                    ordinal = int.from_bytes(records[offset+56:offset+64], "little")
                    pad = records[offset+64:offset+128]
                    slot=offset//32768
                    observations.append(dict(slot=1023 if slot==8 else slot, ordinal=ordinal,
                        offset_in_slot=offset%32768, nonzero_padding=sum(x!=0 for x in pad),
                        first_bytes=records[offset:offset+160].hex()))
            stand.write_json(log / "layout.json", observations)
            return
        stats = json.loads((log / "result.json").read_text())
        if not stats["full_read_total"] or stats["after"]["committed"] != stats["after"]["released"]:
            raise RuntimeError("OBN3 decoding or the committed/released balance failed")
    finally:
        try:
            (log / "probe-agent.log").write_text(stand.ssh(f"cat {remote}/probe-agent.log 2>/dev/null || true"))
        finally:
            stand.restore(log, boot, before)


if __name__ == "__main__":
    main()
