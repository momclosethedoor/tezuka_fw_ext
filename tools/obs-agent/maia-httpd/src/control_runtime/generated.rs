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

pub(super) struct Tile {
    pub rf: [u64; 2],
    pub antenna: u64,
    pub tx: Option<Tuning>,
    pub rx: u64,
}

pub(super) fn tiles(state: &Value) -> Result<Vec<Tile>> {
    let (threshold, mult) = environment()?;
    let scan = &state["scan"];
    let bands = parse_antenna_bands(&scan["antenna_bands"])?;
    let mut tiles = Vec::new();
    let mut previous = None;
    for range in scan["ranges"].as_array().ok_or_else(|| anyhow!("ranges"))? {
        if range["enabled"] != true {
            continue;
        }
        ensure!(range["tx_slot"].is_null(), "v2 selects TX from RF");
        let mut lo = int(range, "lo_hz", 70_000_000, plan_math::MAX_RF)?;
        let end = int(range, "hi_hz", lo + 1, plan_math::MAX_RF)?;
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
            let mut hi = edge.min(lo + 47_600_000 - 20);
            if lo < threshold {
                hi = hi.min(threshold);
            }
            let tx = if lo >= threshold {
                Some(
                    choose(
                        [lo as i64, hi as i64],
                        mult,
                        [2_250_000_000, 2_490_000_000],
                        previous,
                        state["alternative"]["excluded_tx_pll_hz"].as_i64(),
                    )
                    .ok_or_else(|| anyhow!("no measured mixer setting covers [{lo},{hi}) Hz"))?,
                )
            } else {
                None
            };
            let mid = lo + (hi - lo) / 2;
            let mut rx = tx.map_or(mid, |t| (mid as i64 - t.mixer_lo).unsigned_abs());
            if let Some(t) = tx {
                previous = Some(t.pll);
            }
            if tx.is_none()
                && let Some(old) = state["alternative"]["excluded_rx_lo_hz"].as_u64()
            {
                let lowest = hi.saturating_sub(23_800_000 - 10).max(70_000_000);
                let highest = (lo + 23_800_000 - 10).min(6_000_000_000);
                rx = if lowest.abs_diff(old) > highest.abs_diff(old) {
                    lowest
                } else {
                    highest
                };
                ensure!(
                    rx.abs_diff(old) >= 15_000,
                    "no distinct RX observation fits [{lo},{hi})"
                );
            }
            tiles.push(Tile {
                rf: [lo, hi],
                antenna,
                tx,
                rx,
            });
            lo = hi;
        }
    }
    Ok(tiles)
}

pub(super) fn encode(
    tiles: &[Tile],
    rx: &BTreeMap<u64, (u64, [u8; 16])>,
    tx: &BTreeMap<u64, (u64, [u8; 16])>,
    restore: [u64; 2],
    state: &Value,
) -> Result<(Vec<u8>, Value)> {
    ensure!(
        !tiles.is_empty() && tiles.len() <= 128 && rx.len() <= 128 && tx.len() <= 8,
        "row99 requires {} visits, {} RX, {} TX; capacities 128/128/8",
        tiles.len(),
        rx.len(),
        tx.len()
    );
    let version = int(state, "map_version", 1, 0x7fff_fffe)? as u32;
    let mut bytes = vec![0; 12488];
    for (p, v) in [
        (0, 0x3747534f),
        (4, rx.len().max(8) as u32),
        (8, tiles.len() as u32),
        (12, tiles.len() as u32),
        (16, version),
        (20, version + 1),
        (24, 512),
        (12328, (!tx.is_empty()) as u32),
        (12332, (!tx.is_empty()) as u32),
        (12336, if tx.is_empty() { 89750 } else { 3000 }),
    ] {
        put(&mut bytes, p, v);
    }
    put_hz(&mut bytes, 32, restore[0]);
    put_hz(&mut bytes, 12344, restore[1]);
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
            bytes[12352 + 16 * i..12368 + 16 * i].copy_from_slice(&tx[&txkeys[i % tx.len()]].1);
        }
    }
    let (threshold, mult) = environment()?;
    let mut positions = Vec::new();
    let mut ranges = Vec::new();
    let first = tiles
        .iter()
        .find_map(|v| v.tx)
        .map(|t| txkeys.binary_search(&(t.pll as u64)).unwrap());
    put(&mut bytes, 12340, first.unwrap_or(0) as u32);
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
            (a as i64 - center).unsigned_abs() <= 23_800_000
                && (b as i64 - center).unsigned_abs() <= 23_800_000,
            "readback leaves uncovered RF edge [{a},{b})"
        );
        if let Some(p) = pll {
            ensure!(
                choose([a as i64, b as i64], mult, [p, p], Some(p), None)
                    .is_some_and(|t| t.side == side),
                "PLL readback puts measured interference in [{a},{b})"
            );
        }
        let before = &tiles[(i + tiles.len() - 1) % tiles.len()];
        let marker = before.antenna != v.antenna
            || i == 0 && Some(v.antenna) != state["route"]["antenna"].as_u64();
        let flags = slot.map_or(0, |s| 4 | ((s as u32) << 8))
            | if marker {
                3 | ((v.antenna as u32) << 16)
            } else {
                0
            };
        for page in [MAP, NEXT] {
            let at = page + i * 32;
            put(&mut bytes, at, rxkeys.binary_search(&v.rx).unwrap() as u32);
            put(&mut bytes, at + 4, flags);
            if marker {
                put(&mut bytes, at + 8, version + i as u32);
            }
            put_hz(&mut bytes, at + 16, a);
            put_hz(&mut bytes, at + 24, b);
        }
        ranges.push(v.rf);
        positions.push(json!({"rf_ranges":[v.rf],"rx_lo_hz":rx_hz,"rf_center_hz":center,"antenna":v.antenna,
            "side_sign":side,"mixer":slot.is_some(),"tx_slot":slot,"tx_pll_hz":pll,"tx_tone_hz":tone,
            "mixer_lo_hz":tone.map(|_|offset),"image_rf_hz":tone.map(|_|[2*offset-b as i64,2*offset-a as i64]),
            "rx_available":true,"transition_only":false,"marker_route":marker.then_some(v.antenna),"hold_boundary":marker}));
    }
    // The guard drives TX for the full map. A mixed direct/mixer map requires
    // validated per-route gating or a measured direct model with the TX still on.
    ensure!(
        tx.is_empty() || tiles.iter().all(|t| t.tx.is_some()),
        "direct/mixer map needs TX gating at route boundary"
    );
    Ok((
        bytes,
        json!({"rf_ranges":ranges,"positions":positions,"planner_version":2,
        "mixer_threshold_hz":threshold,"mixer_input_multiplier":mult,"first_tx_pll_hz":first.map(|s|tx[&txkeys[s]].0)}),
    ))
}
