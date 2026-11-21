//! GVIEW4 frames retain their original encoding. GSWEEP1 is an independent
//! UDP completion record for one executed pass.
use super::{Header, R, UdpBatch, Value};
use std::{net::UdpSocket,time::Instant};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const RECORD_BYTES: usize = 576;

pub(crate) struct SweepSpec {
    version: u64,
    positions: Vec<Option<usize>>,
    route_to: Vec<usize>,
    first: [usize;2],
    last: [usize;2],
    count: [u64;2],
    mode: Mode,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode { Stream, Timed(u64), Pass }

impl SweepSpec {
    pub(crate) fn from_descriptor(desc: &Value, mode: &str) -> R<Option<Self>> {
        if desc["sweep_wire_version"] != 1 {return Ok(None);}
        let mode = match mode {
            "stream" => Mode::Stream,
            "5ms" => Mode::Timed(5000),
            "10ms" => Mode::Timed(10000),
            "20ms" => Mode::Timed(20000),
            "pass" => Mode::Pass,
            _ => return Err("view delivery must be stream, 5ms, 10ms, 20ms or pass".into()),
        };
        let rows = desc["positions"].as_array().ok_or("sweep map positions")?;
        let mut first = [usize::MAX;2];
        let mut last = [0;2];
        let mut count = [0;2];
        let mut positions = Vec::with_capacity(rows.len());
        for (i,row) in rows.iter().enumerate() {
            if row["transition_only"] == true {
                positions.push(None);
                continue;
            }
            let phase = match row["sweep_phase"].as_str() {
                Some("A") => 0,
                Some("B") => 1,
                _ => return Err("missing sweep phase".into()),
            };
            first[phase] = first[phase].min(i);
            last[phase] = i;
            count[phase] += 1;
            positions.push(Some(phase));
        }
        if count.contains(&0) || last[0] >= first[1] {
            return Err("invalid A/B boundary".into());
        }
        if mode == Mode::Pass && count.iter().any(|n| *n as usize * RECORD_BYTES > MAX_BYTES) {
            return Err("one scan pass exceeds the bounded VIEW buffer".into());
        }
        let version = desc["version"].as_u64().ok_or("missing sweep map version")?;
        let mut next = 2;
        let mut route_to = vec![0;positions.len()];
        for (i,phase) in positions.iter().enumerate().rev() {
            if let Some(p) = phase {next=*p;}
            route_to[i]=next;
        }
        Ok(Some(Self {version,positions,route_to,first,last,count,mode}))
    }
}

struct Pass {
    id: u64,
    phase: usize,
    first_epoch: u64,
    last_epoch: u64,
    last_tick: u64,
    observed: u64,
    drops_before: u64,
}

pub(super) struct Delivery<'a> {
    spec: &'a SweepSpec,
    anchor: u64,
    generation: u64,
    pass: Option<Pass>,
    completed: Option<u64>,
    pub notes_sent: u64,
    pub notes_lost: u64,
    pub peak_bytes: usize,
}

impl<'a> Delivery<'a> {
    pub(super) fn new(spec: &'a SweepSpec, anchor:u64, generation:u64, net:&mut UdpBatch)->Self {
        net.max_records = 2; // 1152 bytes of IPv4/IPv6 UDP payload
        net.coalesce_us = match spec.mode {Mode::Timed(us)=>us,_=>0};
        if spec.mode == Mode::Pass {
            net.bytes.reserve(*spec.count.iter().max().unwrap() as usize * RECORD_BYTES);
        }
        Self{spec,anchor,generation,pass:None,completed:None,notes_sent:0,notes_lost:0,peak_bytes:0}
    }

    pub(super) fn accept(&mut self,h:Header,view:&[u8],net:&mut UdpBatch,sock:&UdpSocket) {
        let Some(relative) = h.epoch.checked_sub(self.anchor) else {return};
        let count = self.spec.positions.len() as u64;
        let position = (relative%count) as usize;
        let assigned = self.spec.route_to[position];
        let phase = assigned % 2;
        let id = 2*(relative/count)+assigned as u64;
        if self.completed.is_some_and(|old| id<=old) {net.suppressed+=1;return;}
        if self.pass.as_ref().is_some_and(|p| p.id != id) {self.finish(net,sock,true);}
        let pass = self.pass.get_or_insert(Pass{id,phase,first_epoch:h.epoch,last_epoch:h.epoch,
            last_tick:h.first_tick,observed:0,drops_before:net.dropped});
        if self.spec.positions[position].is_some() {
            if pass.observed == 0 {pass.first_epoch=h.epoch;}
            if pass.observed == 0 || pass.last_epoch != h.epoch {pass.observed+=1;}
            pass.last_epoch = h.epoch;
            pass.last_tick = h.first_tick;
        }
        if self.spec.positions[position].is_none() {net.transition+=1;}
        if net.bytes.len()+RECORD_BYTES <= MAX_BYTES {
            if net.pending==0 {net.start_record();}
            net.bytes.extend_from_slice(&self.header(h));
            net.bytes.extend_from_slice(view);
            net.pending+=1;
            self.peak_bytes=self.peak_bytes.max(net.bytes.len());
        } else {net.rejected_before_queue+=1;net.dropped+=1;}
        if position == self.spec.last[phase] {self.finish(net,sock,true);}
    }

    fn header(&self,h:Header)->[u8;64] {
        let mut out = [0;64];
        out[..8].copy_from_slice(b"GVIEW4\0\0");
        for (p,n) in out[8..].chunks_exact_mut(8).zip([
            h.ordinal,h.first_seq,h.epoch,h.first_tick,u64::from(h.k),u64::from(h.flags),self.generation]) {
            p.copy_from_slice(&n.to_le_bytes());
        }
        out
    }

    pub(super) fn poll(&self,net:&mut UdpBatch,sock:&UdpSocket) {
        if self.spec.mode == Mode::Stream || matches!(self.spec.mode,Mode::Timed(_)) && net.timer_due(Instant::now()) {
            net.flush(sock);
        }
    }

    pub(super) fn finish(&mut self,net:&mut UdpBatch,sock:&UdpSocket,executed:bool) {
        let Some(pass) = self.pass.take() else {return};
        net.flush(sock);
        let base = self.anchor + (pass.id/2) * self.spec.positions.len() as u64;
        let expected = self.spec.count[pass.phase];
        let dropped = net.dropped.saturating_sub(pass.drops_before);
        let mut message = [0;112];
        message[..8].copy_from_slice(b"GSWEEP1\0");
        for (word,value) in message[8..].chunks_exact_mut(8).zip([
            self.generation,self.spec.version,self.anchor,pass.id,pass.phase as u64,
            base+self.spec.first[pass.phase] as u64,base+self.spec.last[pass.phase] as u64,
            pass.first_epoch,pass.last_epoch,pass.last_tick,expected,pass.observed,
            (dropped<<1)|u64::from(executed)]) {
            word.copy_from_slice(&value.to_le_bytes());
        }
        if net.completion(sock,&message) {self.notes_sent+=1;}
        else {self.notes_lost+=1;}
        self.completed = Some(pass.id);
    }
}
