use std::time::Duration;

#[derive(Clone, Copy)]
pub(super) struct Histogram {
    pub bins: [u64; 32],
    pub max_ns: u64,
}

impl Default for Histogram {
    fn default() -> Self { Self { bins: [0;32], max_ns: 0 } }
}

impl Histogram {
    pub(super) fn observe(&mut self, duration: Duration) {
        let ns=duration.as_nanos().min(u64::MAX as u128) as u64;
        let exponent=u64::BITS-ns.saturating_sub(1).leading_zeros();
        self.bins[exponent.min(31) as usize]+=1;
        self.max_ns=self.max_ns.max(ns);
    }
}
