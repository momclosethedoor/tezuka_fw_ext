"""Compare identical ARM requests on the fixed and generated direct banks."""
import json
import os
import socket
import struct
import threading
import sys
import time
import uuid

from planner_row99 import bench, stage_executables


def lease_loop(stop, ready):
    from sdr.source.maia.lease import Lease
    from sdr.source.maia.mqtt import MqttLink
    lease=Lease.create("192.168.2.1")
    will=MqttLink("192.168.2.1")
    try:
        lease.open(time.perf_counter())
        will.open()
        while not stop.is_set():
            now=time.perf_counter()
            lease.service(now)
            if now>=will.next_due:
                will.service(now)
            if lease.lease is True and will.armed:
                ready.set()
            stop.wait(.025)
    finally:
        lease.close(time.perf_counter())
        will.close(graceful=True)


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
    mixer=os.environ.get("PLANNER_BENCH_ROUTE")=="mixer"
    if mixer:
        assert v2
        scan["ranges"]=[dict(lo_hz=6748000000,hi_hz=6752000000,enabled=True)]
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
    stop,ready=threading.Event(),threading.Event()
    thread=None
    try:
        if mixer:
            thread=threading.Thread(target=lease_loop,args=(stop,ready),daemon=True)
            thread.start()
            assert ready.wait(8),"no live TX lease"
        for i in range(50):
            try:
                a.request(url, "GET", path, timeout=.5)
                break
            except OSError:
                time.sleep(.1)
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as receiver:
            receiver.bind((ip,18010))
            receiver.settimeout(2)
            cmds=[scan,scan,dict(scan,k=1),dict(scan,k=1023),analysis,scan,dict(scan,run=False)]
            if os.environ.get("PLANNER_BENCH_ALTERNATIVE")=="1":
                cmds[1:1]=[dict(op="alternative"),dict(op="cancel_alternative")]
            for cmd in cmds:
                if cmd["op"]=="alternative":
                    prev=calls[-1]["status"]["actual"]
                    lo,hi=prev["spectrum"]["positions"][0]["rf_ranges"][0]
                    cmd.update(source_position=0,source_generation=prev["generation"],lo_hz=lo,hi_hz=min(hi,lo+2000000))
                if cmd["op"]=="cancel_alternative":
                    cmd["request_id"]=calls[-1]["status"]["applied_id"]
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
                    deadline=time.monotonic()+3
                    discarded=0
                    while time.monotonic()<deadline:
                        data,_=receiver.recvfrom(65535)
                        if data[:8]==b"GVIEW4\0\0" and struct.unpack_from("<Q",data,56)[0]==status["actual"]["generation"]:
                            break
                        discarded+=1
                    else:
                        raise RuntimeError("no UDP observation of applied generation")
                    (log/f"view-{len(calls)}.bin").write_bytes(data)
                    calls[-1].update(stale_datagrams_discarded=discarded,fresh_host_ns=time.monotonic_ns()-start)
                    a.write_json(log/"commands.json",calls)
    finally:
        try:
            (log/"owner.log").write_text(a.ssh(f"cd {remote}; kill -TERM $(cat owner.pid) 2>/dev/null || :; sleep 1; cat owner.log"))
        finally:
            stop.set()
            if thread: thread.join(3)


if __name__ == "__main__":
    bench.accepted.stage = stage_executables
    bench.accepted.video_smoke = control
    sys.argv.append("--reference")
    bench.main()
