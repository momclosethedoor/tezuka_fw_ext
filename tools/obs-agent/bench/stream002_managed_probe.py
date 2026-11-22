"""Measure full product-map, zero-FIELD managed validation windows."""
import json
import threading
import time

from egress_probe import HOST, Probe, Receiver, lease_loop


SCAN = dict(op="scan", planner_version=2, path_ack_version=1, sweep_geometry="27/9",
            xtg1_wait_ticks=256, catalog_on_boot=True, run=True, k=2,
            ranges=[dict(lo_hz=70_000_000, hi_hz=12_000_000_000, enabled=True)])


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
    return dict(phase=status["phase"], error=status["error"],
                generation=status["actual"]["generation"], progress=status["progress"], reader=reader)


def mode_regressions(stand, probe, receiver, log, analysis):
    scan = dict(SCAN, view_bins=512)
    analysis = dict(analysis, view_bins=1024)
    video = dict(op="video", center_hz=5_865_000_000, width_hz=8_000_000)
    templates = {"scan":scan, "analysis":analysis, "video":video}
    results = []
    for start, middle, end in (("scan","analysis","scan"), ("scan","video","scan"),
                              ("analysis","video","analysis")):
        stages = []
        previous = None
        for index, mode in enumerate((start, middle, end)):
            command = {"op":"return"} if index == 2 and middle == "video" else templates[mode]
            active = probe.command(f"{start}-{middle}-{end}-{index}", command, seconds=45)
            gen = active.get("generation")
            if active["mode"] != mode or active["running"] is not True or gen == previous:
                raise RuntimeError(f"{mode} was not applied with a new generation")
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
                               tx=active["tx"],guard_tx=active.get("guard_tx")))
            previous = gen
        probe.command(f"stop-{start}-{middle}-{end}", dict(SCAN, run=False), seconds=12)
        results.append(dict(sequence=[start,middle,end],stages=stages))
        stand.write_json(log/"mode-regressions.json",results)


def run(stand, args, log, boot):
    remote = stand.REMOTE
    install_lease(stand, args)
    config = log / "owner.json"
    stopped = dict(SCAN, run=False)
    analysis = dict(op="analysis", center_hz=5_865_000_000, width_hz=24_000_000, k=700,
                    gain_db=35, agc="manual")
    stand.write_json(config, dict(boot_id=boot, output=remote+"/control", scan=stopped,
        analysis=analysis, view=f"{HOST}:18010", route=f"{HOST}:18008", video=f"{HOST}:18009",
        bench_obn3_layout=args.bench_obn3_layout))
    stand.upload(config, remote, config.name)
    receiver = Receiver(log)
    halted, ready = threading.Event(), threading.Event()
    lease = threading.Thread(target=lease_loop, args=(halted, ready, log, threading.Event()), daemon=True)
    lease.start()
    probe = Probe(stand, receiver, log)
    rows = []
    try:
        if not ready.wait(8):
            raise RuntimeError("accepted scanner lease/will unavailable")
        stand.ssh(f"cd {remote}; (OBS_PHASE_TIMING=1 timeout 270 ./maia-delivery --system-control owner.json "
                  "--listen 0.0.0.0:18080 </dev/null >probe-agent.log 2>&1 &)")
        probe.until(lambda s: isinstance(s["actual"].get("catalog"), dict), 100)
        if getattr(args,"regression",False):
            mode_regressions(stand,probe,receiver,log,analysis)
            return
        for width in args.widths:
            label = f"wide-{width}"
            active = probe.command(label, dict(SCAN, view_bins=width), seconds=40)
            if active["running"] is not True or active.get("view_bins") != width:
                raise RuntimeError(f"{label} did not start: {probe.cases[-1]['status']['error']}")
            stand.write_json(log / f"{label}-descriptor.json", active["spectrum"])
            time.sleep(1)
            generation = active["generation"]
            start = time.monotonic()
            snapshots = []
            arm_begin = stand.ssh("cat /proc/stat /proc/net/snmp; pidof maia-delivery | xargs -r -I{} cat /proc/{}/stat")
            host_begin = dict(receiver.view_stats(generation))
            while time.monotonic()-start < args.seconds:
                sample = probe.request()
                snapshots.append(summary(sample))
                if sample["actual"]["running"] is not True or sample["actual"]["generation"] != generation:
                    break
                target = start+len(snapshots)
                time.sleep(max(0,target-time.monotonic()))
            host_end = dict(receiver.view_stats(generation))
            elapsed = time.monotonic()-start
            arm_end = stand.ssh("cat /proc/stat /proc/net/snmp; pidof maia-delivery | xargs -r -I{} cat /proc/{}/stat")
            row = dict(view=width, seconds=elapsed, generation=generation, samples=snapshots,
                       host_begin=host_begin, host_end=host_end, arm_begin=arm_begin, arm_end=arm_end)
            rows.append(row)
            probe.command(f"stop-{width}", dict(stopped, view_bins=width), seconds=12)
            row["drained"]=summary(probe.request())
            stand.write_json(log / "windows.json", rows)
            print(json.dumps(dict(view=width, elapsed_s=elapsed, a=host_end["A"]-host_begin["A"],
                b=host_end["B"]-host_begin["B"])),flush=True)
    finally:
        halted.set()
        lease.join(4)
        receiver.close()
        probe.save()
