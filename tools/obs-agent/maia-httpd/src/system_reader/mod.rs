//! Explicit, bounded r3 board exerciser. No PL load, RF retune, flash or network.
mod continuous;
mod view;
mod discard;
mod view_batch;
use view_batch::UdpBatch;
pub(crate) use continuous::Header;
use continuous::Snapshot;
use view::PairIo;
use std::{fs::{File,OpenOptions},io::Write,os::fd::AsRawFd,path::PathBuf,sync::atomic::{AtomicBool,Ordering,compiler_fence},time::{Duration,Instant}};
use serde::Deserialize;
use serde_json::{json,Value};
static STOP:AtomicBool=AtomicBool::new(false);
extern "C" fn signal(_:libc::c_int){STOP.store(true,Ordering::Relaxed);}
type R<T>=Result<T,String>;
#[derive(Deserialize,Debug)]#[serde(deny_unknown_fields)]struct Config{
 boot_id:String,output:PathBuf,command:String,#[serde(default="kdefault")]k:u32,
 #[serde(default)]mode:u32,#[serde(default)]peak:bool,#[serde(default)]scan:bool,
 #[serde(default="settledefault")]settling:u32,#[serde(default="secondsdefault")]seconds:f64,
 #[serde(default="warmdefault")]warmup:f64,#[serde(default)]shift544:bool,
 #[serde(default)]previous_mode:u32,#[serde(default)]discard_cold:bool,#[serde(default)]check:bool,
 #[serde(default)]save_ordinal:Option<u64>,
 #[serde(default)]save_ring:bool,
 #[serde(default="tracedefault")]trace_limit:usize,
 #[serde(default)]reuse_owned_cache:bool,
 #[serde(default)]owned_pipeline:bool,
 #[serde(default)]read_without_sum:bool,
 #[serde(default)]send_after_release:bool,
 #[serde(default)]diagnostic_sync:Option<String>,
 #[serde(default)]batch_sync:bool,#[serde(default)]direct:bool,#[serde(default)]release_only:bool,
 #[serde(default="batchdefault")]batch_limit:usize,
 #[serde(default)]tx_active:bool,#[serde(default)]managed_tx:bool,#[serde(default)]aligned:bool,#[serde(default)]hot:bool,#[serde(default)]udp:Option<String>,#[serde(default)]neon:bool,
}
fn batchdefault()->usize{32}
fn tracedefault()->usize{400000}
fn kdefault()->u32{2}fn settledefault()->u32{6250}fn secondsdefault()->f64{30.}fn warmdefault()->f64{5.}
#[derive(Default,Debug)]#[repr(C)]struct Info{full:u64,view:u64,full_len:u32,view_len:u32,cached:u32,cache_line:u32,abi:u32,reserved:u32}
#[repr(C)]struct Sync{region:u32,offset:u32,len:u32,for_cpu:u32}
struct Map{ptr:*mut u8,len:usize}
impl Map{fn new(fd:&File,len:usize,off:i64,write:bool)->R<Self>{let p=unsafe{libc::mmap(std::ptr::null_mut(),len,libc::PROT_READ|if write{libc::PROT_WRITE}else{0},libc::MAP_SHARED,fd.as_raw_fd(),off as libc::off_t)};if p==libc::MAP_FAILED{return Err(std::io::Error::last_os_error().to_string())}Ok(Self{ptr:p.cast(),len})}}
impl Drop for Map{fn drop(&mut self){unsafe{libc::munmap(self.ptr.cast(),self.len);}}}
struct Hardware{phase_on:bool,snapshot_ns:u128,release_ns:u128,fast_snapshot_on:bool,pressure_on:bool,pressure_due:Instant,pressure:Vec<[u64;4]>,pressure_omitted:u64,_lease:File,device:File,regs:Map,full:Map,overview:Map,info:Info,valid:u32,active:bool,shift:bool,logical_tail:Option<u64>,physical_high_water:u64,previous_view:usize,ranges:Vec<(u32,u32,u32)>,copied_bytes:u64,view_bytes:u64,max_release_gap_us:u64,last_release:Instant,sync_ns:u128,copy_ns:u128,batch_sync:bool,direct:bool,release_only:bool,batch_limit:usize,covered:Vec<(u32,u32,u32)>,sync_calls:u64,process_ns:u128,actual_copy_bytes:u64,reuse_owned_cache:bool,retained_synced:Option<u64>,sync_reused_bytes:u64}
impl Hardware{
 fn open(c:&Config)->R<Self>{
  let read=|p:&str|std::fs::read_to_string(p).map_err(|e|e.to_string());
  if read("/proc/sys/kernel/random/boot_id")?.trim()!=c.boot_id{return Err("boot lease mismatch".into())}
  if read("/tmp/pl-uart-obs-session/phase")?.trim()!="OBS_READY"{return Err("UART loader prerequisite".into())}
  for p in std::fs::read_dir("/proc").map_err(|e|e.to_string())?{let p=p.map_err(|e|e.to_string())?.path();if p.file_name().and_then(|n|n.to_str()).and_then(|n|n.parse::<u32>().ok()).is_none_or(|pid|pid==std::process::id()){continue;}if std::fs::read_to_string(p.join("comm")).is_ok_and(|s|s.trim()=="maia-httpd"){return Err("legacy Maia owner active".into())}}
  if c.hot{
   let guard=OpenOptions::new().read(true).write(true).open("/dev/obs_fl_guard").map_err(|e|e.to_string())?;let mut status=[0u32;128];
   if unsafe{libc::ioctl(guard.as_raw_fd(),0x4605,status.as_mut_ptr())}!=0||status[0]!=2||status[1]!=2||status[2]!=0||
      (if c.managed_tx { status[9]!=31 || status[117]!=0x31475458 } else {!matches!(status[9],3|7)}){return Err("guard must own calibrated SPI lease".into())}
  }else{
   if read("/sys/bus/iio/devices/iio:device0/out_altvoltage1_TX_LO_powerdown")?.trim()!=if c.tx_active{"0"}else{"1"}{return Err("TX readback differs from declared test mode".into())}
   if read("/sys/bus/iio/devices/iio:device0/in_voltage_sampling_frequency")?.trim()!="61440000"{return Err("Fs prerequisite".into())}
  }
  let lease=OpenOptions::new().read(true).write(true).create(true).truncate(false).open("/run/maia-observation.lock").map_err(|e|e.to_string())?;
  if unsafe{libc::flock(lease.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0{return Err("spectral owner busy".into())}
  let device=OpenOptions::new().read(true).write(true).open("/dev/obs_dma").map_err(|e|e.to_string())?;
  let mut info=Info::default();if unsafe{libc::ioctl(device.as_raw_fd(),0x4f01,&mut info)}!=0{return Err("DMA INFO ioctl".into())}
  if info.abi!=1{return Err("DMA ABI".into())}view::ranges(info.full,info.full_len as usize,info.view,info.view_len as usize)?;
  // Explicitly exclude both installed reserves, even though DMA API owns CMA.
  for(a,n)in[(info.full,info.full_len),(info.view,info.view_len)]{for(lo,hi)in[(0x06000000,0x0e000000),(0x16000000,0x16040000)]{if a<hi&&a+n as u64>lo{return Err("DMA overlaps stock reserve".into())}}}
  let full=Map::new(&device,info.full_len as usize,0,false)?;let overview=Map::new(&device,info.view_len as usize,0x1000000,false)?;
  let mem=OpenOptions::new().read(true).write(true).open("/dev/mem").map_err(|e|e.to_string())?;let regs=Map::new(&mem,4096,0x7c460000,true)?;
  let h=Self{phase_on:std::env::var("OBS_PHASE_TIMING").as_deref()==Ok("1"),snapshot_ns:0,release_ns:0,fast_snapshot_on:std::env::var("OBS_FAST_SNAPSHOT").as_deref()!=Ok("0"),pressure_on:std::env::var("OBS_PRESSURE_TRACE").as_deref()==Ok("1"),pressure_due:Instant::now(),pressure:Vec::with_capacity(if std::env::var("OBS_PRESSURE_TRACE").as_deref()==Ok("1"){65536}else{0}),pressure_omitted:0,_lease:lease,device,regs,full,overview,info,valid:0,active:false,shift:c.shift544,logical_tail:None,physical_high_water:0,previous_view:[0,512,1024,2048][c.previous_mode as usize],ranges:Vec::with_capacity(4),copied_bytes:0,view_bytes:0,max_release_gap_us:0,last_release:Instant::now(),sync_ns:0,copy_ns:0,batch_sync:c.batch_sync,direct:c.direct,release_only:c.release_only,batch_limit:c.batch_limit,covered:Vec::with_capacity(4),sync_calls:0,process_ns:0,actual_copy_bytes:0,reuse_owned_cache:c.reuse_owned_cache,retained_synced:None,sync_reused_bytes:0};
  view::geometry(h.r(0xe4),h.r(0x1c0),h.r(0x1c4),h.r(0x1c8))?;if h.r(0x50)!=continuous::ABI{return Err("OBN2 ID".into())}
  Ok(h)
 }
 fn r(&self,o:usize)->u32{unsafe{std::ptr::read_volatile(self.regs.ptr.add(o).cast())}}
 fn w(&self,o:usize,v:u32){unsafe{std::ptr::write_volatile(self.regs.ptr.add(o).cast(),v)};compiler_fence(Ordering::SeqCst);}
 fn r64(&self,o:usize)->u64{self.r(o)as u64|((self.r(o+4)as u64)<<32)}
 fn live64(&self,o:usize)->u64{loop{let hi=self.r(o+4);let lo=self.r(o);if hi==self.r(o+4){return(hi as u64)<<32|lo as u64}}}
 fn sync(&mut self,region:u32,offset:u32,len:u32,cpu:bool)->R<()>{let t=Instant::now();let s=Sync{region,offset,len,for_cpu:cpu as u32};if unsafe{libc::ioctl(self.device.as_raw_fd(),0x4f02,&s)}!=0{return Err(format!("DMA sync: {}",std::io::Error::last_os_error()))}compiler_fence(Ordering::SeqCst);self.sync_ns+=t.elapsed().as_nanos();self.sync_calls+=1;Ok(())}
 fn copy(&mut self,region:u32,off:usize,out:&mut[u8])->R<()>{self.ensure_sync(region,off as u32,out.len()as u32)?;let p=if region==0{self.full.ptr}else{self.overview.ptr};let t=Instant::now();
  #[cfg(target_arch="arm")] unsafe{obs_neon_copy(out.as_mut_ptr(),p.add(off),out.len())};
  #[cfg(not(target_arch="arm"))] unsafe{std::ptr::copy_nonoverlapping(p.add(off),out.as_mut_ptr(),out.len())};
  self.copy_ns+=t.elapsed().as_nanos();self.actual_copy_bytes+=out.len() as u64;self.ranges.push((region,off as u32,out.len()as u32));Ok(())}
 fn ensure_sync(&mut self,region:u32,off:u32,len:u32)->R<()>{
  if !self.covered.iter().any(|&(r,o,n)|r==region && off>=o && off+len<=o+n){self.sync(region,off,len,true)?;}Ok(())
 }
 fn sync_circular(&mut self,region:u32,off:u32,len:u32,total:u32)->R<()>{
  let first=len.min(total-off);if first>0{self.sync(region,off,first,true)?;self.covered.push((region,off,first));}
  if len>first{self.sync(region,0,len-first,true)?;self.covered.push((region,0,len-first));}Ok(())
 }
 fn give_back(&mut self)->R<()>{
  // DMA_FROM_DEVICE, read-only VMA with VM_MAYWRITE cleared. CPU never changes
  // DMA memory: no sync_for_device is required (Linux6.12 dma-api-howto,
  // receive-buffer example). Complete all reads before cumulative MMIO release.
  self.ranges.clear();
  #[cfg(target_arch="arm")] unsafe{std::arch::asm!("dmb sy",options(nostack,preserves_flags));}
  #[cfg(not(target_arch="arm"))] std::sync::atomic::fence(Ordering::SeqCst);
  Ok(())
 }
 fn prepare(&mut self,c:&Config)->R<()>{let s=self.snapshot()?;if s.running||s.dma_busy||!s.drained||s.committed!=s.released||s.reserved!=s.committed||s.fault{return Err("START needs STOP/DRAINED/released/nonfatal".into())}
  self.w(0x1b8,0);self.w(0x1a8,self.info.full as u32);self.w(0x1ac,self.info.view as u32);self.w(0x1b0,self.info.full_len);self.w(0x1b4,self.info.view_len);self.w(0x1a4,c.mode);
  self.w(0x164,c.scan as u32);self.w(0x168,c.settling);self.w(0x16c,0);self.w(0x178,0);self.w(0x18c,1);self.w(0x20,c.k*2|if c.peak{1<<15}else{0});self.w(0x58,100);self.w(0x5c,0);self.w(0x1b8,0x56494557);
  if self.r(0x1bc)!=1||self.r(0x1a8)!=self.info.full as u32||self.r(0x1ac)!=self.info.view as u32{return Err("staged memory validation".into())}Ok(())
 }
 fn start(&mut self)->R<()>{let before=self.snapshot()?.cookie;self.valid=1;self.active=true;self.w(0x54,3);let s=self.snapshot()?;if !s.running||s.cookie!=before+1{return Err("START rejected".into())}self.last_release=Instant::now();Ok(())}
 fn service_route(&mut self,q:&RunControl)->R<()>{
  if self.r(0x228)&8==0{return Ok(())}
  let id=self.r(0x240);
  let epoch=self.live64(0x24c);
  let version=self.r(0x22c);
  if (q.route)(id,epoch,version,self.r(0x234),self.r(0x244))?==Some(id)
     && self.r(0x240)==id && self.r(0x228)&8!=0 && self.r(0x22c)==version && self.live64(0x24c)==epoch{
   self.w(0x218,id);self.w(0x220,2);
  }
  Ok(())
 }
 fn cold_split(&mut self,control:Option<&RunControl>)->R<Value>{
  let initial=self.snapshot()?;
  if !self.shift||initial.cookie!=1||initial.released!=0{return Ok(Value::Null)}
  let deadline=Instant::now()+Duration::from_secs(if control.is_some(){3}else{1});
  loop{
   if let Some(q)=control{
    if q.stop.load(Ordering::Acquire){return Err("scan cancelled during first visit".into())}
    self.service_route(q)?;
    (q.check)()?;
   }
   let s=self.snapshot()?;
   if !s.running||s.fault||s.cookie!=1||s.released!=0||s.reserved>=view::SLOTS as u64{
    return Err("cold split lost initial ownership".into())
   }
   if s.committed>=2{break}
   if STOP.load(Ordering::Relaxed)||Instant::now()>=deadline{return Err("cold split header deadline".into())}
   std::thread::sleep(Duration::from_micros(40));
  }
  let mut prefix=[0u8;512];
  self.copy(0,(view::SLOTS-1)*32768+28416+self.previous_view,&mut prefix)?;
  if !prefix.iter().skip(7).step_by(8).all(|&byte|byte==0){
   self.give_back()?;
   return Ok(Value::Null)
  }
  self.copy(0,28416+self.previous_view,&mut prefix)?;
  let next=continuous::header_prefix(&prefix,3)?;
  let first_epoch=self.live64(0x198);
  if next.ordinal!=1||next.epoch<first_epoch{return Err("cold split successor identity".into())}
  // Advance the logical cursor while retaining physical slot 0 for ordinal 1.
  self.release(1,1)?;
  Ok(json!({"discarded":1,"run":1,"ordinal":0,"first_epoch":first_epoch,
            "next_epoch":next.epoch,"physical_released":self.r64(0x110)}))
 }
 fn stop(&mut self){self.valid=0;self.w(0x54,4);self.active=false;}
 fn read_snapshot(&mut self,details:bool)->R<Snapshot>{let phase=self.phase_on.then(Instant::now);self.w(0x54,self.valid|32);let status=self.r(0x154);if status&8!=0{return Err("sticky rejected command".into())}
  let physical=self.r64(0x110);let reserved=self.r64(0x108);
  if reserved<physical||reserved-physical>1024{return Err("physical ring ownership".into())}
  self.physical_high_water=self.physical_high_water.max(reserved-physical);
  if self.shift&&self.active&&self.logical_tail.is_none()&&reserved-physical==1024{return Err("first split record lost lookbehind before reader attached; no release".into())}
  let logical=self.logical_tail.unwrap_or(physical);
  if logical<physical||logical-physical>1{return Err("split release ownership".into())}
  if self.pressure_on && self.pressure_due.elapsed()>=Duration::from_millis(2){
   if self.pressure.len()<65536{self.pressure.push([(crate::delivery_runtime::mono()*1e9)as u64,reserved,self.r64(0x100),physical]);}else{self.pressure_omitted+=1;}
   self.pressure_due=Instant::now();
  }
  let value=Snapshot{committed:self.r64(0x100),reserved,released:logical,input_seq:if details{self.r64(0x118)}else{0},finished:if details{self.r64(0x128)}else{0},selected:if details{self.r64(0x130)}else{0},unselected:if details{self.r64(0x138)}else{0},dirty_drop:if details{self.r(0x148)}else{0},stop_drop:if details{self.r(0x14c)}else{0},stop_input_limit:if details{self.r64(0x158)}else{0},cookie:self.r(0x150),dropped:if details{self.r(0x140)as u64+self.r(0x144)as u64+self.r(0x160)as u64}else{0},running:status&1!=0,dma_busy:status&2!=0,drained:status&16!=0,fault:status&4!=0};if let Some(t)=phase{self.snapshot_ns+=t.elapsed().as_nanos();}Ok(value)}
 fn fld(&self)->Value{loop{let v=self.r(0x22c);let epoch=self.live64(0x238);let pass=self.r(0x268);let status=self.r(0x228);let loads=self.r(0x254);let late=self.r(0x258);let errors=self.r(0x25c);let current_pass=self.r(0x230);let position=self.r(0x234);if v==self.r(0x22c){return json!({"version":v,"map_epoch":epoch,"map_pass":pass,"status":status,"loads":loads,"late":late,"errors":errors,"pass":current_pass,"position":position,"tx_changes":self.r(0x260),"transactions":self.r(0x264)})}}}
 fn counters(&mut self)->R<Value>{let s=self.snapshot()?;Ok(json!({"fld":self.fld(),"committed":s.committed,"reserved":s.reserved,"released":self.r64(0x110),"logical_released":s.released,"input_seq":s.input_seq,"formed":s.finished,"selected":s.selected,"unselected":s.unselected,"bank_drop":self.r(0x140),"ring_drop":self.r(0x144),"dirty_drop":s.dirty_drop,"stop_drop":s.stop_drop,"dma_drop":self.r(0x160),"run":s.cookie,"status":self.r(0x154),"visit_epoch":self.live64(0x180),"visit_status":self.r(0x188),"first_epoch":self.live64(0x198)}))}
}
impl Drop for Hardware{fn drop(&mut self){if self.active{self.stop();}}}
impl PairIo for Hardware{
 fn batch_limit(&self)->usize{self.batch_limit}
 fn phase_timing(&self)->bool{self.phase_on}
 fn full_read(&self)->bool{!self.release_only}
 fn prepare_prefix(&mut self,first:u64,end:u64)->R<()>{
  self.covered.clear();if !self.batch_sync||self.release_only||first==end{return Ok(())}
  let count=(end-first) as u32;let slot=(first&1023)as u32;
  if self.reuse_owned_cache && self.shift && self.previous_view!=0 && first.checked_sub(1)==self.retained_synced {
   // The split lookbehind remains physically owned until this record is read.
   // The preceding batch synced its entire committed slot. The read-only VMA
   // cannot dirty it, and PL cannot reuse it before cumulative release.
   let prev=((slot+1023)&1023)as u32;
   self.covered.push((0,prev*32768,32768));
   self.covered.push((1,prev*2048,2048));
   self.sync_circular(0,slot*32768,count*32768,33554432)?;
   self.sync_circular(1,slot*2048,count*2048,2097152)?;
   self.retained_synced=Some(end-1);self.sync_reused_bytes+=32768+2048;
   return Ok(())
  }
  // Every range is a committed prefix plus its still-owned lookbehind. No
  // uncommitted record and no released previous pair may enter this batch.
  let (start,n)=if self.shift&&self.previous_view==0{(((slot+1023)&1023)*32768+28416,count*32768)}else if self.shift{(((slot+1023)&1023)*32768,(count+1)*32768)}else{(slot*32768,count*32768)};
  self.sync_circular(0,start,n,33554432)?;
  if self.previous_view!=0{let vstart=if self.shift{((slot+1023)&1023)*2048}else{slot*2048};self.sync_circular(1,vstart,(count+1)*2048,2097152)?;}
  self.retained_synced=Some(end-1);
  Ok(())
 }
 fn visit_pair<F:FnMut(Header,&[&[u8]],&[u8])->R<()>>(&mut self,slot:usize,full:&mut[u8],overview:&mut[u8],sink:&mut F)->R<()>{
  if self.direct||self.release_only{
   if !self.shift{return Err("direct/header-only requires shift544".into())}
   let pv=self.previous_view;let prev=(slot+1023)&1023;let a=prev*32768+28416+pv;let b=slot*32768;
   self.ensure_sync(0,a as u32,if self.release_only{512}else{(4352-pv)as u32})?;
   let prefix=unsafe{std::slice::from_raw_parts(self.full.ptr.add(a),if self.release_only{512}else{4352-pv})};
   let h=continuous::header_prefix(prefix,3)?;let n=view::view_len(h);
   if !self.release_only{self.ensure_sync(0,b as u32,28416)?;if pv!=0{self.ensure_sync(1,(prev*2048)as u32,pv as u32)?;}if n!=0{self.ensure_sync(0,(b+28416)as u32,n as u32)?;}}
   let t=Instant::now();
   if self.release_only{sink(h,&[],&[])?;}else{
    let suffix=unsafe{std::slice::from_raw_parts(self.full.ptr.add(b),28416)};
    let middle=unsafe{std::slice::from_raw_parts(self.overview.ptr.add(prev*2048),pv)};
    let v=unsafe{std::slice::from_raw_parts(self.full.ptr.add(b+28416),n)};
    sink(h,&[prefix,middle,suffix],v)?;self.copied_bytes+=32768;self.view_bytes+=n as u64;
   }
   self.previous_view=n;
   self.process_ns+=t.elapsed().as_nanos();return Ok(())
  }
  self.copy_full(slot,full)?;let h=continuous::header_revision(full,3)?;let n=view::view_len(h);
  if n!=0{self.copy_view(slot,&mut overview[..n])?;}
  let t=Instant::now();sink(h,&[full],&overview[..n])?;self.process_ns+=t.elapsed().as_nanos();Ok(())
 }

 fn snapshot(&mut self)->R<Snapshot>{self.read_snapshot(true)}
 fn fast_snapshot(&mut self)->R<Snapshot>{self.read_snapshot(!self.fast_snapshot_on)}
 fn copy_full(&mut self,slot:usize,out:&mut[u8])->R<()>{if self.shift{
  let prev=(slot+1023)&1023;let pv=self.previous_view;let(a,rest)=out.split_at_mut(4352-pv);let(b,c)=rest.split_at_mut(pv);
  self.copy(0,prev*32768+28416+pv,a)?;
  if pv!=0{self.copy(1,prev*2048,b)?;}
  self.copy(0,slot*32768,c)?;
  self.previous_view=view::view_len(continuous::header_revision(out,3)?);
 }else{self.copy(0,slot*32768,out)?;}self.copied_bytes+=32768;Ok(())}
 fn copy_view(&mut self,slot:usize,out:&mut[u8])->R<()>{if self.shift{self.copy(0,slot*32768+28416,out)?;}else{self.copy(1,slot*2048,out)?;}self.view_bytes+=out.len()as u64;Ok(())}
 fn release(&mut self,run:u32,tail:u64)->R<()>{let phase=self.phase_on.then(Instant::now);self.give_back()?;
  // Split544 needs the preceding physical slot until the next full record has
  // been copied. A logical cursor may lead the hardware release by exactly one.
  // The network never participates. After STOP/DRAINED flush the held slot.
  let safe_tail=if self.shift&&self.active{tail.checked_sub(1).ok_or("split tail underflow")?}else{tail};
  let physical=self.r64(0x110);if safe_tail<physical{return Err("backward physical release".into())}
  if safe_tail>physical{self.w(0xe8,safe_tail as u32);self.w(0xec,(safe_tail>>32)as u32);self.w(0xf0,run);self.w(0x54,self.valid|64);}
  self.logical_tail=Some(tail);self.max_release_gap_us=self.max_release_gap_us.max(self.last_release.elapsed().as_micros()as u64);self.last_release=Instant::now();if let Some(t)=phase{self.release_ns+=t.elapsed().as_nanos();}Ok(())}
}
fn cpu()->f64{let mut r=std::mem::MaybeUninit::<libc::rusage>::zeroed();unsafe{libc::getrusage(libc::RUSAGE_SELF,r.as_mut_ptr());let r=r.assume_init();(r.ru_utime.tv_sec+r.ru_stime.tv_sec)as f64+(r.ru_utime.tv_usec+r.ru_stime.tv_usec)as f64/1e6}}
fn memory()->String{std::fs::read_to_string("/proc/self/status").unwrap_or_default().lines().filter(|s|s.starts_with("VmRSS:")||s.starts_with("VmHWM:")).collect::<Vec<_>>().join("\n")}
#[cfg(target_arch="arm")]unsafe extern "C"{fn obs_neon_copy(dst:*mut u8,src:*const u8,n:usize);fn obs_neon_read(p:*const u8,n:usize);}
fn full_read_without_sum(b:&[u8]){
 #[cfg(target_arch="arm")] unsafe{obs_neon_read(b.as_ptr(),b.len())};
 #[cfg(not(target_arch="arm"))] for x in b{unsafe{std::ptr::read_volatile(x);}}
}
fn save(c:&Config,name:&str,v:&Value)->R<()>{std::fs::write(c.output.join(name),serde_json::to_vec_pretty(v).unwrap()).map_err(|e|e.to_string())}
fn mark(io:&mut Hardware,reader:&view::PairReader,start:Instant)->R<Value>{let regs=io.counters()?;Ok(json!({"time":start.elapsed().as_secs_f64(),"cpu_seconds":cpu(),"memory":memory(),"regs":regs,"full_read":reader.read,"full_read_ns":reader.full_read_ns.to_string(),"view_own_ns":reader.view_own_ns.to_string(),"after_release_ns":reader.after_release_ns.to_string(),"metadata_read":reader.visited,"sync_calls":io.sync_calls,"process_ns":io.process_ns.to_string(),"phase_timing":io.phase_on,"snapshot_ns":io.snapshot_ns.to_string(),"release_ns":io.release_ns.to_string(),"actual_copy_bytes":io.actual_copy_bytes,"consumed":reader.consumed,"full_bytes":io.copied_bytes,"view_bytes":io.view_bytes,"sync_ns":io.sync_ns.to_string(),"copy_ns":io.copy_ns.to_string()}))}
fn interface_counters()->Value{json!({"monotonic_s":crate::delivery_runtime::mono(),"netdev":std::fs::read_to_string("/proc/net/dev").unwrap_or_default(),"snmp":std::fs::read_to_string("/proc/net/snmp").unwrap_or_default()})}
pub(crate) struct RunControl<'a>{
 pub generation:u64,
 pub stop:&'a AtomicBool,
 pub started:&'a dyn Fn(Header),
 pub route:&'a dyn Fn(u32,u64,u32,u32,u32)->R<Option<u32>>,
 pub check:&'a dyn Fn()->R<()>,
}
fn execute(c:&Config,control:Option<&RunControl<'_>>)->R<()>{
 if (c.trace_limit==0&&control.is_none())||c.trace_limit>400000||c.batch_limit==0||c.batch_limit>64||(c.direct||c.release_only)&&(!c.shift544||(c.release_only&&(c.mode!=0||c.check||c.save_ordinal.is_some()))) {return Err("strategy bounds: trace1..400000 (0 for managed control); header-only OFF without check/save; batch1..64".into())}
 if !(1..=1023).contains(&c.k)||c.mode>3||c.previous_mode>3||!c.seconds.is_finite()||c.seconds<=0.||c.seconds>120.||!c.warmup.is_finite()||c.warmup<0.||c.warmup>15.{return Err("bounded config".into())}
 if c.diagnostic_sync.is_some(){return discard::execute(c)}
 if c.owned_pipeline{return Err("full-copy experiment rejected; use immediate release reader".into())}
 if c.check||c.save_ordinal.is_some()||c.release_only{return Err("immediate release reader requires complete reads; offline checks use saved rings".into())}
 std::fs::create_dir_all(&c.output).map_err(|e|e.to_string())?;
 let mut io=Hardware::open(c)?;
 save(c,"memory.json",&json!({"full_dma":io.info.full,"view_dma":io.info.view,"full_len":io.info.full_len,"view_len":io.info.view_len,"cached":io.info.cached,"cache_line":io.info.cache_line,"mmap":"read-only MAP_SHARED, Linux DMA API","lifetime":"module pinned until reboot","shift544":c.shift544}))?;
 save(c,"before.json",&io.counters()?)?;
 if c.command=="capture"{
  io.prepare(c)?;io.start()?;std::thread::sleep(Duration::from_secs_f64(c.seconds));io.stop();let t=Instant::now();while {let s=io.snapshot()?;s.dma_busy||!s.drained||s.committed!=s.reserved}{if t.elapsed()>Duration::from_secs(3){return Err("capture STOP timeout".into())}std::thread::sleep(Duration::from_millis(1));}
  save(c,"unreleased.json",&io.counters()?)?;
  // Heavy whole-ring evidence is outside every rate window, before ANY release.
  for(r,n)in[(0,0x2000000),(1,0x200000)]{let mut b=vec![0;n];io.copy(r,0,&mut b)?;std::fs::write(c.output.join(if r==0{"full-ring.bin"}else{"view-ring.bin"}),b).map_err(|e|e.to_string())?;io.give_back()?;}
  return Ok(())
 }
 if c.command!="rate"&&c.command!="drain"{return Err("command must be capture, drain, rate".into())}
 if c.discard_cold{let s=io.snapshot()?;if s.running||s.dma_busy||s.released!=0||s.committed==0{return Err("cold discard only stopped retained ordinal0".into())}io.release(s.cookie,1)?;}
 let cold=if c.command=="rate"{io.prepare(c)?;io.start()?;io.cold_split(control.filter(|_|c.scan))?}else{Value::Null};
 save(c,"cold-start.json",&cold)?;
 let mut reader=view::PairReader::attach(io.snapshot()?)?;
 let start=Instant::now();let mut deadline=c.warmup+c.seconds;let mut begin=None;let mut end=None;let mut records=Vec::<[u64;7]>::with_capacity(c.trace_limit);let mut trace_omitted=0u64;let mut max_service_us=0u64;let mut max_poll_us=0u64;let mut last_poll=Instant::now();let mut route_failure=None;
 let socket=if let Some(dest)=&c.udp{let sock=std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e|e.to_string())?;sock.connect(dest).map_err(|e|e.to_string())?;sock.set_nonblocking(true).map_err(|e|e.to_string())?;Some(sock)}else{None};
 let record_bytes=(if control.is_some(){64}else{56})+[0,512,1024,2048][c.mode as usize];
 let network=std::cell::RefCell::new(UdpBatch::new(record_bytes));let mut status_due=Instant::now();let mut begin_link=Value::Null;let mut end_link=Value::Null;let mut begin_network=serde_json::Value::Null;let mut end_network=serde_json::Value::Null;
 let announced=std::cell::Cell::new(false);let draining=std::cell::Cell::new(false);
 let mut sink=|h:Header,overview:&[u8]|->R<()>{
  if let Some(q)=control{if draining.get(){return Ok(())}if !announced.replace(true){(q.started)(h);}}
  if let Some(sock)=&socket{if !overview.is_empty(){if overview.len()!=[0,512,1024,2048][c.mode as usize]{return Err("delivery VIEW geometry differs from active descriptor".into())}let mut net=network.borrow_mut();net.start_record();net.bytes.extend_from_slice(if control.is_some(){b"GVIEW4\0\0"}else{b"GVIEW3\0\0"});for x in [h.ordinal,h.first_seq,h.epoch,h.first_tick,h.k as u64,h.flags as u64]{net.bytes.extend_from_slice(&x.to_le_bytes());}if let Some(q)=control{net.bytes.extend_from_slice(&q.generation.to_le_bytes());}net.bytes.extend_from_slice(overview);net.pending+=1;if net.pending==net.max_records as u64&&!c.send_after_release{net.flush(sock);}}}
  if c.trace_limit!=0{if records.len()<c.trace_limit{records.push([h.ordinal,h.first_seq,h.epoch,h.first_tick,h.k as u64,h.flags as u64,start.elapsed().as_nanos()as u64]);}else{trace_omitted+=1;}}Ok(())
 };
 loop{
  if c.command=="rate"&&begin.is_none()&&start.elapsed().as_secs_f64()>=c.warmup{begin_link=interface_counters();begin_network=network.borrow().counts();begin=Some(mark(&mut io,&reader,start)?);deadline=begin.as_ref().unwrap()["time"].as_f64().unwrap()+c.seconds;reader.high_water=0;io.physical_high_water=0;max_service_us=0;max_poll_us=0;io.max_release_gap_us=0;io.last_release=Instant::now();}
  if c.command=="rate"&&(control.map_or(start.elapsed().as_secs_f64()>=deadline||STOP.load(Ordering::Relaxed),|q|q.stop.load(Ordering::Acquire))){end_link=interface_counters();end_network=network.borrow().counts();end=Some(mark(&mut io,&reader,start)?);break}
  if let Some(q)=control.filter(|_|c.scan){if let Err(e)=io.service_route(q){route_failure=Some(e);break;}}
  if status_due.elapsed()>=Duration::from_millis(100){
   if let Some(q)=control{if let Err(e)=(q.check)(){route_failure=Some(e);break;}}
   if let Ok(v)=mark(&mut io,&reader,start){*STATUS.lock().unwrap()=v;}status_due=Instant::now();
  }
  let t=Instant::now();max_poll_us=max_poll_us.max(t.duration_since(last_poll).as_micros()as u64);last_poll=t;
  let s=reader.service_released(&mut io,full_read_without_sum,&mut sink)?;if let Some(sock)=&socket{network.borrow_mut().flush_due(sock);}max_service_us=max_service_us.max(t.elapsed().as_micros()as u64);
  if c.command=="drain"&&s.committed==s.released{break}
  if s.committed==s.released{std::hint::spin_loop();} // CSR has no completion fd/IRQ; no timer delay.
 }
 let measured=json!({"high_water":io.physical_high_water,"logical_high_water":reader.high_water,"max_service_us":max_service_us,"max_service_interval_us":max_poll_us,"max_release_interval_us":io.max_release_gap_us});
 draining.set(control.is_some());io.stop();let stop=Instant::now();loop{let s=reader.service_released(&mut io,full_read_without_sum,&mut sink)?;if let Some(sock)=&socket{network.borrow_mut().flush(sock);}if s.drained&&!s.dma_busy&&s.released==s.committed{break}if stop.elapsed()>Duration::from_secs(5){return Err("reader stop/drain timeout; allocation remains pinned".into())}}
 let s=io.snapshot()?;io.release(s.cookie,s.committed)?;
 if let Some(err)=route_failure{return Err(err);}
 drop(sink);
 let after=io.counters()?;if c.trace_limit!=0{let mut f=File::create(c.output.join("records.u64le")).map_err(|e|e.to_string())?;for row in &records{for x in row{f.write_all(&x.to_le_bytes()).map_err(|e|e.to_string())?;}}}
 if c.save_ring{
  // Explicit diagnostic export after STOP/DRAINED; RF/SPI ownership remains
  // with this runtime until the immutable snapshot has been saved.
  for(region,size,name)in[(0,io.info.full_len as usize,"full-ring.bin"),(1,io.info.view_len as usize,"view-ring.bin")]{
   let mut bytes=vec![0u8;size];io.copy(region,0,&mut bytes)?;io.give_back()?;
   std::fs::write(c.output.join(name),bytes).map_err(|e|e.to_string())?;
  }
 }
 if io.pressure_on{save(c,"pressure.json",&json!({"columns":["board_monotonic_ns","reserved","committed","physical_released"],"period_min_ms":2,"omitted":io.pressure_omitted,"samples":io.pressure}))?;}
 let result=json!({"begin":begin,"end":end,"measured":measured,"after":after,"cold_start":cold,"full_read_total":reader.read,"consumed_total":reader.consumed,"trace_rows":records.len(),"trace_omitted":trace_omitted,"consumer":"ALL4096 volatile read, release, then VIEW send and trace; no full queue or checksum","network_spectrum":c.udp.is_some(),"udp_sent":network.borrow().sent,"udp_drop":network.borrow().dropped,"udp_datagrams":network.borrow().datagrams,"udp_batch_max":network.borrow().max_records,"begin_interface":begin_link,"end_interface":end_link,"begin_network":begin_network,"end_network":end_network,"strategy":{"read_without_sum":c.read_without_sum,"send_after_release":c.send_after_release,"reuse_owned_cache":c.reuse_owned_cache,"batch_sync":c.batch_sync,"direct":c.direct,"release_only":c.release_only,"batch_limit":c.batch_limit,"aligned":c.aligned,"neon":c.neon},"memory_final":memory(),"seconds_total":start.elapsed().as_secs_f64()});save(c,"result.json",&result)?;println!("{}",result);Ok(())
}

static STATUS:std::sync::Mutex<Value>=std::sync::Mutex::new(Value::Null);
pub fn status()->Value{STATUS.lock().unwrap().clone()}
pub fn stop(){STOP.store(true,Ordering::Release);}
pub fn run_json(data:&[u8])->R<()>{STOP.store(false,Ordering::Release);let c:Config=serde_json::from_slice(data).map_err(|e|e.to_string())?;execute(&c,None)}
pub(crate) fn run_controlled(data:&[u8],q:&RunControl<'_>)->R<()>{let c:Config=serde_json::from_slice(data).map_err(|e|e.to_string())?;execute(&c,Some(q))}

impl UdpBatch {
 fn counts(&self)->Value{json!({"sent":self.sent,"dropped":self.dropped,"datagrams":self.datagrams,"send_ns":self.send_ns.to_string(),"phase_timing":self.phase_on,"coalesce_us":self.coalesce_us,"max_records":self.max_records,"max_wait_ns":self.max_wait_ns.to_string()})}
}
