"""Short RX-only OBN3 RAM check with the existing Pentium UART rollback."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import time


def file_digest(stream, algorithm):
    digest = hashlib.new(algorithm)
    for block in iter(lambda: stream.read(262144), b""):
        digest.update(block)
    return digest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    parser.add_argument("--stand", type=Path, required=True)
    parser.add_argument("--modules", type=Path, required=True)
    parser.add_argument("--reader", type=Path, required=True)
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--capture-layout", action="store_true")
    parser.add_argument("--view-mode", type=int, choices=range(4), default=1)
    parser.add_argument("--bench-obn3-layout", action="store_true")
    parser.add_argument("--managed", action="store_true")
    parser.add_argument("--widths", type=int, nargs="+", choices=(0, 512, 1024, 2048), default=(0,))
    parser.add_argument("--seconds", type=int, choices=range(15, 31), default=20)
    parser.add_argument("--regression", action="store_true")
    parser.add_argument("--field-counts", type=int, nargs="+", choices=(0, 1, 3, 5, 10), default=(0,))
    parser.add_argument("--targets", type=Path)
    parser.add_argument("--roi-center-hz", type=int)
    parser.add_argument("--roi-range-hz", type=int, nargs=2, metavar=("LOW", "HIGH"))
    parser.add_argument("--roi-view-bins", type=int, choices=(512, 1024, 2048, 4096), default=4096)
    parser.add_argument("--full-compact-tcp", action="store_true")
    parser.add_argument("--gate-transmitter", action="store_true",
                        help="pause before FIELD windows for the operator's ready signal")
    parser.add_argument("--cached-dma", action="store_true", help="owned cacheable DMA with exact range sync")
    args = parser.parse_args()
    compact_tcp = args.full_compact_tcp or args.roi_center_hz is not None
    if sum((args.full_compact_tcp, args.roi_center_hz is not None, args.roi_range_hz is not None)) > 1:
        parser.error("choose one of full compact TCP, legacy ROI or packed ROI")
    if compact_tcp and (not args.managed or args.regression or any(args.widths)):
        parser.error("compact TCP requires a managed scan with VIEW OFF")
    if args.roi_range_hz and (not args.managed or args.regression or
                              not 70_000_000 <= args.roi_range_hz[0] < args.roi_range_hz[1] <= 13_000_000_000):
        parser.error("packed ROI requires a managed scan and increasing RF limits")
    if not hasattr(hashlib, "file_digest"):
        hashlib.file_digest = file_digest
    sys.path.insert(0, str(args.stand.resolve()))
    import run_live as stand

    image_hash = "830d6a8268b96d2661b985eb767b247f7398ae67ffcf208cd297f52b68bd6159"
    if stand.digest(args.image) != image_hash:
        raise ValueError("Expected the main PL-SCAN-STREAM-002 RAM image")
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
            b' echo STREAM002_ABI_AND_MODULES_READY',
        ]) + b"\n")
    if args.cached_dma:
        loader.write_bytes(loader.read_bytes().replace(b"insmod ./obs_dma.ko cached=0",b"insmod ./obs_dma.ko cached=1"))
    prepare = log / "loader/prepare-r3.sh"
    prepare.write_bytes(prepare.read_bytes().replace(b"0x324E424F", b"0x334E424F")
                        .replace(b"0x7c4600e4)\" = 0x00000003", b"0x7c4600e4)\" = 0x00000004")
                        .replace(b"OBS006C_R3_GEOMETRY_VERIFIED", b"OBN3_R4_GEOMETRY_VERIFIED"))
    staged = json.loads((log / "staged.json").read_text())
    for name, path in [("row99.bit.bin", args.image), (loader.name, loader), (prepare.name, prepare)]:
        stand.upload(path, remote, name)
        staged[name] = stand.digest(path)
    stand.write_json(log / "staged.json", staged)
    try:
        stand.uart("load")
        stand.uart("scan-setup")
        if args.managed:
            from stream002_managed_probe import run
            run(stand, args, log, boot)
            return
        profiles = [bytes(map(int, row.split()[1].split(",")))
                    for row in stand.ssh(f"cat {remote}/profiles.txt").splitlines()]
        mapping = log / "map.bin"
        mapping.write_bytes(stand.make_map(profiles, 0, 7700))
        config = log / "reader.json"
        reader = dict(
            boot_id=boot, output=remote + "/probe", command="capture" if args.capture_layout else "rate",
            k=2, mode=args.view_mode, scan=True, hot=True, settling=0, seconds=.03 if args.capture_layout else 2.0,
            warmup=.5, batch_limit=64, trace_limit=50000)
        if args.bench_obn3_layout:
            reader["bench_obn3_layout"] = True
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
            stand.ssh(f"cd {remote}/probe; set -e; dd if=full-ring.bin of=full-prefix.bin bs=32768 count=8; "
                      "dd if=full-ring.bin of=full-last.bin bs=32768 skip=1023 count=1; "
                      "dd if=view-ring.bin of=view-prefix.bin bs=2048 count=8; "
                      "dd if=view-ring.bin of=view-last.bin bs=2048 skip=1023 count=1")
            for name in ("full-prefix.bin", "full-last.bin", "view-prefix.bin", "view-last.bin"):
                stand.transfer(f"root@192.168.2.1:{remote}/probe/{name}", str(log / name))
            records = (log / "full-prefix.bin").read_bytes() + (log / "full-last.bin").read_bytes()
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
