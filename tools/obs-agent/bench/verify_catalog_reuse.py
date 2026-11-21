"""Check a RAM capture of full, partial, mixed, seam, DC and NEXT requests."""
import argparse
import json
from pathlib import Path

from accept_027_report import physical_passes, uncovered


def load(path):
    return json.loads(path.read_text(encoding="utf-8-sig"))


def check_geometry(descriptor):
    positions = [p for p in descriptor["positions"] if not p["transition_only"]]
    phases = [p["sweep_phase"] for p in positions]
    assert phases == sorted(phases) and set(phases) == {"A", "B"}
    requests = descriptor["requested_ranges"]
    wanted = [index for index, row in enumerate(requests) if row["enabled"]]
    for phase in ("A", "B"):
        seen = [p["rf_row"] for p in positions if p["sweep_phase"] == phase]
        assert seen == sorted(seen) and sorted(set(seen)) == wanted, (phase, seen, wanted)
    for index in wanted:
        request = requests[index]
        lo, hi = request["lo_hz"], request["hi_hz"]
        bands = [band for p in positions if p["rf_row"] == index for band in p["rf_ranges"]]
        assert all(lo <= a < b <= hi for a, b in bands), (index, bands)
        assert not uncovered(lo, hi, bands), (index, lo, hi)
        if hi - lo >= 8_000_000:
            # Closed channel-start endpoints, converted to half-open integer Hz.
            starts = [(a, b - 8_000_000 + 1) for a, b in bands if b - a >= 8_000_000]
            assert not uncovered(lo, hi - 8_000_000 + 1, starts), (index, lo, hi)
    return {p["rx_lo_hz"] for p in positions}


def signature(command):
    return json.dumps([command["ranges"], command.get("next_ranges")], sort_keys=True)


def main(folders):
    fixtures = load(Path(__file__).with_name("catalog_reuse_cases.json"))
    captured = {}
    for folder in folders:
        for number, case in enumerate(load(folder / "commands.json"), 1):
            if case["command"]["run"] and case["status"]["phase"] == "running":
                captured.setdefault(signature(case["command"]), (folder, number, case))
    baseline = None
    rows = []
    for index, fixture in enumerate(fixtures, 1):
        key = signature(fixture)
        assert key in captured, f"no successful capture for case {index}"
        folder, number, case = captured[key]
        status = case["status"]
        assert status["phase"] == "running" and not status["error"], status["error"]
        actual = status["actual"]
        descriptor = actual["spectrum"]
        rx = check_geometry(descriptor)
        if index == 1:
            baseline = rx
        preparation = descriptor["preparation"]
        assert preparation["tx_calibrated"] == 0
        if index == 6:
            assert 0 < preparation["rx_calibrated"] <= 2 and rx - baseline, preparation
        else:
            assert preparation["rx_calibrated"] == 0 and rx <= baseline, preparation
        passes, _ = physical_passes(folder, number, descriptor, actual["generation"])
        assert all(pass_['complete'] >= 12 for pass_ in passes.values()), passes
        if index == 5:
            markers = {p["marker_path"] for p in descriptor["positions"]}
            assert {"direct", "mixer"} <= markers
        rows.append(dict(case=index, capture=folder.name, required=preparation["rx_required"],
                         calibrated=preparation["rx_calibrated"], prepare_us=preparation["prepare_us"],
                         positions=len(descriptor["positions"]), complete={
                             phase: data["complete"] for phase, data in passes.items()}))
    next_log = load(folder / f"next-{number}.json")
    before, after = next_log["before"], next_log["after"]
    assert before["actual"]["generation"] != after["actual"]["generation"]
    assert check_geometry(after["actual"]["spectrum"]) <= baseline
    print(json.dumps(rows, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("folders", type=Path, nargs="+")
    main(parser.parse_args().folders)
