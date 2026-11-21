"""Verify CONTROL-003 RF order, reuse, channel geometry and observed A/B passes."""
import argparse
import json
from pathlib import Path

from accept_027_report import physical_passes
from verify_catalog_reuse import check_geometry, load


def verify(folder):
    calls = load(folder / "commands.json")
    fixtures = load(Path(__file__).with_name("control003-maps.json"))
    assert len(calls) == len(fixtures) + 1 and calls[-1]["status"]["phase"] == "stopped"
    result = []
    for index, (call, fixture) in enumerate(zip(calls, fixtures), 1):
        assert call["command"]["ranges"] == fixture["ranges"]
        status = call["status"]
        assert status["phase"] == "running" and not status["error"], (index, status["error"])
        actual = status["actual"]
        spectrum = actual["spectrum"]
        check_geometry(spectrum)
        positions = spectrum["positions"]
        slots = [p["tx_slot"] for p in positions]
        cyclic = list(zip(slots, slots[1:] + slots[:1]))
        phases, _ = physical_passes(folder, index, spectrum, actual["generation"])
        assert all(p["complete"] >= 12 for p in phases.values()), (index, phases)
        row = dict(index=index, visits=len(positions),
            antenna_markers=sum(p["marker_route"] is not None for p in positions),
            transition_visits=sum(p["transition_only"] for p in positions),
            tx_changes=sum(a is not None and b is not None and a != b for a,b in cyclic),
            tx_gate_changes=sum((a is None) != (b is None) for a,b in cyclic),
            preparation=spectrum["preparation"], phases=phases,
            fresh_view_ms=call["fresh_host_ns"]/1e6)
        if index <= 3:
            assert row["preparation"]["rx_calibrated"] == row["preparation"]["tx_calibrated"] == 0
        if index == 4:
            row["tx_by_phase"] = {}
            for phase in "AB":
                by_row = [{(p["tx_slot"],p["tx_pll_hz"]) for p in positions
                    if p["rf_row"]==n and p["sweep_phase"]==phase and not p["transition_only"]}
                    for n in range(3)]
                assert all(len(x)==1 for x in by_row)
                a,b,c=(next(iter(x)) for x in by_row)
                assert a == c and a != b, (phase,a,b,c)
                row["tx_by_phase"][phase]=[a,b,c]
        if "soak" in call:
            assert call["soak"]["elapsed_s"] >= 60
            row["soak"] = call["soak"]
        if "next_ranges" in fixture:
            following = load(folder / f"next-{index}.json")
            before, after = following["before"]["actual"], following["after"]["actual"]
            assert before["generation"] != after["generation"]
            check_geometry(after["spectrum"])
        result.append(row)
    return result


if __name__ == "__main__":
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("folder", type=Path)
    print(json.dumps(verify(cli.parse_args().folder), indent=2))
