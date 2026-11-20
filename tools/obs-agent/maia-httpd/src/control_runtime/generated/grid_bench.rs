//! Two consecutive RX sweeps in the same PL ring. Keep the production
//! route, mixing side and calibrated TX for every contiguous compatible span.
use super::*;

pub(super) fn schedule(core: &[Tile], overlap: u64, half_pass: u64, state: Option<&Value>) -> Result<Vec<Tile>> {
    let step = 2 * half_pass - overlap;
    let mut first = Vec::new();
    let mut second = Vec::new();
    let mut remaining = core;
    let bands = parse_antenna_bands(state.map_or(&Value::Null, |s| &s["scan"]["antenna_bands"]))?;
    let mult = environment()?.1;
    let mut previous: Option<&Tile> = None;
    while let Some(start) = remaining.first() {
        let key = |v: &Tile| (v.rf_row, v.antenna, v.tx.map(|t| (t.pll, t.side)));
        let count = 1 + remaining.windows(2).take_while(|w|
            w[0].rf[1] == w[1].rf[0] && key(&w[0]) == key(&w[1])).count();
        let [lo, hi] = [start.rf[0], remaining[count - 1].rf[1]];
        let width = hi - lo;
        let cells = width.div_ceil(step);
        if let Some((old, request)) = previous.zip(state) && old.rf_row == start.rf_row && old.rf[1] == lo {
            let r = &request["scan"]["ranges"][start.rf_row];
            let requested = [r["lo_hz"].as_u64().unwrap(), r["hi_hz"].as_u64().unwrap()];
            let window = [requested[0].max(lo - 8_000_000), requested[1].min(lo + 8_000_000)];
            if window[1] - window[0] >= 8_000_000 {
                for candidate in [start, old] {
                    let admits = bands.get(&candidate.antenna).is_none_or(|rs|
                        rs.iter().any(|r| r[0] <= window[0] && window[1] <= r[1]));
                    if admits && let Some(v) = channel_bridge(candidate, window, mult) {
                        second.push(v);
                        break;
                    }
                }
            }
        }
        // Equalize cells within each route so neither RF edge is lost.
        for i in 0..cells {
            let center = lo + width * (2*i + 1) / (2*cells);
            first.push(position(start, [lo,hi], center, "main")?);
        }
        for i in 0..=cells {
            second.push(position(start, [lo,hi], lo + width*i/cells, "offset")?);
        }
        previous = Some(&remaining[count-1]);
        remaining = &remaining[count..];
    }
    first.extend(second);
    Ok(first)
}

fn channel_bridge(template: &Tile, window: [u64;2], mult: i64) -> Option<Tile> {
    let mut v = template.clone();
    let edges = window.map(|f| f as i64);
    let if_edges = if let Some(old) = v.tx {
        let tuned = choose(edges, mult, [old.pll;2], Some(old.pll), None)?;
        if tuned.side != old.side {return None;}
        v.tx = Some(tuned);
        tuned.if_edges
    } else {edges};
    v.rx = mixer_math::place_rx(if_edges, None)? as u64;
    v.rf = window;
    v.grid = Some(("offset", window));
    v.channel_bridge = true;
    Some(v)
}

fn position(template: &Tile, bounds: [u64;2], center: u64, phase: &'static str) -> Result<Tile> {
    let mut v = template.clone();
    v.grid = Some((phase,bounds));
    // Close sub-MHz requests with complementary edge tunings. A nominal
    // grid center can otherwise leave an entire short band inside DC.
    let guard = (mixer_math::DC_CLEARANCE + mixer_math::READBACK_MARGIN) as u64;
    if bounds[1] - bounds[0] <= 2 * guard {
        let [a,b] = bounds.map(|f| f as i64);
        let if_edges = match v.tx {
            Some(t) if t.side == -1 => [t.mixer_lo-b,t.mixer_lo-a],
            Some(t) => [a-t.mixer_lo,b-t.mixer_lo],
            None => [a,b],
        };
        let preferred = mixer_math::place_rx(if_edges,None)
            .ok_or_else(||anyhow!("short grid interval outside receiver limits"))?;
        v.rx = if phase == "main" {preferred} else {
            mixer_math::place_rx(if_edges,Some(preferred)).unwrap_or(preferred)
        } as u64;
        v.rf = bounds;
        return Ok(v);
    }
    let rx = v.tx.map_or(center as i64, |t| t.side * (center as i64 - t.mixer_lo));
    ensure!((70_000_000..=6_000_000_000).contains(&rx), "grid RX LO outside chip range");
    v.rx = rx as u64;
    // The regular encoder validates one contiguous clean interval. Capture
    // both FFT lobes for the independent offline audit against actual LO.
    let reach = (mixer_math::HALF_PASS - mixer_math::READBACK_MARGIN) as u64;
    let left = [bounds[0].max(center - reach),bounds[1].min(center - guard)];
    let right = [bounds[0].max(center + guard),bounds[1].min(center + reach)];
    v.rf = [left,right].into_iter().max_by_key(|r| r[1].saturating_sub(r[0])).unwrap();
    ensure!(v.rf[0] < v.rf[1], "grid center has no clean bins within {:?}",bounds);
    Ok(v)
}

pub(super) fn lobes(bounds: [u64;2], center: i64, offset: i64, side: i64, tone: Option<i64>)
    -> Result<Vec<[u64;2]>> {
    let [lo,hi] = bounds.map(|f| f as i64);
    let extent = 27_000_000;
    let dc = mixer_math::DC_CLEARANCE;
    let spans = [[center-extent,center-dc],[center+dc,center+extent]];
    let mut passbands = vec![[70_000_000,6_000_000_000]];
    if let Some(tx) = tone {
        passbands = vec![[70_000_000,tx-35_520_000-1],[tx+35_520_000+1,6_000_000_000]];
    }
    let mut clean = Vec::new();
    for [a,b] in spans {
        for &[x,y] in &passbands {
            if x >= y {continue;}
            let mut usable = [offset+side*x,offset+side*y];
            usable.sort_unstable();
            let l = a.max(usable[0]).max(lo);
            let h = b.min(usable[1]).min(hi);
            if l < h {clean.push([l as u64,h as u64]);}
        }
    }
    ensure!(!clean.is_empty(),"empty calibrated grid coverage in {bounds:?}");
    clean.sort_unstable();
    Ok(clean)
}

pub(super) fn validate_cover(desc: &Value) -> Result<()> {
    let ranges = desc["requested_ranges"].as_array().unwrap();
    let mut covered = vec![Vec::new();ranges.len()];
    for p in desc["positions"].as_array().unwrap() {
        if p["transition_only"] != true {
            let row = p["rf_row"].as_u64().unwrap() as usize;
            for pair in p["rf_ranges"].as_array().unwrap() {
                covered[row].push(serde_json::from_value::<[u64;2]>(pair.clone())?);
            }
        }
    }
    for (r,mut intervals) in ranges.iter().zip(covered) {
        if r["enabled"] != true {continue;}
        let mut cursor = r["lo_hz"].as_u64().unwrap();
        let end = r["hi_hz"].as_u64().unwrap();
        intervals.sort_unstable();
        for [lo,hi] in intervals {
            ensure!(lo <= cursor && hi <= end,"uncovered RF [{cursor},{lo}) after calibration");
            cursor = cursor.max(hi);
        }
        ensure!(cursor == end,"uncovered RF [{cursor},{end}) after calibration");
    }
    Ok(())
}
