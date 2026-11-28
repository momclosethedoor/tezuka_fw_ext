//! Compile the opt-in RF plan from the host planner for the RAM trial.
use super::*;
use serde::Deserialize;

#[derive(Deserialize)]
struct Plan {
    ranges: Value,
    antennas: Value,
    slots: Vec<i64>,
    visits: Vec<Visit>,
}

#[derive(Deserialize)]
struct Visit {
    rf: [u64; 2],
    antenna: u64,
    rx: u64,
    pll: i64,
    side: i64,
    phase: String,
    segment: [u64; 2],
    row: usize,
    bridge: bool,
}

pub(super) fn load(state: &Value) -> Result<Option<Vec<Tile>>> {
    let Some(path) = std::env::var_os("GOOFY_SPUR_RAM_PLAN") else {
        return Ok(None);
    };
    if state["grid_27"] != true || !state["alternative"].is_null()
        || !state["scan"]["bench_overlap_hz"].is_null() || !state["scan"]["next_ranges"].is_null()
    {
        return Ok(None);
    }
    let plan: Plan = serde_json::from_slice(&fs::read(path)?)?;
    let scan = &state["scan"];
    let ranges = |v: &Value| -> Option<Vec<_>> {
        Some(v.as_array()?.iter().filter(|r| r["enabled"] == true).map(|r| (
            r["lo_hz"].as_u64(), r["hi_hz"].as_u64(),
            r["path"].as_str().unwrap_or("auto").to_owned(), r["tx_slot"].is_null(),
        )).collect())
    };
    if ranges(&plan.ranges) != ranges(&scan["ranges"])
        || parse_antenna_bands(&scan["antenna_bands"])? != parse_antenna_bands(&plan.antennas)?
    {
        return Ok(None);
    }
    ensure!(state["xtg1_wait_ticks"].is_number(), "spur RAM scan requires managed TX gating");
    let (threshold, mult) = environment()?;
    let [pll_min,pll_max] = mixer_limits(scan)?;
    let slots = plan.visits.iter().filter_map(|v| (v.pll != 0).then_some(v.pll))
        .collect::<std::collections::BTreeSet<_>>();
    ensure!(slots.len() <= 8 && slots.iter().copied().eq(plan.slots),
        "invalid spur RAM TX bank");
    ensure!(!plan.visits.is_empty(), "empty spur RAM plan");
    let bands = parse_antenna_bands(&plan.antennas)?;
    let mut offset_phase = false;
    let mut tiles = Vec::with_capacity(plan.visits.len());
    for v in plan.visits {
        let phase = match v.phase.as_str() {
            "A" if !offset_phase => "main",
            "B" => { offset_phase = true; "offset" }
            _ => bail!("invalid spur RAM phase order"),
        };
        let [a,b] = v.rf;
        let [lo,hi] = v.segment;
        let row = &scan["ranges"][v.row];
        ensure!(lo <= a && a < b && b <= hi && row["enabled"] == true
            && row["lo_hz"].as_u64().is_some_and(|f| f <= lo)
            && row["hi_hz"].as_u64().is_some_and(|f| hi <= f)
            && bands.get(&v.antenna).is_some_and(|rs| rs.iter().any(|r| r[0] <= lo && hi <= r[1])),
            "invalid spur RAM RF interval");
        ensure!((70_000_000..=6_000_000_000).contains(&v.rx), "invalid spur RAM RX");
        let tx = if v.pll == 0 {
            ensure!(v.side == 1 && b <= threshold, "invalid spur RAM direct route");
            None
        } else {
            ensure!((pll_min..=pll_max).contains(&v.pll) && matches!(v.side,-1 | 1)
                && a >= threshold, "invalid spur RAM mixer route");
            let mixer_lo = (v.pll + OFFSET) * mult;
            let mut if_edges = [a as i64,b as i64].map(|f| v.side*(f-mixer_lo));
            if_edges.sort_unstable();
            Some(Tuning { pll:v.pll, mixer_lo, side:v.side, if_edges })
        };
        let center = tx.map_or(v.rx as i64, |t| t.mixer_lo+t.side*v.rx as i64);
        let tone = tx.map(|t| t.pll+OFFSET);
        let clean = grid_bench::lobes(v.segment,center,tx.map_or(0,|t| t.mixer_lo),v.side,tone);
        ensure!(clean.iter().any(|r| r[0] <= a && b <= r[1]), "spur RAM witness is outside capture");
        tiles.push(Tile { rf:v.rf, antenna:v.antenna, tx, rx:v.rx, notify:None, hold:false,
            transition_only:false, handover:"none", grid:Some((phase,v.segment)),
            marker_path:None, rf_row:v.row, channel_bridge:v.bridge });
    }
    ensure!(offset_phase && tiles[0].grid.is_some_and(|(phase,_)| phase=="main"),
        "spur RAM plan requires both phases");
    Ok(Some(transitions(state,&tiles)?))
}

