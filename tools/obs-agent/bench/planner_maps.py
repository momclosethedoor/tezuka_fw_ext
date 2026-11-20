"""Verify the row99 next-map primitive with TX off and a shared RX catalog."""
import json
import os
import struct
import sys
import time

from planner_row99 import bench, stage_executables


def configure_tx_test(api, log, blob):
    # Digital-slot regression. Keep the RF gate off through calibration and recall.
    points = [2250000000 + 30000000*i for i in range(8)]
    rows = api.ssh(f"""set -eu
cd /sys/bus/iio/devices/iio:device0
t=out_altvoltage1_TX_LO
test "$(cat ${{t}}_powerdown)" = 1
old=$(cat ${{t}}_frequency)
trap 'echo "$old" > ${{t}}_frequency' EXIT
for hz in {' '.join(map(str,points))}; do
 echo "$hz" > ${{t}}_frequency
 usleep 100000
 echo 0 > ${{t}}_fastlock_store
 echo 0 > ${{t}}_fastlock_save
 echo "$(cat ${{t}}_frequency) $(cat ${{t}}_fastlock_save)"
done
test "$(cat ${{t}}_powerdown)" = 1""")
    bank=[]
    for hz, row in zip(points, rows.splitlines(), strict=True):
        actual,slot,values=row.split()
        profile=bytes(map(int,values.split(",")))
        assert abs(int(actual)-hz)<=10 and slot=="0" and len(profile)==16
        bank.append(profile)
    assert len(set(bank))==8
    (log/"tx-bank-readback.txt").write_text(rows)
    tail=40+3*4096
    struct.pack_into("<IIII",blob,tail,1,0,89750,0)
    blob[tail+24:tail+152]=b"".join(bank)
    for i in range(8):
        struct.pack_into("<I",blob,40+4096+32*i+4,4|((i%7)<<8))
        struct.pack_into("<I",blob,40+8192+32*i+4,4|(i<<8))


def exercise(log, profiles, boot):
    api = bench.accepted
    remote = api.REMOTE
    version = 7810
    blob = bytearray(api.make_map(profiles, 0, version))
    pages = 40 + 8192
    for i in range(8):
        other = 40 + 4096 + (7 - i) * 32
        blob[pages+32*i:pages+32*(i+1)] = blob[other:other+32]
    test_tx = os.environ.get("PLANNER_BENCH_TX_BANKS") == "1"
    if test_tx:
        configure_tx_test(api, log, blob)
        ninth = blob.copy()
        struct.pack_into("<I",ninth,40+4096+4,4|(8<<8))
        (log/"ninth.bin").write_bytes(ninth)
        api.upload(log/"ninth.bin",remote,"ninth.bin")
        refused=api.ssh(f"cd {remote}; ./obs-fl-ctl load ninth.bin 2>&1; echo rc=$?")
        assert "Invalid argument" in refused and "rc=1" in refused,refused
        (log/"ninth.txt").write_text(refused)
    bad = blob.copy()
    struct.pack_into("<I", bad, 20, version)
    (log/"stale.bin").write_bytes(bad)
    api.upload(log/"stale.bin",remote,"stale.bin")
    err = api.ssh(f"cd {remote}; ./obs-fl-ctl load stale.bin 2>&1; echo rc=$?")
    assert "Invalid argument" in err and "rc=1" in err, err
    (log/"maps.bin").write_bytes(blob)
    api.upload(log/"maps.bin",remote,"maps.bin")
    config = dict(setup=remote+"/maps.bin", replay=None, reader=dict(boot_id=boot,
        output=remote+"/rotation", command="rate", k=2, mode=1, seconds=6, warmup=.1,
        shift544=True, previous_mode=1, settling=0, batch_limit=64, direct=True,
        batch_sync=True, neon=True, hot=True, scan=True, trace_limit=20000))
    api.write_json(log/"rotation.json",config)
    api.upload(log/"rotation.json",remote,"rotation.json")
    api.ssh(f"cd {remote} && mkdir rotation && (timeout 12 ./maia-delivery --system-observations rotation.json --listen 192.168.2.1:18080 </dev/null >rotation.log 2>&1 &)")

    def wait_version(expected):
        deadline = time.monotonic()+4
        while time.monotonic()<deadline:
            try:
                status=api.request("http://192.168.2.1:18080","GET","/api/system/status",timeout=.5)
            except OSError:
                time.sleep(.02)
                continue
            fld=((status.get("reader") or {}).get("regs") or {}).get("fld",{})
            if fld.get("version")==expected and fld.get("map_epoch",0):
                return fld
            time.sleep(.02)
        raise AssertionError(f"map {expected} missing")

    first = wait_version(version)
    received = time.monotonic_ns()
    submitted = json.loads(api.ssh(f"cd {remote} && ./obs-fl-ctl next"))
    second = wait_version(version+1)
    elapsed = time.monotonic_ns()-received
    assert second["map_epoch"]>first["map_epoch"] and not second["errors"]
    if test_tx:
        assert first["tx_changes"]>8 and second["tx_changes"]>first["tx_changes"]
    api.write_json(log/"rotation-result.json",dict(stale=err,prior=first,next=second,
        returned=submitted,host_publish_and_poll_ns=elapsed,tx_bank_changed=False))
    deadline=time.monotonic()+10
    while time.monotonic()<deadline:
        guard=json.loads(api.ssh(f"cd {remote} && ./obs-fl-ctl status"))
        if guard["state"]==3:
            api.write_json(log/"guard-return.json",guard)
            assert guard["return_version"]==version+1 and guard["tx_powerdown_readback"]==1
            return
    raise AssertionError("SPI return missing")


if __name__ == "__main__":
    bench.accepted.stage = stage_executables
    bench.accepted.video_smoke = exercise
    sys.argv.append("--reference")
    bench.main()
