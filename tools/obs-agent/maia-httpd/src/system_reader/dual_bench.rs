//! RAM bench: hand off full records to one bounded local worker, never to a socket.
use super::{continuous, Header, R};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    sync::{mpsc, Arc, atomic::{AtomicU32, Ordering::Relaxed}},
    thread::{self, JoinHandle},
    time::Duration,
};

const SLOTS: usize = 16;
type Buffer = Box<[u8]>;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Load { View, Read, Inline, Worker }

pub(super) fn settings() -> R<Option<(Load, Duration)>> {
    let Some(path) = std::env::var_os("OBS_VIEW_DUAL_BENCH_FILE") else { return Ok(None) };
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut words = text.split_whitespace();
    let full = match words.next() {
        Some("view") => Load::View,
        Some("read") => Load::Read,
        Some("inline") => Load::Inline,
        Some("full") => Load::Worker,
        _ => return Err("dual bench mode".into()),
    };
    let us: u64 = words.next().and_then(|s| s.parse().ok()).ok_or("dual bench wait")?;
    if !matches!(us, 0 | 5 | 8 | 10 | 16) || words.next().is_some() {
        return Err("dual bench wait must be 0, 5, 8, 10 or 16 us".into());
    }
    Ok(Some((full, Duration::from_micros(us))))
}

#[derive(Default)]
struct Counts {
    admitted: AtomicU32,
    processed: AtomicU32,
    unavailable: AtomicU32,
    high: AtomicU32,
}

struct Frame {
    ordinal: u64,
    epoch: u64,
    data: Buffer,
}

#[derive(Default)]
struct Fill {
    bytes: usize,
    data: Option<Buffer>,
}

pub(super) struct Worker {
    counts: Arc<Counts>,
    input: mpsc::SyncSender<Frame>,
    available: mpsc::Receiver<Buffer>,
    fill: RefCell<Fill>,
    worker: JoinHandle<Value>,
}

impl Worker {
    pub(super) fn start() -> R<Self> {
        let (input, incoming) = mpsc::sync_channel::<Frame>(SLOTS);
        let (recycle, available) = mpsc::sync_channel(SLOTS);
        for _ in 0..SLOTS {
            recycle.try_send(vec![0; continuous::BYTES].into_boxed_slice()).unwrap();
        }
        let counts = Arc::new(Counts::default());
        let progress = counts.clone();
        let worker = thread::Builder::new().name("full4096-bench".into()).spawn(move || {
            let mut winner = (0, 0, 0, 0);
            let mut last = None;
            let mut gaps = 0;
            let mut reordered = 0;
            while let Ok(frame) = incoming.recv() {
                let (power, bin) = peak(&frame.data);
                if power > winner.0 {
                    winner = (power, bin, frame.ordinal, frame.epoch);
                }
                if let Some(old) = last {
                    gaps += frame.ordinal.saturating_sub(old + 1);
                    reordered += u64::from(frame.ordinal <= old);
                }
                last = Some(frame.ordinal);
                progress.processed.fetch_add(1, Relaxed);
                // A buffer here was removed from the same fixed pool.
                if recycle.try_send(frame.data).is_err() { break }
            }
            json!({"power":winner.0,"bin":winner.1,"ordinal":winner.2,"epoch":winner.3,
                   "last_ordinal":last,"ordinal_gaps":gaps,"ordinal_reordered":reordered})
        }).map_err(|e| e.to_string())?;
        Ok(Self {counts, input, available, fill: RefCell::default(), worker})
    }

    pub(super) fn copy_span(&self, span: &[u8]) {
        let mut fill = self.fill.borrow_mut();
        let start = fill.bytes;
        if start == 0 { fill.data = self.available.try_recv().ok(); }
        let end = start + span.len();
        if let Some(data) = fill.data.as_mut() {
            #[cfg(target_arch = "arm")]
            unsafe { super::obs_neon_copy(data[start..end].as_mut_ptr(), span.as_ptr(), span.len()) };
            #[cfg(not(target_arch = "arm"))]
            data[start..end].copy_from_slice(span);
        }
        fill.bytes = end;
    }

    pub(super) fn submit(&self, header: Header) -> R<()> {
        let fill = std::mem::take(&mut *self.fill.borrow_mut());
        if fill.bytes != continuous::BYTES { return Err("dual bench native record length".into()) }
        let Some(data) = fill.data else {
            self.counts.unavailable.fetch_add(1, Relaxed);
            return Ok(());
        };
        let processed = self.counts.processed.load(Relaxed);
        let admitted = self.counts.admitted.fetch_add(1, Relaxed) + 1;
        self.counts.high.fetch_max(admitted - processed, Relaxed);
        self.input.try_send(Frame {ordinal:header.ordinal, epoch:header.epoch, data})
            .map_err(|e| e.to_string())
    }

    pub(super) fn counts(&self) -> Value { metrics(&self.counts) }

    pub(super) fn finish(self) -> R<Value> {
        let Self {counts, input, available, worker, ..} = self;
        drop(input);
        let peak = worker.join().map_err(|_| "dual bench worker failed")?;
        drop(available);
        Ok(json!({"counts":metrics(&counts),"peak":peak,"pool_records":SLOTS}))
    }
}

fn metrics(counts: &Counts) -> Value {
    json!({"admitted":counts.admitted.load(Relaxed),"processed":counts.processed.load(Relaxed),
           "unavailable":counts.unavailable.load(Relaxed),"in_flight_peak":counts.high.load(Relaxed)})
}

fn peak(bytes: &[u8]) -> (u64, usize) {
    let mut best = (0, 0);
    for (bin, word) in bytes.chunks_exact(8).enumerate() {
        let packed = u64::from_le_bytes(word.try_into().unwrap());
        let power = (packed & ((1 << 47) - 1)) << (2 * ((packed >> 47) & 7));
        if power > best.0 { best = (power, bin); }
    }
    best
}

#[derive(Default)]
pub(super) struct Inline {
    bins: Cell<usize>,
    record_peak: Cell<(u64, usize)>,
    winner: Cell<(u64, usize, u64, u64)>,
    processed: Cell<u64>,
}

impl Inline {
    pub(super) fn consume(&self, span: &[u8]) {
        let offset = self.bins.get();
        let (power, bin) = peak(span);
        if power > self.record_peak.get().0 { self.record_peak.set((power, offset+bin)); }
        self.bins.set(offset + span.len()/8);
    }

    pub(super) fn finish(&self, header: Header) -> R<()> {
        if self.bins.replace(0) != continuous::BINS { return Err("inline bench bin count".into()) }
        let (power, bin) = self.record_peak.replace((0,0));
        if power > self.winner.get().0 { self.winner.set((power,bin,header.ordinal,header.epoch)); }
        self.processed.set(self.processed.get()+1);
        Ok(())
    }

    pub(super) fn result(&self) -> Value {
        let (power,bin,ordinal,epoch)=self.winner.get();
        json!({"processed":self.processed.get(),"peak":{"power":power,"bin":bin,"ordinal":ordinal,"epoch":epoch}})
    }
}
