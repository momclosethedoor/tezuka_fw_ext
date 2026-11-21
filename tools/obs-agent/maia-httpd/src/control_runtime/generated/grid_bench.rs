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

pub(super) fn choices(
    grid: &[Tile], rx: &calibration::Bank, tx: &calibration::Bank,
    bounds: [u64; 2], row: usize, mult: i64,
    route: impl Fn(&Tile, [u64; 2], i64) -> Option<([u64; 2], u64)>,
) -> Vec<Vec<Tile>> {
    let mut visits = Vec::new();
    for tile in grid {
        let Some((phase, segment)) = tile.grid else { continue };
        let Some(clip) = intersection(segment, bounds) else { continue };
        let Some(&(rx_hz, _)) = rx.get(&tile.rx) else { continue };
        let (tone, side) = match tile.tx {
            Some(t) => {
                let Some(&(pll, _)) = tx.get(&(t.pll as u64)) else { continue };
                (Some(pll as i64 + OFFSET), t.side)
            }
            None => (None, 1),
        };
        let offset = tone.map_or(0, |f| f * mult);
        let Some((clip, antenna)) = route(tile, clip, offset) else { continue };
        let center = offset + side * rx_hz as i64;
        let clean = lobes(clip, center, offset, side, tone);
        let witness = [
            (center - mixer_math::HALF_PASS).max(0) as u64,
            (center + mixer_math::HALF_PASS) as u64,
        ];
        // The raw map carries one verified clean interval. VIEW carries
        // both usable lobes, clipped to the specified RF row.
        let Some(rf) = intersection(tile.rf, clip).or_else(|| clean.iter()
            .filter_map(|&r| intersection(r, witness)).max_by_key(|r| r[1] - r[0]))
            else { continue };
        let mut visit = tile.clone();
        visit.rf = rf;
        visit.rf_row = row;
        visit.antenna = antenna;
        visit.grid = Some((phase, clip));
        if !clean.is_empty() { visits.push((visit, clean)); }
    }
    let mut options = Vec::new();
    let all = visits.iter().collect::<Vec<_>>();
    if let Some(map) = covered_map(bounds, &all) { options.push(map); }
    let plls = visits.iter().filter_map(|(p, _)| p.tx.map(|t| t.pll))
        .collect::<std::collections::BTreeSet<_>>();
    if plls.len() > 1 {
        for pll in plls {
            let candidates = all.iter().copied()
                .filter(|(v, _)| v.tx.is_none_or(|t| t.pll == pll)).collect::<Vec<_>>();
            if let Some(map) = covered_map(bounds, &candidates) { options.push(map); }
        }
    }
    options
}

fn covered_map(bounds: [u64; 2], visits: &[&(Tile, Vec<[u64; 2]>)]) -> Option<Vec<Tile>> {
    let mut phases = [false; 2];
    let coverage = visits.iter().flat_map(|(v, clean)| {
        phases[usize::from(v.grid.is_some_and(|(phase, _)| phase == "offset"))] = true;
        clean.iter().copied()
    }).collect::<Vec<_>>();
    let channel = 8_000_000;
    let covers_channels = bounds[1] - bounds[0] < channel
        || covers([bounds[0], bounds[1] - channel], coverage.iter()
            .filter(|[lo, hi]| hi - lo >= channel).map(|&[lo, hi]| [lo, hi - channel]));
    (phases == [true, true] && covers(bounds, coverage) && covers_channels)
        .then(|| visits.iter().map(|(v, _)| v.clone()).collect())
}

fn intersection(a: [u64; 2], b: [u64; 2]) -> Option<[u64; 2]> {
    let clip = [a[0].max(b[0]), a[1].min(b[1])];
    (clip[0] < clip[1]).then_some(clip)
}

fn covers([mut cursor, end]: [u64; 2], spans: impl IntoIterator<Item = [u64; 2]>) -> bool {
    let mut spans: Vec<_> = spans.into_iter().collect();
    spans.sort_unstable();
    for [lo, hi] in spans {
        if lo > cursor { return false; }
        if hi < cursor { continue; }
        cursor = cursor.max(hi);
        if cursor >= end { return true; }
    }
    false
}

pub(super) fn lobes(bounds: [u64;2], center: i64, offset: i64, side: i64, tone: Option<i64>)
    -> Vec<[u64;2]> {
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
    clean.sort_unstable();
    clean
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
