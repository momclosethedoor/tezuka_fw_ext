"""Run the accepted Pentium RAM harness with executable CAP2 helpers."""
from pathlib import Path
import sys
import os
import hashlib
import json

workspace = Path("C:/proj/pluto_ag")
sys.path.insert(0, str(workspace.parent / "goofy_scan/local-only/HOST-SCAN-CONTROL-001"))
import row99_smoke as bench

stage = bench.accepted.stage


def stage_executables(*args, **kwargs):
    if "PLANNER_BENCH_MODULES" in os.environ:
        args=(args[0],Path(os.environ["PLANNER_BENCH_MODULES"]),*args[2:])
    stage(*args, **kwargs)
    api = bench.accepted
    api.ssh(f"cd {api.REMOTE} && chmod 700 cap2-run sig0-arm")
    if image := os.environ.get("PLANNER_BENCH_XTG1"):
        image = Path(image)
        variants = {
            "a79c03ace0524a43b4ee90a1582fe01ebec4e0f368ab0a98138eb78e1c1ed5f8": (1_124_992, b""),
            "07399ea386ed080b49e130d0d58db32627abf3f406c1b6d036c38370004ae94b": (
                1_083_552, b'test "$(devmem 0x7c46033c)" = 0x00000000\necho XTG1_CONTROL=1\n'
            ),
            "65e8ffa41926026fb14a3aaf6bf4c052136775403b0d8c1027bbf2cbc729b17a": (
                1_124_480, b'test "$(devmem 0x7c46033c)" = 0x32435054\necho TPC2_ID=0x32435054\n'
            ),
        }
        expected = api.digest(image)
        if expected not in variants or image.stat().st_size != variants[expected][0]:
            raise ValueError("Unrecognized XTG1 RAM image or size")
        log = args[0]
        loader = log / "loader/check-row99.sh"
        original = "7401ebc0de93033f6388c3c081561e61a5a9004f029c25fe0551caa719ecab78"
        script = loader.read_bytes()
        assert script.count(original.encode()) == 1
        loader.write_bytes(script.replace(original.encode(), expected.encode()) +
                           b'\ntest "$(devmem 0x7c460328)" = 0x31475458\necho XTG1_ID=0x31475458\n' +
                           variants[expected][1])
        api.upload(image, api.REMOTE, "row99.bit.bin")
        api.upload(loader, api.REMOTE, loader.name)
        board = log / "staged.json"
        staged = json.loads(board.read_text())
        staged.update({"row99.bit.bin": expected,
                       loader.name: hashlib.sha256(loader.read_bytes()).hexdigest()})
        api.write_json(board, staged)


if __name__ == "__main__":
    bench.accepted.stage = stage_executables
    if os.environ.get("PLANNER_BENCH_CAP2"):
        def capture_and_hold(log, profiles, boot):
            if os.environ.get("PLANNER_BENCH_XTG1"):
                api = bench.accepted
                gate = json.loads(api.ssh(f"cd {api.REMOTE} && ./obs-fl-ctl gate-status"))
                assert gate["supported"] and gate["capability"] == 0x31475458 and gate["visit"] is None
                api.write_json(log / "xtg1-idle.json", gate)
            bench.accepted.run_case(log, profiles, boot, "window-18432", "reuse", 18432, 102, save=True)
            bench.accepted.mode_smoke(log, profiles, boot)
        bench.accepted.video_smoke = capture_and_hold
        sys.argv.append("--reference")
    bench.main()
