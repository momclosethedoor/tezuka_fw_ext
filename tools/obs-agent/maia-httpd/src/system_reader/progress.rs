use std::time::Duration;

pub(super) const POLL: Duration = Duration::from_millis(100);
pub(crate) const LOST: &str = "PL producer stopped making progress";
const FFT_WINDOW_NS: u64 = (4096u64 * 1_000_000_000).div_ceil(61_440_000);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Execution {
    pub generation: u64,
    pub version: u32,
    pub anchor: u64,
    pub run: u32,
}

impl Execution {
    pub(super) fn matches(self, run: u32, map: Option<(u32, u64)>, epoch: u64) -> bool {
        self.run == run && map.is_none_or(|m| m == (self.version, self.anchor)) && epoch >= self.anchor
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum State {
    Running,
    WaitingAck,
    Backpressure,
    Stalled,
}

impl State {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::WaitingAck => "waiting_ack",
            Self::Backpressure => "backpressure",
            Self::Stalled => "stalled",
        }
    }
}

pub(super) struct Progress {
    budget: Duration,
    last: Option<[u64; 3]>,
    since: Duration,
}

impl Progress {
    pub(super) fn reset(&mut self) { self.last = None; }

    pub(super) fn new(k: u32, scan: bool, settling: u32, gate_wait_ticks: u32) -> Self {
        let group = u64::from(k) + u64::from(scan);
        let visit = (group + u64::from(settling)) * FFT_WINDOW_NS + u64::from(gate_wait_ticks) * 16;
        // Allow two accumulated groups, their split544 lookbehind, and the
        // quantization of consecutive software observation intervals.
        let budget = Duration::from_nanos(3 * visit) + 2 * POLL;
        Self { budget, last: None, since: Duration::ZERO }
    }

    pub(super) fn observe(&mut self, now: Duration, counts: [u64; 3], running: bool,
        waiting_ack: bool, backpressured: bool) -> State {
        if !running { return State::Stalled; }
        if self.last.replace(counts) != Some(counts) || waiting_ack || backpressured {
            self.since = now;
        }
        if waiting_ack { return State::WaitingAck; }
        if backpressured { return State::Backpressure; }
        if now.saturating_sub(self.since) >= self.budget { return State::Stalled; }
        State::Running
    }

    pub(super) fn budget(&self) -> Duration { self.budget }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_requires_the_same_run_and_the_confirmed_map_anchor() {
        let old = Execution { generation: 5, version: 7, anchor: 31, run: 8 };
        let next = Execution { generation: 6, version: 8, anchor: 51, run: 8 };
        assert!(old.matches(8, Some((7, 31)), 50));
        assert!(!old.matches(8, Some((8, 51)), 51));
        assert!(!next.matches(8, Some((8, 51)), 50));
        assert!(!next.matches(8, Some((8, 31)), 51));
        assert!(!next.matches(9, Some((8, 51)), 51));
        assert!(next.matches(8, Some((8, 51)), 51));
    }

    #[test]
    fn route_wait_and_backpressure_without_output_do_not_spend_progress_budget() {
        let mut p = Progress::new(2, true, 0, 256);
        let now = Duration::from_secs(5);
        let counts = [10, 9, 0];
        p.observe(Duration::ZERO, counts, true, false, false);
        assert_eq!(p.observe(now, counts, true, true, false).as_str(), "waiting_ack");
        assert_eq!(p.observe(2 * now, counts, true, false, true).as_str(), "backpressure");
        assert_eq!(p.observe(2 * now + POLL, counts, true, false, false).as_str(), "running");
        assert_eq!(p.observe(2 * now + p.budget(), counts, true, false, false).as_str(), "stalled");
    }

    #[test]
    fn k_and_mode_provide_distinct_budgets_and_dma_drops_show_progress() {
        let fast = Progress::new(1, true, 0, 256);
        let mut p = Progress::new(1023, true, 0, 256);
        assert!(fast.budget() < p.budget());
        assert!(Progress::new(700, false, 0, 0).budget() < Progress::new(700, true, 0, 0).budget());
        p.observe(Duration::ZERO,[1,1,0],true,false,false);
        assert_eq!(p.observe(p.budget(),[1,1,1],true,false,false).as_str(),"running");
        assert_eq!(p.observe(2*p.budget(),[1,1,1],true,false,false).as_str(),"stalled");
    }
}
