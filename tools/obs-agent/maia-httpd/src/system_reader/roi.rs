//! Select calibrated RF bins from an existing fftshifted record.
use std::ops::Range;

pub(super) const FFT_BINS: i64 = 4096;
pub(super) const FFT_STEP_HZ: i64 = 61_440_000 / FFT_BINS;

pub(crate) fn bin_ranges(
    [start, stop]: [i64; 2],
    profile_center_hz: i64,
    side: i64,
    usable_hz: &[[i64; 2]],
    source_bins: u16,
) -> Option<Vec<Range<u16>>> {
    if start >= stop
        || !(70_000_000..=13_000_000_000).contains(&profile_center_hz)
        || !matches!(side, -1 | 1)
        || !matches!(source_bins, 512 | 1024 | 2048 | 4096)
    {
        return None;
    }
    let bins = i64::from(source_bins);
    let stride = FFT_BINS / bins;
    let margin = (stride - 1) * FFT_STEP_HZ / 2;
    let center = profile_center_hz + side * margin;
    let step = stride * FFT_STEP_HZ;
    let mut ranges = Vec::with_capacity(usable_hz.len());
    for &[usable_start, usable_stop] in usable_hz {
        // Include an aggregated VIEW bin only if all its source bins fit.
        let lo = start.max(usable_start) + margin;
        let hi = stop.min(usable_stop) - margin;
        if lo >= hi {
            continue;
        }
        let (first, end) = if side == 1 {
            let ceil_bin = |hz: i64| (hz - center + step - 1).div_euclid(step);
            (ceil_bin(lo), ceil_bin(hi))
        } else {
            let after_bin = |hz: i64| (center - hz).div_euclid(step) + 1;
            (after_bin(hi), after_bin(lo))
        };
        let clip = |index: i64| (index + bins / 2).clamp(0, bins) as u16;
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
            let limits = [roi - 28_000_000, roi + 28_000_000];
            for side in [-1, 1] {
                for bins in [512, 1024, 2048, 4096] {
                    let stride = FFT_BINS / i64::from(bins);
                    let ranges = bin_ranges(limits, center, side, &usable, bins).unwrap();
                    for bin in 0..bins {
                        let hz = |offset| center + side * (i64::from(bin) * stride + offset - FFT_BINS / 2) * FFT_STEP_HZ;
                        let endpoints = [hz(0), hz(stride - 1)];
                        let expected = endpoints.iter().all(|hz| (limits[0]..limits[1]).contains(hz))
                            && usable.iter().any(|&[lo,hi]| endpoints.iter().all(|hz| (lo..hi).contains(hz)));
                        assert_eq!(ranges.iter().any(|range| range.contains(&bin)), expected);
                    }
                }
            }
        }
    }
}
