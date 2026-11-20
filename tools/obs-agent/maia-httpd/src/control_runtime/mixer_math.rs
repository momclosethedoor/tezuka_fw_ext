//! Signed Hz throughout. Frequency windows use continuous half-open edges.
//! The IF veto encloses the measured TX-centred sweep at fixed Fs and drive.
pub const OFFSET: i64 = 960_000;
const IF_MIN: i64 = 70_000_000;
const IF_MAX: i64 = 6_000_000_000;
const PILOT_HALF_SPAN: i64 = 61_440_000 / 2 + 4_800_000;
// At the tested gain the central DC feature crossed one VIEW code within
// 10 bins on 39/40 sides. On the remaining side a broader TX-off feature
// had unknown origin. This bound models the central feature only. Include
// a full 8-bin VIEW512 cell on each edge.
pub const DC_CLEARANCE: i64 = (10 + 8) * 15_000;
pub const HALF_PASS: i64 = 23_800_000;
// RX readback <=10 Hz plus multiplier * TX readback <=20 Hz.
pub(super) const READBACK_MARGIN: i64 = 30;
pub const TILE_WIDTH: i64 = HALF_PASS - DC_CLEARANCE - 2 * READBACK_MARGIN;

pub fn place_rx(edges: [i64; 2], exclude: Option<i64>) -> Option<i64> {
    let [a, b] = edges;
    if a < IF_MIN || a >= b || b > IF_MAX || b - a > TILE_WIDTH {
        return None;
    }
    let guard = DC_CLEARANCE + READBACK_MARGIN;
    [a - guard, b + guard]
        .into_iter()
        .filter(|v| (IF_MIN..=IF_MAX).contains(v))
        .filter(|v| exclude.is_none_or(|x| v.abs_diff(x) >= 15_000))
        .max_by_key(|v| exclude.map_or(0, |x| v.abs_diff(x)))
}

/// Prefer a PLL for which the image of the whole requested tile is outside
/// the admitted antenna bands. An in-band image remains visible in readback.
pub fn choose_on_antenna(
    rf: [i64; 2],
    m: i64,
    p: [i64; 2],
    previous: Option<i64>,
    exclude: Option<i64>,
    bands: &[[u64; 2]],
) -> Option<Tuning> {
    let fallback = choose(rf, m, p, previous, exclude)?;
    let mut free = vec![p];
    for &[a, b] in bands {
        if a >= b || b > 18_000_000_000 {
            return None;
        }
        let lo = (a as i64 + rf[0]).div_euclid(2 * m) - OFFSET + 1;
        let hi = (b as i64 + rf[1] - 1).div_euclid(2 * m) - OFFSET;
        let mut next = Vec::new();
        for [l, h] in free {
            if h < lo || l > hi {
                next.push([l, h]);
                continue;
            }
            if l < lo {
                next.push([l, lo - 1]);
            }
            if hi < h {
                next.push([hi + 1, h]);
            }
        }
        free = next;
    }
    free.into_iter()
        .filter_map(|r| choose(rf, m, r, previous, exclude))
        .min_by_key(|t| (Some(t.pll) != previous, t.pll.abs_diff(fallback.pll)))
        .or(Some(fallback))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Tuning {
    pub pll: i64,
    pub mixer_lo: i64,
    pub side: i64,
    pub if_edges: [i64; 2],
}

/// Solve for an integer PLL interval using the four edges of the useful IF,
/// the measured IF veto, and the limits on PLL and IF. All inequalities are
/// affine in PLL. No search grid or rejected bins.
pub fn choose(
    rf: [i64; 2],
    mult: i64,
    pll_range: [i64; 2],
    previous: Option<i64>,
    exclude: Option<i64>,
) -> Option<Tuning> {
    let [a, b] = rf;
    if !(1..=2).contains(&mult)
        || a < IF_MIN
        || a >= b
        || b > 18_000_000_000
        || pll_range[0] < IF_MIN
        || pll_range[1] > IF_MAX - OFFSET
        || pll_range[0] > pll_range[1]
    {
        return None;
    }
    let mut best = None;
    for side in [1, -1] {
        // if_lo = k * PLL + l, if_hi = k * PLL + h
        let k = -side * mult;
        let [l, h] = if side == 1 {
            [a - mult * OFFSET, b - mult * OFFSET]
        } else {
            [mult * OFFSET - b, mult * OFFSET - a]
        };
        for below in [true, false] {
            let mut lo = pll_range[0];
            let mut hi = pll_range[1];
            // coeff * PLL >= value. Strict separation of measured edges by one Hz.
            let clean = if below {
                (1 - k, h - OFFSET + PILOT_HALF_SPAN + 1 + READBACK_MARGIN)
            } else {
                (k - 1, OFFSET + PILOT_HALF_SPAN + 1 + READBACK_MARGIN - l)
            };
            for (coeff, value) in [(k, IF_MIN - l), (-k, h - IF_MAX), clean] {
                if coeff > 0 {
                    lo = lo.max(-(-value).div_euclid(coeff));
                } else if coeff < 0 {
                    hi = hi.min((-value).div_euclid(-coeff));
                } else if value > 0 {
                    hi = lo - 1;
                    break;
                }
            }
            if lo > hi {
                continue;
            }
            let mid = lo + (hi - lo) / 2;
            for pll in [
                previous.unwrap_or(mid).clamp(lo, hi),
                mid,
                lo + (mid - lo) / 2,
                mid + (hi - mid) / 2,
                lo,
                hi,
            ] {
                // The two images of this target are disjoint if the external LO
                // changes by more than half the target width (same side).
                if exclude.is_some_and(|old| {
                    pll.abs_diff(old).saturating_mul((2 * mult) as u64) <= (b - a) as u64 + 40
                }) {
                    continue;
                }
                let rank = (
                    previous != Some(pll),
                    -(hi - lo),
                    pll.abs_diff(mid),
                    -side,
                    pll,
                );
                let t = Tuning {
                    pll,
                    mixer_lo: mult * (pll + OFFSET),
                    side,
                    if_edges: [k * pll + l, k * pll + h],
                };
                if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                    best = Some((rank, t));
                }
            }
        }
    }
    best.map(|(_, t)| t)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owned_boundary_holdout_spur_and_alternative() {
        let rf = [6_748_000_000, 6_752_000_000];
        assert!(choose(rf, 2, [2_249_040_000; 2], None, None).is_none());
        let limits = [2_250_000_000, 2_490_000_000];
        let main = choose(rf, 2, limits, None, None).unwrap();
        let other = choose(rf, 2, limits, None, Some(main.pll)).unwrap();
        assert!(other.mixer_lo.abs_diff(main.mixer_lo) * 2 > (rf[1] - rf[0]) as u64);
        for t in [main, other] {
            assert_eq!(
                [
                    t.mixer_lo + t.side * t.if_edges[0],
                    t.mixer_lo + t.side * t.if_edges[1]
                ],
                rf
            );
            assert!(place_rx(t.if_edges, None).is_some());
        }
        let image = [
            (2 * main.mixer_lo - rf[1]) as u64,
            (2 * main.mixer_lo - rf[0]) as u64,
        ];
        let filtered = choose_on_antenna(rf, 2, limits, None, None, &[image]).unwrap();
        assert!(
            2 * filtered.mixer_lo - rf[0] <= image[0] as i64
                || 2 * filtered.mixer_lo - rf[1] >= image[1] as i64
        );
        assert!(choose(rf, 2, [main.pll; 2], None, Some(main.pll)).is_none());
    }
    #[test]
    fn owned_boundary_useful_band_and_dc_are_disjoint_even_after_readback() {
        for a in [IF_MIN, 1_000_000_000, IF_MAX - TILE_WIDTH] {
            for w in [1, TILE_WIDTH - 1, TILE_WIDTH] {
                let b = a + w;
                let rx = place_rx([a, b], None).unwrap();
                assert!(
                    rx + DC_CLEARANCE + READBACK_MARGIN <= a
                        || rx - DC_CLEARANCE - READBACK_MARGIN >= b
                );
                assert!(
                    rx.abs_diff(a) <= (HALF_PASS - READBACK_MARGIN) as u64
                        && rx.abs_diff(b) <= (HALF_PASS - READBACK_MARGIN) as u64
                );
                if a == 1_000_000_000 {
                    assert_ne!(rx, place_rx([a, b], Some(rx)).unwrap());
                }
            }
        }
        for bad in [
            [0, 1],
            [IF_MAX, IF_MAX],
            [IF_MIN, IF_MIN + TILE_WIDTH + 1],
            [i64::MIN, i64::MAX],
        ] {
            assert!(place_rx(bad, None).is_none());
        }
    }
    #[test]
    fn owned_boundary_integer_solver_matches_exhaustive_pll_and_sides() {
        for p in [70_000_000, 2_250_000_000, 5_999_039_980] {
            for mult in [1, 2] {
                for d in [-1, 1] {
                    for f in [
                        IF_MIN,
                        IF_MAX,
                        p + OFFSET - PILOT_HALF_SPAN,
                        p + OFFSET + PILOT_HALF_SPAN,
                    ] {
                        for shift in [-150, -1, 0, 1, 150] {
                            let a = mult * (p + OFFSET) + d * f + shift;
                            let b = a + 120;
                            let allowed = |pll: i64, s: i64| {
                                let x = s * (a - mult * (pll + OFFSET));
                                let y = s * (b - mult * (pll + OFFSET));
                                let l = x.min(y);
                                let h = x.max(y);
                                a >= IF_MIN
                                    && b <= 18_000_000_000
                                    && l >= IF_MIN
                                    && h <= IF_MAX
                                    && (h < pll + OFFSET - PILOT_HALF_SPAN - READBACK_MARGIN
                                        || l > pll + OFFSET + PILOT_HALF_SPAN + READBACK_MARGIN)
                            };
                            let answer = choose([a, b], mult, [p, p + 20], None, None);
                            assert_eq!(
                                answer.is_some(),
                                (p..=p + 20).any(|q| allowed(q, 1) || allowed(q, -1))
                            );
                            if let Some(t) = answer {
                                assert!(allowed(t.pll, t.side));
                            }
                        }
                    }
                }
            }
        }
        assert!(choose([i64::MIN, i64::MAX], 2, [0, 1], None, None).is_none());
    }
}
