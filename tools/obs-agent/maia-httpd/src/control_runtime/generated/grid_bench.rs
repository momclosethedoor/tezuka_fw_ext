//! Two consecutive RX sweeps in the same muted PL ring. Keep the production
//! route, mixing side and calibrated TX for every contiguous compatible span.
use super::*;

pub(super) fn schedule(core: &[Tile], overlap: u64, half_pass: u64) -> Result<Vec<Tile>> {
    let step = 2 * half_pass - overlap;
    let mut first = Vec::new();
    let mut second = Vec::new();
    let mut remaining = core;
    while let Some(start) = remaining.first() {
        let key = |v: &Tile| (v.antenna, v.tx.map(|t| (t.pll, t.side)));
        let count = 1 + remaining.windows(2).take_while(|w|
            w[0].rf[1] == w[1].rf[0] && key(&w[0]) == key(&w[1])).count();
        let [lo, hi] = [start.rf[0], remaining[count - 1].rf[1]];
        let width = hi - lo;
        let cells = width.div_ceil(step);
        // Equalize cells within each route so neither RF edge is lost.
        for i in 0..cells {
            let center = lo + width * (2*i + 1) / (2*cells);
            first.push(position(start, [lo,hi], center, "main")?);
        }
        for i in 0..=cells {
            second.push(position(start, [lo,hi], lo + width*i/cells, "offset")?);
        }
        remaining = &remaining[count..];
    }
    first.extend(second);
    Ok(first)
}

fn position(template: &Tile, bounds: [u64;2], center: u64, phase: &'static str) -> Result<Tile> {
    let mut v = template.clone();
    let rx = v.tx.map_or(center as i64, |t| t.side * (center as i64 - t.mixer_lo));
    ensure!((70_000_000..=6_000_000_000).contains(&rx), "grid RX LO outside chip range");
    v.rx = rx as u64;
    v.grid = Some((phase,bounds));
    // The regular encoder validates one contiguous clean interval. Capture
    // both FFT lobes for the independent offline audit against actual LO.
    let guard = (mixer_math::DC_CLEARANCE + mixer_math::READBACK_MARGIN) as u64;
    let reach = (mixer_math::HALF_PASS - mixer_math::READBACK_MARGIN) as u64;
    let left = [bounds[0].max(center - reach),bounds[1].min(center - guard)];
    let right = [bounds[0].max(center + guard),bounds[1].min(center + reach)];
    v.rf = [left,right].into_iter().max_by_key(|r| r[1].saturating_sub(r[0])).unwrap();
    ensure!(v.rf[0] < v.rf[1], "grid center has no clean bins within {:?}",bounds);
    Ok(v)
}
