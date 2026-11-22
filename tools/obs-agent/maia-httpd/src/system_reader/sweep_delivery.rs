//! GVIEW4 frames retain their original encoding. GSWEEP1 is an independent
//! UDP completion record for one executed pass.
use super::{Header, R, UdpBatch, Value};
use std::net::UdpSocket;
use std::os::unix::net::UnixDatagram;

const MAX_BYTES: usize = 4 * 1024 * 1024;

pub(crate) struct SweepSpec {
    version: u64,
    positions: Vec<Option<usize>>,
    route_to: Vec<usize>,
    first: [usize;2],
    last: [usize;2],
    count: [u64;2],
    mode: Mode,
    bench: Option<BenchMap>,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode { Stream, Pass }

impl SweepSpec {
    pub(crate) fn from_descriptor(desc: &Value, mode: &str) -> R<Option<Self>> {
        if desc["sweep_wire_version"] != 1 {return Ok(None);}
        let mode = match mode {
            "stream" => Mode::Stream,
            "pass" => Mode::Pass,
            _ => return Err("view delivery must be stream or pass".into()),
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
        let version = desc["version"].as_u64().ok_or("missing sweep map version")?;
        let mut next = 2;
        let mut route_to = vec![0;positions.len()];
        for (i,phase) in positions.iter().enumerate().rev() {
            if let Some(p) = phase {next=*p;}
            route_to[i]=next;
        }
        let bench=BenchMap::open(rows,&positions)?;
        Ok(Some(Self {version,positions,route_to,first,last,count,mode,bench}))
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

struct BenchMap {
    endpoint: std::path::PathBuf,
    targets: Vec<Option<(u64,u64)>>,
    frequencies: Vec<u64>,
}

impl BenchMap {
    fn open(rows:&[Value], phases:&[Option<usize>])->R<Option<Self>> {
        let Some(endpoint)=std::env::var_os("OBS_PREVIEW_SYNTH_SOCKET") else { return Ok(None) };
        let count_path=std::env::var_os("OBS_PREVIEW_SYNTH_COUNT_FILE").ok_or("synthetic target count path")?;
        let text=std::fs::read_to_string(count_path).map_err(|e|e.to_string())?;
        let count:usize=text.trim().parse().map_err(|_|"synthetic target count")?;
        if !matches!(count,1|3|5|10) {return Err("synthetic target count must be 1, 3, 5 or 10".into())}
        let nominal_mhz=[240,1212,2425,3500,4400,5865,6850,8050,9620,11250];
        let mut targets=vec![None;rows.len()];
        let mut frequencies=Vec::with_capacity(count);
        for id in 0..count {
            let center=nominal_mhz[id*nominal_mhz.len()/count]*1_000_000u64;
            let (a,b,rf)=(0..=50).find_map(|step| {
                let rf=center+step*100_000;
                let has=|i:usize, phase| phases[i]==Some(phase) && targets[i].is_none()
                    && rows[i]["rf_ranges"].as_array().is_some_and(|bands|bands.iter().any(|b|
                        b[0].as_u64().zip(b[1].as_u64()).is_some_and(|(lo,hi)|lo<=rf&&rf<hi)));
                Some(((0..rows.len()).find(|i|has(*i,0))?,(0..rows.len()).find(|i|has(*i,1))?,rf))
            }).ok_or("synthetic RF lacks a shared A/B visit")?;
            targets[a]=Some((id as u64,rf));
            targets[b]=Some((id as u64,rf));
            frequencies.push(rf);
        }
        Ok(Some(Self {endpoint:endpoint.into(),targets,frequencies}))
    }
}

struct BenchOutput<'a> {
    map: &'a BenchMap,
    socket: UnixDatagram,
    offered: u64,
    dropped: u64,
    first: Option<u64>,
    last: Option<u64>,
}

impl<'a> BenchOutput<'a> {
    fn open(map: &'a BenchMap) -> R<Self> {
        let socket = UnixDatagram::unbound().map_err(|e| e.to_string())?;
        socket.set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(Self { map, socket, offered:0, dropped:0, first:None, last:None })
    }

    fn send(&mut self, generation:u64, version:u64, pass:&Pass, position:usize) {
        let Some((signal,rf))=self.map.targets[position] else {return};
        self.offered += 1;
        self.first.get_or_insert(pass.id);
        self.last=Some(pass.id);
        let arm_ns=(crate::delivery_runtime::mono()*1e9) as u64;
        let words=[generation, version, pass.id, pass.phase as u64, pass.last_epoch, arm_ns, signal, rf, position as u64];
        let mut message=[0u8;72];
        for (slot,word) in message.chunks_exact_mut(8).zip(words) { slot.copy_from_slice(&word.to_le_bytes()); }
        if self.socket.send_to(&message,&self.map.endpoint).ok()!=Some(message.len()) { self.dropped+=1; }
    }
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
    bench: Option<BenchOutput<'a>>,
}

impl<'a> Delivery<'a> {
    pub(super) fn new(spec: &'a SweepSpec, anchor:u64, generation:u64, net:&mut UdpBatch)->R<Self> {
        if spec.mode == Mode::Pass {
            let bytes = *spec.count.iter().max().unwrap() as usize * net.record_size();
            if bytes > MAX_BYTES {
                return Err("one scan pass exceeds the bounded VIEW buffer".into());
            }
            net.bytes.reserve(bytes);
        }
        let bench=spec.bench.as_ref().map(BenchOutput::open).transpose()?;
        Ok(Self{spec,anchor,generation,pass:None,completed:None,notes_sent:0,notes_lost:0,peak_bytes:0,bench})
    }

    pub(super) fn bench_counts(&self)->Option<Value> {
        self.bench.as_ref().map(|b| serde_json::json!({"offered":b.offered,"dropped":b.dropped,"first":b.first,"last":b.last,"frequencies":b.map.frequencies}))
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
            let first_visit=pass.observed == 0 || pass.last_epoch != h.epoch;
            if first_visit {pass.observed+=1;}
            pass.last_epoch = h.epoch;
            pass.last_tick = h.first_tick;
            if first_visit && let Some(worker)=&mut self.bench {
                worker.send(self.generation,self.spec.version,pass,position);
            }
        }
        if self.spec.positions[position].is_none() {net.transition+=1;}
        if net.bytes.len()+net.record_size() <= MAX_BYTES {
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
        if self.spec.mode == Mode::Stream {
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
