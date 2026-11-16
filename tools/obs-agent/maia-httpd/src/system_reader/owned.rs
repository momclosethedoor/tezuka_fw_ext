//! Full owned copies, immediate cumulative release, independent bounded consumer.
use super::*;
use std::sync::{Arc,Mutex,mpsc};

struct Packet { full:Box<[u64]>, view:Vec<u8>, header:Option<Header>, observed:u64 }
impl Packet {
 fn new()->Self{Self{full:vec![0u64;4096].into_boxed_slice(),view:vec![0;512],header:None,observed:0}}
 fn bytes(&self)->&[u8]{unsafe{std::slice::from_raw_parts(self.full.as_ptr().cast(),32768)}}
 fn bytes_mut(&mut self)->&mut[u8]{unsafe{std::slice::from_raw_parts_mut(self.full.as_mut_ptr().cast(),32768)}}
}

pub(super) fn execute(c:&Config)->R<()> {
 if c.command!="rate"||c.mode!=1||!c.shift544||c.release_only||c.discard_cold||c.save_ordinal.is_some(){return Err("owned pipeline requires warmed rate/full4096/VIEW512".into())}
 std::fs::create_dir_all(&c.output).map_err(|e|e.to_string())?;
 let mut io=Hardware::open(c)?;
 let capacity=io.info.full_len as usize/continuous::BYTES;
 let pool_bytes=capacity*(continuous::BYTES+512);
 save(c,"memory.json",&json!({"full_dma":io.info.full,"view_dma":io.info.view,"full_len":io.info.full_len,"view_len":io.info.view_len,"cached":io.info.cached,"cache_line":io.info.cache_line,"shift544":c.shift544,"owned_pool_records":capacity,"owned_pool_bytes":pool_bytes,"capacity_basis":"one private record per physical ring slot"}))?;
 save(c,"before.json",&io.counters()?)?;
 let (free_tx,free_rx)=mpsc::sync_channel::<Packet>(capacity);
 let (ready_tx,ready_rx)=mpsc::sync_channel::<Packet>(capacity);
 for _ in 0..capacity{free_tx.send(Packet::new()).map_err(|e|e.to_string())?;}
 let progress=Arc::new(Mutex::new(json!({"consumed":0,"sent":0,"datagrams":0,"dropped":0})));
 let consumer_progress=progress.clone();let udp=c.udp.clone();let trace_limit=c.trace_limit;let check=c.check;
 let consumer=std::thread::spawn(move||->R<(Vec<[u64;7]>,Value)>{
  let socket=if let Some(dest)=udp{let s=std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e|e.to_string())?;s.connect(dest).map_err(|e|e.to_string())?;s.set_nonblocking(true).map_err(|e|e.to_string())?;Some(s)}else{None};
  let mut net=UdpBatch::new();let mut rows=Vec::with_capacity(trace_limit);let mut consumed=0u64;let mut omitted=0u64;let mut checksum=0u64;let mut view_hash=14695981039346656037u64;let mut last_ordinal=None;let mut process_ns=0u128;
  while let Ok(mut packet)=ready_rx.recv(){
   let t=Instant::now();let h=packet.header.take().ok_or("missing owned header")?;
   if last_ordinal.is_some_and(|v|h.ordinal!=v+1){return Err("owned consumer ordinal gap".into())}last_ordinal=Some(h.ordinal);
   // Only private memory is accessed here. The producer has released its slot.
   checksum=checksum.wrapping_add(neon_sum(packet.bytes()));
   if check{view::decode(packet.bytes(),&packet.view,&mut vec![0.;4096])?;}
   let mut header=[0u8;56];header[..8].copy_from_slice(b"GVIEW3\0\0");
   for (dst,x) in header[8..].chunks_exact_mut(8).zip([h.ordinal,h.first_seq,h.epoch,h.first_tick,h.k as u64,h.flags as u64]){dst.copy_from_slice(&x.to_le_bytes());}
   // Streaming payload/order checksum includes every VIEW byte; no payload history.
   for b in header.iter().chain(packet.view.iter()){view_hash=(view_hash^u64::from(*b)).wrapping_mul(1099511628211);}
   if let Some(s)=&socket{net.bytes.extend_from_slice(&header);net.bytes.extend_from_slice(&packet.view);net.pending+=1;if net.pending==2{net.flush(s);}}
   if rows.len()<trace_limit{rows.push([h.ordinal,h.first_seq,h.epoch,h.first_tick,h.k as u64,h.flags as u64,packet.observed]);}else{omitted+=1;}
   consumed+=1;process_ns+=t.elapsed().as_nanos();
   if consumed%64==0{*consumer_progress.lock().unwrap()=json!({"consumed":consumed,"sent":net.sent,"datagrams":net.datagrams,"dropped":net.dropped});}
   // At most capacity packets exist, so returning this packet cannot fill a
   // capacity-sized free pool while another packet remains in this consumer.
   free_tx.send(packet).map_err(|e|e.to_string())?;
  }
  if let Some(s)=&socket{net.flush(s);}
  let v=json!({"consumed":consumed,"sent":net.sent,"datagrams":net.datagrams,"dropped":net.dropped,"checksum":checksum.to_string(),"view_fnv1a64":format!("{view_hash:016x}"),"process_ns":process_ns.to_string(),"trace_omitted":omitted});
  *consumer_progress.lock().unwrap()=v.clone();Ok((rows,v))
 });
 io.prepare(c)?;io.start()?;let mut reader=view::PairReader::attach(io.snapshot()?)?;
 let start=Instant::now();let mono_start=crate::delivery_runtime::mono();let mut begin=None;let mut end=None;let mut deadline=c.warmup+c.seconds;
 let mut max_service_us=0u64;let mut max_poll_us=0u64;let mut last_poll=Instant::now();let mut status_due=Instant::now();let mut queue_high=0usize;let mut pool_wait_ns=0u128;let mut pool_waits=0u64;let mut timing=Vec::<[u64;8]>::with_capacity(c.trace_limit);
 let mut stop_started=None;
 let acquisition=(||->R<()>{loop {
  if begin.is_none()&&start.elapsed().as_secs_f64()>=c.warmup{begin=Some(mark(&mut io,&reader,start)?);deadline=begin.as_ref().unwrap()["time"].as_f64().unwrap()+c.seconds;io.physical_high_water=0;reader.high_water=0;io.max_release_gap_us=0;io.last_release=Instant::now();max_service_us=0;max_poll_us=0;}
  if stop_started.is_none()&&(start.elapsed().as_secs_f64()>=deadline||STOP.load(Ordering::Relaxed)){end=Some(mark(&mut io,&reader,start)?);io.stop();stop_started=Some(Instant::now());}
  if status_due.elapsed()>=Duration::from_millis(100){let mut v=mark(&mut io,&reader,start)?;v["owned_consumer"]=progress.lock().unwrap().clone();*STATUS.lock().unwrap()=v;status_due=Instant::now();}
  let t=Instant::now();max_poll_us=max_poll_us.max(t.duration_since(last_poll).as_micros()as u64);last_poll=t;
  let s=io.snapshot()?;
  if s.cookie!=reader.run||s.released!=reader.next||s.committed<reader.next||s.committed>s.reserved||s.fault{return Err("owned reader cursor/fault".into())}
  reader.high_water=reader.high_water.max(s.reserved-reader.next);
  let until=s.committed.min(reader.next+c.batch_limit as u64);io.prepare_prefix(reader.next,until)?;
  for ordinal in reader.next..until {
   let mut packet=match free_rx.try_recv(){Ok(v)=>v,Err(mpsc::TryRecvError::Empty)=>{let wait=Instant::now();pool_waits+=1;let p=free_rx.recv_timeout(Duration::from_secs(1)).map_err(|e|format!("owned consumer stalled: {e}"))?;pool_wait_ns+=wait.elapsed().as_nanos();p},Err(e)=>return Err(e.to_string())};
   let before=start.elapsed().as_nanos()as u64;
   let full=unsafe{std::slice::from_raw_parts_mut(packet.full.as_mut_ptr().cast::<u8>(),32768)};
   let mut copied=0;
   // Exact ownership boundary: full+VIEW private copy complete, then release.
   // No checksum, queue send, socket call or consumer work precedes this release.
   let h=view::copy_release(&mut io,reader.run,ordinal,full,&mut packet.view,||{copied=start.elapsed().as_nanos()as u64;})?;
   reader.next=ordinal+1;reader.read+=1;reader.visited+=1;reader.consumed+=1;
   let released=start.elapsed().as_nanos()as u64;packet.observed=released;packet.header=Some(h);
   if timing.len()<c.trace_limit{timing.push([ordinal,before,copied,released,s.committed,s.reserved,s.released,s.dropped]);}
   ready_tx.send(packet).map_err(|e|e.to_string())?;
  }
  let completed=progress.lock().unwrap()["consumed"].as_u64().unwrap_or(0);
  queue_high=queue_high.max(reader.read.saturating_sub(completed).min(capacity as u64)as usize);
  max_service_us=max_service_us.max(t.elapsed().as_micros()as u64);
  if let Some(stopped)=stop_started{if s.drained&&!s.dma_busy&&reader.next==s.committed{break}if stopped.elapsed()>Duration::from_secs(5){return Err("owned STOP/DRAIN timeout".into())}}
  if reader.next==s.committed{std::thread::sleep(Duration::from_micros(50));}
 }Ok(())})();
 io.stop();drop(ready_tx);
 let work=consumer.join().map_err(|_|"owned consumer panic".to_string())?;
 acquisition?;let (rows,consumer_result)=work?;
 let s=io.snapshot()?;io.release(s.cookie,s.committed)?;
 let after=io.counters()?;
 for (name,rows) in [("records.u64le",rows.iter().flat_map(|r|r.iter().copied()).collect::<Vec<_>>()),("copy-release.u64le",timing.iter().flat_map(|r|r.iter().copied()).collect::<Vec<_>>())]{let mut f=std::io::BufWriter::new(File::create(c.output.join(name)).map_err(|e|e.to_string())?);for x in rows{f.write_all(&x.to_le_bytes()).map_err(|e|e.to_string())?;}}
 if c.save_ring{for(region,size,name)in[(0,io.info.full_len as usize,"full-ring.bin"),(1,io.info.view_len as usize,"view-ring.bin")]{let mut b=vec![0u8;size];io.copy(region,0,&mut b)?;io.give_back()?;std::fs::write(c.output.join(name),b).map_err(|e|e.to_string())?;}}
 let result=json!({"begin":begin,"end":end,"after":after,"measured":{"high_water":io.physical_high_water,"logical_high_water":reader.high_water,"max_release_interval_us":io.max_release_gap_us,"max_service_interval_us":max_poll_us,"max_service_us":max_service_us},"full_read_total":reader.read,"consumed_total":consumer_result["consumed"],"trace_rows":rows.len(),"trace_omitted":consumer_result["trace_omitted"],"consumer":"full owned copy -> release -> bounded independent all4096 consumer/VIEW sender","udp_sent":consumer_result["sent"],"udp_drop":consumer_result["dropped"],"udp_datagrams":consumer_result["datagrams"],"owned_consumer":consumer_result,"pool_capacity_records":capacity,"pool_bytes":pool_bytes,"pool_depth_upper_bound":queue_high,"pool_depth_counter_granularity":64,"pool_waits":pool_waits,"pool_wait_ns":pool_wait_ns.to_string(),"sync_reused_bytes":io.sync_reused_bytes,"strategy":{"owned_pipeline":true,"reuse_owned_cache":c.reuse_owned_cache,"batch_limit":c.batch_limit},"reader_monotonic_start_s":mono_start,"memory_final":memory(),"seconds_total":start.elapsed().as_secs_f64()});save(c,"result.json",&result)?;
 Ok(())
}
