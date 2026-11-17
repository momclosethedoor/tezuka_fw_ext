use std::{net::UdpSocket,time::{Duration,Instant}};
pub struct UdpBatch {
 pub bytes:Vec<u8>,pub pending:u64,pub sent:u64,pub dropped:u64,pub datagrams:u64,
 pub phase_on:bool,pub send_ns:u128,pub coalesce_us:u64,pub max_wait_ns:u128,
 first:Option<Instant>,
}
impl UdpBatch {
 pub fn configured(coalesce_us:u64,phase_on:bool)->Self {
  Self{bytes:Vec::with_capacity(1136),pending:0,sent:0,dropped:0,datagrams:0,
   phase_on,send_ns:0,coalesce_us,max_wait_ns:0,first:None}
 }
 pub fn new()->Self {
  let delay=std::env::var("OBS_VIEW_COALESCE_US").ok().and_then(|s|s.parse::<u64>().ok()).filter(|v|*v<=1000).unwrap_or(0);
  Self::configured(delay,std::env::var("OBS_PHASE_TIMING").as_deref()==Ok("1"))
 }
 pub fn start_record(&mut self){if self.pending==0{self.first=Some(Instant::now());}}
 pub fn due(&self,now:Instant)->bool {
  self.pending>0&&(self.coalesce_us==0||self.pending>=2||self.first.is_some_and(|t|now.saturating_duration_since(t)>=Duration::from_micros(self.coalesce_us)))
 }
 pub fn flush_due(&mut self,s:&UdpSocket){if self.due(Instant::now()){self.flush(s);}}
 pub fn flush(&mut self,s:&UdpSocket){
  if self.pending==0{return}
  let now=Instant::now();if let Some(t)=self.first{self.max_wait_ns=self.max_wait_ns.max(now.duration_since(t).as_nanos());}
  for packet in self.bytes.chunks(1136){let count=(packet.len()/568)as u64;
   match s.send(packet){Ok(n)if n==packet.len()=>{self.sent+=count;self.datagrams+=1;},_=>self.dropped+=count}
  }
  self.bytes.clear();self.pending=0;self.first=None;
  if self.phase_on{self.send_ns+=now.elapsed().as_nanos();}
 }
}
#[cfg(test)] mod tests {
 use super::*;
 fn add(q:&mut UdpBatch,value:u8){q.start_record();q.bytes.extend_from_slice(&[value;568]);q.pending+=1;}
 #[test] fn owned_boundary_view_batch_pairs_and_preserves_wire_records(){
  let receiver=UdpSocket::bind("127.0.0.1:0").unwrap();receiver.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
  let sender=UdpSocket::bind("127.0.0.1:0").unwrap();sender.connect(receiver.local_addr().unwrap()).unwrap();
  let mut q=UdpBatch::configured(400,false);add(&mut q,11);let first=q.first.unwrap();
  assert!(!q.due(first+Duration::from_micros(399)));assert!(q.due(first+Duration::from_micros(400)));
  add(&mut q,22);assert!(q.due(first));q.flush_due(&sender);
  let mut buf=[0u8;1200];assert_eq!(receiver.recv(&mut buf).unwrap(),1136);
  assert_eq!(&buf[..568],&[11;568]);assert_eq!(&buf[568..1136],&[22;568]);assert_eq!((q.sent,q.datagrams,q.pending),(2,1,0));
  // STOP forces the final single record out, regardless of deadline.
  add(&mut q,33);q.flush(&sender);assert_eq!(receiver.recv(&mut buf).unwrap(),568);assert_eq!(&buf[..568],&[33;568]);
  assert_eq!((q.sent,q.datagrams,q.dropped),(3,2,0));assert!(!q.due(Instant::now()));
 }
 #[test] fn owned_boundary_view_batch_zero_delay_preserves_control(){let mut q=UdpBatch::configured(0,false);add(&mut q,7);assert!(q.due(q.first.unwrap()));}
}
