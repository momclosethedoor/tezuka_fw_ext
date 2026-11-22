//! Bench-only compact zoom transport. It never owns RF or a DMA slot.
use super::{Header, R, RoiRequest as Request, Value, histogram, obn3, roi, timing::Histogram};
use serde_json::json;
use std::{
    collections::VecDeque,
    io::{self, Write},
    net::{SocketAddr, TcpStream},
    os::fd::AsRawFd,
    ops::Range,
    time::{Duration, Instant},
};

const CAPACITY: usize = 16;
const FRAME_LIMIT: usize = 128 + obn3::COMPACT_BYTES;
const MAX_RANGES: usize = 12;
const IO_DEADLINE: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct Visit {
    center_hz: i64,
    side: i64,
    phase: u8,
    usable: Vec<[i64; 2]>,
    bins: Vec<Range<u16>>,
}

#[derive(Clone)]
pub(crate) struct Map {
    version: u32,
    generation: u64,
    rows: Vec<Option<Visit>>,
}

impl Map {
    pub(crate) fn from_descriptor(desc: &Value, version: u32, generation: u64) -> R<Self> {
        let rows = desc["positions"].as_array().ok_or("ROI needs physical A/B positions")?;
        if rows.is_empty() || rows.len()>65535 { return Err("ROI map length".into()); }
        let visits = rows.iter().map(|row| {
            if row["transition_only"]==true || row["rx_available"]!=true { return Ok(None); }
            let center_hz=row["rf_center_hz"].as_i64().ok_or("ROI row RF center")?;
            let side=row["side_sign"].as_i64().filter(|s| matches!(*s,-1|1)).ok_or("ROI row orientation")?;
            let phase=match row["sweep_phase"].as_str() {
                Some("A")=>0, Some("B")=>1, _=>return Err("ROI row phase"),
            };
            let usable=serde_json::from_value(row["rf_ranges"].clone()).map_err(|_|"ROI usable RF ranges")?;
            Ok(Some(Visit{center_hz,side,phase,usable,bins:Vec::new()}))
        }).collect::<Result<Vec<_>,&str>>()?;
        Ok(Self{version,generation,rows:visits})
    }

    fn select(&mut self, request: Request) -> R<()> {
        for row in self.rows.iter_mut().flatten() {
            row.bins=roi::bin_ranges(request.center_hz,row.center_hz,row.side,&row.usable)
                .filter(|bins| bins.len()<=MAX_RANGES).ok_or("ROI bin selection bounds")?;
        }
        Ok(())
    }
}

struct Packet {
    bytes: Vec<u8>,
    offset: usize,
    bins: u64,
    ready: Instant,
    first_send: Option<Instant>,
}

pub(super) struct Stream {
    full_scan: bool,
    request: Option<Request>,
    initial: Map,
    next: Option<Map>,
    socket: Option<TcpStream>,
    free: Vec<Vec<u8>>,
    pending: VecDeque<Packet>,
    selected: u64,
    selected_bins: u64,
    prepared: u64,
    sent: u64,
    sent_bins: u64,
    sent_bytes: u64,
    accepted_bytes: u64,
    rejected: u64,
    abandoned: u64,
    partial: u64,
    blocked: u64,
    queued_bytes: usize,
    peak_messages: usize,
    peak_bytes: usize,
    send: Histogram,
    queue: Histogram,
    ready: Histogram,
}

impl Stream {
    pub(super) fn open(peer: SocketAddr, initial: Map, next: Option<Map>, full_scan: bool) -> R<Self> {
        let socket=TcpStream::connect_timeout(&peer,IO_DEADLINE).map_err(|e|e.to_string())?;
        socket.set_nonblocking(true).map_err(|e|e.to_string())?;
        Ok(Self{full_scan,request:None,initial,next,socket:Some(socket),
            free:(0..CAPACITY).map(|_|Vec::with_capacity(FRAME_LIMIT)).collect(),
            pending:VecDeque::with_capacity(CAPACITY),selected:0,selected_bins:0,prepared:0,
            sent:0,sent_bins:0,sent_bytes:0,accepted_bytes:0,rejected:0,abandoned:0,partial:0,
            blocked:0,queued_bytes:0,peak_messages:0,peak_bytes:0,send:Histogram::default(),
            queue:Histogram::default(),ready:Histogram::default()})
    }

    pub(super) fn update(&mut self, request: Option<Request>) -> R<()> {
        if request!=self.request {
            if let Some(request)=request {
                self.initial.select(request)?;
                if let Some(next)=self.next.as_mut() { next.select(request)?; }
            }
            self.request=request;
        }
        Ok(())
    }

    pub(super) fn prepare(&mut self, h: Header, raw: &[u8], anchor: u64, next_anchor: Option<u64>) -> R<()> {
        if !self.full_scan && self.request.is_none() {return Ok(());}
        let (map,anchor)=match (self.next.as_ref(),next_anchor) {
            (Some(next),Some(start)) if h.epoch>=start => (next,start),
            _ => (&self.initial,anchor),
        };
        let Some(relative)=h.epoch.checked_sub(anchor) else { return Ok(()); };
        if self.full_scan {
            let generation=map.generation;
            self.prepare_full(h,raw,generation)?;
            return Ok(());
        }
        let request=self.request.unwrap();
        let position=relative%map.rows.len() as u64;
        let Some(row)=map.rows[position as usize].as_ref() else { return Ok(()); };
        if row.bins.is_empty() { return Ok(()); }
        if raw.len()!=obn3::COMPACT_BYTES || h.flags&2!=0 { return Err("ROI requires compact raw scan records".into()); }
        let bins=row.bins.iter().map(|b|u64::from(b.end-b.start)).sum::<u64>();
        self.selected+=1;
        self.selected_bins+=bins;
        if self.socket.is_none() {self.rejected+=1;return Ok(());}
        let Some(mut frame)=self.free.pop() else { self.rejected+=1;return Ok(()); };
        frame.clear();
        frame.extend_from_slice(b"GROI1\0\0\0");
        frame.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        frame.extend_from_slice(&(row.bins.len() as u16).to_le_bytes());
        frame.push(row.phase);
        frame.push(row.side as i8 as u8);
        for value in [request.generation,u64::from(map.version),map.generation,
            anchor,position,relative/map.rows.len() as u64,row.center_hz as u64,request.center_hz as u64] {
            frame.extend_from_slice(&value.to_le_bytes());
        }
        for range in &row.bins {
            frame.extend_from_slice(&range.start.to_le_bytes());
            frame.extend_from_slice(&range.end.to_le_bytes());
        }
        frame.extend_from_slice(raw);
        self.enqueue(frame,bins);
        Ok(())
    }

    fn prepare_full(&mut self, h: Header, raw: &[u8], generation: u64) -> R<()> {
        if raw.len()!=obn3::COMPACT_BYTES {return Err("full compact TCP requires format 3".into());}
        let bins=super::continuous::BINS as u64;
        self.selected+=1;
        self.selected_bins+=bins;
        if self.socket.is_none() { self.rejected+=1;return Ok(()); }
        let Some(mut frame)=self.free.pop() else { self.rejected+=1;return Ok(()); };
        frame.clear();
        frame.extend_from_slice(b"GCMP1\0\0\0");
        for value in [h.ordinal,h.first_seq,h.epoch,h.first_tick,u64::from(h.k),u64::from(h.flags),generation] {
            frame.extend_from_slice(&value.to_le_bytes());
        }
        frame.extend_from_slice(raw);
        self.enqueue(frame,bins);
        Ok(())
    }

    fn enqueue(&mut self, frame: Vec<u8>, bins: u64) {
        self.queued_bytes+=frame.len();
        self.pending.push_back(Packet{bytes:frame,offset:0,bins,ready:Instant::now(),first_send:None});
        self.prepared+=1;
        self.peak_messages=self.peak_messages.max(self.pending.len());
        self.peak_bytes=self.peak_bytes.max(self.queued_bytes);
    }

    pub(super) fn pump(&mut self) {
        let (Some(socket),Some(frame))=(self.socket.as_mut(),self.pending.front_mut()) else {return;};
        let now=Instant::now();
        if now.duration_since(frame.ready)>IO_DEADLINE { self.close();return; }
        let first=*frame.first_send.get_or_insert(now);
        match socket.write(&frame.bytes[frame.offset..]) {
            Ok(0)=>self.close(),
            Ok(n)=>{
                self.accepted_bytes+=n as u64;
                self.queued_bytes-=n;
                frame.offset+=n;
                if frame.offset==frame.bytes.len() {
                    self.sent+=1;
                    self.sent_bins+=frame.bins;
                    self.sent_bytes+=frame.offset as u64;
                    self.send.observe(first.elapsed());
                    self.queue.observe(first.duration_since(frame.ready));
                    self.ready.observe(frame.ready.elapsed());
                    self.free.push(self.pending.pop_front().unwrap().bytes);
                }
            }
            Err(e) if matches!(e.kind(),io::ErrorKind::WouldBlock|io::ErrorKind::Interrupted)=>self.blocked+=1,
            Err(_)=>self.close(),
        }
    }

    pub(super) fn close(&mut self) {
        self.socket.take();
        self.partial+=u64::from(self.pending.front().is_some_and(|packet|packet.offset!=0));
        self.abandoned+=self.pending.len() as u64;
        self.free.extend(self.pending.drain(..).map(|p|p.bytes));
        self.queued_bytes=0;
    }

    pub(super) fn pending(&self) -> bool { !self.pending.is_empty() }

    pub(super) fn stats(&self) -> Value {
        let socket_pending=self.socket.as_ref().and_then(|socket|{
            let mut value=0i32;
            (unsafe{libc::ioctl(socket.as_raw_fd(),libc::TIOCOUTQ,&mut value)}==0).then_some(value)
        });
        json!({"full_scan":self.full_scan,"generation":self.request.map(|r|r.generation),"selected":self.selected,
            "selected_bins":self.selected_bins,"prepared":self.prepared,"sent":self.sent,"sent_bins":self.sent_bins,
            "accepted_bytes":self.accepted_bytes,"sent_bytes":self.sent_bytes,"rejected":self.rejected,
            "abandoned":self.abandoned,"partial":self.partial,"blocked":self.blocked,
            "pending":self.pending.len(),"capacity":CAPACITY,"pending_bytes":self.queued_bytes,
            "peak_messages":self.peak_messages,"peak_bytes":self.peak_bytes,
            "oldest_ns":self.pending.front().map(|p|p.ready.elapsed().as_nanos() as u64),
            "socket_pending_bytes":socket_pending,"send_ns":histogram(self.send),
            "queue_ns":histogram(self.queue),"ready_to_api_complete_ns":histogram(self.ready)})
    }
}
