"""Summarize bounded ARM 27/9 captures using each clock in its own domain."""
import argparse
import json
import math
from pathlib import Path
import statistics


def percentiles(sequence):
    if not sequence:
        return None
    values=sorted(sequence)
    return dict(n=len(values),p50=statistics.median(values),
                p95=values[math.ceil(.95*len(values))-1],
                p99=values[math.ceil(.99*len(values))-1],max=values[-1])


def uncovered(lo, hi, intervals):
    result=[]
    if hi <= lo:
        return result
    for a,b in sorted(intervals):
        if b <= lo or a >= hi:
            continue
        if a > lo:
            result.append([lo,a])
        lo=max(lo,b)
        if lo>=hi:
            break
    if lo<hi:
        result.append([lo,hi])
    return result


def summarize(folder):
    calls=json.loads((folder/"commands.json").read_text())
    logs=[]
    for line in (folder/"owner.log").read_text(errors="replace").splitlines():
        if line.startswith('{"after"'):
            logs.append(json.loads(line))
    out=[]
    for i,c in enumerate(calls,1):
        if not c["command"].get("run"):
            continue
        item=dict(command=c["command"],error=c["status"]["error"])
        out.append(item)
        if item["error"] or not c["status"]["actual"]["running"]:
            continue
        actual=c["status"]["actual"]
        d=actual["spectrum"]
        item.update(generation=actual["generation"],version=d["version"],gate=actual["guard_tx"],
                    fresh_host_ms=c["fresh_host_ns"]/1e6,
                    preparation=d["preparation"],bank_apply=actual["bank_apply"],
                    laps=c["laps"])
        reader=next((x for x in logs if x["after"]["fld"]["version"]==d["version"]),None)
        if reader and reader.get("end"):
            u,v=reader["begin"],reader["end"]
            wall=v["time"]-u["time"]
            delta=v["consumed"]-u["consumed"]
            item["load"]=dict(wall_s=wall,arm_cpu_s=v["cpu_seconds"]-u["cpu_seconds"],
                cpu_us_per_record=(v["cpu_seconds"]-u["cpu_seconds"])*1e6/max(1,delta),
                rss=v["memory"],records=delta,tx_records=reader["udp_sent"],
                tx_dropped=reader["udp_drop"],datagrams=reader["udp_datagrams"],
                capture=reader["measured"],assembly=reader.get("sweep_network"),
                counters=dict(full_read_ns=int(v["full_read_ns"])-int(u["full_read_ns"]),
                              view_own_ns=int(v["view_own_ns"])-int(u["view_own_ns"]),
                              post_release_ns=int(v["after_release_ns"])-int(u["after_release_ns"])),
                send=reader["end_network"],iface_before=reader["begin_interface"],
                iface_after=reader["end_interface"],regs=v["regs"])
        if d.get("sweep_wire_version")!=1:
            continue
        item["maps"]={p:[sum(not r["transition_only"] and r["sweep_phase"]==p and r["rf_row"]==j
                           for r in d["positions"]) for j,_ in enumerate(c["command"]["ranges"])] for p in "AB"}
        item["coverage"]=[]
        for j,row in enumerate(c["command"]["ranges"]):
            if not row["enabled"]:
                continue
            spans=[ab for r in d["positions"] if not r["transition_only"] and r["rf_row"]==j
                   for ab in r["rf_ranges"]]
            a,b=row["lo_hz"],row["hi_hz"]
            item["coverage"].append(dict(request_hz=[a,b],uncovered_hz=uncovered(a,b,spans),
                channel_8m_start_gaps=uncovered(a,b-8_000_000+1,
                    [(x,y-8_000_000+1) for x,y in spans if y-x>=8_000_000])))
        passes=json.loads((folder/f"sweeps-{i}.json").read_text())["passes"]
        complete=[p for p in passes if p["delivery_complete"]]
        phases={}
        for phase in "AB":
            packets=[p for p in complete if p["phase"]==phase]
            phases[phase]=dict(expected=percentiles([p["expected"] for p in packets]),
                observation_span_ms=percentiles([(p["last_record_tick"]-p["first_record_tick"])/62500 for p in packets]),
                host_first_to_end_ms=percentiles([(p["host_ns"]-p["first_record_host_ns"])/1e6 for p in packets]),
                host_last_to_end_ms=percentiles([(p["host_ns"]-p["last_record_host_ns"])/1e6 for p in packets]),
                start_to_next_start_ms=percentiles([(q["first_record_tick"]-p["first_record_tick"])/62500
                    for p,q in zip(complete,complete[1:]) if p["phase"]==phase and q["sweep"]==p["sweep"]+1]))
        item["passes"]=dict(phases=phases,received=len(passes),complete=len(complete),
            noninitial_failed=[p for p in passes[1:] if not p["delivery_complete"]],
            pairs=sum(p["phase"]=="A" and q["sweep"]==p["sweep"]+1
                      for p,q in zip(complete,complete[1:])))
    (folder/"accept-027.json").write_text(json.dumps(out,indent=2)+"\n")
    return out


if __name__ == "__main__":
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("folder",type=Path)
    for row in summarize(parser.parse_args().folder):
        print(row["command"].get("view_delivery"),row.get("maps"),row.get("passes",{}).get("complete"),
              row.get("error"))
