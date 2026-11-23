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
const MAX_RANGES: usize = 12;
const RANGE_WIRE_BYTES: usize = 4;
const PACKED_ROI_HEADER: usize = 92;
const FRAME_LIMIT: usize = PACKED_ROI_HEADER + MAX_RANGES * RANGE_WIRE_BYTES + obn3::COMPACT_BYTES;
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

    fn select(&mut self, request: Request, source_bins: u16, fft_bins: u16) -> R<()> {
        for row in self.rows.iter_mut().flatten() {
            row.bins=roi::bin_ranges(request.range,row.center_hz,row.side,&row.usable,source_bins,fft_bins)
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
    configured_view_bins: u16,
    source_bins: u16,
    fft_bins: u16,
    obn5: bool,
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
    pub(super) fn open(peer: SocketAddr, initial: Map, next: Option<Map>,
                       full_scan: bool, configured_view_bins: usize, fft_bins: usize, obn5: bool) -> R<Self> {
        let socket=TcpStream::connect_timeout(&peer,IO_DEADLINE).map_err(|e|e.to_string())?;
        socket.set_nonblocking(true).map_err(|e|e.to_string())?;
        Ok(Self{full_scan,configured_view_bins:configured_view_bins as u16,source_bins:fft_bins as u16,
            fft_bins:fft_bins as u16,obn5,
            request:None,initial,next,socket:Some(socket),
            free:(0..CAPACITY).map(|_|Vec::with_capacity(FRAME_LIMIT)).collect(),
            pending:VecDeque::with_capacity(CAPACITY),selected:0,selected_bins:0,prepared:0,
            sent:0,sent_bins:0,sent_bytes:0,accepted_bytes:0,rejected:0,abandoned:0,partial:0,
            blocked:0,queued_bytes:0,peak_messages:0,peak_bytes:0,send:Histogram::default(),
            queue:Histogram::default(),ready:Histogram::default()})
    }

    pub(super) fn update(&mut self, request: Option<Request>) -> R<()> {
        if request!=self.request {
            // Full-record legacy requests are retained for VIEW-OFF bench comparisons.
            self.request=request.filter(|r| !r.legacy_full_record || self.configured_view_bins==0);
            if let Some(request)=self.request {
                if request.legacy_full_record && self.fft_bins!=4096 {
                    return Err("legacy ROI requires FFT4096".into());
                }
                self.source_bins=if request.view_bins==self.configured_view_bins {
                    self.configured_view_bins
                } else { self.fft_bins };
                self.initial.select(request,self.source_bins,self.fft_bins)?;
                if let Some(next)=self.next.as_mut() { next.select(request,self.source_bins,self.fft_bins)?; }
            }
        }
        Ok(())
    }

    pub(super) fn filters_view(&self) -> bool {
        self.request.is_some_and(|request| !request.legacy_full_record)
    }

    fn locate<'a>(initial:&'a Map, next:Option<&'a Map>, h: Header, anchor: u64,
                  next_anchor: Option<u64>) -> Option<(&'a Map,u64,u64)> {
        let (map,anchor)=match (next,next_anchor) {
            (Some(next),Some(start)) if h.epoch>=start => (next,start),
            _ => (initial,anchor),
        };
        Some((map,anchor,h.epoch.checked_sub(anchor)?))
    }

    pub(super) fn keeps_record(&self, h: Header, anchor: u64, next_anchor: Option<u64>) -> bool {
        if self.obn5 && !self.full_scan && self.request.is_none() { return false; }
        if self.full_scan || !self.filters_view() { return true; }
        Self::locate(&self.initial,self.next.as_ref(),h,anchor,next_anchor).is_some_and(|(map,_,relative)| {
            map.rows[(relative%map.rows.len() as u64) as usize].as_ref().is_some_and(|row| !row.bins.is_empty())
        })
    }

    pub(super) fn prepare(&mut self, h: Header, raw: &[u8], view: &[u8],
                          anchor: u64, next_anchor: Option<u64>) -> R<()> {
        if !self.full_scan && self.request.is_none() {return Ok(());}
        let Some((map,anchor,relative))=Self::locate(&self.initial,self.next.as_ref(),h,anchor,next_anchor) else { return Ok(()); };
        if self.full_scan {
            let generation=map.generation;
            self.prepare_full(h,raw,generation)?;
            return Ok(());
        }
        let request=self.request.unwrap();
        let position=relative%map.rows.len() as u64;
        let Some(row)=map.rows[position as usize].as_ref() else { return Ok(()); };
        if row.bins.is_empty() { return Ok(()); }
        if raw.len()!=obn3::COMPACT_PREFIX_BYTES+obn3::M45E3_WORD_BYTES*h.bins || h.flags&2!=0 {
            return Err("ROI requires compact raw scan records".into());
        }
        let bins=row.bins.iter().map(|b|u64::from(b.end-b.start)).sum::<u64>();
        self.selected+=1;
        self.selected_bins+=bins;
        if self.socket.is_none() {self.rejected+=1;return Ok(());}
        let Some(mut frame)=self.free.pop() else { self.rejected+=1;return Ok(()); };
        frame.clear();
        let (payload, word_bytes)=if self.source_bins==self.fft_bins {
            (&raw[obn3::COMPACT_PREFIX_BYTES..],obn3::M45E3_WORD_BYTES)
        } else { (view,1) };
        if payload.len()!=usize::from(self.source_bins)*word_bytes {
            return Err("ROI source payload differs from its bin grid".into());
        }
        let wire_bytes=if request.legacy_full_record {
            frame.extend_from_slice(b"GROI1\0\0\0");
            raw.len()
        } else {
            frame.extend_from_slice(b"GROI2\0\0\0");
            bins as usize*word_bytes
        };
        frame.extend_from_slice(&(wire_bytes as u32).to_le_bytes());
        frame.extend_from_slice(&(row.bins.len() as u16).to_le_bytes());
        frame.push(row.phase);
        frame.push(row.side as i8 as u8);
        for value in [request.generation,u64::from(map.version),map.generation,
            anchor,position,relative/map.rows.len() as u64,row.center_hz as u64] {
            frame.extend_from_slice(&value.to_le_bytes());
        }
        let [low,high]=request.range;
        if request.legacy_full_record {
            frame.extend_from_slice(&((low+(high-low)/2) as u64).to_le_bytes());
        } else {
            frame.extend_from_slice(&low.to_le_bytes());
            frame.extend_from_slice(&high.to_le_bytes());
            frame.extend_from_slice(&self.source_bins.to_le_bytes());
            frame.extend_from_slice(&request.view_bins.to_le_bytes());
        }
        for range in &row.bins {
            frame.extend_from_slice(&range.start.to_le_bytes());
            frame.extend_from_slice(&range.end.to_le_bytes());
        }
        if request.legacy_full_record {
            frame.extend_from_slice(raw);
        } else {
            frame.extend_from_slice(&raw[..obn3::COMPACT_PREFIX_BYTES]);
            for range in &row.bins {
                frame.extend_from_slice(&payload[usize::from(range.start)*word_bytes..usize::from(range.end)*word_bytes]);
            }
        }
        self.enqueue(frame,bins);
        Ok(())
    }

    fn prepare_full(&mut self, h: Header, raw: &[u8], generation: u64) -> R<()> {
        if raw.len()!=obn3::COMPACT_PREFIX_BYTES+obn3::M45E3_WORD_BYTES*h.bins {
            return Err("full compact TCP requires format 3".into());
        }
        let bins=h.bins as u64;
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
