#[derive(Clone, Copy)]
pub(super) struct Span {
    first: [Option<i64>; 2],
    last: [Option<i64>; 2],
    changes: usize,
}

impl Span {
    pub(super) fn of(visits: impl IntoIterator<Item = (usize, Option<i64>)>) -> Self {
        let mut span = Self { first: [None; 2], last: [None; 2], changes: 0 };
        let mut seen = [false; 2];
        for (phase, pll) in visits {
            if seen[phase] {
                span.changes += usize::from(span.last[phase] != pll);
            } else {
                span.first[phase] = pll;
                seen[phase] = true;
            }
            span.last[phase] = pll;
        }
        span
    }

    fn then(self, following: Self) -> usize {
        (0..2).filter(|&phase| self.last[phase] != following.first[phase]).count()
    }
}

// Rows remain in request order in each phase, and B wraps back to A.
pub(super) fn minimum_cycle(rows: &[Vec<Span>]) -> Vec<usize> {
    let Some(first_row) = rows.first() else { return Vec::new() };
    let mut best = (usize::MAX, Vec::new());
    for (start, &first) in first_row.iter().enumerate() {
        let mut prior = vec![(first, first.changes, vec![start])];
        for options in &rows[1..] {
            prior = options.iter().enumerate().map(|(index, &option)| {
                let (_, cost, path) = prior.iter().map(|(end, cost, path)|
                    (end, cost + end.then(option), path))
                    .min_by(|a,b| a.1.cmp(&b.1).then_with(|| a.2.cmp(b.2))).unwrap();
                let mut path = path.clone();
                path.push(index);
                (option, cost + option.changes, path)
            }).collect();
        }
        for (end, cost, path) in prior {
            let cycle = cost + usize::from(end.last[0] != first.first[1])
                + usize::from(end.last[1] != first.first[0]);
            best = best.min((cycle, path));
        }
    }
    best.1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(pll: i64) -> Span {
        Span::of([(0, Some(pll)), (1, Some(pll))])
    }

    #[test]
    fn chooses_shared_pll_without_reordering_or_changing_phases() {
        let a = fixed(2_374_519_996);
        let b = fixed(2_574_575_018);
        assert_eq!(minimum_cycle(&[vec![a], vec![b, a], vec![a, b]]), [0, 1, 0]);
        assert_eq!(minimum_cycle(&[vec![a], vec![b], vec![a]]), [0, 0, 0]);
    }

    #[test]
    fn accounts_for_the_last_a_to_first_b_boundary_and_cycle_wrap() {
        let a = Some(2_374_519_996);
        let b = Some(2_574_575_018);
        let split = Span::of([(0, a), (1, b)]);
        let constant = Span::of([(0, b), (1, b)]);
        assert_eq!(minimum_cycle(&[vec![split, constant]]), [1]);
    }
}
