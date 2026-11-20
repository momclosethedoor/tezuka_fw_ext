"""Bounded stock-PL spectrum measurement with the accepted Pentium tone fixture."""
import argparse
import json
from pathlib import Path
import sys
import time

import numpy as np


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("output", type=Path)
    p.add_argument("--workspace", type=Path, default=Path("C:/proj/pluto_ag"))
    p.add_argument("--route", choices=["direct", "mixer"], required=True)
    p.add_argument("--rf-hz",type=int,default=6750000000)
    p.add_argument("--pll-hz",type=int,nargs="+",default=[2250000000,2370000000,2490000000])
    p.add_argument("--source-58",action="store_true",help="locate the enabled 5.8 GHz reference with TX muted")
    p.add_argument("--edge-27",action="store_true",help="compare offsets around the nominal 27 MHz edge at 56 MHz bandwidth")
    p.add_argument("--direct-tx-on",action="store_true",
                   help="measure a physically confirmed direct RX with the mixer LO driven")
    args = p.parse_args()
    if args.direct_tx_on and (args.route != "direct" or args.source_58):
        p.error("--direct-tx-on requires direct RX and the carrier-comparison series")
    host = args.workspace.parent / "goofy_scan"
    sys.path[:0] = [str(args.workspace / "obs-007a-hw-system/scripts"), str(host),
                    str(host / "local-only/HOST-SCAN-CONTROL-001")]
    from access import api, ssh, request
    from sdr.source.maia.ws import WsClient
    from tone_fixture import ToneFixture

    args.output.mkdir(parents=True, exist_ok=False)
    board = "192.168.2.1"
    start = api()
    ad = start["ad9361"]
    sp = start["spectrometer"]
    gate = "cat /sys/bus/iio/devices/iio:device0/out_altvoltage1_TX_LO_powerdown"
    # Stock PL advances the index in bin zero even with pin control disabled.
    rx_lock = "r=/sys/kernel/debug/iio/iio:device0/direct_reg_access; echo 0x25a > $r && cat $r"
    assert ssh(gate).strip() == "1"
    records = []

    def save(name, value):
        (args.output / name).write_text(json.dumps(value, indent=2), encoding="utf-8")

    def patch(resource, fields):
        return request("http://" + board, "PATCH", "/api/" + resource, fields)

    def capture(pll, rx, fixture, tx):
        patch("ad9361", {"rx_lo_frequency": rx})
        state = api()
        assert abs(state["ad9361"]["rx_lo_frequency"] - rx) <= 10
        lock_before = int(ssh(rx_lock).strip(), 0)
        assert lock_before & 3 == 0, f"RX fastlock active ({lock_before:#x})"
        ws = WsClient(max_msg=65536)
        frames, times = [], []
        try:
            ws.connect(board, 80, "/waterfall", 2)
            deadline = time.monotonic() + 4
            while len(frames) < 12 and time.monotonic() < deadline:
                if fixture and not fixture.service():
                    raise RuntimeError("fixture lease lost")
                if not ws.poll():
                    time.sleep(.002)
                    continue
                if ws.msg_len == 4096 * 4:
                    frames.append(bytes(memoryview(ws.msg)[:ws.msg_len]))
                    times.append(time.monotonic_ns())
        finally:
            ws.close()
        assert len(frames) == 12, "incomplete waterfall sample"
        name = f"{len(records):02d}.f32le"
        (args.output / name).write_bytes(b"".join(frames))
        tags = [float(np.frombuffer(f, "<f4")[0]) for f in frames]
        lock_after = int(ssh(rx_lock).strip(), 0)
        records.append(dict(file=name, host_ns=times, profile_tags=tags, route=args.route,
                            rx_fastlock_setup=[lock_before, lock_after],direct_tx_on_comparison=args.direct_tx_on,
                            nominal_pll_hz=pll, tx=tx, arm=state, discard_prefix_frames=2))
        save("series.json", records)
        assert lock_after & 3 == 0, "RX fastlock changed during static observation"

    save("before.json", start)
    try:
        patch("ad9361", {"sampling_frequency": 61440000, "rx_rf_bandwidth": 56000000 if args.edge_27 else 48000000,
                         "rx_gain_mode": "Manual", "rx_gain": 35})
        patch("spectrometer", {"input": "AD9361", "mode": "Average", "number_integrations": 128})
        if args.edge_27:
            pll, = args.pll_hz
            tone = pll + 960000
            target = args.rf_hz if args.route == "direct" else abs(args.rf_hz - 2*tone)
            offsets = (1_000_000,-26_500_000,-27_000_000,-27_500_000,
                       26_500_000,27_000_000,27_500_000,1_000_000)
            fixture = ToneFixture(board) if args.route=="mixer" else None
            try:
                tx = {"powerdown":1}
                if fixture:
                    fixture.enter()
                    tx = fixture.transmit(tone,3)
                for offset in offsets:
                    capture(pll,target-offset,fixture,tx)
                result=[]
                for offset,row in zip(offsets,records,strict=True):
                    mean=np.median(np.fromfile(args.output/row["file"],"<f4").reshape(-1,4096)[2:],axis=0)
                    nominal=2048+round(offset/15000)
                    window=range(nominal-8,nominal+9)
                    peak=max(window,key=lambda i:mean[i])
                    background=np.median(np.r_[mean[nominal-28:nominal-13],mean[nominal+13:nominal+28]])
                    result.append(dict(offset_hz=offset,bin=peak,power_db=float(10*np.log10(mean[peak])),
                        background_db=float(10*np.log10(background)),file=row["file"]))
                save("edges.json",result)
            finally:
                if fixture:
                    save("tx-off.json",fixture.close())
            return
        if args.source_58:
            assert args.route=="direct"
            for rx in range(5640000000,5980000001,20000000):
                capture(None,rx,None,{"powerdown":1})
            return
        for pll in args.pll_hz:
            tone = pll + 960000
            target_if = args.rf_hz if args.direct_tx_on else abs(args.rf_hz - 2*tone)
            points = (tone - 4800000, tone + 4800000, target_if)
            for rx in points:
                capture(pll, rx, None, {"powerdown": 1})
            if args.route == "mixer" or args.direct_tx_on:
                fixture = ToneFixture(board)
                try:
                    fixture.enter()
                    tx = fixture.transmit(tone, 3)
                    for rx in points:
                        capture(pll, rx, fixture, tx)
                finally:
                    save(f"tx-off-{pll}.json", fixture.close())
    finally:
        patch("ad9361", {key: ad[key] for key in ("sampling_frequency", "rx_rf_bandwidth",
              "rx_lo_frequency", "tx_lo_frequency", "rx_gain_mode")})
        if ad["rx_gain_mode"] == "Manual":
            patch("ad9361", {"rx_gain": ad["rx_gain"]})
        patch("spectrometer", {key: sp[key] for key in ("input", "mode", "number_integrations")})
        save("after.json", api())
        assert ssh(gate).strip() == "1"


if __name__ == "__main__":
    main()
