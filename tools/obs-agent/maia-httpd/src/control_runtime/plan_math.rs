//! Pure RF/IF coverage over ordered, calibrated RX centers.
pub(super) const MAX_RF: u64 = 18_000_000_000;
pub(super) const DIRECT_LIMIT: u64 = 6_000_000_000;
const HALF_USEFUL: u64 = 23_800_000;
const MIN_IF: i64 = 70_000_000;
const MAX_IF: i64 = 6_000_000_000;

/// RX PLL targets for half-open direct RF intervals. Split at antenna edges
/// before centering each tile so a short last tile keeps its requested bins.
pub(super) fn direct_centers(ranges: &[[u64; 2]], bands: &[[u64; 2]]) -> Result<Vec<u64>, String> {
    let mut positions = Vec::new();
    for &[lo, hi] in ranges {
        if lo < MIN_IF as u64 || hi > MAX_IF as u64 || hi <= lo {
            return Err(format!("direct RF outside tuner bounds: [{lo},{hi})"));
        }
        let mut cursor = lo;
        while cursor < hi {
            let end = if bands.is_empty() {
                hi
            } else {
                bands
                    .iter()
                    .filter(|b| b[0] <= cursor && cursor < b[1])
                    .map(|b| b[1].min(hi))
                    .max()
                    .ok_or_else(|| {
                        let next = bands
                            .iter()
                            .map(|b| b[0])
                            .filter(|a| *a > cursor)
                            .min()
                            .unwrap_or(hi)
                            .min(hi);
                        format!("no antenna covers [{cursor},{next}) Hz")
                    })?
            };
            while cursor < end {
                let edge = end.min(cursor + 2 * HALF_USEFUL);
                positions.push(cursor + (edge - cursor) / 2);
                cursor = edge;
            }
        }
    }
    positions.sort_unstable();
    positions.dedup();
    Ok(positions)
}

pub(super) struct Candidate<'a> {
    pub offset: i64,
    pub slot: Option<usize>,
    pub antenna: u64,
    pub antenna_bands: &'a [[u64; 2]],
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Segment {
    pub slot: Option<usize>,
    pub antenna: u64,
    pub side: i64,
    pub lo: u64,
    pub hi: u64,
    pub indexes: Vec<usize>,
}

pub(super) fn image_overlap(candidate: &Candidate<'_>, lo: u64, hi: u64) -> Option<u64> {
    if candidate.antenna_bands.is_empty() {
        return None;
    }
    let (a, b) = (
        2 * candidate.offset - hi as i64,
        2 * candidate.offset - lo as i64,
    );
    let mut covered = a;
    let mut overlap = 0;
    for [lo, hi] in candidate.antenna_bands {
        let (start, end) = ((*lo as i64).max(covered), (*hi as i64).min(b));
        if end > start {
            overlap += (end - start) as u64;
            covered = end;
        }
    }
    Some(overlap)
}

fn prefix(positions: &[u64], candidate: &Candidate<'_>, lo: u64, hi: u64) -> Option<Segment> {
    let offset = candidate.offset;
    let side = if (lo as i64) > offset { 1 } else { -1 };
    if !(MIN_IF..=MAX_IF).contains(&((lo as i64 - offset) * side)) {
        return None;
    }
    let edge = if side == 1 {
        offset + MAX_IF
    } else {
        offset - MIN_IF
    };
    let mut hi = hi.min(edge as u64).min(MAX_RF);
    if !candidate.antenna_bands.is_empty() {
        let mut limit = lo;
        for [a, b] in candidate.antenna_bands {
            if *a > limit {
                break;
            }
            limit = limit.max(*b);
        }
        hi = hi.min(limit);
    }
    if hi <= lo {
        return None;
    }
    let mut covered = lo;
    let mut intervals = Vec::with_capacity(2 * positions.len());
    for ordinal in 0..positions.len() {
        let idx = if side == 1 {
            ordinal
        } else {
            positions.len() - 1 - ordinal
        };
        let rf = offset + side * positions[idx] as i64;
        if !(MIN_IF as u64..=MAX_IF as u64).contains(&positions[idx])
            || rf < 0
            || rf > MAX_RF as i64 + HALF_USEFUL as i64
        {
            continue;
        }
        let f = rf as u64;
        let (a, b) = (f.saturating_sub(HALF_USEFUL), f + HALF_USEFUL);
        if candidate.offset == 0 {
            intervals.push((a, b, ordinal, idx));
        } else {
            // Match the conservative exclusion at DC for the active VIEW512.
            intervals.push((a, f - 360_000, ordinal, idx));
            intervals.push((f + 360_000, b, ordinal, idx));
        }
    }
    intervals.sort_unstable();
    let mut used = vec![None; positions.len()];
    for (a, b, ordinal, idx) in intervals {
        if b <= lo || a >= hi {
            continue;
        }
        if a > covered {
            break;
        }
        covered = covered.max(b.min(hi));
        used[ordinal] = Some(idx);
    }
    (covered > lo).then(|| Segment {
        slot: candidate.slot,
        antenna: candidate.antenna,
        side,
        lo,
        hi: covered,
        indexes: used.into_iter().flatten().collect(),
    })
}

pub(super) fn cover(
    positions: &[u64],
    candidate: &Candidate<'_>,
    lo: u64,
    hi: u64,
) -> Option<Segment> {
    prefix(positions, candidate, lo, hi).filter(|segment| segment.hi == hi)
}

pub(super) fn automatic(
    positions: &[u64],
    candidates: &[Candidate<'_>],
    mut previous: Option<(Option<usize>, u64)>,
    mut lo: u64,
    hi: u64,
) -> Option<Vec<Segment>> {
    let mut plan = Vec::new();
    while lo < hi {
        // Farthest contiguous endpoint first. Equal reaches reuse the current
        // slot, then prefer fewer RX positions and the lowest slot number.
        let (segment, _) = candidates
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                prefix(positions, candidate, lo, hi).map(|s| (s, index))
            })
            .min_by_key(|(s, index)| {
                (
                    std::cmp::Reverse(s.hi),
                    previous.is_some_and(|(slot, _)| s.slot.is_some() && slot != s.slot),
                    previous.is_some_and(|(_, antenna)| antenna != s.antenna),
                    image_overlap(&candidates[*index], s.lo, s.hi).unwrap_or(u64::MAX),
                    s.indexes.len(),
                    s.slot,
                    s.antenna,
                )
            })?;
        lo = segment.hi;
        previous = Some((segment.slot, segment.antenna));
        plan.push(segment);
        if plan.len() > 128 {
            return None;
        }
    }
    Some(plan)
}

/// Select an overlap window wholly inside both RF coverage bands. Reuse this
/// window for an early notice and hold at the subsequent main RX position.
pub(super) fn handover(
    positions: &[u64],
    before: &Candidate<'_>,
    after: &Candidate<'_>,
    window: [u64; 2],
    boundary: u64,
) -> Option<([u64; 2], usize, i64, usize, i64)> {
    let mut best = None;
    for [alo, ahi] in before.antenna_bands {
        for [blo, bhi] in after.antenna_bands {
            let lo = window[0].max(*alo).max(*blo);
            let hi = window[1].min(*ahi).min(*bhi);
            if hi.saturating_sub(lo) < 2 * HALF_USEFUL || boundary < lo || boundary > hi {
                continue;
            }
            let mid = lo + (hi - lo) / 2;
            let pick = |candidate: &Candidate<'_>| {
                positions
                    .iter()
                    .enumerate()
                    .flat_map(|(i, &f)| {
                        let offset = candidate.offset;
                        [1i64, -1]
                            .into_iter()
                            .map(move |side| (i, side, offset + side * f as i64))
                    })
                    .filter(|(_, _, rf)| {
                        *rf >= (lo + HALF_USEFUL) as i64 && *rf <= (hi - HALF_USEFUL) as i64
                    })
                    .min_by_key(|(_, _, rf)| rf.abs_diff(mid as i64))
            };
            let (Some((a, sa, fra)), Some((b, sb, frb))) = (pick(before), pick(after)) else {
                continue;
            };
            let rank = (
                fra.abs_diff(mid as i64) + frb.abs_diff(mid as i64),
                std::cmp::Reverse(hi - lo),
            );
            if best.as_ref().is_none_or(|(key, _)| rank < *key) {
                best = Some((rank, ([lo, hi], a, sa, b, sb)));
            }
        }
    }
    best.map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;
    const RX: [u64; 5] = [
        1_339_080_000,
        1_351_080_000,
        1_363_080_000,
        1_375_080_000,
        1_387_080_000,
    ];
    const LO: i64 = 4_501_920_000;

    #[test]
    fn owned_boundary_generated_centers_and_high_mixer_rf() {
        let lo = 70_000_000;
        let hi = lo + 2 * HALF_USEFUL + 1;
        assert_eq!(
            direct_centers(&[[lo, hi]], &[]).unwrap(),
            [lo + HALF_USEFUL, hi - 1]
        );
        assert!(direct_centers(&[[lo, hi]], &[[lo, lo + 1], [lo + 2, hi]]).is_err());
        assert!(direct_centers(&[[lo, u64::MAX]], &[]).is_err());
        let mixer = Candidate {
            offset: 9_500_000_000,
            slot: Some(0),
            antenna: 0,
            antenna_bands: &[],
        };
        for center in [8_150_000_000, 10_850_000_000] {
            assert!(
                cover(
                    &[1_345_000_000, 1_355_000_000],
                    &mixer,
                    center - 1_000_000,
                    center + 1_000_000
                )
                .is_some()
            );
        }
    }

    #[test]
    fn owned_boundary_full_range_uses_one_slot_and_labels_both_images() {
        let candidates = std::array::from_fn::<_, 3, _>(|i| Candidate {
            offset: [LO, 4_410_000_000, LO][i],
            slot: Some(i),
            antenna: 3,
            antenna_bands: &[],
        });
        let upper = automatic(&RX, &candidates, None, 5_841_000_000, 5_889_000_000).unwrap();
        let lower = automatic(
            &RX,
            &candidates,
            Some((Some(2), 3)),
            3_114_840_000,
            3_162_840_000,
        )
        .unwrap();
        assert_eq!((upper.len(), upper[0].slot, upper[0].side), (1, Some(0), 1));
        assert_eq!(
            (lower.len(), lower[0].slot, lower[0].side),
            (1, Some(2), -1)
        );
        assert_eq!(upper[0].indexes, [0, 1, 2, 3, 4]);
        assert_eq!(lower[0].indexes, [4, 3, 2, 1, 0]);
        assert_eq!(LO + RX[2] as i64, 5_865_000_000);
        assert_eq!(LO - RX[2] as i64, 3_138_840_000);
        assert!(automatic(&RX, &candidates, None, 4_499_000_000, 4_504_000_000).is_none());
        assert!(
            cover(
                &[1_250_000_000, 1_450_000_000],
                &candidates[0],
                5_730_000_000,
                5_960_000_000
            )
            .is_none()
        );
    }

    #[test]
    fn owned_boundary_auto_uses_largest_useful_chunks_and_antenna_bands() {
        let a = [[5_830_000_000, 5_870_000_000]];
        let b = [[5_840_000_000, 5_890_000_000]];
        let c = [[5_870_000_000, 5_910_000_000]];
        let profiles = [
            Candidate {
                offset: LO,
                slot: Some(0),
                antenna: 0,
                antenna_bands: &a,
            },
            Candidate {
                offset: LO + 18_000_000,
                slot: Some(1),
                antenna: 1,
                antenna_bands: &b,
            },
            Candidate {
                offset: LO + 36_000_000,
                slot: Some(2),
                antenna: 2,
                antenna_bands: &c,
            },
        ];
        let plan = automatic(
            &RX,
            &profiles,
            Some((Some(0), 0)),
            5_840_000_000,
            5_905_000_000,
        )
        .unwrap();
        assert_eq!(
            plan.iter()
                .map(|p| (p.slot, p.lo, p.hi))
                .collect::<Vec<_>>(),
            [
                (Some(1), 5_840_000_000, 5_890_000_000),
                (Some(2), 5_890_000_000, 5_905_000_000)
            ]
        );
        assert!(automatic(&RX, &profiles, None, 5_840_000_000, 5_911_000_000).is_none());
        assert!(automatic(&RX, &profiles, None, 12_500_000_000, 14_000_000_000).is_none());
    }

    #[test]
    fn owned_boundary_overlap_profile_is_inside_both_antennas() {
        let old = [[2_300_000_000, 2_470_000_000]];
        let new = [[2_385_000_000, 2_550_000_000]];
        let a = Candidate {
            offset: 0,
            slot: None,
            antenna: 1,
            antenna_bands: &old,
        };
        let b = Candidate {
            offset: 0,
            slot: None,
            antenna: 2,
            antenna_bands: &new,
        };
        let centers = [
            2_380_000_000,
            2_400_000_000,
            2_425_000_000,
            2_440_000_000,
            2_465_000_000,
        ];
        assert_eq!(
            handover(
                &centers,
                &a,
                &b,
                [2_380_000_000, 2_490_000_000],
                2_470_000_000
            ),
            Some(([2_385_000_000, 2_470_000_000], 2, 1, 2, 1))
        );
        assert!(
            handover(
                &centers,
                &a,
                &b,
                [2_440_000_000, 2_490_000_000],
                2_470_000_000
            )
            .is_none()
        );
    }
}
