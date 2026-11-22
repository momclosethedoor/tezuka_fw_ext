//! Select calibrated RF bins from an existing fftshifted record.
use std::ops::Range;

pub(crate) const WIDTH_HZ: i64 = 56_000_000;
const BINS: i64 = 4096;
const STEP_HZ: i64 = 61_440_000 / BINS;

pub(crate) fn bin_ranges(
    roi_center_hz: i64,
    profile_center_hz: i64,
    side: i64,
    usable_hz: &[[i64; 2]],
) -> Option<Vec<Range<u16>>> {
    if !(70_000_000..=13_000_000_000).contains(&roi_center_hz)
        || !(70_000_000..=13_000_000_000).contains(&profile_center_hz)
        || !matches!(side, -1 | 1)
    {
        return None;
    }
    let half = WIDTH_HZ / 2;
    let start = roi_center_hz - half;
    let stop = roi_center_hz + half;
    let mut ranges = Vec::with_capacity(usable_hz.len());
    for &[usable_start, usable_stop] in usable_hz {
        let lo = start.max(usable_start);
        let hi = stop.min(usable_stop);
        if lo >= hi {
            continue;
        }
        let (first, end) = if side == 1 {
            let ceil_bin = |hz: i64| (hz - profile_center_hz + STEP_HZ - 1).div_euclid(STEP_HZ);
            (ceil_bin(lo), ceil_bin(hi))
        } else {
            let after_bin = |hz: i64| (profile_center_hz - hz).div_euclid(STEP_HZ) + 1;
            (after_bin(hi), after_bin(lo))
        };
        let clip = |index: i64| (index + BINS / 2).clamp(0, BINS) as u16;
        let range = clip(first)..clip(end);
        if !range.is_empty() {
            ranges.push(range);
        }
    }
    ranges.sort_unstable_by_key(|range| range.start);
    let mut merged: Vec<Range<u16>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(last) = merged.last_mut().filter(|last| last.end >= range.start) {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    Some(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_selection_matches_rf_coordinate_for_both_mixer_orientations() {
        let center = 5_865_000_000;
        let usable = [[center - 27_000_000, center - 500_000],
            [center + 500_000, center + 27_000_000]];
        for roi in [center, center + 28_000_000, center - 28_000_001, center + 56_000_000] {
            for side in [-1, 1] {
                let ranges = bin_ranges(roi, center, side, &usable).unwrap();
                for bin in 0..BINS {
                    let hz = center + side * (bin - BINS / 2) * STEP_HZ;
                    let expected = (roi - WIDTH_HZ/2..roi + WIDTH_HZ/2).contains(&hz)
                        && usable.iter().any(|&[lo,hi]| (lo..hi).contains(&hz));
                    assert_eq!(ranges.iter().any(|range| range.contains(&(bin as u16))), expected);
                }
            }
        }
    }
}
