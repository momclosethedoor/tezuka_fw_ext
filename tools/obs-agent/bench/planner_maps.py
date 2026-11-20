"""Verify the row99 next-map primitive with TX off and a shared RX catalog."""
import json
import struct
import sys
import time

from planner_row99 import bench, stage_executables


def exercise(log, profiles, boot):
    api = bench.accepted
    remote = api.REMOTE
    version = 7810
    blob = bytearray(api.make_map(profiles, 0, version))
    pages = 40 + 8192
    for i in range(8):
        other = 40 + 4096 + (7 - i) * 32
        blob[pages+32*i:pages+32*(i+1)] = blob[other:other+32]
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
