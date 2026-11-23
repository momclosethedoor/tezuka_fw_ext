use std::{io,net::UdpSocket,time::{Duration,Instant}};
use super::timing::Histogram;
pub(super) const MAX_SENDMMSG:usize=16;
type SendDatagrams=fn(&UdpSocket,&[u8],usize)->io::Result<usize>;
pub(super) struct DatagramDiagnostic {
 pub limit:usize,pub calls:u64,pub wait:Histogram,
 send:SendDatagrams,
}
pub struct UdpBatch {
 pub bytes:Vec<u8>,pub pending:u64,pub sent:u64,pub dropped:u64,pub datagrams:u64,pub send_calls:u64,
 pub mtu:Option<u32>,pub ip_fragmentation:bool,
 pub oldest_wait_bins:[u64;32],
 pub max_records:usize,pub phase_on:bool,pub send_ns:u128,pub coalesce_us:u64,pub max_wait_ns:u128,
 pub sent_bytes:u64,pub high_records:u64,pub would_block:u64,pub send_errors:u64,
 pub last_errno:Option<i32>,pub notes_sent:u64,pub notes_lost:u64,pub transition:u64,
 pub rejected_before_queue:u64,pub suppressed:u64,
 pub(super) api_send:Histogram,pub(super) oldest_to_send:Histogram,
 pub(super) diagnostic:Option<DatagramDiagnostic>,
 first:Option<Instant>,record_size:usize,
}
impl UdpBatch {
 pub fn configured(coalesce_us:u64,phase_on:bool,max_records:usize,record_size:usize)->Self {
  assert!(matches!(max_records,1|2|4|8|16));
  Self{max_records,bytes:Vec::with_capacity(record_size*max_records),pending:0,sent:0,dropped:0,datagrams:0,
   phase_on,send_ns:0,coalesce_us,max_wait_ns:0,first:None,record_size,send_calls:0,mtu:None,ip_fragmentation:false,oldest_wait_bins:[0;32],
   sent_bytes:0,high_records:0,would_block:0,send_errors:0,last_errno:None,
   notes_sent:0,notes_lost:0,transition:0,rejected_before_queue:0,suppressed:0,
   api_send:Histogram::default(),oldest_to_send:Histogram::default(),diagnostic:None}
 }
 pub fn new(record_size:usize)->Self {
  let delay=std::env::var("OBS_VIEW_COALESCE_US").ok().and_then(|s|s.parse::<u64>().ok()).filter(|v|*v<=10000).unwrap_or(0);
  let count=std::env::var("OBS_VIEW_BATCH_RECORDS").ok().and_then(|s|s.parse::<usize>().ok()).filter(|n|matches!(*n,2|4|8|16)).unwrap_or(2);
  Self::configured(delay,std::env::var("OBS_PHASE_TIMING").as_deref()==Ok("1"),count,record_size)
 }
 pub fn record_size(&self)->usize{self.record_size}
 pub fn set_path_mtu(&mut self,mtu:u32){
  self.mtu=Some(mtu);
  let budget=mtu.saturating_sub(28) as usize; // IPv4 and UDP headers
  self.max_records=(budget/self.record_size).clamp(1,2);
  self.ip_fragmentation=self.record_size>budget;
 }
 pub fn start_record(&mut self){if self.pending==0{self.first=Some(Instant::now());}}
 pub fn due(&self,now:Instant)->bool {
  self.pending>0&&(self.coalesce_us==0||self.pending>=self.max_records as u64||self.first.is_some_and(|t|now.saturating_duration_since(t)>=Duration::from_micros(self.coalesce_us)))
 }
 pub fn timer_due(&self,now:Instant)->bool {
  self.first.is_some_and(|t|now.saturating_duration_since(t)>=Duration::from_micros(self.coalesce_us))
 }
 pub fn flush_due(&mut self,s:&UdpSocket){if self.due(Instant::now()){self.flush(s);}}
 pub(super) fn enable_datagram_diagnostic(&mut self,send:SendDatagrams){
  assert_eq!(self.max_records,1);
  self.bytes.reserve(self.record_size * MAX_SENDMMSG);
  self.diagnostic=Some(DatagramDiagnostic{limit:1,calls:0,wait:Histogram::default(),send});
 }
 pub(super) fn stream_flush(&mut self,s:&UdpSocket){
  if let Some(diag)=self.diagnostic.as_ref() && diag.limit>1 && self.pending<diag.limit as u64 &&
      self.first.is_some_and(|first|first.elapsed()<Duration::from_millis(4)){
   return;
  }
  self.flush(s);
 }
 #[cfg(feature="scan-stream-bench")]
 pub(super) fn suppress_pending(&mut self){
  self.suppressed+=self.pending;
  self.bytes.clear();self.pending=0;self.first=None;
 }
 pub fn completion(&mut self,s:&UdpSocket,bytes:&[u8])->bool {
  self.send_calls+=1;
  match s.send(bytes) {
   Ok(n) if n==bytes.len()=>{self.notes_sent+=1;true}
   result=>{
    self.notes_lost+=1;
    let (blocked,errno)=send_error(result);
    self.would_block+=u64::from(blocked);
    self.send_errors+=1;
    self.last_errno=errno;
    false
   }
  }
 }
 pub fn flush(&mut self,s:&UdpSocket){
  if self.pending==0{return}
  self.high_records=self.high_records.max(self.pending);
  let now=Instant::now();
  if let Some(t)=self.first {
   let age=now.duration_since(t).as_nanos();
   self.max_wait_ns=self.max_wait_ns.max(age);
   if let Some(diag)=self.diagnostic.as_mut(){diag.wait.observe(now.duration_since(t));}
   if self.phase_on {
    let upper_exponent=u128::BITS-age.saturating_sub(1).leading_zeros();
    self.oldest_wait_bins[upper_exponent.min(31) as usize]+=1;
   }
  }
  if let Some(diag)=self.diagnostic.as_mut().filter(|d|d.limit>1){
   let mut offset=0;
   while offset<self.bytes.len(){
    let count=diag.limit.min((self.bytes.len()-offset)/self.record_size);
    let stop=offset+count*self.record_size;
    self.send_calls+=1;
    diag.calls+=1;
    let result=(diag.send)(s,&self.bytes[offset..stop],self.record_size);
    match result {
     Ok(sent) if (1..=count).contains(&sent)=>{
      self.sent+=sent as u64;
      self.datagrams+=sent as u64;
      self.sent_bytes+=(sent*self.record_size) as u64;
      offset+=sent*self.record_size;
     }
     result=>{
      self.dropped+=count as u64;
      let (blocked,errno)=send_error(result);
      self.would_block+=u64::from(blocked);
      self.send_errors+=1;
      self.last_errno=errno;
      offset=stop;
     }
    }
   }
  }else{for packet in self.bytes.chunks(self.record_size*self.max_records){let count=(packet.len()/self.record_size)as u64;
   self.send_calls+=1;
   let before=(self.phase_on && cfg!(feature="scan-stream-bench")).then(Instant::now);
   match s.send(packet){
    Ok(n)if n==packet.len()=>{self.sent+=count;self.datagrams+=1;self.sent_bytes+=n as u64;
     if let Some(before)=before {self.api_send.observe(before.elapsed());}
     if before.is_some() && let Some(ready)=self.first {self.oldest_to_send.observe(ready.elapsed());}
    },
    result=>{
     self.dropped+=count;
     let (blocked,errno)=send_error(result);
     self.would_block+=u64::from(blocked);
     self.send_errors+=1;
     self.last_errno=errno;
    }
   }
  }}
  self.bytes.clear();self.pending=0;self.first=None;
  if self.phase_on{self.send_ns+=now.elapsed().as_nanos();}
 }
}
fn send_error(result:io::Result<usize>)->(bool,Option<i32>){
 match result {
  Ok(_)=>(false,None),
  Err(error)=>(error.kind()==io::ErrorKind::WouldBlock,error.raw_os_error()),
 }
}
#[cfg(test)] mod tests {
 use super::*;
 fn add(q:&mut UdpBatch,value:u8){q.start_record();q.bytes.resize(q.bytes.len()+q.record_size,value);q.pending+=1;}
 #[test] fn owned_boundary_view_batch_pairs_and_preserves_wire_records(){
  let receiver=UdpSocket::bind("127.0.0.1:0").unwrap();receiver.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
  let sender=UdpSocket::bind("127.0.0.1:0").unwrap();sender.connect(receiver.local_addr().unwrap()).unwrap();
  let mut q=UdpBatch::configured(400,false,2,568);add(&mut q,11);let first=q.first.unwrap();
  assert!(!q.due(first+Duration::from_micros(399)));assert!(q.due(first+Duration::from_micros(400)));
  add(&mut q,22);assert!(q.due(first));q.flush_due(&sender);
  let mut buf=[0u8;1200];assert_eq!(receiver.recv(&mut buf).unwrap(),1136);
  assert_eq!(&buf[..568],&[11;568]);assert_eq!(&buf[568..1136],&[22;568]);assert_eq!((q.sent,q.datagrams,q.pending),(2,1,0));
  // STOP forces the final single record out, regardless of deadline.
  add(&mut q,33);q.flush(&sender);assert_eq!(receiver.recv(&mut buf).unwrap(),568);assert_eq!(&buf[..568],&[33;568]);
  assert_eq!((q.sent,q.datagrams,q.dropped),(3,2,0));assert!(!q.due(Instant::now()));
 }
 #[test] fn owned_boundary_view_batch_zero_delay_preserves_control(){let mut q=UdpBatch::configured(0,false,2,568);add(&mut q,7);assert!(q.due(q.first.unwrap()));}
 #[test] fn owned_boundary_large_view_datagrams_preserve_every_record(){
  let receiver=UdpSocket::bind("127.0.0.1:0").unwrap();receiver.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
  let sender=UdpSocket::bind("127.0.0.1:0").unwrap();sender.connect(receiver.local_addr().unwrap()).unwrap();
  let mut buf=[0u8;65535];
  for count in [4usize,8,16]{let mut q=UdpBatch::configured(4000,false,count,568);
   for n in 0..count{add(&mut q,n as u8);}
   assert!(q.due(q.first.unwrap()));q.flush_due(&sender);
   assert_eq!(receiver.recv(&mut buf).unwrap(),count*568);
   for n in 0..count{assert_eq!(&buf[n*568..(n+1)*568],&[n as u8;568]);}
   assert_eq!((q.sent,q.datagrams,q.dropped),(count as u64,1,0));
  }
 }
 #[test] fn managed_ipv4_datagram_geometry_respects_path_mtu(){
  for (size,batch,frag) in [(576,2,false),(1088,1,false),(2112,1,true)] {
   let mut q=UdpBatch::configured(0,false,2,size);
   q.set_path_mtu(1500);
   assert_eq!((q.record_size(),q.max_records,q.ip_fragmentation),(size,batch,frag));
  }
 }
 #[test] fn controlled_view_preserves_extended_records_when_batch_splits(){
  let receiver=UdpSocket::bind("127.0.0.1:0").unwrap();receiver.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
  let sender=UdpSocket::bind("127.0.0.1:0").unwrap();sender.connect(receiver.local_addr().unwrap()).unwrap();
  let mut q=UdpBatch::configured(0,false,2,576);
  for n in 1..=3{add(&mut q,n);}
  q.flush(&sender);
  let mut buf=[0u8;1500];
  assert_eq!(receiver.recv(&mut buf).unwrap(),1152);
  assert_eq!(&buf[..576],&[1;576]);assert_eq!(&buf[576..1152],&[2;576]);
  assert_eq!(receiver.recv(&mut buf).unwrap(),576);assert_eq!(&buf[..576],&[3;576]);
  assert_eq!((q.sent,q.datagrams,q.dropped),(3,2,0));
 }

 #[test] fn datagram_diagnostic_retries_unsent_suffix_without_merging_records(){
  let receiver=UdpSocket::bind("127.0.0.1:0").unwrap();receiver.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
  let sender=UdpSocket::bind("127.0.0.1:0").unwrap();sender.connect(receiver.local_addr().unwrap()).unwrap();
  let mut q=UdpBatch::configured(0,false,1,4160);
  q.enable_datagram_diagnostic(|sock,chunk,record_size|{
   // Force a successful partial write regardless of the requested batch size.
   sock.send(&chunk[..record_size]).map(|_|1)
  });
  q.diagnostic.as_mut().unwrap().limit=4;
  for n in 1..=4{add(&mut q,n);}
  q.stream_flush(&sender);
  let mut buf=[0u8;5000];
  for n in 1..=4{
   assert_eq!(receiver.recv(&mut buf).unwrap(),4160);
   assert_eq!(&buf[..4160],&[n;4160]);
  }
  assert_eq!((q.sent,q.datagrams,q.dropped,q.send_calls),(4,4,0,4));
  assert_eq!(q.diagnostic.as_ref().unwrap().calls,4);
 }

}
