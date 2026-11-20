"""Summarize saved measurements. Power ratios refer to native linear FFT units."""
import argparse
import json
from pathlib import Path
import re
import statistics
import struct

import numpy as np
from planner_laps import from_capture


def powers(folder,row):
    raw=np.fromfile(folder/row["file"],dtype="<f4").reshape(-1,4096)
    return raw[row["discard_prefix_frames"]:]


def rf(root):
    result={}
    for name in ["mixer-01","holdout-6750","source-58-mixer"]:
        folder=root/name
        rows=json.loads((folder/"series.json").read_text())
        pairs=[]
        for on in (r for r in rows if r["tx"].get("lo_powerdown")==0):
            p=on["nominal_pll_hz"]
            rx=on["arm"]["ad9361"]["rx_lo_frequency"]
            off=next(r for r in rows if r["nominal_pll_hz"]==p and r["tx"].get("powerdown")==1 and
                     r["arm"]["ad9361"]["rx_lo_frequency"]==rx)
            f=rx+(np.arange(4096)-2048)*15000
            target=5865190000 if name=="source-58-mixer" else 6750000000
            rf_bins=f+2*on["tx"]["tone_hz"]
            band=np.flatnonzero((rf_bins>=target-2000000)&(rf_bins<target+2000000))
            a,b=powers(folder,on).mean(0),powers(folder,off).mean(0)
            if len(band)<265:continue
            peak=band[a[band].argmax()]
            pairs.append(dict(pll_hz=p,rx_hz=rx,bins=int(len(band)),rf_peak_hz=int(rf_bins[peak]),
                              peak_on_db=float(10*np.log10(a[peak])),
                              peak_off_db=float(10*np.log10(b[band].max())),file=on["file"]))
        result[name]=pairs
    return result


def control(folder):
    calls=json.loads((folder/"commands.json").read_text())
    out=[]
    for i,c in enumerate(calls,1):
        s=c["status"]
        a=s["actual"]
        spec=a["spectrum"] or {}
        tm=s["command_timing"]
        obs=folder/f"view-{i}.bin"
        gen=struct.unpack_from("<Q",obs.read_bytes(),56)[0] if obs.exists() else None
        out.append(dict(op=c["command"]["op"],k=a["k"],error=s["error"],
                        applied_ms=(int(tm["completed_ns"])-int(tm["received_ns"]))/1e6,
                        fresh_ms=c.get("fresh_host_ns",0)/1e6 if c.get("fresh_host_ns") else None,
                        view_generation_matches=gen==a["generation"] if gen is not None else None,
                        tiles=len(spec.get("positions",[])),prep=spec.get("preparation"),
                        load=a.get("bank_apply"),tx=a.get("guard_tx"),
                        laps=from_capture(folder/f"lap-{i}.dat",a) if (folder/f"lap-{i}.dat").exists() else None))
    measured=[]
    for line in (folder/"owner.log").read_text(errors="replace").splitlines():
        if not line.startswith("{"):continue
        row=json.loads(line)
        if "begin" not in row:continue
        cpu=row["end"]["cpu_seconds"]-row["begin"]["cpu_seconds"]
        span=row["end"]["time"]-row["begin"]["time"]
        pos=row["after"]["fld"]
        measured.append(dict(cpu_ms=cpu*1000,seconds=span,one_core_pct=cpu/span*100 if span else None,
                             rss_kib=int(re.search(r"VmRSS:\s*(\d+)",row["memory_final"])[1]),
                             udp_drop=row["udp_drop"],ring_drop=row["after"]["ring_drop"],
                             dma_drop=row["after"]["dma_drop"],stop_drop=row["after"]["stop_drop"],
                             visit_epoch=row["after"]["visit_epoch"],fld=pos))
    return dict(calls=out,reader=measured)


if __name__=="__main__":
    p=argparse.ArgumentParser()
    p.add_argument("root",type=Path)
    args=p.parse_args()
    data={"rf":rf(args.root),"hardware":{}}
    for folder in sorted(args.root.iterdir()):
        if (folder/"commands.json").exists() and (folder/"owner.log").exists():
            data["hardware"][folder.name]=control(folder)
    print(json.dumps(data,indent=2,allow_nan=False))
