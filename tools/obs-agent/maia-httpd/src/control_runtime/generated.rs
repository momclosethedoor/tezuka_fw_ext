use super::*;
use mixer_math::{OFFSET, Tuning, choose};
mod grid_bench;

pub(super) fn environment() -> Result<(u64, i64)> {
    let threshold = option_env!("GOOFY_MIXER_THRESHOLD_HZ")
        .unwrap_or("6000000000")
        .parse()?;
    let multiplier = option_env!("GOOFY_MIXER_INPUT_MULTIPLIER")
        .unwrap_or("2")
        .parse()?;
    ensure!(
        (70_000_000..=6_000_000_000).contains(&threshold) && (1..=2).contains(&multiplier),
        "invalid compiled mixer threshold or multiplier"
    );
    Ok((threshold, multiplier))
}

#[derive(Clone)]
pub(super) struct Tile {
    pub rf: [u64; 2],
    pub antenna: u64,
    pub tx: Option<Tuning>,
    pub rx: u64,
    notify: Option<u64>,
    hold: bool,
    transition_only: bool,
    handover: &'static str,
    grid: Option<(&'static str, [u64; 2])>,
    marker_path: Option<&'static str>,
    pub rf_row: usize,
    channel_bridge: bool,
}

pub(super) fn failure(scan: &Value, command: &Value, reason: String) -> Value {
    let ranges = scan["ranges"].as_array().into_iter().flatten()
        .filter(|r| r["enabled"] == true)
        .filter_map(|r| Some([r["lo_hz"].as_u64()?, r["hi_hz"].as_u64()?]))
        .collect::<Vec<_>>();
    let mut gaps = Vec::new();
    if let Ok(antennas) = parse_antenna_bands(&scan["antenna_bands"]) {
        let mut bands = antennas.into_values().flatten().collect::<Vec<_>>();
        bands.sort_unstable();
        if !bands.is_empty() {
            for &[start,end] in &ranges {
                let mut cursor = start;
                for &[a,b] in &bands {
                    if a >= end || cursor >= end { break; }
                    if b <= cursor { continue; }
                    if cursor < a { gaps.push([cursor,a]); }
                    cursor = cursor.max(b);
                }
                if cursor < end { gaps.push([cursor,end]); }
            }
        }
    }
    json!({"command":command,"applied_rf_ranges":[],"unapplied_rf_ranges":ranges,
        "antenna_gaps_hz":gaps,"reason":reason})
}

pub(super) fn tiles(state: &Value) -> Result<Vec<Tile>> {
    executable_tiles(state, &core_tiles(state)?)
}

pub(super) fn executable_tiles(state: &Value, core: &[Tile]) -> Result<Vec<Tile>> {
    if let Some(overlap) = state["scan"].get("bench_overlap_hz") {
        ensure!(state["pl_timing_only"] == true, "grid experiment requires muted timing fixture");
        let overlap = overlap.as_u64().ok_or_else(|| anyhow!("invalid overlap"))?;
        ensure!(matches!(overlap, 8_000_000 | 9_000_000), "unsupported bench overlap");
        let half = state["scan"]["bench_half_pass_hz"].as_u64().unwrap_or(mixer_math::HALF_PASS as u64);
        ensure!(matches!(half,23_800_000 | 27_700_000), "unsupported bench aperture");
        let schedule = grid_bench::schedule(core, overlap, half, None)?;
        return transitions(state, &schedule);
    }
    if state["grid_27"] == true && state["alternative"].is_null() {
        return transitions(state, &grid_bench::schedule(core, 9_000_000, 27_000_000, Some(state))?);
    }
    transitions(state, core)
}

pub(super) fn core_tiles(state: &Value) -> Result<Vec<Tile>> {
    let (threshold, mult) = environment()?;
    let scan = &state["scan"];
    let bands = parse_antenna_bands(&scan["antenna_bands"])?;
    let pll_bounds = if scan["mixer"]["pll_range_hz"].is_null() {
        [2_250_000_000, 2_490_000_000]
    } else {
        let limits: [i64; 2] = serde_json::from_value(scan["mixer"]["pll_range_hz"].clone())?;
        ensure!(
            limits[0] >= 70_000_000
                && limits[0] <= limits[1]
                && limits[1] <= 6_000_000_000 - OFFSET,
            "invalid TX PLL interval"
        );
        limits
    };
    let mut tiles = Vec::new();
    let mut previous = None;
    for (rf_row, range) in scan["ranges"].as_array().ok_or_else(|| anyhow!("ranges"))?.iter().enumerate() {
        if range["enabled"] != true {
            continue;
        }
        ensure!(range["tx_slot"].is_null(), "v2 selects TX from RF");
        let mut lo = int(range, "lo_hz", 70_000_000, plan_math::MAX_RF)?;
        let end = int(range, "hi_hz", lo + 1, plan_math::MAX_RF)?;
        let path = range.get("path").and_then(Value::as_str).unwrap_or("auto");
        ensure!(
            matches!(path, "auto" | "mixer" | "direct"),
            "invalid RF path"
        );
        ensure!(
            path != "direct" || end <= 6_000_000_000,
            "direct path cannot cover [{lo},{end})"
        );
        while lo < end {
            let (antenna, edge) = if bands.is_empty() {
                (state["route"]["antenna"].as_u64().unwrap_or(0), end)
            } else {
                bands
                    .iter()
                    .flat_map(|(&id, rs)| rs.iter().map(move |r| (id, r)))
                    .filter(|(_, r)| r[0] <= lo && lo < r[1])
                    .map(|(id, r)| (id, r[1].min(end)))
                    .max_by_key(|(id, edge)| {
                        let keep_direct = state["catalog_policy"] == true && *id == 0
                            && path != "mixer" && lo < threshold && *edge >= threshold.min(end);
                        (keep_direct, *edge, std::cmp::Reverse(*id))
                    })
                    .ok_or_else(|| {
                        anyhow!(
                            "missing antenna coverage [{lo},{}) Hz",
                            bands
                                .values()
                                .flatten()
                                .map(|r| r[0])
                                .filter(|x| *x > lo)
                                .min()
                                .unwrap_or(end)
                                .min(end)
                        )
                    })?
            };
            // Allow the maximum 10 Hz PLL readback error at either edge.
            let mut hi = edge.min(lo + mixer_math::TILE_WIDTH as u64);
            if path == "auto" && lo < threshold {
                hi = hi.min(threshold);
            }
            let tx = if path == "mixer" || path == "auto" && lo >= threshold {
                Some(
                    mixer_math::choose_on_antenna(
                        [lo as i64, hi as i64],
                        mult,
                        pll_bounds,
                        previous,
                        state["alternative"]["excluded_tx_pll_hz"].as_i64(),
                        bands.get(&antenna).map_or(&[], Vec::as_slice),
                    )
                    .ok_or_else(|| anyhow!("no measured mixer setting covers [{lo},{hi}) Hz"))?,
                )
            } else {
                None
            };
            let edges = tx.map_or([lo as i64, hi as i64], |t| t.if_edges);
            let rx = mixer_math::place_rx(edges, state["alternative"]["excluded_rx_lo_hz"].as_i64())
                .ok_or_else(|| anyhow!("no RX tuning with DC clearance for [{lo},{hi})"))?
                as u64;
            if let Some(t) = tx {
                previous = Some(t.pll);
            }
            tiles.push(Tile {
                rf: [lo, hi],
                antenna,
                tx,
                rx,
                notify: None,
                hold: false,
                transition_only: false,
                handover: "none",
                grid: None,
                marker_path: None,
                rf_row,
                channel_bridge: false,
            });
            lo = hi;
        }
    }
    Ok(tiles)
}

pub(super) fn transitions(state: &Value, tiles: &[Tile]) -> Result<Vec<Tile>> {
    let (threshold, mult) = environment()?;
    let bands = parse_antenna_bands(&state["scan"]["antenna_bands"])?;
    let mut result = Vec::new();
    for (i, current) in tiles.iter().enumerate() {
        let old = &tiles[(i + tiles.len() - 1) % tiles.len()];
        let mut main = current.clone();
        if state["xtg1_wait_ticks"].is_number() && old.tx.is_some() != current.tx.is_some() {
            // Request the physical path at the held boundary. An early ACK
            // could switch the antenna while preceding visits are still valid.
            main.notify = Some(main.antenna);
            main.hold = true;
            main.marker_path = Some(if main.tx.is_some() {"mixer"} else {"direct"});
            main.handover = "path";
        } else if old.antenna != current.antenna {
            let width = mixer_math::TILE_WIDTH as u64;
            let mut common = Vec::new();
            for l in &bands[&old.antenna] {
                for r in &bands[&current.antenna] {
                    let a = l[0].max(r[0]);
                    let mut b = l[1].min(r[1]);
                    if state["catalog_policy"] == true && old.tx.is_some() != current.tx.is_some() {
                        b = b.min(threshold);
                    }
                    if a < b {
                        common.push([a, b]);
                    }
                }
            }
            let target = if state["catalog_policy"] != true {
                current.rf[0]
            } else if old.tx.is_some() != current.tx.is_some() {
                threshold
            } else {
                bands[&old.antenna].iter().map(|r| r[1]).max().unwrap()
            };
            let overlap = common
                .iter()
                .filter(|r| r[1] - r[0] >= width)
                .map(|r| target.clamp(r[0] + width / 2, r[1] - width / 2))
                .min_by_key(|f| f.abs_diff(target));
            if let Some(center) = overlap {
                let window = [center - width / 2, center + width / 2];
                let mut pair = Vec::new();
                for t in [old, current] {
                    let mut v = t.clone();
                    v.rf = window;
                    v.transition_only = true;
                    v.handover = "overlap";
                    if let Some(tx) = t.tx {
                        v.tx = choose(
                            [window[0] as i64, window[1] as i64],
                            mult,
                            [tx.pll, tx.pll],
                            Some(tx.pll),
                            None,
                        );
                        if v.tx.is_none() {
                            break;
                        }
                    }
                    let edges =
                        v.tx.map_or([window[0] as i64, window[1] as i64], |t| t.if_edges);
                    let Some(rx) = mixer_math::place_rx(edges, None) else {
                        break;
                    };
                    v.rx = rx as u64;
                    pair.push(v);
                }
                if pair.len() == 2 {
                    // Leave margin for PLL readback while preserving the entire
                    // requested main coverage adjoining the handover.
                    for v in &mut pair {
                        v.rf[0] += 10;
                        v.rf[1] -= 10;
                    }
                    pair[0].notify = Some(main.antenna);
                    pair[1].hold = true;
                    result.extend(pair);
                } else {
                    main.handover = "no_common_tuning";
                }
            } else {
                main.handover = if common.is_empty() {
                    "no_overlap"
                } else {
                    "overlap_too_narrow"
                };
            }
            if main.handover != "none" {
                main.notify = Some(main.antenna);
                main.hold = true;
            }
        }
        if i == 0 && result.is_empty() && Some(main.antenna) != state["route"]["antenna"].as_u64() {
            main.notify = Some(main.antenna);
            main.hold = true;
        }
        result.push(main);
    }
    if state["catalog_policy"] == true {
        let prefix = result.iter().take_while(|v| v.transition_only).count();
        result.rotate_left(prefix);
    }
    Ok(result)
}

/// Requests can reorder subbands. Warm each distinct RX/TX tuning needed to
/// hand off between any two configured antenna routes, including the wrap.
pub(super) fn catalog_profiles(state: &Value, core: &[Tile]) -> Result<Vec<Tile>> {
    let mut representatives = BTreeMap::new();
    let mut profiles = BTreeMap::new();
    if state["grid_27"] == true {
        for tile in executable_tiles(state, core)? {
            profiles.insert((tile.rx, tile.tx.map(|t| t.pll)), tile);
        }
    }
    for tile in core {
        let pll = tile.tx.map(|t| t.pll);
        representatives.entry((tile.antenna, pll)).or_insert(tile);
        if state["grid_27"] != true {
            profiles.insert((tile.rx, pll), tile.clone());
        }
    }
    let routes: Vec<_> = representatives.into_values().collect();
    for (i, left) in routes.iter().enumerate() {
        for right in &routes[i+1..] {
            if left.antenna == right.antenna { continue; }
            // transitions() emits both directions for this two-element ring.
            for t in transitions(state,&[(*left).clone(),(*right).clone()])? {
                if t.transition_only {
                    profiles.insert((t.rx,t.tx.map(|tx|tx.pll)),t);
                }
            }
        }
    }
    Ok(profiles.into_values().collect())
}

pub(super) fn encode(
    tiles: &[Tile],
    rx: &BTreeMap<u64, (u64, [u8; 16])>,
    tx: &BTreeMap<u64, (u64, [u8; 16])>,
    restore: [u64; 2],
    state: &Value,
) -> Result<(Vec<u8>, Value)> {
    ensure!(
        !tiles.is_empty() && tiles.len() <= 65535 && rx.len() <= 65535 && tx.len() <= 8,
        "row99 requires {} visits, {} RX, {} TX; capacities 65535/65535/8",
        tiles.len(),
        rx.len(),
        tx.len()
    );
    let version = int(state, "map_version", 1, 0x7fff_fffe)? as u32;
    let stride = 32 * tiles.len().max(rx.len()).max(128);
    // Timing fixture recalls the actual TX bank while the driver keeps RF muted.
    let timing_only = state["pl_timing_only"] == true;
    let tx_rf_on = !tx.is_empty() && !timing_only;
    let managed = tx_rf_on && state["xtg1_wait_ticks"].is_number();
    let working_grid = state["grid_27"] == true && state["alternative"].is_null()
        && state["scan"]["bench_overlap_hz"].is_null();
    let tail = 40 + 3 * stride;
    let mut bytes = vec![0; tail + 160];
    for (p, v) in [
        (
            0,
            if stride > 4096 {
                0x3847534f
            } else {
                0x3747534f
            },
        ),
        (4, rx.len().max(8) as u32),
        (8, tiles.len() as u32),
        (12, tiles.len() as u32),
        (16, version),
        (20, version + 1),
        (24, 512),
        (tail, (!tx.is_empty()) as u32),
        (tail + 4, tx_rf_on as u32),
        (tail + 8, if tx_rf_on { 3000 } else { 89750 }),
        (tail + 156, if managed {1 | (state["xtg1_wait_ticks"].as_u64().unwrap() as u32) << 16} else {0}),
    ] {
        put(&mut bytes, p, v);
    }
    put_hz(&mut bytes, 32, restore[0]);
    put_hz(&mut bytes, tail + 16, restore[1]);
    let rxkeys = rx.keys().copied().collect::<Vec<_>>();
    let txkeys = tx.keys().copied().collect::<Vec<_>>();
    for i in 0..rx.len().max(8) {
        let at = 40 + 32 * i;
        put(&mut bytes, at, (i + 1) as u32);
        put(&mut bytes, at + 4, 1);
        bytes[at + 8..at + 24].copy_from_slice(&rx[&rxkeys[i % rx.len()]].1);
    }
    if !tx.is_empty() {
        for i in 0..8 {
            bytes[tail + 24 + 16 * i..tail + 40 + 16 * i]
                .copy_from_slice(&tx[&txkeys[i % tx.len()]].1);
        }
    }
    let (threshold, mult) = environment()?;
    let mut positions = Vec::new();
    let antennas = parse_antenna_bands(&state["scan"]["antenna_bands"])?;
    let mut ranges = Vec::new();
    let first = tiles
        .iter()
        .find_map(|v| v.tx)
        .map(|t| txkeys.binary_search(&(t.pll as u64)).unwrap());
    put(&mut bytes, tail + 12, first.unwrap_or(0) as u32);
    for (i, v) in tiles.iter().enumerate() {
        let [a, b] = v.rf;
        let rx_hz = rx[&v.rx].0;
        let slot = v.tx.map(|t| txkeys.binary_search(&(t.pll as u64)).unwrap());
        let pll = slot.map(|s| tx[&txkeys[s]].0 as i64);
        let tone = pll.map(|p| p + OFFSET);
        let offset = tone.map_or(0, |f| f * mult);
        let side = v.tx.map_or(1, |t| t.side);
        let center = offset + side * rx_hz as i64;
        ensure!(
            center + mixer_math::DC_CLEARANCE <= a as i64
                || center - mixer_math::DC_CLEARANCE >= b as i64,
            "readback places DC in useful interval [{a},{b})"
        );
        ensure!(
            (a as i64 - center).unsigned_abs() <= 23_800_000
                && (b as i64 - center).unsigned_abs() <= 23_800_000,
            "readback leaves uncovered RF edge [{a},{b})"
        );
        if let Some(p) = pll {
            let x = side * (a as i64 - offset);
            let y = side * (b as i64 - offset);
            let low = x.min(y);
            let high = x.max(y);
            ensure!(
                low >= 70_000_000
                    && high <= 6_000_000_000
                    && (high < p + OFFSET - 35_520_000 || low > p + OFFSET + 35_520_000),
                "PLL readback puts measured interference in [{a},{b})"
            );
        }
        let flags = slot.map_or(0, |s| 4 | ((s as u32) << 8))
            | v.notify.map_or(0, |id| 1 | ((id as u32) << 16))
            | if v.hold { 2 } else { 0 }
            | if managed && slot.is_some() { 16 } else { 0 };
        for page in [40 + stride, 40 + 2 * stride] {
            let at = page + i * 32;
            put(&mut bytes, at, rxkeys.binary_search(&v.rx).unwrap() as u32);
            put(&mut bytes, at + 4, flags);
            if v.notify.is_some() {
                put(&mut bytes, at + 8, version + i as u32);
            }
            put_hz(&mut bytes, at + 16, a);
            put_hz(&mut bytes, at + 24, b);
        }
        let useful = if let Some((_,bounds)) = v.grid.filter(|_| working_grid && !v.transition_only) {
            grid_bench::lobes(bounds, center, offset, side, tone)?
        } else if !v.transition_only {
            vec![v.rf]
        } else {
            Vec::new()
        };
        if !timing_only {
            ranges.extend(useful.iter().copied());
        }
        positions.push(json!({"rf_ranges":[v.rf],"rx_lo_hz":rx_hz,"rf_center_hz":center,"antenna":v.antenna,
            "side_sign":side,"mixer":slot.is_some(),"tx_slot":slot,"tx_pll_hz":pll,"tx_tone_hz":tone,
            "mixer_lo_hz":tone.map(|_|offset),"if_edges_hz":if side==1 {[a as i64-offset,b as i64-offset]}else{[offset-b as i64,offset-a as i64]},
            "ddc_hz":0,"dc_clearance_hz":mixer_math::DC_CLEARANCE,"image_rf_hz":tone.map(|_|[2*offset-b as i64,2*offset-a as i64]),
            "image_in_antenna":tone.and_then(|_|antennas.get(&v.antenna).map(|bands|bands.iter().any(|r|
                (r[0] as i64)<2*offset-a as i64 && (r[1] as i64)>2*offset-b as i64))),
            "tx_if_veto_hz":tone.map(|f|[f-35_520_000,f+35_520_000]),"tx_model_extrapolated":pll.is_some_and(|p|!(2_250_000_000..=2_490_000_000).contains(&p)),
            "rx_available":!v.transition_only && !timing_only,"transition_only":v.transition_only,"marker_route":v.notify,
            "hold_boundary":v.hold,"transition":v.handover}));
        if let Some((phase, bounds)) = v.grid && !v.transition_only {
            let row = positions.last_mut().unwrap();
            row["bench_grid_phase"] = json!(phase);
            row["bench_segment_hz"] = json!(bounds);
        }
        let row = positions.last_mut().unwrap();
        row["marker_path"] = json!(v.marker_path);
        row["tx_gate_requested"] = if managed {json!(slot.is_some())} else {Value::Null};
        if working_grid {
            row["rf_ranges"] = json!(useful);
            row["useful_if_ranges_hz"] = json!(useful.iter().map(|&[l,h]| {
                let mut edges = [side*(l as i64-offset),side*(h as i64-offset)];
                edges.sort_unstable();
                edges
            }).collect::<Vec<_>>());
            row["image_rf_ranges_hz"] = json!(tone.map(|_| useful.iter()
                .map(|&[l,h]| [2*offset-h as i64,2*offset-l as i64]).collect::<Vec<_>>()));
            row["rf_row"] = json!(v.rf_row);
            row["channel_bridge"] = json!(v.channel_bridge && !v.transition_only);
            row["sweep_phase"] = json!(v.grid.map(|(p,_)| if p == "main" {"A"} else {"B"}));
        }
    }
    // The guard drives TX for the full map. A mixed direct/mixer map requires
    // validated per-route gating or a measured direct model with the TX still on.
    ensure!(
        !tx_rf_on || managed || tiles.iter().all(|t| t.tx.is_some()),
        "direct/mixer map needs TX gating at route boundary"
    );
    let mut desc = json!({"rf_ranges":ranges,"requested_ranges":state["scan"]["ranges"],"positions":positions,"planner_version":2,
        "pl_timing_only":timing_only,"mixer_threshold_hz":threshold,"mixer_input_multiplier":mult,
        "first_tx_pll_hz":first.map(|s|tx[&txkeys[s]].0)});
    if working_grid {
        grid_bench::validate_cover(&desc)?;
        desc["sweep_geometry"] = json!({"half_pass_hz":27_000_000,"overlap_hz":9_000_000,
            "max_step_hz":45_000_000,"phase_order":["A","B"]});
        desc["sweep_wire_version"] = json!(1);
    }
    Ok((bytes, desc))
}
