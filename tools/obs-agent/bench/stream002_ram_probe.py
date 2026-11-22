"""Short RX-only OBN3 RAM check with the existing Pentium UART rollback."""
import argparse
import json
import os
from pathlib import Path
import re
import sys
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    parser.add_argument("--stand", type=Path, required=True)
    parser.add_argument("--modules", type=Path, required=True)
    parser.add_argument("--reader", type=Path, required=True)
    parser.add_argument("--image", type=Path, required=True)
    args = parser.parse_args()
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
    prepare = log / "loader/prepare-r3.sh"
    prepare.write_bytes(prepare.read_bytes().replace(b"0x324E424F", b"0x334E424F")
                        .replace(b"0x7c4600e4)\" = 0x00000003", b"0x7c4600e4)\" = 0x00000004"))
    staged = json.loads((log / "staged.json").read_text())
    for name, path in [("row99.bit.bin", args.image), (loader.name, loader), (prepare.name, prepare)]:
        stand.upload(path, remote, name)
        staged[name] = stand.digest(path)
    stand.write_json(log / "staged.json", staged)
    try:
        stand.uart("load")
        stand.uart("scan-setup")
        profiles = [bytes(map(int, row.split()[1].split(",")))
                    for row in stand.ssh(f"cat {remote}/profiles.txt").splitlines()]
        mapping = log / "map.bin"
        mapping.write_bytes(stand.make_map(profiles, 0, 7700))
        config = log / "reader.json"
        stand.write_json(config, dict(setup=remote + "/map.bin", replay=None, reader=dict(
            boot_id=boot, output=remote + "/probe", command="rate", k=2, mode=1, scan=True,
            hot=True, settling=0, seconds=2.0, warmup=.5, batch_limit=64, trace_limit=50000)))
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
        stand.fetch(log, remote + "/probe")
        status = json.loads(result)
        if "Ok" not in status["reader_outcome"] or status["guard_return"].get("state") != 3:
            raise RuntimeError(f"Reader or RF return failed: {status}")
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
