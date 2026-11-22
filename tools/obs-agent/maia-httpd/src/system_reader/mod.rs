//! Explicit, bounded r3 board exerciser. No PL load, RF retune, flash or network.
mod continuous;
mod view;
mod discard;
mod view_batch;
mod sweep_delivery;
mod progress;
mod obn3;
#[cfg(feature="scan-stream-bench")]
mod roi;
#[cfg(feature="scan-stream-bench")]
mod roi_stream;
mod timing;
#[cfg(feature="scan-stream-bench")]
pub(crate) use roi_stream::Map as RoiMap;
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct RoiRequest {pub center_hz:i64,pub generation:u64}
pub(crate) use progress::{Execution, LOST as PROGRESS_LOST};
pub(crate) use sweep_delivery::SweepSpec;
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
impl SweepSpec {
 pub(crate) fn from_descriptor(desc:&Value,mode:&str)->R<Option<Self>>{
  if desc["sweep_wire_version"]!=1{return Ok(None);}
  let rows=desc["positions"].as_array().ok_or("sweep map positions")?;
  let positions=rows.iter().map(|row|{
   if row["transition_only"]==true{return Ok(None);}
   match row["sweep_phase"].as_str(){Some("A")=>Ok(Some(0)),Some("B")=>Ok(Some(1)),_=>Err("missing sweep phase")}
  }).collect::<Result<Vec<_>,_>>()?;
  let version=desc["version"].as_u64().ok_or("missing sweep map version")?;
  Self::from_layout(version,positions,mode).map(Some)
 }
}
#[derive(Deserialize,Debug)]#[serde(deny_unknown_fields)]struct Config{
 boot_id:String,output:PathBuf,command:String,#[serde(default="kdefault")]k:u32,
 #[serde(default)]mode:u32,#[serde(default)]peak:bool,#[serde(default)]scan:bool,
 #[serde(default)]stream_config:Option<u32>,
 #[serde(default)]bench_obn3_layout:bool,
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
struct Hardware{stream_format:Option<obn3::Format>,bench_obn3_layout:bool,previous_full_len:usize,managed_view_only:bool,phase_on:bool,snapshot_ns:u128,release_ns:u128,snapshots:u64,empty_snapshots:u64,release_commands:u64,idle_wait_us:u64,sync_bytes:u64,fast_snapshot_on:bool,pressure_on:bool,pressure_due:Instant,pressure:Vec<[u64;4]>,pressure_omitted:u64,_lease:File,device:File,regs:Map,full:Map,overview:Map,info:Info,valid:u32,active:bool,shift:bool,logical_tail:Option<u64>,physical_high_water:u64,previous_view:usize,ranges:Vec<(u32,u32,u32)>,copied_bytes:u64,view_bytes:u64,max_release_gap_us:u64,last_release:Instant,sync_ns:u128,copy_ns:u128,batch_sync:bool,direct:bool,release_only:bool,batch_limit:usize,covered:Vec<(u32,u32,u32)>,sync_calls:u64,process_ns:u128,actual_copy_bytes:u64,reuse_owned_cache:bool,retained_synced:Option<u64>,sync_reused_bytes:u64}
impl Hardware{
 fn open(c:&Config,managed_view_only:bool)->R<Self>{
  if c.bench_obn3_layout && !cfg!(feature="scan-stream-bench") {
   return Err("bench_obn3_layout requires the scan-stream-bench feature".into());
  }
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
  let mut h=Self{stream_format:None,bench_obn3_layout:c.bench_obn3_layout,previous_full_len:continuous::BYTES,managed_view_only,phase_on:std::env::var("OBS_PHASE_TIMING").as_deref()==Ok("1"),snapshot_ns:0,release_ns:0,snapshots:0,empty_snapshots:0,release_commands:0,idle_wait_us:0,sync_bytes:0,fast_snapshot_on:std::env::var("OBS_FAST_SNAPSHOT").as_deref()!=Ok("0"),pressure_on:std::env::var("OBS_PRESSURE_TRACE").as_deref()==Ok("1"),pressure_due:Instant::now(),pressure:Vec::with_capacity(if std::env::var("OBS_PRESSURE_TRACE").as_deref()==Ok("1"){65536}else{0}),pressure_omitted:0,_lease:lease,device,regs,full,overview,info,valid:0,active:false,shift:c.shift544||c.bench_obn3_layout,logical_tail:None,physical_high_water:0,previous_view:[0,512,1024,2048][c.previous_mode as usize],ranges:Vec::with_capacity(4),copied_bytes:0,view_bytes:0,max_release_gap_us:0,last_release:Instant::now(),sync_ns:0,copy_ns:0,batch_sync:c.batch_sync,direct:c.direct,release_only:c.release_only,batch_limit:c.batch_limit,covered:Vec::with_capacity(4),sync_calls:0,process_ns:0,actual_copy_bytes:0,reuse_owned_cache:c.reuse_owned_cache,retained_synced:None,sync_reused_bytes:0};
  if (h.r(0x1c0),h.r(0x1c4),h.r(0x1c8))!=(1024,32768,2048) {
   return Err("DMA ring geometry mismatch".into());
  }
  match (h.r(0x50),h.r(0xe4)) {
   (obn3::ID,4) if h.r(0xe0)&0x60==0x60 => {
    if c.shift544 || c.discard_cold || c.diagnostic_sync.is_some() {
     return Err("OBN3 requires separate full and VIEW slots".into());
    }
    h.stream_format=Some(obn3::Format::from_config(h.r(0x34c))?);
    h.managed_view_only=false;
   }
   (continuous::ABI,3) if c.stream_config.is_none() && !c.bench_obn3_layout && !(managed_view_only && c.mode==0) => {}
   _ => return Err("spectrum ABI/revision/configuration mismatch".into()),
  }
  Ok(h)
 }
 fn r(&self,o:usize)->u32{unsafe{std::ptr::read_volatile(self.regs.ptr.add(o).cast())}}
 fn w(&self,o:usize,v:u32){unsafe{std::ptr::write_volatile(self.regs.ptr.add(o).cast(),v)};compiler_fence(Ordering::SeqCst);}
 fn r64(&self,o:usize)->u64{self.r(o)as u64|((self.r(o+4)as u64)<<32)}
 fn live64(&self,o:usize)->u64{loop{let hi=self.r(o+4);let lo=self.r(o);if hi==self.r(o+4){return(hi as u64)<<32|lo as u64}}}
 fn sync(&mut self,region:u32,offset:u32,len:u32,cpu:bool)->R<()>{let t=Instant::now();let s=Sync{region,offset,len,for_cpu:cpu as u32};if unsafe{libc::ioctl(self.device.as_raw_fd(),0x4f02,&s)}!=0{return Err(format!("DMA sync: {}",std::io::Error::last_os_error()))}compiler_fence(Ordering::SeqCst);self.sync_ns+=t.elapsed().as_nanos();self.sync_calls+=1;self.sync_bytes+=u64::from(len);Ok(())}
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
  if self.stream_format.is_some(){
   let config=c.stream_config.unwrap_or(2);
   obn3::Format::from_config(config)?;
   self.w(0x348,config);
   if self.r(0x348)!=config{return Err("OBN3 staged configuration readback".into());}
  }
  self.w(0x1b8,0);self.w(0x1a8,self.info.full as u32);self.w(0x1ac,self.info.view as u32);self.w(0x1b0,self.info.full_len);self.w(0x1b4,self.info.view_len);self.w(0x1a4,c.mode);
  self.w(0x164,c.scan as u32);self.w(0x168,c.settling);self.w(0x16c,0);self.w(0x178,0);self.w(0x18c,1);self.w(0x20,c.k*2|if c.peak{1<<15}else{0});self.w(0x58,100);self.w(0x5c,0);self.w(0x1b8,0x56494557);
  if self.r(0x1bc)!=1||self.r(0x1a8)!=self.info.full as u32||self.r(0x1ac)!=self.info.view as u32{return Err("staged memory validation".into())}Ok(())
 }
 fn start(&mut self)->R<()>{let before=self.snapshot()?.cookie;let staged=self.stream_format.map(|_|self.r(0x348));self.valid=1;self.active=true;self.w(0x54,3);let s=self.snapshot()?;if !s.running||s.cookie!=before+1{return Err("START rejected".into())}
  if let Some(staged)=staged {
   let effective=self.r(0x34c);
   if effective!=staged{return Err("OBN3 configuration was not latched at START".into());}
   self.stream_format=Some(obn3::Format::from_config(effective)?);
  }
  self.last_release=Instant::now();Ok(())}
 fn service_route(&mut self,q:&RunControl)->R<()>{
  if self.r(0x228)&8==0{return Ok(())}
  let id=self.r(0x240);
  let epoch=self.live64(0x24c);
  let version=self.r(0x22c);
  if (q.route)(id,epoch,version,self.r(0x234),self.r(0x244))?==Some(id)
     && self.r(0x240)==id && self.r(0x228)&8!=0 && self.r(0x22c)==version && self.live64(0x24c)==epoch{
   self.w(0x218,id);self.w(0x220,2);
   (q.route_released)(id,epoch,version);
  }
  Ok(())
 }
 fn cold_split(&mut self,control:Option<&RunControl>,c:&Config)->R<Value>{
  #[cfg(feature="scan-stream-bench")]
  if self.bench_obn3_layout{return self.bind_obn3_prefix(control,c);}
  let initial=self.snapshot()?;
  if !self.shift||control.is_none()&&(initial.cookie!=1||initial.released!=0){return Ok(Value::Null)}
  let start=Instant::now();
  let mut sampled=start;
  let mut progress=progress::Progress::new(c.k,c.scan,c.settling,if c.managed_tx{self.r(0x330)}else{0});
  loop{
   if let Some(q)=control{
    if q.stop.load(Ordering::Acquire){return Err("scan cancelled during first visit".into())}
    if c.scan{self.service_route(q)?;(q.check)()?;}
   }
   let map=self.map_stamp();
   let s=self.snapshot()?;
   if !s.running||s.fault||s.cookie!=initial.cookie||s.released!=initial.released||s.reserved-s.released>=view::SLOTS as u64{
    return Err("cold split lost initial ownership".into())
   }
   if control.is_some() && s.committed>initial.released {
    if let Some((width,h))=self.start_view_prefix(c,initial.released)?{
     self.previous_view=width;
     return Ok(json!({"run":initial.cookie,"prefix_view_bytes":width,
                     "ordinal":h.ordinal,"epoch":h.epoch,"first_tick":h.first_tick}))
    }
    if s.committed-initial.released>=2 && (initial.cookie!=1||initial.released!=0) {
     return Err("split544 cannot identify the first ordinal in the committed START prefix".into())
    }
   }
   if initial.cookie==1 && initial.released==0 && s.committed>=2{break}
   if STOP.load(Ordering::Relaxed){return Err("cold split cancelled".into())}
   if let Some(q)=control.filter(|_|c.scan) {
    if sampled.elapsed()>=progress::POLL {self.check_progress(&mut progress,start,&s,q,None,Some(map))?;sampled=Instant::now();}
   }else if start.elapsed()>=Duration::from_secs(1){return Err("cold split header deadline".into())}
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
 #[cfg(feature="scan-stream-bench")]
 fn bind_obn3_prefix(&mut self,control:Option<&RunControl>,c:&Config)->R<Value>{
  let initial=self.snapshot()?;
  let cold=initial.cookie==1 && initial.released==0;
  let format=self.stream_format.ok_or("OBN3 layout without OBN3 ABI")?;
  let start=Instant::now();
  loop {
   if STOP.load(Ordering::Relaxed)||control.is_some_and(|q|q.stop.load(Ordering::Acquire)){
    return Err("OBN3 RAM prefix binding cancelled".into());
   }
   if let Some(q)=control.filter(|_|c.scan){self.service_route(q)?;(q.check)()?;}
   let s=self.fast_snapshot()?;
   if !s.running||s.fault||s.cookie!=initial.cookie||s.released!=initial.released
      ||s.reserved-s.released>=view::SLOTS as u64 {return Err("OBN3 RAM prefix no longer owned".into());}
   let mut first=initial.released;
   let mut binding=None;
   if s.committed>first{binding=self.find_obn3_prefix(format,first,c)?;}
   if binding.is_none() && cold && s.committed>=2 {
    first=1;
    binding=self.find_obn3_prefix(format,first,c)?;
   }
   if let Some((length,view_width,header))=binding {
    self.previous_full_len=length;
    self.previous_view=view_width;
    if first!=initial.released {self.release(initial.cookie,first)?;}
    return Ok(json!({"run":initial.cookie,"ordinal":first,"epoch":header.epoch,
        "previous_active_bytes":length,"previous_view_bytes":view_width,
        "unrecoverable_cold_records":first-initial.released}));
   }
   if s.committed-initial.released>=2 || start.elapsed()>OBN3_PREFIX_TIMEOUT{
    return Err("could not bind the observed OBN3 RAM prefix".into());
   }
   std::thread::sleep(OBN3_PREFIX_POLL);
  }
 }
 #[cfg(feature="scan-stream-bench")]
 fn find_obn3_prefix(&mut self,format:obn3::Format,ordinal:u64,c:&Config)->R<Option<(usize,usize,Header)>>{
  let slot=ordinal.wrapping_sub(1) as usize&(view::SLOTS-1);
  for previous_active in [obn3::COMPACT_BYTES,continuous::BYTES] {
   for previous_view in [0,512,1024,2048] {
    let offset=slot*continuous::BYTES+previous_active-OBN3_RAM_SHIFT_BYTES+previous_view;
    let len=format.prefix_bytes();
    self.ensure_sync(0,offset as u32,len as u32)?;
    let prefix=unsafe{std::slice::from_raw_parts(self.full.ptr.add(offset),len)};
    if let Ok(h)=format.header(prefix) && h.ordinal==ordinal && h.k==c.k && (h.flags>>3)&3==c.mode {
     return Ok(Some((previous_active,previous_view,h)));
    }
   }
  }
  Ok(None)
 }
 fn start_view_prefix(&mut self,c:&Config,ordinal:u64)->R<Option<(usize,Header)>>{
  // START advances the cookie but keeps the cumulative ordinal and physical
  // write slot. Commit of the first record protects its split lookbehind.
  let prev=ordinal.wrapping_sub(1) as usize & (view::SLOTS-1);
  for width in [0,512,1024,2048]{
   let offset=prev*32768+28416+width;
   self.ensure_sync(0,offset as u32,512)?;
   let bytes=unsafe{std::slice::from_raw_parts(self.full.ptr.add(offset),512)};
   if let Ok(h)=continuous::header_prefix(bytes,3)
      && h.ordinal==ordinal && h.k==c.k && (h.flags>>3)&3==c.mode
   {return Ok(Some((width,h)))}
  }
  Ok(None)
 }
 fn map_stamp(&self)->(u32,u64){(self.r(0x22c),self.live64(0x238))}
 fn check_progress(&self,progress:&mut progress::Progress,start:Instant,s:&Snapshot,q:&RunControl,
   execution:Option<Execution>,map:Option<(u32,u64)>)->R<()>{
  let mux=self.r(0x228);
  let epoch=if map.is_some(){self.live64(0x180)}else{self.live64(0x198)};
  if map.is_some_and(|m| m!=self.map_stamp()) || s.cookie!=self.r(0x150){
   (q.progress)(Value::Null);
   return Ok(());
  }
  let observed=execution.filter(|e| e.matches(s.cookie,map,epoch));
  let injected=q.bench_stall || q.bench_stall_after_next && observed.is_some_and(|e|e.generation!=q.generation);
  let counts=if injected {[0;3]}else{[s.finished,s.committed,s.dropped]};
  let state=progress.observe(start.elapsed(),counts,s.running && !s.fault,
   mux&8!=0,s.reserved.saturating_sub(s.released)>=1023);
  let sample=json!({"state":state.as_str(),"generation":observed.map(|e|e.generation),"run":s.cookie,
   "arm_ns":crate::delivery_runtime::mono_ns().to_string(),
   "budget_us":progress.budget().as_micros() as u64,"input_seq":s.input_seq,"formed":s.finished,"committed":s.committed,
   "released":s.released,"dropped":s.dropped,"epoch":epoch,"version":map.map(|m|m.0).or(observed.map(|e|e.version)),
   "map_epoch":map.map(|m|m.1).or(observed.map(|e|e.anchor)),"mux_status":mux,"test_injected":injected});
  if state==progress::State::Stalled && !q.stop.load(Ordering::Acquire){
   (q.stalled)(sample);
   return Err(progress::LOST.into());
  }
  (q.progress)(if observed.is_some(){sample}else{Value::Null});
  Ok(())
 }
 fn stop(&mut self){self.valid=0;self.w(0x54,4);self.active=false;}
 fn release_drained(&mut self,run:u32,tail:u64)->R<()> {
  let split=std::mem::replace(&mut self.shift,false);
  let result=self.release(run,tail);
  self.shift=split;
  result
 }
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
  let value=Snapshot{committed:self.r64(0x100),reserved,released:logical,input_seq:if details{self.r64(0x118)}else{0},finished:if details{self.r64(0x128)}else{0},selected:if details{self.r64(0x130)}else{0},unselected:if details{self.r64(0x138)}else{0},dirty_drop:if details{self.r(0x148)}else{0},stop_drop:if details{self.r(0x14c)}else{0},stop_input_limit:if details{self.r64(0x158)}else{0},cookie:self.r(0x150),dropped:if details{self.r(0x140)as u64+self.r(0x144)as u64+self.r(0x160)as u64}else{0},running:status&1!=0,dma_busy:status&2!=0,drained:status&16!=0,fault:status&4!=0};self.snapshots+=1;if value.committed==value.released{self.empty_snapshots+=1;}if let Some(t)=phase{self.snapshot_ns+=t.elapsed().as_nanos();}Ok(value)}
 fn fld(&self)->Value{loop{let v=self.r(0x22c);let epoch=self.live64(0x238);let pass=self.r(0x268);let status=self.r(0x228);let loads=self.r(0x254);let late=self.r(0x258);let errors=self.r(0x25c);let current_pass=self.r(0x230);let position=self.r(0x234);if v==self.r(0x22c){return json!({"version":v,"map_epoch":epoch,"map_pass":pass,"status":status,"loads":loads,"late":late,"errors":errors,"pass":current_pass,"position":position,"tx_changes":self.r(0x260),"transactions":self.r(0x264)})}}}
 fn counters(&mut self)->R<Value>{let s=self.snapshot()?;Ok(json!({"fld":self.fld(),"committed":s.committed,"reserved":s.reserved,"released":self.r64(0x110),"logical_released":s.released,"input_seq":s.input_seq,"formed":s.finished,"selected":s.selected,"unselected":s.unselected,"bank_drop":self.r(0x140),"ring_drop":self.r(0x144),"dirty_drop":s.dirty_drop,"stop_drop":s.stop_drop,"dma_drop":self.r(0x160),"run":s.cookie,"status":self.r(0x154),"visit_epoch":self.live64(0x180),"visit_status":self.r(0x188),"first_epoch":self.live64(0x198)}))}
}
impl Drop for Hardware{fn drop(&mut self){if self.active{self.stop();}}}
impl PairIo for Hardware{
 fn batch_limit(&self)->usize{self.batch_limit}
 fn phase_timing(&self)->bool{self.phase_on}
 fn full_read(&self)->bool{!self.release_only&&!self.managed_view_only}
 fn owned_full_bytes(&self)->usize{self.stream_format.map_or(0,obn3::Format::active_bytes)}
 fn prepare_prefix(&mut self,first:u64,end:u64)->R<()>{
  self.covered.clear();if !self.batch_sync||self.release_only||self.managed_view_only||self.stream_format.is_some()||first==end{return Ok(())}
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
  if let Some(format)=self.stream_format {
   let offset=slot*continuous::BYTES;
   let length=format.active_bytes();
   if cfg!(feature="scan-stream-bench") && self.bench_obn3_layout {
    let previous=slot.wrapping_sub(1)&(view::SLOTS-1);
    let prev_view=self.previous_view;
    let prefix=OBN3_RAM_SHIFT_BYTES-prev_view;
    let (head,rest)=full[..length].split_at_mut(prefix);
    let (middle,tail)=rest.split_at_mut(prev_view);
    self.copy(0,previous*continuous::BYTES+self.previous_full_len-prefix,head)?;
    if prev_view!=0{self.copy(1,previous*view::VIEW_BYTES,middle)?;}
    self.copy(0,offset,tail)?;
   }else{self.copy(0,offset,&mut full[..length])?;}
   let header=format.header(&full[..length])?;
   let n=view::view_len(header);
   let (region,view_offset,base)=if cfg!(feature="scan-stream-bench") && self.bench_obn3_layout {
     (0,offset+length-OBN3_RAM_SHIFT_BYTES,self.full.ptr)}
     else {(1,slot*view::VIEW_BYTES,self.overview.ptr)};
   if n!=0{self.ensure_sync(region,view_offset as u32,n as u32)?;}
   let payload=unsafe{std::slice::from_raw_parts(base.add(view_offset),n)};
   self.previous_view=n;
   self.previous_full_len=length;
   self.copied_bytes+=length as u64;
   self.view_bytes+=n as u64;
   return sink(header,&[],payload);
  }
  if self.managed_view_only {
   let (header_offset,region,view_offset)=view::delivery_offsets(slot,self.shift,self.previous_view);
   self.ensure_sync(0,header_offset as u32,512)?;
   let prefix=unsafe{std::slice::from_raw_parts(self.full.ptr.add(header_offset),512)};
   let h=continuous::header_prefix(prefix,3)?;
   let n=view::view_len(h);
   if n!=0{self.ensure_sync(region,view_offset as u32,n as u32)?;}
   let base=if region==0{self.full.ptr}else{self.overview.ptr};
   let payload=unsafe{std::slice::from_raw_parts(base.add(view_offset),n)};
   let t=Instant::now();
   sink(h,&[],payload)?;
   self.process_ns+=t.elapsed().as_nanos();
   self.view_bytes+=n as u64;
   self.previous_view=n;
   return Ok(())
  }
  if self.direct||self.release_only{
   if !self.shift{return Err("direct/header-only requires shift544".into())}
   let pv=self.previous_view;let prev=(slot+1023)&1023;let a=prev*32768+28416+pv;let b=slot*32768;
   let prefix_len=if self.release_only{512}else{4352-pv};
   self.ensure_sync(0,a as u32,prefix_len as u32)?;
   let prefix=unsafe{std::slice::from_raw_parts(self.full.ptr.add(a),prefix_len)};
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
  let safe_tail=if self.shift{tail.saturating_sub(1)}else{tail};
  let physical=self.r64(0x110);if safe_tail<physical{return Err("backward physical release".into())}
  if safe_tail>physical{self.release_commands+=1;self.w(0xe8,safe_tail as u32);self.w(0xec,(safe_tail>>32)as u32);self.w(0xf0,run);self.w(0x54,self.valid|64);}
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
fn mark(io:&mut Hardware,reader:&view::PairReader,start:Instant)->R<Value>{let regs=io.counters()?;Ok(json!({"time":start.elapsed().as_secs_f64(),"cpu_seconds":cpu(),"memory":memory(),"regs":regs,"full_read":reader.read,"stream_config":io.stream_format.map(|_|io.r(0x34c)),"managed_view_only":io.managed_view_only,"full_read_ns":reader.full_read_ns.to_string(),"view_own_ns":reader.view_own_ns.to_string(),"after_release_ns":reader.after_release_ns.to_string(),"metadata_read":reader.visited,"sync_calls":io.sync_calls,"sync_bytes":io.sync_bytes,
"payload_read_bytes":io.copied_bytes,"metadata_span_bytes":if io.managed_view_only{reader.visited*512}else{0},
"owned_view_bytes":io.view_bytes,"snapshot_calls":io.snapshots,"empty_snapshots":io.empty_snapshots,"release_commands":io.release_commands,"managed_idle_wait_us":if io.managed_view_only{io.idle_wait_us}else{0},"process_ns":io.process_ns.to_string(),"phase_timing":io.phase_on,"snapshot_ns":io.snapshot_ns.to_string(),"release_ns":io.release_ns.to_string(),"actual_copy_bytes":io.actual_copy_bytes,"consumed":reader.consumed,"full_bytes":io.copied_bytes,"view_bytes":io.view_bytes,"sync_ns":io.sync_ns.to_string(),"copy_ns":io.copy_ns.to_string()}))}
fn interface_counters()->Value{json!({"monotonic_s":crate::delivery_runtime::mono(),"netdev":std::fs::read_to_string("/proc/net/dev").unwrap_or_default(),"snmp":std::fs::read_to_string("/proc/net/snmp").unwrap_or_default()})}
pub(crate) fn view_bins(header: Header) -> usize {
 view::view_len(header)
}

pub(crate) struct RunControl<'a>{
 pub generation:u64,
 pub version:u32,
 pub stop:&'a AtomicBool,
 pub started:&'a dyn Fn(Header,Execution),
 pub trace:&'a dyn Fn(&'static str),
 pub route:&'a dyn Fn(u32,u64,u32,u32,u32)->R<Option<u32>>,
 pub route_released:&'a dyn Fn(u32,u64,u32),
 pub check:&'a dyn Fn()->R<()>,
 pub progress:&'a dyn Fn(Value),
 pub stalled:&'a dyn Fn(Value),
 pub bench_stall:bool,
 pub bench_stall_after_next:bool,
 pub sweep:Option<&'a SweepSpec>,
 pub next:Option<NextView<'a>>,
 #[cfg(feature="scan-stream-bench")]
 pub roi:Option<RoiControl<'a>>,
}
#[cfg(feature="scan-stream-bench")]
pub(crate) struct RoiControl<'a>{
 pub peer:std::net::SocketAddr,
 pub full_scan:bool,
 pub initial:RoiMap,
 pub next:Option<RoiMap>,
 pub request:&'a dyn Fn()->Option<RoiRequest>,
}
// RAM uses the observed 544-beat displacement. The ABI stride stays 32768 bytes.
const OBN3_RAM_SHIFT_BYTES:usize=544*8;
#[cfg(feature="scan-stream-bench")]
const OBN3_PREFIX_TIMEOUT:Duration=Duration::from_secs(8);
#[cfg(feature="scan-stream-bench")]
const OBN3_PREFIX_POLL:Duration=Duration::from_micros(50);
pub(crate) struct NextView<'a>{
 pub generation:u64,
 pub version:u32,
 pub sweep:&'a SweepSpec,
 pub anchor:&'a dyn Fn()->Option<u64>,
 pub started:&'a dyn Fn(Header,Execution),
}
fn view_idle_wait(k:u32)->Duration {
 // At 61.44 MS/s one group spans K*4096 samples. Check the ring
 // again after at most one eighth of a group (and at most 64 us).
 // This only applies after an empty snapshot, never to a ready prefix.
 Duration::from_micros((u64::from(k)*4096*1_000_000/(8*61_440_000)).min(64))
}

fn histogram(h:timing::Histogram)->Value {
 json!({"bins_upper_ns_pow2":h.bins,"max_ns":h.max_ns})
}

fn udp_socket_queue(socket:&std::net::UdpSocket)->Option<i32> {
 let mut value=0i32;
 (unsafe{libc::ioctl(socket.as_raw_fd(),libc::TIOCOUTQ,&mut value)}==0).then_some(value)
}

fn execute(c:&Config,control:Option<&RunControl<'_>>)->R<()>{
 if (c.trace_limit==0&&control.is_none())||c.trace_limit>400000||c.batch_limit==0||c.batch_limit>64||(c.direct||c.release_only)&&(!c.shift544||(c.release_only&&(c.mode!=0||c.check||c.save_ordinal.is_some()))) {return Err("strategy bounds: trace1..400000 (0 for managed control); header-only OFF without check/save; batch1..64".into())}
 if !(1..=1023).contains(&c.k)||c.mode>3||c.previous_mode>3||!c.seconds.is_finite()||c.seconds<=0.||c.seconds>120.||!c.warmup.is_finite()||c.warmup<0.||c.warmup>15.{return Err("bounded config".into())}
 if c.diagnostic_sync.is_some(){return discard::execute(c)}
 if c.owned_pipeline{return Err("full-copy experiment rejected; use immediate release reader".into())}
 if c.check||c.save_ordinal.is_some()||c.release_only{return Err("immediate release reader requires complete reads; offline checks use saved rings".into())}
 std::fs::create_dir_all(&c.output).map_err(|e|e.to_string())?;
 let mut io=Hardware::open(c,control.is_some())?;
 #[cfg(feature="scan-stream-bench")]
 let stream002=io.stream_format.is_some();
 #[cfg(feature="scan-stream-bench")]
 let roi_control=control.and_then(|q|q.roi.as_ref());
 #[cfg(feature="scan-stream-bench")]
 let zoom=std::cell::RefCell::new(if let Some(config)=roi_control {
  if !c.scan || !stream002 || c.mode != 0 { return Err("compact TCP requires an active OBN3 scan with VIEW OFF".into()); }
  let mut stream=roi_stream::Stream::open(config.peer,config.initial.clone(),config.next.clone(),config.full_scan)?;
  stream.update((config.request)())?;
  Some(stream)
 }else{None});
 let managed_view_only=io.managed_view_only;
 let empty_wait=if control.is_some(){view_idle_wait(c.k)}else{Duration::ZERO};
 io.idle_wait_us=empty_wait.as_micros() as u64;
 save(c,"memory.json",&json!({"full_dma":io.info.full,"view_dma":io.info.view,"full_len":io.info.full_len,"view_len":io.info.view_len,"cached":io.info.cached,"cache_line":io.info.cache_line,"mmap":"read-only MAP_SHARED, Linux DMA API","lifetime":"module pinned until reboot","shift544":c.shift544}))?;
 save(c,"before.json",&io.counters()?)?;
 if c.command=="capture"{
  io.prepare(c)?;io.start()?;std::thread::sleep(Duration::from_secs_f64(c.seconds));io.stop();let t=Instant::now();while {let s=io.snapshot()?;s.dma_busy||!s.drained||s.committed!=s.reserved}{if t.elapsed()>Duration::from_secs(3){return Err("capture STOP timeout".into())}std::thread::sleep(Duration::from_millis(1));}
  save(c,"unreleased.json",&io.counters()?)?;
  // Heavy whole-ring evidence is outside every rate window, before ANY release.
  for(r,n)in[(0,0x2000000),(1,0x200000)]{let mut b=vec![0;n];io.copy(r,0,&mut b)?;std::fs::write(c.output.join(if r==0{"full-ring.bin"}else{"view-ring.bin"}),b).map_err(|e|e.to_string())?;io.give_back()?;}
  // Diagnostic export owns both copied rings. Drain the descriptor tail
  // before handing the radio back, so another diagnostic can start safely.
  let drained=io.snapshot()?;
  io.release_drained(drained.cookie,drained.committed)?;
  return Ok(())
 }
 if c.command!="rate"&&c.command!="drain"{return Err("command must be capture, drain, rate".into())}
 if c.discard_cold{let s=io.snapshot()?;if s.running||s.dma_busy||s.released!=0||s.committed==0{return Err("cold discard only stopped retained ordinal0".into())}io.release(s.cookie,1)?;}
 let cold=if c.command=="rate"{io.prepare(c)?;io.start()?;io.cold_split(control,c)?}else{Value::Null};
 save(c,"cold-start.json",&cold)?;
 let initial=io.snapshot()?;
 let mut reader=view::PairReader::attach(initial)?;
 let start=Instant::now();let mut deadline=c.warmup+c.seconds;let mut begin=None;let mut end=None;let mut records=Vec::<[u64;7]>::with_capacity(c.trace_limit);let mut trace_omitted=0u64;let mut max_service_us=0u64;let mut max_poll_us=0u64;let mut last_poll=Instant::now();let mut route_failure=None;
 let socket=if let Some(dest)=&c.udp{let sock=std::net::UdpSocket::bind("0.0.0.0:0").map_err(|e|e.to_string())?;sock.connect(dest).map_err(|e|e.to_string())?;sock.set_nonblocking(true).map_err(|e|e.to_string())?;Some(sock)}else{None};
 let record_bytes=(if control.is_some(){64}else{56})+[0,512,1024,2048][c.mode as usize];
 let network=std::cell::RefCell::new(UdpBatch::new(record_bytes));let mut status_due=Instant::now();let mut begin_link=Value::Null;let mut end_link=Value::Null;let mut begin_network=serde_json::Value::Null;let mut end_network=serde_json::Value::Null;
 if control.is_some() && let Some(sock)=&socket{
  let mut net=network.borrow_mut();
  net.coalesce_us=0;
  let mut mtu:libc::c_int=0;
  let mut len=std::mem::size_of_val(&mtu) as libc::socklen_t;
  let fd=sock.as_raw_fd();
  if unsafe{libc::getsockopt(fd,libc::IPPROTO_IP,libc::IP_MTU,(&mut mtu as *mut libc::c_int).cast(),&mut len)}!=0 || mtu<=28{
   return Err("managed VIEW needs an IPv4 route MTU".into());
  }
  net.set_path_mtu(mtu as u32);
  if net.ip_fragmentation {
   let mode:libc::c_int=libc::IP_PMTUDISC_DONT;
   let len=std::mem::size_of_val(&mode) as libc::socklen_t;
   if unsafe{libc::setsockopt(fd,libc::IPPROTO_IP,libc::IP_MTU_DISCOVER,(&mode as *const libc::c_int).cast(),len)}!=0 {
    return Err(format!("VIEW IPv4 fragmentation option: {}",std::io::Error::last_os_error()));
   }
  }
 }
 let initial_anchor=io.live64(0x238);
 let sweeps=std::cell::RefCell::new(control.and_then(|q|q.sweep.map(|spec|
   sweep_delivery::Delivery::new(spec,initial_anchor,q.generation,&mut network.borrow_mut()))).transpose()?);
 let execution=std::cell::Cell::<Option<Execution>>::new(None);
 let draining=std::cell::Cell::new(false);
 let next_announced=std::cell::Cell::new(false);
 let mut first_view_sent=false;
 let mut progress=progress::Progress::new(c.k,c.scan,c.settling,if c.managed_tx{io.r(0x330)}else{0});
 let mut sampled_execution=None;
 let mut sink=|h:Header,overview:&[u8],_owned_full:&[u8]|->R<()>{
  #[cfg(feature="scan-stream-bench")]
  if let Some(zoom)=zoom.borrow_mut().as_mut(){
   zoom.prepare(h,_owned_full,initial_anchor,control.and_then(|q|q.next.as_ref()).and_then(|next|(next.anchor)()))?;
   zoom.pump();
  }
  if let Some(q)=control {
   if overview.len()!=[0,512,1024,2048][c.mode as usize]{return Err("delivery VIEW geometry differs from active descriptor".into())}
   if execution.get().is_none(){
    let e=Execution{generation:q.generation,version:q.version,anchor:if c.scan{initial_anchor}else{h.epoch},run:initial.cookie};
    if h.epoch<e.anchor{network.borrow_mut().suppressed+=1;return Ok(())}
    execution.set(Some(e));
    (q.started)(h,e);
   }
  }
  if let Some(next)=control.and_then(|q|q.next.as_ref()) && !next_announced.get()
      && let Some(anchor)=(next.anchor)() && h.epoch>=anchor {
    let mut net=network.borrow_mut();
    let mut delivery=sweeps.borrow_mut();
    if let (Some(old), Some(sock))=(delivery.as_mut(),socket.as_ref()){old.finish(&mut net,sock,true);}
    *delivery=Some(sweep_delivery::Delivery::new(next.sweep,anchor,next.generation,&mut net)?);
    next_announced.set(true);
    let e=Execution{generation:next.generation,version:next.version,anchor,run:initial.cookie};
    execution.set(Some(e));
    (next.started)(h,e);
   }
  if let Some(sock)=&socket{
   if control.is_none() && !overview.is_empty() && overview.len()!=[0,512,1024,2048][c.mode as usize]{return Err("delivery VIEW geometry differs from active descriptor".into())}
   let mut net=network.borrow_mut();
   if execution.get().is_some_and(|e|h.epoch<e.anchor){net.suppressed+=1;return Ok(())}
   if let Some(sweep)=sweeps.borrow_mut().as_mut() {
    sweep.accept(h,overview,&mut net,sock);
   } else if !overview.is_empty() {
    net.start_record();net.bytes.extend_from_slice(if control.is_some(){b"GVIEW4\0\0"}else{b"GVIEW3\0\0"});for x in [h.ordinal,h.first_seq,h.epoch,h.first_tick,h.k as u64,h.flags as u64]{net.bytes.extend_from_slice(&x.to_le_bytes());}if let Some(q)=control{net.bytes.extend_from_slice(&q.generation.to_le_bytes());}net.bytes.extend_from_slice(overview);net.pending+=1;if net.pending==net.max_records as u64&&!c.send_after_release{net.flush(sock);}
   }}
  if c.trace_limit!=0{if records.len()<c.trace_limit{records.push([h.ordinal,h.first_seq,h.epoch,h.first_tick,h.k as u64,h.flags as u64,start.elapsed().as_nanos()as u64]);}else{trace_omitted+=1;}}Ok(())
 };
 loop{
  if c.command=="rate"&&begin.is_none()&&start.elapsed().as_secs_f64()>=c.warmup{begin_link=interface_counters();begin_network=network.borrow().counts();begin=Some(mark(&mut io,&reader,start)?);deadline=begin.as_ref().unwrap()["time"].as_f64().unwrap()+c.seconds;reader.high_water=0;io.physical_high_water=0;max_service_us=0;max_poll_us=0;io.max_release_gap_us=0;io.last_release=Instant::now();}
  if c.command=="rate"&&(control.map_or(start.elapsed().as_secs_f64()>=deadline||STOP.load(Ordering::Relaxed),|q|q.stop.load(Ordering::Acquire))){
   if let Some(q)=control{draining.set(true);io.stop();(q.trace)("admission_stop");}
   end_link=interface_counters();end_network=network.borrow().counts();end=Some(mark(&mut io,&reader,start)?);break
  }
  if let Some(q)=control.filter(|_|c.scan){if let Err(e)=io.service_route(q){route_failure=Some(e);break;}}
  if status_due.elapsed()>=progress::POLL{
   #[cfg(feature="scan-stream-bench")]
   if let (Some(zoom),Some(config))=(zoom.borrow_mut().as_mut(),roi_control){zoom.update((config.request)())?;}
   if let Some(q)=control{if let Err(e)=(q.check)(){route_failure=Some(e);break;}}
   if let Some(q)=control {
    let map=c.scan.then(||io.map_stamp());
    let s=io.snapshot()?;
    if sampled_execution!=execution.get(){progress.reset();sampled_execution=execution.get();}
    if let Err(e)=io.check_progress(&mut progress,start,&s,q,execution.get(),map){route_failure=Some(e);break;}
   }
   if let Ok(mut v)=mark(&mut io,&reader,start){
    v["network"]=network.borrow().counts();
    v["network"]["socket_pending_bytes"]=json!(socket.as_ref().and_then(udp_socket_queue));
    if io.phase_on {
     v["observed_to_payload"]=histogram(reader.observed_to_payload);
     v["release"]=histogram(reader.release);
    }
    #[cfg(feature="scan-stream-bench")]
    {v["roi"]=zoom.borrow().as_ref().map_or(Value::Null,roi_stream::Stream::stats);}
    *STATUS.lock().unwrap()=v;
   }status_due=Instant::now();
  }
  let t=Instant::now();max_poll_us=max_poll_us.max(t.duration_since(last_poll).as_micros()as u64);last_poll=t;
  let s=reader.service_released(&mut io,full_read_without_sum,&mut sink)?;
  #[cfg(feature="scan-stream-bench")]
  if let Some(zoom)=zoom.borrow_mut().as_mut(){zoom.pump();}
  if let Some(sock)=&socket {
   let mut net=network.borrow_mut();
   if let Some(sweep)=sweeps.borrow().as_ref(){sweep.poll(&mut net,sock);}else{net.flush_due(sock);}
   if let Some(q)=control && !first_view_sent && net.sent>0{first_view_sent=true;(q.trace)("first_view_sent");}
  }
  max_service_us=max_service_us.max(t.elapsed().as_micros()as u64);
  if c.command=="drain"&&s.committed==s.released{break}
  if s.committed==s.released {
   if empty_wait.is_zero(){std::hint::spin_loop();}
   else{std::thread::sleep(empty_wait);}
  }
 }
 let measured=json!({"high_water":io.physical_high_water,"logical_high_water":reader.high_water,"max_service_us":max_service_us,"max_service_interval_us":max_poll_us,"max_release_interval_us":io.max_release_gap_us});
 if !draining.get(){draining.set(control.is_some());io.stop();if let Some(q)=control{(q.trace)("admission_stop");}}
 let stop=Instant::now();loop{let s=reader.service_released(&mut io,full_read_without_sum,&mut sink)?;
  let drained=s.drained&&!s.running&&!s.dma_busy&&s.reserved==s.committed&&reader.next==s.committed;
  if drained{io.release_drained(s.cookie,s.committed)?;}
  if let Some(sock)=&socket {
   let mut net=network.borrow_mut();
   if let Some(sweep)=sweeps.borrow_mut().as_mut(){
    if drained{sweep.finish_drained(&mut net,sock,io.live64(0x180));}else{sweep.poll(&mut net,sock);}
   }else{net.flush(sock);}
  }
  if drained{break}if stop.elapsed()>Duration::from_secs(5){return Err("reader stop/drain timeout; allocation remains pinned".into())}}
 #[cfg(feature="scan-stream-bench")]
 if let Some(zoom)=zoom.borrow_mut().as_mut(){
  let deadline=Instant::now()+Duration::from_secs(1);
  while zoom.pending() && Instant::now()<deadline {
   zoom.pump();
   std::thread::sleep(Duration::from_millis(1));
  }
  zoom.close();
 }
 if let Ok(mut v)=mark(&mut io,&reader,start){
  v["network"]=network.borrow().counts();
  v["network"]["socket_pending_bytes"]=json!(socket.as_ref().and_then(udp_socket_queue));
  if io.phase_on {
   v["observed_to_payload"]=histogram(reader.observed_to_payload);
   v["release"]=histogram(reader.release);
  }
  #[cfg(feature="scan-stream-bench")]
  {v["roi"]=zoom.borrow().as_ref().map_or(Value::Null,roi_stream::Stream::stats);}
  *STATUS.lock().unwrap()=v;
 }
 if let Some(q)=control{(q.trace)("reader_drained");}
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
 let sweep_network=sweeps.borrow().as_ref().map(|s|json!({"completion_datagrams":s.notes_sent,"completion_send_drop":s.notes_lost,"peak_payload_bytes":s.peak_bytes}));
 let result=json!({"begin":begin,"end":end,"measured":measured,"after":after,"cold_start":cold,"full_read_total":reader.read,"consumed_total":reader.consumed,"trace_rows":records.len(),"trace_omitted":trace_omitted,"consumer":if managed_view_only{"owned metadata and VIEW, release, then VIEW send"}else{"ALL4096 volatile read, release, then VIEW send and trace; no full queue or checksum"},"network_spectrum":c.udp.is_some(),"udp_sent":network.borrow().sent,"udp_drop":network.borrow().dropped,"udp_datagrams":network.borrow().datagrams,"udp_batch_max":network.borrow().max_records,"sweep_network":sweep_network,"begin_interface":begin_link,"end_interface":end_link,"begin_network":begin_network,"end_network":end_network,"strategy":{"read_without_sum":c.read_without_sum,"send_after_release":c.send_after_release,"reuse_owned_cache":c.reuse_owned_cache,"batch_sync":c.batch_sync,"direct":c.direct,"release_only":c.release_only,"batch_limit":c.batch_limit,"aligned":c.aligned,"neon":c.neon},"memory_final":memory(),"seconds_total":start.elapsed().as_secs_f64()});save(c,"result.json",&result)?;println!("{}",result);Ok(())
}

static STATUS:std::sync::Mutex<Value>=std::sync::Mutex::new(Value::Null);
pub fn status()->Value{STATUS.lock().unwrap().clone()}
pub fn stop(){STOP.store(true,Ordering::Release);}
pub fn run_json(data:&[u8])->R<()>{STOP.store(false,Ordering::Release);let c:Config=serde_json::from_slice(data).map_err(|e|e.to_string())?;execute(&c,None)}
pub(crate) fn run_controlled(data:&[u8],q:&RunControl<'_>)->R<()>{let c:Config=serde_json::from_slice(data).map_err(|e|e.to_string())?;execute(&c,Some(q))}

impl UdpBatch {
 fn counts(&self)->Value{json!({"sent":self.sent,"sent_bytes":self.sent_bytes,"dropped":self.dropped,
  "datagrams":self.datagrams,"send_calls":self.send_calls,"path_mtu":self.mtu,"ip_fragmentation":self.ip_fragmentation,"pending":self.pending,"pending_bytes":self.bytes.len(),"queue_high_records":self.high_records.max(self.pending),
  "send_errors":self.send_errors,"send_would_block":self.would_block,"last_socket_errno":self.last_errno,
  "admitted_records":self.sent+self.dropped+self.pending-self.rejected_before_queue,
  "rejected_before_queue":self.rejected_before_queue,"transition_records":self.transition,"suppressed_records":self.suppressed,
  "completion_sent":self.notes_sent,"completion_send_drop":self.notes_lost,
  "send_ns":self.send_ns.to_string(),"phase_timing":self.phase_on,"coalesce_us":self.coalesce_us,
  "max_records":self.max_records,"max_wait_ns":self.max_wait_ns.to_string(),"oldest_view_wait_ns_pow2":self.phase_on.then_some(self.oldest_wait_bins),
  "api_send_ns":self.phase_on.then(||histogram(self.api_send)),
  "batch_oldest_to_api_complete_ns":self.phase_on.then(||histogram(self.oldest_to_send)),})}
}
