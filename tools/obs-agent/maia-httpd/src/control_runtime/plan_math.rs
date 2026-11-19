//! Pure RF/IF coverage over ordered, calibrated RX centers.
pub(super) const MAX_RF: u64 = 12_500_000_000;
const MIN_IF: i64 = 70_000_000;
const MAX_IF: i64 = 6_000_000_000;

pub(super) struct Candidate<'a> {
    pub offset: i64,
    pub antenna_bands: &'a [[u64; 2]],
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Segment {
    pub slot: Option<usize>,
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
        if rf < MIN_IF || rf > MAX_RF as i64 + 23_800_000 {
            continue;
        }
        let f = rf as u64;
        let (a, b) = (f.saturating_sub(23_800_000), f + 23_800_000);
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
        slot: None,
        side,
        lo,
        hi: covered,
        indexes: used.into_iter().flatten().collect(),
    })
}

pub(super) fn cover(
    positions: &[u64],
    candidate: &Candidate<'_>,
    slot: Option<usize>,
    lo: u64,
    hi: u64,
) -> Option<Segment> {
    let mut segment = prefix(positions, candidate, lo, hi)?;
    if segment.hi != hi {
        return None;
    }
    segment.slot = slot;
    Some(segment)
}

pub(super) fn automatic(
    positions: &[u64],
    candidates: &[Candidate<'_>],
    mut previous: Option<usize>,
    mut lo: u64,
    hi: u64,
) -> Option<Vec<Segment>> {
    let mut plan = Vec::new();
    while lo < hi {
        // Farthest contiguous endpoint first. Equal reaches reuse the current
        // slot, then prefer fewer RX positions and the lowest slot number.
        let segment = candidates
            .iter()
            .enumerate()
            .filter_map(|(slot, candidate)| {
                let mut segment = prefix(positions, candidate, lo, hi)?;
                segment.slot = Some(slot);
                Some(segment)
            })
            .min_by_key(|s| {
                (
                    std::cmp::Reverse(s.hi),
                    s.slot != previous,
                    image_overlap(&candidates[s.slot.unwrap()], s.lo, s.hi).unwrap_or(u64::MAX),
                    s.indexes.len(),
                    s.slot,
                )
            })?;
        lo = segment.hi;
        previous = segment.slot;
        plan.push(segment);
        if plan.len() > 128 {
            return None;
        }
    }
    Some(plan)
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
    fn owned_boundary_full_range_uses_one_slot_and_labels_both_images() {
        let candidates = [LO, 4_410_000_000, LO].map(|offset| Candidate {
            offset,
            antenna_bands: &[],
        });
        let upper = automatic(&RX, &candidates, None, 5_841_000_000, 5_889_000_000).unwrap();
        let lower = automatic(&RX, &candidates, Some(2), 3_114_840_000, 3_162_840_000).unwrap();
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
                Some(0),
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
                antenna_bands: &a,
            },
            Candidate {
                offset: LO + 18_000_000,
                antenna_bands: &b,
            },
            Candidate {
                offset: LO + 36_000_000,
                antenna_bands: &c,
            },
        ];
        let plan = automatic(&RX, &profiles, Some(0), 5_840_000_000, 5_905_000_000).unwrap();
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
}
