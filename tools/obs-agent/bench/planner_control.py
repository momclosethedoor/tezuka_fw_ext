"""Compare identical ARM requests on the fixed and generated direct banks."""
import json
import os
import socket
import struct
import threading
import sys
import time
import uuid
from pathlib import Path
from urllib.error import HTTPError

from planner_row99 import bench, stage_executables
from planner_sinks import Sinks
from planner_laps import Laps
from planner_sweeps import Sweeps


def record_laps(receiver,data,status,folder,index):
    load_test = os.environ.get("PLANNER_BENCH_LOAD") == "1"
    wide = "PLANNER_BENCH_SWEEP" in os.environ
    timing_position = 0
    if wide:
        positions = status["actual"]["spectrum"]["positions"]
        timing_position = next((i for i,p in enumerate(positions)
                                if not p["transition_only"] and p["marker_route"] is None), 0)
    laps = Laps(status["actual"], timing_position)
    sweep = None
    if status["actual"]["spectrum"].get("sweep_wire_version") == 1:
        sweep = Sweeps(status["actual"],folder/f"sweeps-{index}.json")
    began=time.monotonic()
    deadline=began+(15 if wide else 8 if load_test else 3.5)
    minimum=float(os.environ.get("PLANNER_BENCH_MIN_SECONDS","0"))
    assert 0 <= minimum <= 10
    limit = int(os.environ.get("PLANNER_BENCH_LAPS", "12"))
    assert 12 <= limit <= 64
    with (folder/f"lap-{index}.dat").open("wb") as f:
        while time.monotonic()<deadline:
            if sweep:
                sweep.accept(data)
            if data[:8]==b"GVIEW4\0\0":
                f.write(struct.pack("<I",len(data))+data)
                laps.accept(data)
                if wide and len(laps.ticks)>=limit+1 and time.monotonic()-began>=minimum:break
                if not wide and not load_test and len(laps.ticks)>=5 and len(laps.seen)==laps.n:break
            data,_=receiver.recvfrom(65535)
    return dict(laps.result(),last_ordinal=laps.last,**(sweep.result() if sweep else {}))


def receive_after_pause(receiver,status,folder,last):
    receiver.setsockopt(socket.SOL_SOCKET,socket.SO_RCVBUF,32<<10)
    pause_ns=int(os.environ["PLANNER_BENCH_RX_PAUSE_MS"])*1_000_000
    start=time.perf_counter_ns()
    time.sleep(pause_ns/1e9)
    resume=time.perf_counter_ns()
    laps=Laps(status["actual"])
    laps.last=last
    sweep=Sweeps(status["actual"],folder/"pause-passes.json")
    until=time.perf_counter_ns()+1_200_000_000
    while time.perf_counter_ns()<until:
        data,_=receiver.recvfrom(65535)
        sweep.accept(data)
        if data[:8]==b"GVIEW4\0\0":
            laps.accept(data)
    receiver.setsockopt(socket.SOL_SOCKET,socket.SO_RCVBUF,4<<20)
    return dict(laps.result(),**sweep.result(),paused_ms=(resume-start)/1e6)


def start_sig0_replay(api, remote, index):
    # Fixed cadence in the device's own monotonic clock. If a job overruns,
    # skip deadlines so the load never becomes a catch-up burst.
    api.ssh(f"""cd {remote}
(timeout 9 sh -c '
ms() {{ awk "{{print int(\$1*1000)}}" /proc/uptime; }}
next=$(ms)
for i in 0 1 2 3 4 5 6 7
do
  start=$(ms)
  if [ "$start" -lt "$next" ]; then usleep $(((next-start)*1000)); fi
  start=$(ms)
  ./sig0-arm replay-window.bin replay-result-{index} replay-stages-{index} >/dev/null 2>&1
  rc=$?
  end=$(ms)
  echo "$i $start $end $rc"
  next=$((next+250))
  while [ "$next" -lt "$end" ]; do next=$((next+250)); done
done' </dev/null >sig0-replay-{index}.txt 2>&1 &)
""")


def lease_loop(stop, ready,log, silence):
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
            if not silence.is_set():
                lease.service(now)
            if now>=will.next_due:
                will.service(now)
            if lease.lease is True and will.armed:
                ready.set()
            if now>=next_log:
                with (log/"lease.ndjson").open("a") as out:
                    print(json.dumps({"host_s":now,"lease":lease.lease,"will":will.armed,"state":lease._raw,
                                      "heartbeat_sent":lease.sent,"heartbeat_missed":lease.missed}),file=out)
                next_log=now+.25
            stop.wait(.025)
    finally:
        lease.close(time.perf_counter())
        will.close(graceful=True)


def verify_recovery(api, url, path, scan, log):
    """Exercise interrupted calibration and reconnect using the public interface."""
    def submit(command):
        ident = uuid.uuid4().hex
        response = api.request(url, "POST", path, dict(command, id=ident), timeout=2)
        assert response["accepted_id"] == ident
        return ident

    def wait_for(condition):
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            status = api.request(url, "GET", path, timeout=1)
            if condition(status):
                return status
            time.sleep(.02)
        raise AssertionError(f"last phase {status['phase']}, error {status['error']}")

    long_plan = dict(scan, ranges=[dict(lo_hz=80000000, hi_hz=1800000000, enabled=True)])
    pending = submit(long_plan)
    preparing = wait_for(lambda s: s["pending_id"] == pending and s["phase"] == "calibrating")
    assert not preparing["actual"]["running"]
    time.sleep(.35)
    stop = submit(dict(scan, run=False))
    stopped = wait_for(lambda s: s["applied_id"] == stop)
    assert stopped["failed_id"] == pending and not stopped["actual"]["running"]
    rejected_ranges = [dict(lo_hz=2400000000,hi_hz=2410000000,enabled=True),
                       dict(lo_hz=2410000000,hi_hz=2415000000,enabled=True)]
    rejected = submit(dict(scan, ranges=rejected_ranges, antenna_bands={"0": [[2402000000,2408000000]]}))
    failure = wait_for(lambda s: s["failed_id"] == rejected)
    assert "missing antenna coverage" in failure["error"] and not failure["actual"]["running"]
    assert failure["actual"]["failed_plan"]["applied_rf_ranges"] == []
    assert failure["actual"]["failed_plan"]["antenna_gaps_hz"] == [
        [2400000000,2402000000],[2408000000,2410000000],[2410000000,2415000000]]
    assert len(failure["actual"]["failed_plan"]["unapplied_rf_ranges"]) == 2
    ranges = [[2410000000,2412000000], [2412000000,2413000000],
              [2411000000,2412500000], [2449000000,2449000001]]
    ordered = submit(dict(scan, ranges=[dict(lo_hz=a,hi_hz=b,enabled=True) for a,b in ranges]+
                          [dict(lo_hz=5999000000,hi_hz=6000000000,enabled=False)],
                          antenna_bands={"0":[[2399000000,2450000000]]}))
    ordered_state = wait_for(lambda s:s["applied_id"]==ordered)
    assert [p["rf_ranges"][0] for p in ordered_state["actual"]["spectrum"]["positions"]] == ranges
    cross = submit(dict(scan, ranges=[dict(lo_hz=5999000000,hi_hz=6001000000,enabled=True)],
                        antenna_bands={"0":[[5900000000,6100000000]]}))
    mixed = wait_for(lambda s:s["failed_id"]==cross)
    assert "cannot gate TX" in mixed["error"] and not mixed["actual"]["running"]
    resumed = submit(dict(scan, antenna_bands={"0": [[2400000000,2450000000]]}))
    live = wait_for(lambda s: s["applied_id"] == resumed and s["actual"]["running"])
    time.sleep(.12)
    # request() opens a new connection each time. Reconstruct exclusively from GET.
    connected = api.request(url, "GET", path, timeout=1)
    assert connected["actual"]["generation"] == live["actual"]["generation"]
    assert connected["actual"]["spectrum"] == live["actual"]["spectrum"]
    try:
        submit(dict(op="alternative", source_position=0, source_generation=live["actual"]["generation"]-1,
                    lo_hz=2400000000, hi_hz=2400100000))
    except HTTPError as e:
        assert e.code == 409
    else:
        raise AssertionError("stale generation accepted")
    end = submit(dict(scan, run=False))
    wait_for(lambda s: s["applied_id"] == end)
    api.write_json(log/"recovery.json", dict(preparing=preparing, stopped=stopped,
        failure=failure, ordered=ordered_state, mixed=mixed, reconnected=connected, stale_http=409))


def control(log, profiles, boot):
    a = bench.accepted
    v2 = os.environ.get("PLANNER_BENCH_VERSION", "2") == "2"
    remote = a.REMOTE
    url = "http://192.168.2.1:18080"
    path = "/api/system/control"
    if source := os.environ.get("PLANNER_BENCH_LEASE_SCRIPT"):
        lease_script = log/"lease-overlay.sh"
        original = Path(source).read_text(encoding="utf-8")
        assert original.startswith("#!/bin/sh\n")
        lease_script.write_bytes(original.replace("#!/bin/sh\n", f"#!/bin/sh\nexport GOOFY_LEASE_TRACE={remote}/lease-board.ndjson\n", 1).encode())
        a.upload(lease_script, remote, lease_script.name)
        a.ssh(f"""set -eu
cd {remote}
sha256sum /usr/sbin/goofy_lease.sh lease-overlay.sh
chmod 755 lease-overlay.sh
old=$(cat /run/goofy/goofy_lease.pid)
test "$(cat /proc/$old/comm)" = goofy_lease.sh
mount -o bind "$PWD/lease-overlay.sh" /usr/sbin/goofy_lease.sh
kill -TERM "$old"
for i in $(seq 1 60); do
  new=$(cat /run/goofy/goofy_lease.pid 2>/dev/null || :)
  if test -n "$new" && test "$new" != "$old" && kill -0 "$new"; then exit 0; fi
  usleep 100000
done
exit 1""")
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as route:
        route.connect(("192.168.2.1", 18010))
        ip = route.getsockname()[0]
    scan = dict(op="scan", run=True, k=2, ranges=[
        dict(lo_hz=2400000000, hi_hz=2448000000, enabled=True)])
    mixer=os.environ.get("PLANNER_BENCH_ROUTE")=="mixer"
    if mixer:
        assert v2
        scan["ranges"]=[dict(lo_hz=6748000000,hi_hz=6752000000,enabled=True)]
        if os.environ.get("PLANNER_BENCH_EDGE") == "1":
            scan["ranges"] = [dict(lo_hz=6000000001, hi_hz=6002000001, enabled=True)]
        if os.environ.get("PLANNER_BENCH_LOWER"):
            scan["mixer"] = {"pll_range_hz": [4750000000, 4760000000]}
        if os.environ.get("PLANNER_BENCH_SOURCE58"):
            scan["ranges"]=[dict(lo_hz=5864000000,hi_hz=5866500000,path="mixer",enabled=True)]
    analysis = dict(op="analysis", center_hz=2425000000, width_hz=24000000,
                    k=700, gain_db=35, agc="manual")
    cfg = dict(boot_id=boot, output=remote+"/control", view=ip+":18010",
               video=ip+":18009", route=ip+":18008", scan=scan, analysis=analysis)
    if os.environ.get("PLANNER_BENCH_FIELD"):
        collector = Path(sys.argv[sys.argv.index("--agent") + 1]) / "obs-field1"
        a.upload(collector, remote, "obs-field1")
        a.ssh(f"cd {remote} && chmod 700 obs-field1")
        scan["ranges"] = [dict(lo_hz=2424000000, hi_hz=2426000000, enabled=True)]
        cfg["field_preview"] = dict(collector=remote+"/obs-field1", target_hz=2425000000, timeout_ms=90)
    if v2:
        scan["planner_version"] = 2
        if "PLANNER_BENCH_GATE_TICKS" in os.environ:
            scan["xtg1_wait_ticks"] = int(os.environ["PLANNER_BENCH_GATE_TICKS"])
        if geometry := os.environ.get("PLANNER_BENCH_GEOMETRY"):
            scan["sweep_geometry"] = geometry
        scan["catalog_on_boot"] = os.environ.get("PLANNER_BENCH_CATALOG") == "1"
        if os.environ.get("PLANNER_BENCH_TIMING_ONLY") == "1":
            scan["bench_timing_only"] = True
        if os.environ.get("PLANNER_BENCH_PATH_ACK") == "1":
            scan["path_ack_version"] = 1
    else:
        cfg.update(setup=remote+"/control-map.bin", positions=bench.FREQUENCIES)
    a.write_json(log/"cli-config.json", cfg)
    a.upload(log/"cli-config.json", remote, "cli-config.json")
    if replay_window := os.environ.get("PLANNER_BENCH_SIG0_REPLAY"):
        a.upload(Path(replay_window),remote,"replay-window.bin")
    owner_seconds = 215 if scan.get("catalog_on_boot") else 135
    phase_timing = int(os.environ.get("PLANNER_BENCH_PHASE_TIMING") == "1")
    if os.environ.get("PLANNER_BENCH_RECORD_GATE"):
        a.ssh(f"""cd {remote}
(timeout 25 sh -c 'last=
while :; do
  v=$(./obs-fl-ctl gate-status) || break
  if [ "$v" != "$last" ]; then printf "%s %s\\n" "$(cut -d" " -f1 /proc/uptime)" "$v"; last=$v; fi
  usleep 2500
done' </dev/null >gate-probe.ndjson 2>&1 &)
""")
    trace = "OBS_CONTROL_TRACE=1 " if os.environ.get("PLANNER_BENCH_CONTROL_TRACE") else ""
    a.ssh(f"cd {remote} && ({trace}OBS_PHASE_TIMING={phase_timing} timeout {owner_seconds} ./maia-delivery --system-control cli-config.json --listen 192.168.2.1:18080 </dev/null >owner.log 2>&1 & echo $! > owner.pid)")
    calls = []
    stop,ready,silence=threading.Event(),threading.Event(),threading.Event()
    thread=None
    sinks=Sinks(ip,log)
    try:
        if mixer:
            thread=threading.Thread(target=lease_loop,args=(stop,ready,log,silence),daemon=True)
            thread.start()
            assert ready.wait(8),"no live TX lease"
        for i in range(50):
            try:
                a.request(url, "GET", path, timeout=.5)
                break
            except OSError:
                time.sleep(.1)
        if scan.get("catalog_on_boot"):
            deadline = time.monotonic()+90
            while time.monotonic()<deadline:
                startup = a.request(url,"GET",path,timeout=2)
                if startup["phase"] in ("failed","stopped"):
                    a.write_json(log/"startup-catalog.json", startup)
                    assert startup["actual"].get("catalog"), startup.get("error")
                    break
                time.sleep(.15)
            else:
                raise RuntimeError("boot catalog did not finish")
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as receiver:
            receiver.bind((ip,18010))
            receiver.setsockopt(socket.SOL_SOCKET,socket.SO_RCVBUF,4<<20)
            receiver.settimeout(2)
            cmds=[scan,scan,dict(scan,k=1),dict(scan,k=1023),analysis,scan,dict(scan,run=False)]
            if os.environ.get("PLANNER_BENCH_LOAD") == "1":
                cmds = [scan, dict(scan, run=False)]
            if os.environ.get("PLANNER_BENCH_ALTERNATIVE")=="1":
                cmds[1:1]=[dict(op="alternative"),dict(op="cancel_alternative")]
            if os.environ.get("PLANNER_BENCH_VIDEO"):
                cmds[-1:-1]=[analysis,dict(op="video",center_hz=2425000000,width_hz=6000000),dict(op="return")]
            if os.environ.get("PLANNER_BENCH_BANK"):
                cmds=[dict(scan,ranges=[dict(lo_hz=70000000,hi_hz=70000000+n*23_529_940,enabled=True)])
                      for n in (127,128,129)]+[dict(scan,run=False)]
            if os.environ.get("PLANNER_BENCH_FIELD"):
                cmds = [scan, dict(op="video", center_hz=2425000000, width_hz=6000000),
                        dict(op="return"), dict(scan, run=False)]
            if os.environ.get("PLANNER_BENCH_ROUTE_ACK"):
                ranges=[dict(lo_hz=2300000000,hi_hz=2600000000,enabled=True)]
                routes=dict(scan,ranges=ranges,antenna_bands={"0":[[2300000000,2450000000]],
                    "1":[[2395000000,2530000000]],"2":[[2515000000,2600000000]]})
                cmds=[routes,dict(routes,k=1),dict(scan,run=False)]
            if os.environ.get("PLANNER_BENCH_PATH_RETURN"):
                direct=dict(scan,ranges=[dict(lo_hz=2400000000,hi_hz=2402000000,enabled=True)],
                            antenna_bands={"2":[[2390000000,2410000000]]})
                cmds=[scan,direct,dict(direct,run=False)]
            sweep = os.environ.get("PLANNER_BENCH_SWEEP")
            if sweep:
                cases = json.loads(Path(sweep).read_text(encoding="utf-8-sig"))
                cmds = [case if case.get("op")=="antennas" else dict(scan, **case) for case in cases] + [dict(scan,run=False)]
            for cmd in cmds:
                if cmd["op"]=="alternative":
                    prev=calls[-1]["status"]["actual"]
                    lo,hi=prev["spectrum"]["positions"][0]["rf_ranges"][0]
                    cmd.update(source_position=0,source_generation=prev["generation"],lo_hz=lo,hi_hz=min(hi,lo+2000000))
                if cmd["op"]=="cancel_alternative":
                    cmd["request_id"]=calls[-1]["status"]["applied_id"]
                ident = uuid.uuid4().hex
                start = time.perf_counter_ns()
                accepted = a.request(url,"POST",path,dict(cmd,id=ident),timeout=2)
                assert accepted["accepted_id"] == ident
                while time.perf_counter_ns()-start < (75 if sweep else 35)*1_000_000_000:
                    status = a.request(url,"GET",path,timeout=5 if sweep else 1)
                    if ident in (status["applied_id"],status["failed_id"]): break
                    time.sleep(.02)
                calls.append(dict(command=cmd,status=status,host_elapsed_ns=time.perf_counter_ns()-start))
                a.write_json(log/"commands.json",calls)
                if sweep and status["failed_id"]==ident:
                    print("Sweep rejected:",status["error"],flush=True)
                    continue
                if os.environ.get("PLANNER_BENCH_PATH_TIMEOUT"):
                    assert status["failed_id"]==ident and not status["actual"]["running"]
                    assert "ACK timeout" in status["error"]
                    receiver.settimeout(.2)
                    try:
                        receiver.recvfrom(65535)
                    except socket.timeout:
                        break
                    raise AssertionError("VIEW arrived without initial path ACK")
                assert status["applied_id"] == ident, status.get("error")
                if cmd.get("run") and scan.get("bench_timing_only"):
                    actual=status["actual"]
                    assert actual["spectrum"]["pl_timing_only"] and not actual["spectrum"]["rf_ranges"]
                    assert not any(p["rx_available"] for p in actual["spectrum"]["positions"])
                    assert actual["guard_tx"]["powerdown"]==1
                if os.environ.get("PLANNER_BENCH_PATH_RETURN") and len(calls)==2:
                    actual=status["actual"]
                    pos=actual["spectrum"]["positions"][0]
                    route=actual["spectrum"]["path_confirmation"]
                    assert route["required_path"]=="direct" and route["antenna"]==2 and route["confirmed_by_client"]
                    assert pos["antenna"]==2 and not pos["mixer"] and actual["guard_tx"]["powerdown"]==1
                if os.environ.get("PLANNER_BENCH_LOWER") and cmd["op"] == "scan" and cmd["run"]:
                    for position in status["actual"]["spectrum"]["positions"]:
                        assert position["side_sign"] == -1, position
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
                    calls[-1].update(stale_datagrams_discarded=discarded,fresh_host_ns=time.perf_counter_ns()-start)
                    if replay_window and cmd.get("test_sig0_replay"):
                        start_sig0_replay(a,remote,len(calls))
                    if status["actual"]["mode"]=="scan" and not os.environ.get("PLANNER_BENCH_ROUTE_ACK"):
                        calls[-1]["laps"]=record_laps(receiver,data,status,log,len(calls))
                    if len(calls)==1 and os.environ.get("PLANNER_BENCH_RX_PAUSE_MS"):
                        calls[-1]["pause"]=receive_after_pause(receiver,status,log,calls[-1]["laps"]["last_ordinal"])
                    if os.environ.get("PLANNER_BENCH_RECORD_GATE") and status["actual"]["guard_tx"].get("per_visit"):
                        receipts = []
                        for _ in range(36):
                            t0=time.perf_counter_ns()
                            readback=a.request(url,"GET",path,timeout=1)
                            tx=readback["actual"]["tx"]
                            receipts.append(dict(tx=tx,rtt_ns=time.perf_counter_ns()-t0,
                                generation=readback["actual"]["generation"],error=readback["error"]))
                            time.sleep(.021)
                        a.write_json(log/f"gate-{len(calls)}.json",receipts)
                    a.write_json(log/"commands.json",calls)
                    if os.environ.get("PLANNER_BENCH_MISSING_ACK4") and len(calls)==1:
                        sinks.missing=True
                        withheld=time.perf_counter_ns()
                        while time.perf_counter_ns()-withheld<4_000_000_000:
                            failed=a.request(url,"GET",path,timeout=1)
                            if failed["failed_id"]==ident:
                                break
                            time.sleep(.02)
                        else:
                            raise AssertionError("no failure after missing ACK4")
                        elapsed=time.perf_counter_ns()-withheld
                        returned=json.loads(a.ssh(f"cd {remote} && ./obs-fl-ctl status"))
                        a.write_json(log/"missing-ack4.json",dict(status=failed,guard=returned,
                            host_upper_bound_ns=elapsed))
                        assert "route ACK timeout after retry" in failed["error"]
                        assert not failed["actual"]["running"]
                        assert returned["fld_control"]==0 and returned["tx_powerdown_readback"]==1
                        sinks.missing=False
                    if os.environ.get("PLANNER_BENCH_STOP_WHILE_ROUTE") and len(calls)==1:
                        sinks.missing=True
                        end=time.monotonic()+1.5
                        while time.monotonic()<end:
                            pending=a.request(url,"GET",path,timeout=1)
                            if (pending.get("route_pending") or {}).get("ack_version")==4:
                                a.write_json(log/"stop-during-route.json",pending)
                                break
                            time.sleep(.01)
                        else:
                            raise AssertionError("no active ACK4 boundary before STOP")
                    if os.environ.get("PLANNER_BENCH_EDGE") == "1" and len(calls)==1:
                        actual = status["actual"]
                        pos = actual["spectrum"]["positions"][0]
                        events = log/"antenna.ndjson"
                        readback = dict(request=cmd, rf_ranges=pos["rf_ranges"], generation=actual["generation"],
                            fresh_generation=struct.unpack_from("<Q", data, 56)[0], guard=actual["guard_tx"],
                            mixer_lo_hz=pos["mixer_lo_hz"], rx_lo_hz=pos["rx_lo_hz"], marker_route=pos["marker_route"],
                            route_events=events.read_text().splitlines() if events.exists() else [],
                            physical_switch_verified=False)
                        readback["path_confirmation"] = actual["spectrum"].get("path_confirmation")
                        path_events=log/"path-events.ndjson"
                        readback["path_events"] = [json.loads(s) for s in path_events.read_text().splitlines()] if path_events.exists() else []
                        a.write_json(log/"edge-start.json",readback)
                        assert pos["mixer"] and readback["guard"]["powerdown"]==0
                        assert readback["generation"]==readback["fresh_generation"]
                    if os.environ.get("PLANNER_BENCH_LEASE_LOSS") == str(len(calls)):
                        time.sleep(6)
                        healthy=a.request(url,"GET",path,timeout=1)
                        assert healthy["actual"]["running"] and healthy["phase"]=="running", healthy["error"]
                        silence.set()
                        start_loss=time.perf_counter_ns()
                        while time.perf_counter_ns()-start_loss < 8_000_000_000:
                            expired=a.request(url,"GET",path,timeout=1)
                            if expired["failed_id"]==ident:break
                            time.sleep(.05)
                        assert expired["failed_id"]==ident and not expired["actual"]["running"]
                        gate=a.ssh("cat /sys/bus/iio/devices/iio:device0/out_altvoltage1_TX_LO_powerdown /sys/bus/iio/devices/iio:device0/out_voltage0_hardwaregain")
                        api_elapsed=time.perf_counter_ns()-start_loss
                        a.write_json(log/"host-lease-loss.json",dict(status=expired,physical=gate,
                            host_upper_bound_ns=api_elapsed))
                        assert gate.splitlines()[0]=="1" and float(gate.splitlines()[1].split()[0])==-89.75
                        break
                    if os.environ.get("PLANNER_BENCH_FIELD") and len(calls)==1:
                        actual = status["actual"]
                        watch = actual["field_watch"]
                        assert watch["target_hz"] == 2425000000, watch
                        deadline = time.monotonic() + 8
                        while actual["generation"] not in sinks.fields and time.monotonic()<deadline:
                            time.sleep(.02)
                        meta = sinks.fields.get(actual["generation"])
                        if meta is None and os.environ.get("PLANNER_BENCH_FIELD_OPTIONAL"):
                            a.write_json(log/"field-observation.json",dict(watch=watch,end_received=False))
                        else:
                            assert meta and meta["map_version"] == watch["version"], (watch,meta)
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
            if os.environ.get("PLANNER_BENCH_RECOVERY"):
                verify_recovery(a, url, path, scan, log)
    finally:
        try:
            if replay_window:
                for i,c in enumerate(calls,1):
                    if c["command"].get("test_sig0_replay"):
                        (log/f"sig0-replay-{i}.txt").write_text(a.ssh(f"cd {remote} && cat sig0-replay-{i}.txt"))
            if os.environ.get("PLANNER_BENCH_RECORD_GATE"):
                (log/"gate-probe.ndjson").write_text(a.ssh(f"cd {remote} && cat gate-probe.ndjson"))
            if mixer:
                (log/"lease-board.txt").write_text(a.ssh("ls -l /run/goofy; cat /run/goofy/goofy_lease.pid; pidof goofy_lease.sh; logread | tail -60"))
            if os.environ.get("PLANNER_BENCH_LEASE_SCRIPT"):
                (log/"lease-board.ndjson").write_text(a.ssh(f"cat {remote}/lease-board.ndjson"))
            (log/"owner.log").write_text(a.ssh(f"cd {remote}; kill -TERM $(cat owner.pid) 2>/dev/null || :; sleep 1; cat owner.log"))
            if os.environ.get("PLANNER_BENCH_VERIFY_HANDBACK"):
                returned = json.loads(a.ssh(f"cd {remote} && ./obs-fl-ctl status"))
                radio = a.ssh("cat /sys/bus/iio/devices/iio:device0/out_altvoltage1_TX_LO_powerdown "
                              "/sys/bus/iio/devices/iio:device0/out_voltage0_hardwaregain").splitlines()
                a.write_json(log/"handback.json",dict(guard=returned,radio=radio))
                assert returned["fld_control"] == 0
                assert radio[0] == "1" and float(radio[1].split()[0]) == -89.75
                if os.environ.get("PLANNER_BENCH_TPC2_DIAGNOSTIC"):
                    ident, word = a.ssh("devmem 0x7c46033c 32; devmem 0x7c460344 32").splitlines()
                    assert int(ident,16)==0x32435054
                    a.write_json(log/"tpc2-handback.json",dict(id=ident,word=word))
        finally:
            stop.set()
            if thread: thread.join(3)
            sinks.close()


if __name__ == "__main__":
    bench.accepted.stage = stage_executables
    bench.accepted.video_smoke = control
    sys.argv.append("--reference")
    bench.main()
