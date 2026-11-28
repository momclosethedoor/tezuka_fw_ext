//! A/B pass delivery. Adjacent passes carry the prior outcome in VIEW headers.
use super::{Header, R, UdpBatch};
use std::net::UdpSocket;

const MAX_BYTES: usize = 4 * 1024 * 1024;
const OUTCOME_COPIES: usize = 3;
const MAX_DROPS: u64 = (1 << 14) - 1;
pub(super) const HEADER_BYTES: usize = 40;

pub(super) fn header(h: Header, generation: u64, outcome: u32) -> [u8; HEADER_BYTES] {
    let mut out = [0; HEADER_BYTES];
    out[..4].copy_from_slice(b"GVW5");
    out[4..12].copy_from_slice(&generation.to_le_bytes());
    out[12..20].copy_from_slice(&h.epoch.to_le_bytes());
    out[20..28].copy_from_slice(&h.first_tick.to_le_bytes());
    out[28..32].copy_from_slice(&(h.ordinal as u32).to_le_bytes());
    out[32..34].copy_from_slice(&(h.k as u16).to_le_bytes());
    out[34..36].copy_from_slice(&(h.flags as u16).to_le_bytes());
    out[36..40].copy_from_slice(&outcome.to_le_bytes());
    out
}

pub(crate) struct SweepSpec {
    positions: Vec<Option<usize>>,
    route_to: Vec<usize>,
    last: [usize;2],
    count: [u64;2],
    mode: Mode,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn run(epochs: &[u64], final_epoch: u64) -> Vec<Vec<u8>> {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.connect(receiver.local_addr().unwrap()).unwrap();
        let mut net = UdpBatch::configured(0,false,1,552);
        let spec = SweepSpec::from_layout(vec![Some(0),Some(0),Some(1),Some(1)],"pass").unwrap();
        let mut delivery = Delivery::new(&spec,100,7,&mut net).unwrap();
        for (ordinal,&epoch) in epochs.iter().enumerate() {
            delivery.accept(Header{first_seq:ordinal as u64*2,last_seq:ordinal as u64*2+1,
                ordinal:ordinal as u64,epoch,first_tick:epoch*100,bins:4096,k:2,flags:9},&[0;512],&mut net,&sender);
        }
        delivery.finish_drained(&mut net,&sender,final_epoch);
        assert_eq!((net.sent,net.pending,net.suppressed),(epochs.len() as u64,0,0));
        (0..net.sent+delivery.notes_sent).map(|_| {
            let mut buf = vec![0;576];
            let received = receiver.recv(&mut buf).unwrap();
            buf.truncate(received);
            buf
        }).collect()
    }

    fn summary(packet: &[u8]) -> u32 {
        let offset = if &packet[..4] == b"GVW5" {36} else {20};
        u32::from_le_bytes(packet[offset..offset+4].try_into().unwrap())
    }

    #[test]
    fn embeds_prior_outcome_and_preserves_hold_and_terminal() {
        let data = run(&[100,101,101,101,102,103,104],104);
        assert_eq!(data.iter().map(|packet| packet.len()).collect::<Vec<_>>(), [552;7].into_iter().chain([24]).collect::<Vec<_>>());
        assert_eq!(data.iter().map(|packet|summary(packet)).collect::<Vec<_>>(),
                   [0,0,0,0,0xc0000002,0xc0000002,0xc0000002,0x80000001]);
        assert_eq!(&data[0][..4],b"GVW5");
        assert_eq!(&data[0][4..12],&7u64.to_le_bytes());
        assert_eq!(&data[0][12..20],&100u64.to_le_bytes());
        assert_eq!(&data[7][..4],b"GEND");
        assert_eq!(&data[7][12..20],&2u64.to_le_bytes());
    }

    #[test]
    fn a_missing_pass_uses_an_explicit_identity() {
        let data = run(&[100,101,104,105],105);
        assert_eq!(&data[2][..4],b"GEND");
        assert_eq!(&data[2][12..20],&0u64.to_le_bytes());
        assert_eq!(summary(&data[3]),0);
        assert_eq!(summary(&data[5]),0xc0000002);
        assert!(run(&[],100).is_empty());
    }

    #[test]
    fn stream_boundary_enqueues_completion_without_sending_a_burst() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.connect(receiver.local_addr().unwrap()).unwrap();
        let mut net = UdpBatch::configured(0,false,2,552);
        net.enable_paced(super::super::view_pace::Rate { packets: 0, bytes: 0 });
        let spec = SweepSpec::from_layout(vec![Some(0),Some(0),Some(1),Some(1)],"stream").unwrap();
        let mut delivery = Delivery::new(&spec,100,7,&mut net).unwrap();
        for epoch in 100..104 {
            delivery.accept(Header {first_seq:0,last_seq:1,ordinal:epoch-100,epoch,
                first_tick:0,bins:4096,k:2,flags:9}, &[0;512], &mut net, &sender);
        }
        delivery.finish_drained(&mut net, &sender, 103);
        assert_eq!((net.sent, net.notes_sent, net.pending), (0, 0, 4));
        delivery.poll(&mut net, &sender);
        assert_eq!((net.sent, net.notes_sent, net.pending), (4, 2, 0));
        let mut packet = [0; 1200];
        for pass in 0..2 {
            let size = receiver.recv(&mut packet).unwrap();
            assert_eq!(size, 1104);
            assert_eq!(u64::from_le_bytes(packet[12..20].try_into().unwrap()), 100 + pass * 2);
            let size = receiver.recv(&mut packet).unwrap();
            assert_eq!(&packet[..4], b"GEND");
            assert_eq!(size, 24);
            assert_eq!(u64::from_le_bytes(packet[12..20].try_into().unwrap()), pass);
            assert_eq!(summary(&packet[..size]), 0xc0000002);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode { Stream, Pass }

impl SweepSpec {
    pub(super) fn streams(&self) -> bool {
        self.mode == Mode::Stream
    }

    pub(super) fn from_layout(positions: Vec<Option<usize>>, mode: &str) -> R<Self> {
        let mode = match mode {
            "stream" => Mode::Stream,
            "pass" => Mode::Pass,
            _ => return Err("view delivery must be stream or pass".into()),
        };
        let mut first = [usize::MAX;2];
        let mut last = [0;2];
        let mut count = [0;2];
        for (i,row) in positions.iter().enumerate() {
            let Some(phase) = *row else {continue};
            if phase > 1 {return Err("invalid sweep phase".into());}
            first[phase] = first[phase].min(i);
            last[phase] = i;
            count[phase] += 1;
        }
        if positions.len() > u16::MAX as usize || count.contains(&0) || last[0] >= first[1] {
            return Err("invalid A/B boundary".into());
        }
        let mut next = 2;
        let mut route_to = vec![0;positions.len()];
        for (i,phase) in positions.iter().enumerate().rev() {
            if let Some(p) = phase {next=*p;}
            route_to[i]=next;
        }
        Ok(Self {positions,route_to,last,count,mode})
    }
}

struct Pass {
    id: u64,
    phase: usize,
    last_epoch: u64,
    observed: u64,
    drops_before: u64,
}

struct Outcome {
    ident: u64,
    summary: u32,
    copies: usize,
}

pub(super) struct Delivery<'a> {
    spec: &'a SweepSpec,
    anchor: u64,
    generation: u64,
    pass: Option<Pass>,
    completed: Option<u64>,
    outcome: Option<Outcome>,
    pub notes_sent: u64,
    pub notes_lost: u64,
    pub peak_bytes: usize,
}

impl<'a> Delivery<'a> {
    pub(super) fn new(spec: &'a SweepSpec, anchor:u64, generation:u64, net:&mut UdpBatch)->R<Self> {
        if spec.mode == Mode::Pass {
            let bytes = *spec.count.iter().max().unwrap() as usize * net.record_size();
            if bytes > MAX_BYTES {
                return Err("one scan pass exceeds the bounded VIEW buffer".into());
            }
            if let Some(queue) = net.paced.as_mut() { queue.hold_until_completion(); }
            else { net.bytes.reserve(bytes); }
        }
        Ok(Self{spec,anchor,generation,pass:None,completed:None,outcome:None,notes_sent:0,notes_lost:0,peak_bytes:0})
    }

    pub(super) fn accept(&mut self,h:Header,view:&[u8],net:&mut UdpBatch,sock:&UdpSocket) {
        let Some(relative) = h.epoch.checked_sub(self.anchor) else {return};
        let count = self.spec.positions.len() as u64;
        let position = (relative%count) as usize;
        let assigned = self.spec.route_to[position];
        let phase = assigned % 2;
        let id = 2*(relative/count)+assigned as u64;
        if self.completed.is_some_and(|old| id<=old) {net.suppressed+=1;return;}
        if self.pass.as_ref().is_some_and(|p| p.id != id) {self.close(net,sock,true,Some(id));}
        let drops = if net.paced.is_some() { net.rejected_before_queue } else { net.dropped };
        let pass = self.pass.get_or_insert(Pass{id,phase,last_epoch:h.epoch,observed:0,drops_before:drops});
        if self.spec.positions[position].is_some() {
            if pass.observed == 0 || pass.last_epoch != h.epoch {pass.observed+=1;}
            pass.last_epoch = h.epoch;
        }
        if self.spec.positions[position].is_none() {net.transition+=1;}
        if view.is_empty() {
            return;
        }
        if net.paced.is_some() {
            net.enqueue_paced(&header(h,self.generation,0), view);
            self.peak_bytes = self.peak_bytes.max(net.paced.as_ref().unwrap().bytes());
            return;
        }
        if net.bytes.len()+net.record_size() <= MAX_BYTES {
            if net.pending==0 {net.start_record();}
            let summary = match self.outcome.as_mut() {
                Some(outcome) if outcome.copies < OUTCOME_COPIES * net.max_records => {
                    outcome.copies += 1;
                    outcome.summary
                }
                _ => 0,
            };
            net.bytes.extend_from_slice(&header(h,self.generation,summary));
            net.bytes.extend_from_slice(view);
            net.pending+=1;
            self.peak_bytes=self.peak_bytes.max(net.bytes.len());
        } else {net.rejected_before_queue+=1;net.dropped+=1;}
    }

    pub(super) fn poll(&self,net:&mut UdpBatch,sock:&UdpSocket) {
        if self.spec.streams() || net.paced.is_some() {
            net.stream_flush(sock);
        }
    }

    pub(super) fn finish_drained(&mut self, net: &mut UdpBatch, sock: &UdpSocket, final_epoch: u64) {
        let executed = self.pass.as_ref().is_some_and(|pass| {
            let base = self.anchor + (pass.id / 2) * self.spec.positions.len() as u64;
            final_epoch >= base + self.spec.last[pass.phase] as u64
        });
        self.finish(net, sock, executed);
    }

    pub(super) fn finish(&mut self,net:&mut UdpBatch,sock:&UdpSocket,executed:bool) {
        self.close(net,sock,executed,None);
    }

    fn close(&mut self,net:&mut UdpBatch,sock:&UdpSocket,executed:bool,next:Option<u64>) {
        let Some(pass) = self.pass.take() else {return};
        if net.paced.is_some() {
            let dropped = net.rejected_before_queue.saturating_sub(pass.drops_before).min(MAX_DROPS) as u32;
            let summary = (1 << 31) | (u32::from(executed) << 30) | (dropped << 16) | pass.observed as u32;
            self.send_outcome(net,sock,&Outcome { ident:pass.id,summary,copies:0 });
            self.completed = Some(pass.id);
            return;
        }
        net.flush(sock);
        if let Some(previous) = self.outcome.take().filter(|outcome| outcome.copies == 0) {
            self.send_outcome(net,sock,&previous);
        }
        let dropped = net.dropped.saturating_sub(pass.drops_before).min(MAX_DROPS) as u32;
        let summary = (1 << 31) | (u32::from(executed) << 30) | (dropped << 16) | pass.observed as u32;
        let outcome = Outcome {ident:pass.id,summary,copies:0};
        if pass.id.checked_add(1) == next {
            self.outcome = Some(outcome);
        } else {
            self.send_outcome(net,sock,&outcome);
        }
        self.completed = Some(pass.id);
    }

    fn send_outcome(&mut self,net:&mut UdpBatch,sock:&UdpSocket,outcome:&Outcome) {
        let mut message = [0;24];
        message[..4].copy_from_slice(b"GEND");
        message[4..12].copy_from_slice(&self.generation.to_le_bytes());
        message[12..20].copy_from_slice(&outcome.ident.to_le_bytes());
        message[20..].copy_from_slice(&outcome.summary.to_le_bytes());
        if net.paced.is_some() { net.paced_completion(&message); }
        else if net.completion(sock,&message) {self.notes_sent+=1;}
        else {self.notes_lost+=1;}
    }
}
