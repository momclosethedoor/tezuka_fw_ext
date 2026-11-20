"""Run the accepted Pentium RAM harness with executable CAP2 helpers."""
from pathlib import Path
import sys
import os

workspace = Path("C:/proj/pluto_ag")
sys.path.insert(0, str(workspace.parent / "goofy_scan/local-only/HOST-SCAN-CONTROL-001"))
import row99_smoke as bench

stage = bench.accepted.stage


def stage_executables(*args, **kwargs):
    if "PLANNER_BENCH_MODULES" in os.environ:
        args=(args[0],Path(os.environ["PLANNER_BENCH_MODULES"]),*args[2:])
    stage(*args, **kwargs)
    bench.accepted.ssh(f"cd {bench.accepted.REMOTE} && chmod 700 cap2-run sig0-arm")


if __name__ == "__main__":
    bench.accepted.stage = stage_executables
    bench.main()
