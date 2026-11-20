//! Signed Hz throughout. Frequency windows use continuous half-open edges.
//! The IF veto encloses the measured TX-centred sweep at fixed Fs and drive.
pub const OFFSET: i64 = 960_000;
const IF_MIN: i64 = 70_000_000;
const IF_MAX: i64 = 6_000_000_000;
const PILOT_HALF_SPAN: i64 = 61_440_000 / 2 + 4_800_000;

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
                (1 - k, h - OFFSET + PILOT_HALF_SPAN + 1)
            } else {
                (k - 1, OFFSET + PILOT_HALF_SPAN + 1 - l)
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
            for pll in [previous.unwrap_or(mid).clamp(lo, hi), mid, lo, hi] {
                // The two images of this target are disjoint if the external LO
                // changes by more than half the target width (same side).
                if exclude
                    .is_some_and(|old| pll.abs_diff(old) * (2 * mult) as u64 <= (b - a) as u64 + 40)
                {
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
                                    && (h < pll + OFFSET - PILOT_HALF_SPAN
                                        || l > pll + OFFSET + PILOT_HALF_SPAN)
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
