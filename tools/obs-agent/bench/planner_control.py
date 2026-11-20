"""Compare identical ARM requests on the fixed and generated direct banks."""
import json
import os
import socket
import struct
import statistics
import threading
import sys
import time
import uuid

from planner_row99 import bench, stage_executables
from planner_sinks import Sinks


def record_laps(receiver,data,status,folder,index):
    a=status["actual"]
    spec=a["spectrum"]
    n=len(spec["positions"])
    generation=a["generation"]
    anchor=spec["map_epoch"]
    seen=set()
    boundaries=[]
    stamps=[]
    count=missing=0
    prior=None
    deadline=time.monotonic()+3.5
    with (folder/f"lap-{index}.dat").open("wb") as f:
        while time.monotonic()<deadline:
            if data[:8]==b"GVIEW4\0\0":
                ordinal,seq,epoch,tick,k,flags,gen=struct.unpack_from("<7Q",data,8)
                if gen==generation and epoch>=anchor:
                    f.write(struct.pack("<I",len(data))+data)
                    pos=(epoch-anchor)%n
                    seen.add(pos)
                    if pos==0 and (not boundaries or epoch!=boundaries[-1]):
                        boundaries.append(epoch)
                        stamps.append(tick)
                    count+=1
                    if prior is not None:missing+=max(0,ordinal-prior-1)
                    prior=ordinal
                    if len(stamps)>=5 and len(seen)==n:break
            data,_=receiver.recvfrom(65535)
    laps=[(b-a)/62.5 for a,b in zip(stamps,stamps[1:])]
    return dict(packets=count,missing_ordinals=missing,covered_positions=len(seen),total=n,
                lap_us=laps,lap_median_us=statistics.median(laps) if laps else None)


def lease_loop(stop, ready,log):
    from sdr.source.maia.lease import Lease
    from sdr.source.maia.mqtt import MqttLink
    lease=Lease.create("192.168.2.1")
    will=MqttLink("192.168.2.1")
    try:
        lease.open(time.perf_counter())
        will.open()
        next_log=0
        while not stop.is_set():
            now=time.perf_counter()
            lease.service(now)
            if now>=will.next_due:
                will.service(now)
            if lease.lease is True and will.armed:
                ready.set()
            if now>=next_log:
                with (log/"lease.ndjson").open("a") as out:
                    print(json.dumps({"host_s":now,"lease":lease.lease,"will":will.armed,"state":lease._raw}),file=out)
                next_log=now+.25
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
        if os.environ.get("PLANNER_BENCH_SOURCE58"):
            scan["ranges"]=[dict(lo_hz=5864000000,hi_hz=5866500000,path="mixer",enabled=True)]
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
    sinks=Sinks(ip,log)
    try:
        if mixer:
            thread=threading.Thread(target=lease_loop,args=(stop,ready,log),daemon=True)
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
            receiver.setsockopt(socket.SOL_SOCKET,socket.SO_RCVBUF,4<<20)
            receiver.settimeout(2)
            cmds=[scan,scan,dict(scan,k=1),dict(scan,k=1023),analysis,scan,dict(scan,run=False)]
            if os.environ.get("PLANNER_BENCH_ALTERNATIVE")=="1":
                cmds[1:1]=[dict(op="alternative"),dict(op="cancel_alternative")]
            if os.environ.get("PLANNER_BENCH_VIDEO"):
                cmds[-1:-1]=[analysis,dict(op="video",center_hz=2425000000,width_hz=6000000),dict(op="return")]
            if os.environ.get("PLANNER_BENCH_BANK"):
                cmds=[dict(scan,ranges=[dict(lo_hz=70000000,hi_hz=70000000+n*23_529_940,enabled=True)])
                      for n in (127,128,129)]+[dict(scan,run=False)]
            if os.environ.get("PLANNER_BENCH_ROUTE_ACK"):
                ranges=[dict(lo_hz=2300000000,hi_hz=2600000000,enabled=True)]
                routes=dict(scan,ranges=ranges,antenna_bands={"0":[[2300000000,2450000000]],
                    "1":[[2395000000,2530000000]],"2":[[2515000000,2600000000]]})
                cmds=[routes,dict(routes,k=1),dict(scan,run=False)]
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
                while time.monotonic_ns()-start < 35_000_000_000:
                    status = a.request(url,"GET",path,timeout=1)
                    if ident in (status["applied_id"],status["failed_id"]): break
                    time.sleep(.02)
                calls.append(dict(command=cmd,status=status,host_elapsed_ns=time.monotonic_ns()-start))
                a.write_json(log/"commands.json",calls)
                assert status["applied_id"] == ident, status.get("error")
                if status["actual"]["running"] and status["actual"]["mode"]=="video":
                    deadline=time.monotonic()+3
                    while time.monotonic()<deadline and status["actual"]["generation"] not in sinks.video:
                        time.sleep(.01)
                    assert sinks.video[status["actual"]["generation"]]>0
                elif status["actual"]["running"]:
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
                    if status["actual"]["mode"]=="scan" and not os.environ.get("PLANNER_BENCH_ROUTE_ACK"):
                        calls[-1]["laps"]=record_laps(receiver,data,status,log,len(calls))
                    a.write_json(log/"commands.json",calls)
                    if os.environ.get("PLANNER_BENCH_ROUTE_ACK"):
                        time.sleep(.3)
                        if len(calls)==2:
                            sinks.missing=True
                            deadline=time.monotonic()+5
                            while time.monotonic()<deadline:
                                failed=a.request(url,"GET",path,timeout=1)
                                if failed["failed_id"]==ident:break
                                time.sleep(.05)
                            a.write_json(log/"missing-ack.json",failed)
                            assert failed["failed_id"]==ident and "ACK timeout" in failed["error"]
                    if os.environ.get("PLANNER_BENCH_BANK"):
                        time.sleep(.25)
                    if os.environ.get("PLANNER_BENCH_WATCHDOG") and len(calls)==1:
                        probe=a.ssh(f"""set -eu
cd {remote}
p=$(pidof maia-delivery)
test "$(readlink /proc/$p/exe)" = "{remote}/maia-delivery"
trap 'kill -CONT $p' EXIT
kill -STOP "$p"
sleep 2
./obs-fl-ctl status
cat /sys/bus/iio/devices/iio:device0/out_altvoltage1_TX_LO_powerdown
cat /sys/bus/iio/devices/iio:device0/out_voltage0_hardwaregain""")
                        (log/"watchdog.txt").write_text(probe)
                        assert probe.splitlines()[-2]=="1" and float(probe.splitlines()[-1].split()[0])==-89.75
                        break
    finally:
        try:
            if mixer:
                (log/"lease-board.txt").write_text(a.ssh("ls -l /run/goofy; cat /run/goofy/goofy_lease.pid; pidof goofy_lease.sh; logread | tail -60"))
            (log/"owner.log").write_text(a.ssh(f"cd {remote}; kill -TERM $(cat owner.pid) 2>/dev/null || :; sleep 1; cat owner.log"))
        finally:
            stop.set()
            if thread: thread.join(3)
            sinks.close()


if __name__ == "__main__":
    bench.accepted.stage = stage_executables
    bench.accepted.video_smoke = control
    sys.argv.append("--reference")
    bench.main()
