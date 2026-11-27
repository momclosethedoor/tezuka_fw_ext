//! One independent FIFO for paced managed VIEW UDP, including pass outcomes.

use std::{
    collections::VecDeque,
    io,
    time::{Duration, Instant},
};

pub(super) const MAX_BATCH: usize = 64;
const QUEUE_BYTES: usize = 8 * 1024 * 1024;
const QUEUE_DATAGRAMS: usize = 131072;
const RETRY_DELAY: Duration = Duration::from_millis(2);
const MAX_DROPS: u32 = (1 << 14) - 1;

#[derive(Clone, Copy)]
pub(super) struct Rate {
    pub packets: u32,
    pub bytes: u32,
}

impl Rate {
    pub(super) fn from_env() -> Result<Self, String> {
        fn limit(name: &str) -> Result<u32, String> {
            match std::env::var(name) {
                Ok(value) => value.parse().map_err(|_| format!("{name} must be an unsigned integer")),
                Err(std::env::VarError::NotPresent) => Ok(0),
                Err(error) => Err(format!("{name}: {error}")),
            }
        }
        let packets = limit("OBS_VIEW_PACE_PPS")?;
        let bytes = limit("OBS_VIEW_PACE_BPS")?;
        Ok(Self { packets, bytes })
    }

    fn interval(&self, bytes: usize, packets: usize) -> Duration {
        fn ns(units: u64, rate: u32) -> u64 {
            if rate == 0 { 0 } else { (units * 1_000_000_000).div_ceil(u64::from(rate)) }
        }
        Duration::from_nanos(ns(packets as u64, self.packets).max(ns(bytes as u64, self.bytes)))
    }
}

pub(super) struct Settings {
    pub rate: Rate,
    pub batch: usize,
    pub budget: usize,
}

impl Settings {
    pub(super) fn from_numbers([batch, queue_mib, packets, bytes]: [u32; 4]) -> Result<Self, String> {
        if !matches!(batch, 1 | 8 | 32 | 64) || !(1..=50).contains(&queue_mib)
            || packets > 50_000 || bytes > 100_000_000 {
            return Err("VIEW egress limits: batch 1/8/32/64, queue 1..50 MiB, 0..50000 pps, 0..100000000 bytes/s".into());
        }
        Ok(Self { rate: Rate { packets, bytes }, batch: batch as usize, budget: queue_mib as usize * 1024 * 1024 })
    }

    pub(super) fn from_env() -> Result<Self, String> {
        let rate = Rate::from_env()?;
        let batch = if rate.packets == 0 && rate.bytes == 0 { 8 } else { 1 };
        Ok(Self { rate, batch, budget: QUEUE_BYTES })
    }
}

struct Datagram {
    bytes: Vec<u8>,
    records: u64,
    ready: Instant,
}

pub(super) enum ResultOfSend {
    Idle,
    Sent { bytes: usize, records: u64, datagrams: usize, age: Duration },
    Deferred(Option<i32>),
    Failed { records: u64, errno: Option<i32> },
}

pub(super) struct Queue {
    pub rate: Rate,
    pub batch: usize,
    pub budget: usize,
    pending: VecDeque<Datagram>,
    recycled: Vec<Vec<u8>>,
    next: Option<Instant>,
    bytes: usize,
    pub high_bytes: usize,
    pub cancelled: u64,
    failed_in_pass: u32,
    sealed_only: bool,
    sealed: usize,
}

impl Queue {
    pub fn new(rate: Rate) -> Self {
        Self::configured(Settings { rate, batch: 1, budget: QUEUE_BYTES })
    }

    pub fn configured(settings: Settings) -> Self {
        Self { rate: settings.rate, batch: settings.batch, budget: settings.budget,
            pending: VecDeque::new(), recycled: Vec::new(), next: None, bytes: 0,
            high_bytes: 0, cancelled: 0, failed_in_pass: 0, sealed_only: false, sealed: 0 }
    }

    pub fn bytes(&self) -> usize { self.bytes }
    pub fn is_empty(&self) -> bool { self.pending.is_empty() }
    pub fn hold_until_completion(&mut self) { self.sealed_only = true; }

    pub fn push_record(&mut self, header: &[u8], payload: &[u8], limit: usize, now: Instant) -> bool {
        let size = header.len() + payload.len();
        // Leave space for the current pass outcome while it still has data queued.
        if self.bytes + size + 24 > self.budget || self.pending.len() >= QUEUE_DATAGRAMS - 1 {
            return false;
        }
        if let Some(back) = self.pending.back_mut()
            && back.records != 0 && back.records < limit as u64
        {
            back.bytes.reserve_exact(size);
            back.bytes.extend_from_slice(header);
            back.bytes.extend_from_slice(payload);
            back.records += 1;
        } else {
            self.push(header, payload, 1, now);
            return true;
        }
        self.bytes += size;
        self.high_bytes = self.high_bytes.max(self.bytes);
        true
    }

    pub fn completion(&mut self, bytes: &[u8; 24], now: Instant) -> bool {
        if self.bytes + bytes.len() > self.budget || self.pending.len() == QUEUE_DATAGRAMS {
            return false;
        }
        self.push(bytes, &[], 0, now);
        if self.sealed_only { self.sealed = self.pending.len(); }
        true
    }

    fn push(&mut self, header: &[u8], payload: &[u8], records: u64, ready: Instant) {
        let mut bytes = self.recycled.pop().unwrap_or_default();
        bytes.reserve_exact(header.len() + payload.len());
        bytes.extend_from_slice(header);
        bytes.extend_from_slice(payload);
        self.bytes += bytes.len();
        self.high_bytes = self.high_bytes.max(self.bytes);
        self.pending.push_back(Datagram { bytes, records, ready });
    }

    pub fn pump(&mut self, now: Instant, mut send: impl FnMut(&[u8]) -> io::Result<usize>) -> ResultOfSend {
        if self.sealed_only && self.sealed == 0 { return ResultOfSend::Idle; }
        if self.next.is_some_and(|due| now < due) { return ResultOfSend::Idle; }
        let Some(head) = self.pending.front_mut() else { return ResultOfSend::Idle };
        let records = head.records;
        let bytes = head.bytes.len();
        if records == 0 {
            let outcome = u32::from_le_bytes(head.bytes[20..24].try_into().unwrap());
            let dropped = ((outcome >> 16) & MAX_DROPS).saturating_add(self.failed_in_pass).min(MAX_DROPS);
            // Build a candidate in place and restore its original value on retry.
            head.bytes[20..24].copy_from_slice(&((outcome & !(MAX_DROPS << 16)) | (dropped << 16)).to_le_bytes());
            let result = send(&head.bytes);
            head.bytes[20..24].copy_from_slice(&outcome.to_le_bytes());
            return self.complete_send(now, result, bytes, records);
        }
        let result = send(&head.bytes);
        self.complete_send(now, result, bytes, records)
    }

    pub fn pump_many(&mut self, now: Instant, mut send: impl FnMut(&[&[u8]]) -> io::Result<usize>) -> ResultOfSend {
        if self.next.is_some_and(|due| now < due) { return ResultOfSend::Idle; }
        let available = if self.sealed_only { self.sealed } else { self.pending.len() };
        if available == 0 { return ResultOfSend::Idle; }
        let head = &self.pending[0];
        if self.batch == 1 || head.records == 0 {
            return self.pump(now, |packet| send(&[packet]).map(|n| if n == 1 { packet.len() } else { 0 }));
        }
        let head_bytes = head.bytes.len();
        let deadline = self.rate.interval(head_bytes * self.batch, self.batch)
            .clamp(Duration::from_millis(4), Duration::from_millis(20));
        let age = now.saturating_duration_since(head.ready);
        let mut packets = [&[][..]; MAX_BATCH];
        let mut count = 0;
        let mut boundary = false;
        for packet in self.pending.iter().take(available.min(self.batch)) {
            if packet.records == 0 { boundary = true; break; }
            packets[count] = &packet.bytes;
            count += 1;
        }
        if count < self.batch && !boundary && age < deadline { return ResultOfSend::Idle; }
        match send(&packets[..count]) {
            Ok(n) if (1..=count).contains(&n) => {
                let mut bytes = 0;
                let mut records = 0;
                for _ in 0..n {
                    let packet = self.recycle_front();
                    bytes += packet.bytes;
                    records += packet.records;
                }
                self.schedule(now, bytes, n);
                ResultOfSend::Sent { bytes, records, datagrams: n, age }
            }
            Ok(0) => self.complete_send(now, Err(io::ErrorKind::WouldBlock.into()), head_bytes, self.pending[0].records),
            result => self.complete_send(now, result.map(|_| 0), head_bytes, self.pending[0].records),
        }
    }

    fn complete_send(&mut self, now: Instant, result: io::Result<usize>, bytes: usize, records: u64) -> ResultOfSend {
        let errno = result.as_ref().err().and_then(io::Error::raw_os_error);
        if result.as_ref().err().is_some_and(retryable) {
            self.next = Some(now + self.rate.interval(bytes, 1).max(RETRY_DELAY));
            return ResultOfSend::Deferred(errno);
        }
        self.schedule(now, bytes, 1);
        let packet = self.recycle_front();
        let sent = matches!(result, Ok(n) if n == bytes);
        if records == 0 {
            self.failed_in_pass = 0;
        } else if !sent {
            self.failed_in_pass = self.failed_in_pass.saturating_add(records as u32);
        }
        if sent {
            ResultOfSend::Sent { bytes, records, datagrams: 1, age: now.saturating_duration_since(packet.ready) }
        } else {
            ResultOfSend::Failed { records, errno }
        }
    }

    fn schedule(&mut self, now: Instant, bytes: usize, datagrams: usize) {
        let interval = self.rate.interval(bytes, datagrams);
        // Keep the long-term schedule through small wakeup delays. An idle or
        // blocked socket cannot build up credit for a catch-up burst.
        self.next = Some((self.next.unwrap_or(now) + interval).max(now + interval / 2));
    }

    fn recycle_front(&mut self) -> Retired {
        let mut packet = self.pending.pop_front().unwrap();
        if self.sealed_only { self.sealed -= 1; }
        let bytes = packet.bytes.len();
        self.bytes -= bytes;
        packet.bytes.clear();
        self.recycled.push(packet.bytes);
        Retired { bytes, records: packet.records, ready: packet.ready }
    }

    pub fn cancel(&mut self) -> (u64, u64) {
        let (records, completions) = self.discard();
        self.cancelled += records;
        (records, completions)
    }

    pub fn discard(&mut self) -> (u64, u64) {
        let mut records = 0;
        let mut completions = 0;
        for packet in self.pending.drain(..) {
            if packet.records == 0 { completions += 1; }
            records += packet.records;
        }
        self.bytes = 0;
        self.failed_in_pass = 0;
        self.sealed = 0;
        (records, completions)
    }
}

struct Retired {
    bytes: usize,
    records: u64,
    ready: Instant,
}

pub(super) fn retryable(error: &io::Error) -> bool {
    use io::ErrorKind::*;
    matches!(error.kind(), WouldBlock | Interrupted | ConnectionRefused | ConnectionReset |
        NetworkUnreachable | HostUnreachable | NetworkDown | TimedOut | NotConnected)
        || cfg!(target_os = "linux") && matches!(error.raw_os_error(), Some(105 | 12))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_from_pressure_in_order_without_replaying_credit() {
        let mut queue = Queue::new(Rate { packets: 2500, bytes: 200_000 });
        let start = Instant::now();
        for value in 1..=4 {
            assert!(queue.push_record(&[value; 8], &[value; 92], 1, start));
        }
        let mut sent = Vec::new();
        assert!(matches!(queue.pump(start, |_| Err(io::ErrorKind::WouldBlock.into())), ResultOfSend::Deferred(_)));
        assert!(matches!(queue.pump(start + Duration::from_millis(1), |_| panic!()), ResultOfSend::Idle));
        let resumed = start + Duration::from_millis(25);
        for (i, expected) in (1..=4).enumerate() {
            let now = resumed + Duration::from_micros(i as u64 * 500);
            assert!(matches!(queue.pump(now, |p| { sent.push(p[0]); Ok(p.len()) }), ResultOfSend::Sent { records: 1, .. }));
            assert_eq!(sent[i], expected);
            assert!(matches!(queue.pump(now, |_| panic!()), ResultOfSend::Idle));
        }
        assert!(queue.is_empty());
    }

    #[test]
    fn pass_outcomes_follow_last_record_and_account_for_send_failures() {
        let mut queue = Queue::new(Rate { packets: 1000, bytes: 0 });
        let start = Instant::now();
        assert!(queue.push_record(&[1], &[], 2, start));
        let mut summary = [0; 24];
        summary[20..].copy_from_slice(&0xc0000001u32.to_le_bytes());
        assert!(queue.completion(&summary, start));
        assert!(queue.push_record(&[2], &[], 2, start));
        assert!(matches!(queue.pump(start, |_| Err(io::ErrorKind::InvalidData.into())), ResultOfSend::Failed { records: 1, .. }));
        let note = start + Duration::from_millis(1);
        assert!(matches!(queue.pump(note, |p| {
            assert_eq!(u32::from_le_bytes(p[20..].try_into().unwrap()), 0xc0010001);
            Err(io::ErrorKind::Interrupted.into())
        }), ResultOfSend::Deferred(_)));
        queue.pump(note + RETRY_DELAY, |p| {
            assert_eq!(u32::from_le_bytes(p[20..].try_into().unwrap()), 0xc0010001);
            Ok(p.len())
        });
        queue.pump(note + RETRY_DELAY + Duration::from_millis(1), |p| {
            assert_eq!(p, &[2]);
            Ok(p.len())
        });
        assert!(queue.is_empty());
    }

    #[test]
    fn a_short_outage_smooths_bursts_without_loss_at_both_view_widths() {
        for record_size in [1064, 4136] {
            let mut queue = Queue::new(Rate { packets: 5000, bytes: 0 });
            let mut record = vec![0; record_size];
            let start = Instant::now();
            let mut sent_at = Vec::new();
            let mut received = Vec::new();
            for slot in 0..1500 {
                let now = start + Duration::from_micros(slot * 100);
                if slot < 1000 && slot % 20 == 0 {
                    for i in 0..10 {
                        record[..8].copy_from_slice(&(slot / 2 + i).to_le_bytes());
                        assert!(queue.push_record(&record, &[], 1, now));
                    }
                }
                queue.pump(now, |p| {
                    if (250..350).contains(&slot) { return Err(io::ErrorKind::WouldBlock.into()); }
                    sent_at.push(slot);
                    received.push(u64::from_le_bytes(p[..8].try_into().unwrap()));
                    Ok(p.len())
                });
            }
            assert_eq!(received, (0..500).collect::<Vec<_>>());
            assert!(sent_at.windows(2).all(|t| t[1] - t[0] >= 2));
            assert!(queue.is_empty());
            println!("record={record_size} bytes count={} peak_bytes={} last_us={}",
                     received.len(), queue.high_bytes, sent_at.last().unwrap() * 100);
        }
    }

    #[test]
    fn persistent_overload_is_bounded_and_keeps_the_pass_outcome() {
        let mut queue = Queue::configured(Settings::from_numbers([64, 50, 3000, 0]).unwrap());
        let now = Instant::now();
        let mut admitted = 0;
        let record = [0; 4136];
        while queue.push_record(&record, &[], 1, now) { admitted += 1; }
        assert!(admitted > 12_000);
        assert!(queue.completion(&[0; 24], now));
        assert!(queue.bytes() <= 50 * 1024 * 1024);
        assert_eq!(queue.cancel(), (admitted, 1));
        assert!(queue.is_empty());
    }

    #[test]
    fn pass_buffer_holds_open_data_and_releases_only_complete_prefix() {
        let mut queue = Queue::new(Rate { packets: 0, bytes: 0 });
        queue.hold_until_completion();
        let start = Instant::now();
        assert!(queue.push_record(&[1], &[], 2, start));
        assert!(matches!(queue.pump(start, |_| panic!()), ResultOfSend::Idle));
        assert!(queue.completion(&[0; 24], start));
        assert!(queue.push_record(&[2], &[], 2, start));
        assert!(matches!(queue.pump(start, |buf| { assert_eq!(buf, &[1]); Ok(buf.len()) }), ResultOfSend::Sent { records: 1, .. }));
        assert!(matches!(queue.pump(start, |buf| Ok(buf.len())), ResultOfSend::Sent { records: 0, .. }));
        assert!(matches!(queue.pump(start, |_| panic!()), ResultOfSend::Idle));
        assert_eq!(queue.bytes(), 1);
    }

    #[test]
    fn partial_sendmmsg_retries_only_the_unsent_suffix() {
        let mut queue = Queue::configured(Settings::from_numbers([32, 50, 5000, 0]).unwrap());
        let start = Instant::now();
        for ordinal in 0..32 {
            assert!(queue.push_record(&[ordinal], &[], 1, start));
        }
        assert!(matches!(queue.pump_many(start, |batch| {
            assert_eq!(batch.len(), 32);
            assert_eq!(batch[6], &[6]);
            Ok(7)
        }), ResultOfSend::Sent { records: 7, datagrams: 7, .. }));
        let after = start + Duration::from_millis(7);
        assert!(matches!(queue.pump_many(after, |batch| {
            assert_eq!(batch.len(), 25);
            assert_eq!(batch[0], &[7]);
            Err(io::ErrorKind::WouldBlock.into())
        }), ResultOfSend::Deferred(_)));
        assert!(matches!(queue.pump_many(after + RETRY_DELAY, |batch| {
            assert_eq!(batch.len(), 25);
            assert_eq!(batch[0], &[7]);
            assert_eq!(batch[24], &[31]);
            Ok(25)
        }), ResultOfSend::Sent { records: 25, datagrams: 25, .. }));
        assert!(queue.is_empty());
    }
}
