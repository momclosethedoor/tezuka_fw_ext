use super::*;
use mixer_math::{OFFSET, Tuning, choose};

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
}

pub(super) fn tiles(state: &Value) -> Result<Vec<Tile>> {
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
    for range in scan["ranges"].as_array().ok_or_else(|| anyhow!("ranges"))? {
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
                    .max_by_key(|(id, edge)| (*edge, std::cmp::Reverse(*id)))
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
            });
            lo = hi;
        }
    }
    let mut result = Vec::new();
    for (i, current) in tiles.iter().enumerate() {
        let old = &tiles[(i + tiles.len() - 1) % tiles.len()];
        let mut main = current.clone();
        if old.antenna != current.antenna {
            let width = mixer_math::TILE_WIDTH as u64;
            let mut common = Vec::new();
            for l in &bands[&old.antenna] {
                for r in &bands[&current.antenna] {
                    let a = l[0].max(r[0]);
                    let b = l[1].min(r[1]);
                    if a < b {
                        common.push([a, b]);
                    }
                }
            }
            let overlap = common
                .iter()
                .filter(|r| r[1] - r[0] >= width)
                .map(|r| current.rf[0].clamp(r[0] + width / 2, r[1] - width / 2))
                .min_by_key(|f| f.abs_diff(current.rf[0]));
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
    Ok(result)
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
        (tail + 4, (!tx.is_empty()) as u32),
        (tail + 8, if tx.is_empty() { 89750 } else { 3000 }),
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
            | if v.hold { 2 } else { 0 };
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
        if !v.transition_only {
            ranges.push(v.rf);
        }
        positions.push(json!({"rf_ranges":[v.rf],"rx_lo_hz":rx_hz,"rf_center_hz":center,"antenna":v.antenna,
            "side_sign":side,"mixer":slot.is_some(),"tx_slot":slot,"tx_pll_hz":pll,"tx_tone_hz":tone,
            "mixer_lo_hz":tone.map(|_|offset),"if_edges_hz":if side==1 {[a as i64-offset,b as i64-offset]}else{[offset-b as i64,offset-a as i64]},
            "ddc_hz":0,"dc_clearance_hz":mixer_math::DC_CLEARANCE,"image_rf_hz":tone.map(|_|[2*offset-b as i64,2*offset-a as i64]),
            "image_in_antenna":tone.and_then(|_|antennas.get(&v.antenna).map(|bands|bands.iter().any(|r|
                (r[0] as i64)<2*offset-a as i64 && (r[1] as i64)>2*offset-b as i64))),
            "tx_if_veto_hz":tone.map(|f|[f-35_520_000,f+35_520_000]),"tx_model_extrapolated":pll.is_some_and(|p|!(2_250_000_000..=2_490_000_000).contains(&p)),
            "rx_available":!v.transition_only,"transition_only":v.transition_only,"marker_route":v.notify,
            "hold_boundary":v.hold,"transition":v.handover}));
    }
    // The guard drives TX for the full map. A mixed direct/mixer map requires
    // validated per-route gating or a measured direct model with the TX still on.
    ensure!(
        tx.is_empty() || tiles.iter().all(|t| t.tx.is_some()),
        "direct/mixer map needs TX gating at route boundary"
    );
    Ok((
        bytes,
        json!({"rf_ranges":ranges,"requested_ranges":state["scan"]["ranges"],"positions":positions,"planner_version":2,
        "mixer_threshold_hz":threshold,"mixer_input_multiplier":mult,"first_tx_pll_hz":first.map(|s|tx[&txkeys[s]].0)}),
    ))
}
