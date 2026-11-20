"""Compare identical ARM requests on the fixed and generated direct banks."""
import json
import os
import socket
import sys
import time
import uuid

from planner_row99 import bench, stage_executables


def control(log, profiles, boot):
    a = bench.accepted
    v2 = os.environ.get("PLANNER_BENCH_VERSION", "2") == "2"
    remote = a.REMOTE
    url = "http://192.168.2.1:18080"
    path = "/api/system/control"
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as route:
        route.connect(("192.168.2.1", 18010))
        ip = route.getsockname()[0]
    scan = dict(op="scan", run=True, k=2, ranges=[
        dict(lo_hz=2400000000, hi_hz=2448000000, enabled=True)])
    analysis = dict(op="analysis", center_hz=2425000000, width_hz=24000000,
                    k=700, gain_db=35, agc="manual")
    cfg = dict(boot_id=boot, output=remote+"/control", view=ip+":18010",
               video=ip+":18009", route=ip+":18008", scan=scan, analysis=analysis)
    if v2:
        scan["planner_version"] = 2
    else:
        cfg.update(setup=remote+"/control-map.bin", positions=bench.FREQUENCIES)
    a.write_json(log/"cli-config.json", cfg)
    a.upload(log/"cli-config.json", remote, "cli-config.json")
    a.ssh(f"cd {remote} && (timeout 135 ./maia-delivery --system-control cli-config.json --listen 192.168.2.1:18080 </dev/null >owner.log 2>&1 & echo $! > owner.pid)")
    calls = []
    try:
        for i in range(50):
            try:
                a.request(url, "GET", path, timeout=.5)
                break
            except OSError:
                time.sleep(.1)
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as receiver:
            receiver.bind((ip,18010))
            receiver.settimeout(2)
            for cmd in [scan,scan,dict(scan,k=1),dict(scan,k=1023),analysis,scan,dict(scan,run=False)]:
                ident = uuid.uuid4().hex
                start = time.monotonic_ns()
                accepted = a.request(url,"POST",path,dict(cmd,id=ident),timeout=2)
                assert accepted["accepted_id"] == ident
                while time.monotonic_ns()-start < 10_000_000_000:
                    status = a.request(url,"GET",path,timeout=1)
                    if ident in (status["applied_id"],status["failed_id"]): break
                    time.sleep(.02)
                calls.append(dict(command=cmd,status=status,host_elapsed_ns=time.monotonic_ns()-start))
                a.write_json(log/"commands.json",calls)
                assert status["applied_id"] == ident, status.get("error")
                if status["actual"]["running"]:
                    data,_=receiver.recvfrom(65535)
                    (log/f"view-{len(calls)}.bin").write_bytes(data)
    finally:
        (log/"owner.log").write_text(a.ssh(f"cd {remote}; kill -TERM $(cat owner.pid) 2>/dev/null || :; sleep 1; cat owner.log"))


if __name__ == "__main__":
    bench.accepted.stage = stage_executables
    bench.accepted.video_smoke = control
    sys.argv.append("--reference")
    bench.main()
