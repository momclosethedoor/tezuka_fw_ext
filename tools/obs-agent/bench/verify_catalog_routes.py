"""Check the warm RF-route scenarios and their captured A/B maps."""
import argparse
import json
from pathlib import Path

from verify_catalog_reuse import check_geometry, load


def verify(folder):
    calls = load(folder / "commands.json")
    assert len(calls) == 8
    report = []
    for index, call in enumerate(calls, 1):
        status = call["status"]
        actual = status["actual"]
        if index == 6:
            assert status["phase"] == "failed" and "direct path cannot cover" in status["error"], status["error"]
            report.append({"case": index, "error": status["error"]})
            continue
        if index == 8:
            assert status["phase"] == "stopped" and not actual["running"], status["error"]
            continue
        descriptor = actual["spectrum"]
        assert status["phase"] == "running" and actual["running"], status["error"]
        assert status["progress_failure"] is None
        assert call["laps"]["missing_ordinals"] == 0
        check_geometry(descriptor)
        positions = [p for p in descriptor["positions"] if not p["transition_only"]]
        preparation = descriptor["preparation"]
        paths = [{p["mixer"] for p in positions if p["rf_row"] == row} for row in range(3)]
        antenna = {p["antenna"] for p in positions}
        if index <= 4:
            assert (preparation["rx_calibrated"], preparation["tx_calibrated"]) == (0, 0), preparation
            assert antenna == ({1} if index <= 2 else {0}), antenna
            assert paths == [{True}] * 3, paths
        if index == 5:
            assert paths == [{True}, {False}, {True, False}], paths
            assert any(p["channel_bridge"] and p["rf_row"] == 2 for p in positions)
        if index == 7:
            assert paths == [{True}, {False}, {True}], paths
        report.append({
            "case": index,
            "antenna": sorted(antenna),
            "mixer_by_row": [sorted(p) for p in paths],
            "rx_tx_used": [preparation["rx_required"], preparation["tx_required"]],
            "rx_tx_calibrated": [preparation["rx_calibrated"], preparation["tx_calibrated"]],
            "prepare_ms": preparation["prepare_us"] / 1000,
            "first_view_ms": call["fresh_host_ns"] / 1e6,
            "complete_passes": call["laps"]["passes_complete"],
        })
    return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run", type=Path)
    args = parser.parse_args()
    print(json.dumps(verify(args.run), indent=2))
