"""Measure complete circuits from saved GVIEW4 records and the applied ARM map."""
import argparse
import json
from pathlib import Path
import statistics
import struct


def records(path, generation):
    with path.open("rb") as stream:
        while size := stream.read(4):
            count, = struct.unpack("<I", size)
            data = stream.read(count)
            assert len(data) == count and count % 576 == 0
            for offset in range(0, count, 576):
                magic, ordinal, _, epoch, tick, _, _, gen = struct.unpack_from("<8s7Q", data, offset)
                assert magic == b"GVIEW4\0\0"
                if gen == generation:
                    yield ordinal, epoch, tick


def analyze(folder, index, call):
    status = call["status"]
    actual = status["actual"]
    result = dict(command=call["command"], phase=status["phase"], error=status["error"])
    if not actual["running"] or status["error"]:
        result["failure"] = actual.get("failed_plan")
        return result
    spectrum = actual["spectrum"]
    positions = spectrum["positions"]
    n = len(positions)
    anchor = spectrum["map_epoch"]
    goal = [p["rf_ranges"][0] for p in positions if not p["transition_only"]]
    assert all(a[1] == b[0] for a, b in zip(goal, goal[1:]))
    request, = call["command"]["ranges"]
    assert (goal[0][0], goal[-1][1]) == (request["lo_hz"], request["hi_hz"])
    points = {}
    last = None
    missing = 0
    for ordinal, epoch, tick in records(folder/f"lap-{index}.dat", actual["generation"]):
        if epoch < anchor:
            continue
        if last is not None:
            assert ordinal > last
            missing += ordinal - last - 1
        last = ordinal
        points.setdefault(epoch, tick)
    useful = {i for i,p in enumerate(positions) if not p["transition_only"]}
    origin = min((i for i in useful if positions[i]["marker_route"] is None), default=min(useful))
    starts = [epoch for epoch in points if (epoch-anchor) % n == origin]
    def covered(start):
        return sum((epoch-anchor) % n in useful and epoch in points for epoch in range(start,start+n))
    runs = [dict(first_epoch=a, ms=(points[b]-points[a])/62500,
                 observed_useful=covered(a),
                 observed_positions=sum(e in points for e in range(a,b)))
            for a,b in zip(starts, starts[1:]) if b-a == n]
    complete = [run for run in runs if run["observed_useful"] == len(useful)]
    valid = {run["first_epoch"] for run in complete}
    ten = [(points[start+10*n]-points[start])/62500 for start in starts
           if start+10*n in points and all(start+i*n in valid for i in range(10))]
    overlap = []
    for i,p in enumerate(positions):
        if p["transition_only"] or p["marker_route"] is not None or p["hold_boundary"]:
            offset = (i-origin) % n
            times = [(points[s+offset]-points[s])/62500 for s in starts if s+offset in points]
            overlap.append(dict(position=i, rf=p["rf_ranges"], antenna=p["antenna"],
                                next_antenna=p["marker_route"], hold=p["hold_boundary"],
                                from_start_ms_median=statistics.median(times) if times else None))
    result.update(map_version=actual["map_version"],generation=actual["generation"], positions=n,timing_origin=origin,
                  pl_timing_only=spectrum.get("pl_timing_only",False),guard_tx=actual.get("guard_tx"),
                  rf_available_positions=sum(p["rx_available"] for p in positions),
                  useful=len(goal), rx=len({p["rx_lo_hz"] for p in positions}),
                  tx_plls_hz=sorted({p["tx_pll_hz"] for p in positions if p["mixer"]}),
                  overlap=overlap, apertures_hz=[goal[0][0],goal[-1][1]],
                  last_width_hz=goal[-1][1]-goal[-1][0], fresh_ms=call["fresh_host_ns"]/1e6,
                  preparation=spectrum["preparation"],apply=actual["bank_apply"],
                  missing_ordinals=missing, circuits=runs, complete_count=len(complete), ten_circuits_ms=ten)
    events = folder/"antenna.ndjson"
    pairs = {}
    if events.exists():
        rows=[json.loads(s) for s in events.read_text().splitlines()]
        rows=[r for r in rows if r["event"]["generation"]==actual["generation"]]
        for old,new in zip(rows,rows[1:]):
            a,b=old["event"],new["event"]
            key=(a["position"],b["position"],a["route"],b["route"],b["epoch"]-a["epoch"])
            pairs.setdefault(key,[]).append((new["host_ns"]-old["host_ns"])/1e6)
    result["event_intervals"]=[dict(from_pos=a,to_pos=b,from_target=m,to_target=nxt,visits=count,
                                    samples=len(times),host_ms_min_med_max=[min(times),statistics.median(times),max(times)])
                               for (a,b,m,nxt,count),times in pairs.items()]
    geometry = folder/f"applied-map-{index}.json"
    geometry.write_text(json.dumps(positions,indent=2),encoding="utf-8")
    result["applied_map"] = geometry.name
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("folder", type=Path)
    folder = parser.parse_args().folder
    calls = json.loads((folder/"commands.json").read_text())
    result = [analyze(folder,i,c) for i,c in enumerate(calls,1)]
    snapshots = [((c["status"].get("metrics") or {}).get("reader") or {}).get("regs",{}) for c in calls]
    for item in result:
        if "map_version" in item:
            matches = [r for r in snapshots if r.get("fld",{}).get("version") == item["map_version"]]
            if matches:
                regs = max(matches, key=lambda r:r["fld"]["pass"])
                item["pl_readback"] = regs["fld"]
                item["dma_ring_drop"] = [regs["dma_drop"],regs["ring_drop"]]
    (folder/"sweep-report.json").write_text(json.dumps(result,indent=2),encoding="utf-8")
    for item in result:
        times=[r["ms"] for r in item.get("circuits",[])]
        ten=item.get("ten_circuits_ms",[])
        print(item["phase"],item["error"],item.get("positions"),item.get("complete_count"),
              (min(times),statistics.median(times),max(times)) if times else None,
              statistics.median(ten) if ten else None)
