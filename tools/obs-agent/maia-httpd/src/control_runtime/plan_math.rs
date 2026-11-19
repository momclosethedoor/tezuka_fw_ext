//! Pure RF/IF coverage over the ordered, calibrated RX centers.
pub(super) fn cover(positions: &[u64], offset: i64, lo: u64, hi: u64) -> Option<(i64, Vec<usize>)> {
    let side = if (lo as i64) > offset { 1 } else { -1 };
    let supported = |rf: u64| (70_000_000..=6_000_000_000).contains(&((rf as i64 - offset) * side));
    if !supported(lo) || !supported(hi) {
        return None;
    }
    let mut covered = lo;
    let mut indexes = Vec::new();
    for ordinal in 0..positions.len() {
        let idx = if side == 1 {
            ordinal
        } else {
            positions.len() - 1 - ordinal
        };
        let rf = offset + side * positions[idx] as i64;
        if !(70_000_000..=18_000_000_000).contains(&rf) {
            continue;
        }
        let f = rf as u64;
        let (a, b) = (f.saturating_sub(23_800_000), f + 23_800_000);
        if b <= lo || a >= hi {
            continue;
        }
        if a > covered {
            return None;
        }
        covered = covered.max(b);
        indexes.push(idx);
    }
    (covered >= hi).then_some((side, indexes))
}

pub(super) fn automatic(
    positions: &[u64],
    offsets: &[i64],
    previous: Option<usize>,
    lo: u64,
    hi: u64,
) -> Option<(usize, i64, Vec<usize>)> {
    let candidate = |slot: usize| {
        offsets
            .get(slot)
            .and_then(|offset| cover(positions, *offset, lo, hi))
            .map(|(side, indices)| (slot, side, indices))
    };
    previous.and_then(candidate).or_else(|| {
        (0..offsets.len())
            .filter_map(candidate)
            .min_by_key(|(slot, _, indices)| (indices.len(), *slot))
    })
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
        let offsets = [LO, 4_410_000_000, LO];
        let upper = automatic(&RX, &offsets, None, 5_841_000_000, 5_889_000_000).unwrap();
        let lower = automatic(&RX, &offsets, Some(2), 3_114_840_000, 3_162_840_000).unwrap();
        assert_eq!(upper, (0, 1, vec![0, 1, 2, 3, 4]));
        assert_eq!(lower, (2, -1, vec![4, 3, 2, 1, 0]));
        assert_eq!(LO + RX[2] as i64, 5_865_000_000);
        assert_eq!(LO - RX[2] as i64, 3_138_840_000);
        assert!(automatic(&RX, &offsets, None, 4_499_000_000, 4_504_000_000).is_none());
        assert!(
            cover(
                &[1_250_000_000, 1_450_000_000],
                LO,
                5_730_000_000,
                5_960_000_000
            )
            .is_none()
        );
    }
}
