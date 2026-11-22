//! GVIEW4 frames retain their original encoding. GSWEEP1 is an independent
//! UDP completion record for one executed pass.
use super::{Header, R, UdpBatch};
use std::net::UdpSocket;

const MAX_BYTES: usize = 4 * 1024 * 1024;

pub(crate) struct SweepSpec {
    version: u64,
    positions: Vec<Option<usize>>,
    route_to: Vec<usize>,
    first: [usize;2],
    last: [usize;2],
    count: [u64;2],
    mode: Mode,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[derive(Debug, PartialEq)]
    struct Closed(u64, u64, u64, Vec<u64>, bool);

    fn replay(versions: &[(u64,u64)], epochs: &[u64], final_epoch: u64) -> Vec<Closed> {
        let receiver=UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let sender=UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.connect(receiver.local_addr().unwrap()).unwrap();
        let mut network=UdpBatch::configured(0,false,1,576);
        let specs=versions.iter().map(|&(version,_)|
            SweepSpec::from_layout(version,vec![Some(0),Some(0),Some(1),Some(1)],"pass").unwrap())
            .collect::<Vec<_>>();
        let mut index=0;
        let mut notes=0;
        let mut delivery=Delivery::new(&specs[index],versions[index].1,1,&mut network).unwrap();
        for (ordinal,&epoch) in epochs.iter().enumerate() {
            if versions.get(index+1).is_some_and(|&(_,anchor)|epoch>=anchor) {
                delivery.finish(&mut network,&sender,true);
                notes+=delivery.notes_sent;
                index+=1;
                delivery=Delivery::new(&specs[index],versions[index].1,2,&mut network).unwrap();
            }
            delivery.accept(Header{first_seq:ordinal as u64*2,last_seq:ordinal as u64*2+1,
                ordinal:ordinal as u64,epoch,first_tick:epoch*100,k:2,flags:9},&[0;512],&mut network,&sender);
        }
        delivery.finish_drained(&mut network,&sender,final_epoch);
        notes+=delivery.notes_sent;
        assert_eq!(network.sent,epochs.len() as u64);
        assert_eq!((network.pending,network.suppressed),(0,0));
        let mut result=Vec::new();
        let mut ordinals=Vec::new();
        let mut bytes=[0;576];
        while result.len()<notes as usize {
            let n=receiver.recv(&mut bytes).unwrap();
            let word=|at: usize|u64::from_le_bytes(bytes[at..at+8].try_into().unwrap());
            if n==112 && &bytes[..8]==b"GSWEEP1\0" {
                result.push(Closed(word(16),word(32)/2,word(40),std::mem::take(&mut ordinals),word(104)&1==1));
            } else {
                assert_eq!((n,&bytes[..8]),(576,&b"GVIEW4\0\0"[..]));
                ordinals.push(word(8));
            }
        }
        assert!(ordinals.is_empty());
        result
    }

    #[test]
    fn replays_the_four_pass_publication_vectors() {
        // Commit epochs from PL-SCAN-STREAM-002/evidence/pass-publication-vectors.json.
        // Recall alone changes no publication state.
        assert_eq!(replay(&[(11,1)],&[1,2,2,2,3,4,4,5],5),[
            Closed(11,0,0,vec![0,1,2,3],true),Closed(11,0,1,vec![4,5,6],true),
            Closed(11,1,0,vec![7],false)]);
        assert_eq!(replay(&[(21,100),(22,104)],&[100,101,102,103,103,104,105,105],105),[
            Closed(21,0,0,vec![0,1],true),Closed(21,0,1,vec![2,3,4],true),
            Closed(22,0,0,vec![5,6,7],true)]);
        assert_eq!(replay(&[(31,300)],&[300,300],300),[Closed(31,0,0,vec![0,1],false)]);
        assert!(replay(&[(34,1)],&[],0).is_empty());
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode { Stream, Pass }

impl SweepSpec {
    pub(super) fn from_layout(version: u64, positions: Vec<Option<usize>>, mode: &str) -> R<Self> {
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
        if count.contains(&0) || last[0] >= first[1] {
            return Err("invalid A/B boundary".into());
        }
        let mut next = 2;
        let mut route_to = vec![0;positions.len()];
        for (i,phase) in positions.iter().enumerate().rev() {
            if let Some(p) = phase {next=*p;}
            route_to[i]=next;
        }
        Ok(Self {version,positions,route_to,first,last,count,mode})
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
    pub(super) fn new(spec: &'a SweepSpec, anchor:u64, generation:u64, net:&mut UdpBatch)->R<Self> {
        if spec.mode == Mode::Pass {
            let bytes = *spec.count.iter().max().unwrap() as usize * net.record_size();
            if bytes > MAX_BYTES {
                return Err("one scan pass exceeds the bounded VIEW buffer".into());
            }
            net.bytes.reserve(bytes);
        }
        Ok(Self{spec,anchor,generation,pass:None,completed:None,notes_sent:0,notes_lost:0,peak_bytes:0})
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
        if view.is_empty() {
            return;
        }
        if net.bytes.len()+net.record_size() <= MAX_BYTES {
            if net.pending==0 {net.start_record();}
            net.bytes.extend_from_slice(&self.header(h));
            net.bytes.extend_from_slice(view);
            net.pending+=1;
            self.peak_bytes=self.peak_bytes.max(net.bytes.len());
        } else {net.rejected_before_queue+=1;net.dropped+=1;}
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

    pub(super) fn finish_drained(&mut self, net: &mut UdpBatch, sock: &UdpSocket, final_epoch: u64) {
        let executed = self.pass.as_ref().is_some_and(|pass| {
            let base = self.anchor + (pass.id / 2) * self.spec.positions.len() as u64;
            final_epoch >= base + self.spec.last[pass.phase] as u64
        });
        self.finish(net, sock, executed);
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
