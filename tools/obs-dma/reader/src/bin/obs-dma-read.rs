//! Explicit, bounded r3 board exerciser. No PL load, RF retune, flash or network.
#[path="../continuous.rs"] mod continuous;
#[path="../view.rs"] mod view;
use continuous::{Header,Snapshot};
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
}
fn kdefault()->u32{2}fn settledefault()->u32{6250}fn secondsdefault()->f64{30.}fn warmdefault()->f64{5.}
#[derive(Default,Debug)]#[repr(C)]struct Info{full:u64,view:u64,full_len:u32,view_len:u32,cached:u32,cache_line:u32,abi:u32,reserved:u32}
#[repr(C)]struct Sync{region:u32,offset:u32,len:u32,for_cpu:u32}
struct Map{ptr:*mut u8,len:usize}
impl Map{fn new(fd:&File,len:usize,off:i64,write:bool)->R<Self>{let p=unsafe{libc::mmap(std::ptr::null_mut(),len,libc::PROT_READ|if write{libc::PROT_WRITE}else{0},libc::MAP_SHARED,fd.as_raw_fd(),off as libc::off_t)};if p==libc::MAP_FAILED{return Err(std::io::Error::last_os_error().to_string())}Ok(Self{ptr:p.cast(),len})}}
impl Drop for Map{fn drop(&mut self){unsafe{libc::munmap(self.ptr.cast(),self.len);}}}
struct Hardware{_lease:File,device:File,regs:Map,full:Map,overview:Map,info:Info,valid:u32,active:bool,shift:bool,logical_tail:Option<u64>,physical_high_water:u64,previous_view:usize,ranges:Vec<(u32,u32,u32)>,copied_bytes:u64,view_bytes:u64,max_release_gap_us:u64,last_release:Instant,sync_ns:u128,copy_ns:u128}
impl Hardware{
 fn open(c:&Config)->R<Self>{
  let read=|p:&str|std::fs::read_to_string(p).map_err(|e|e.to_string());
  if read("/proc/sys/kernel/random/boot_id")?.trim()!=c.boot_id{return Err("boot lease mismatch".into())}
  if read("/tmp/pl-uart-obs-session/phase")?.trim()!="OBS_READY"{return Err("UART loader prerequisite".into())}
  for p in std::fs::read_dir("/proc").map_err(|e|e.to_string())?{let p=p.map_err(|e|e.to_string())?.path();if std::fs::read_to_string(p.join("comm")).is_ok_and(|s|s.trim()=="maia-httpd"){return Err("legacy Maia owner active".into())}}
  if read("/sys/bus/iio/devices/iio:device0/out_altvoltage1_TX_LO_powerdown")?.trim()!="1"{return Err("TX must be off".into())}
  if read("/sys/bus/iio/devices/iio:device0/in_voltage_sampling_frequency")?.trim()!="61440000"{return Err("Fs prerequisite".into())}
  let lease=OpenOptions::new().read(true).write(true).create(true).truncate(false).open("/run/maia-observation.lock").map_err(|e|e.to_string())?;
  if unsafe{libc::flock(lease.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0{return Err("spectral owner busy".into())}
  let device=OpenOptions::new().read(true).write(true).open("/dev/obs_dma").map_err(|e|e.to_string())?;
  let mut info=Info::default();if unsafe{libc::ioctl(device.as_raw_fd(),0x4f01,&mut info)}!=0{return Err("DMA INFO ioctl".into())}
  if info.abi!=1{return Err("DMA ABI".into())}view::ranges(info.full,info.full_len as usize,info.view,info.view_len as usize)?;
  // Explicitly exclude both installed reserves, even though DMA API owns CMA.
  for(a,n)in[(info.full,info.full_len),(info.view,info.view_len)]{for(lo,hi)in[(0x06000000,0x0e000000),(0x16000000,0x16040000)]{if a<hi&&a+n as u64>lo{return Err("DMA overlaps stock reserve".into())}}}
  let full=Map::new(&device,info.full_len as usize,0,false)?;let overview=Map::new(&device,info.view_len as usize,0x1000000,false)?;
  let mem=OpenOptions::new().read(true).write(true).open("/dev/mem").map_err(|e|e.to_string())?;let regs=Map::new(&mem,4096,0x7c460000,true)?;
  let h=Self{_lease:lease,device,regs,full,overview,info,valid:0,active:false,shift:c.shift544,logical_tail:None,physical_high_water:0,previous_view:[0,512,1024,2048][c.previous_mode as usize],ranges:Vec::with_capacity(4),copied_bytes:0,view_bytes:0,max_release_gap_us:0,last_release:Instant::now(),sync_ns:0,copy_ns:0};
  view::geometry(h.r(0xe4),h.r(0x1c0),h.r(0x1c4),h.r(0x1c8))?;if h.r(0x50)!=continuous::ABI{return Err("OBN2 ID".into())}
  Ok(h)
 }
 fn r(&self,o:usize)->u32{unsafe{std::ptr::read_volatile(self.regs.ptr.add(o).cast())}}
 fn w(&self,o:usize,v:u32){unsafe{std::ptr::write_volatile(self.regs.ptr.add(o).cast(),v)};compiler_fence(Ordering::SeqCst);}
 fn r64(&self,o:usize)->u64{self.r(o)as u64|((self.r(o+4)as u64)<<32)}
 fn live64(&self,o:usize)->u64{loop{let hi=self.r(o+4);let lo=self.r(o);if hi==self.r(o+4){return(hi as u64)<<32|lo as u64}}}
 fn sync(&mut self,region:u32,offset:u32,len:u32,cpu:bool)->R<()>{let t=Instant::now();let s=Sync{region,offset,len,for_cpu:cpu as u32};if unsafe{libc::ioctl(self.device.as_raw_fd(),0x4f02,&s)}!=0{return Err(format!("DMA sync: {}",std::io::Error::last_os_error()))}compiler_fence(Ordering::SeqCst);self.sync_ns+=t.elapsed().as_nanos();Ok(())}
 fn copy(&mut self,region:u32,off:usize,out:&mut[u8])->R<()>{self.sync(region,off as u32,out.len()as u32,true)?;let p=if region==0{self.full.ptr}else{self.overview.ptr};let t=Instant::now();unsafe{std::ptr::copy_nonoverlapping(p.add(off),out.as_mut_ptr(),out.len())};self.copy_ns+=t.elapsed().as_nanos();self.ranges.push((region,off as u32,out.len()as u32));Ok(())}
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
 fn stop(&mut self){self.valid=0;self.w(0x54,4);self.active=false;}
 fn counters(&mut self)->R<Value>{let s=self.snapshot()?;Ok(json!({"committed":s.committed,"reserved":s.reserved,"released":self.r64(0x110),"logical_released":s.released,"input_seq":s.input_seq,"formed":s.finished,"selected":s.selected,"unselected":s.unselected,"bank_drop":self.r(0x140),"ring_drop":self.r(0x144),"dirty_drop":s.dirty_drop,"stop_drop":s.stop_drop,"dma_drop":self.r(0x160),"run":s.cookie,"status":self.r(0x154),"visit_epoch":self.live64(0x180),"visit_status":self.r(0x188),"first_epoch":self.live64(0x198)}))}
}
impl Drop for Hardware{fn drop(&mut self){if self.active{self.stop();}}}
impl PairIo for Hardware{
 fn snapshot(&mut self)->R<Snapshot>{self.w(0x54,self.valid|32);let status=self.r(0x154);if status&8!=0{return Err("sticky rejected command".into())}
  let physical=self.r64(0x110);let reserved=self.r64(0x108);
  if reserved<physical||reserved-physical>512{return Err("physical ring ownership".into())}
  self.physical_high_water=self.physical_high_water.max(reserved-physical);
  if self.shift&&self.active&&self.logical_tail.is_none()&&reserved-physical==512{return Err("first split record lost lookbehind before reader attached; no release".into())}
  let logical=self.logical_tail.unwrap_or(physical);
  if logical<physical||logical-physical>1{return Err("split release ownership".into())}
  Ok(Snapshot{committed:self.r64(0x100),reserved:self.r64(0x108),released:logical,input_seq:self.r64(0x118),finished:self.r64(0x128),selected:self.r64(0x130),unselected:self.r64(0x138),dirty_drop:self.r(0x148),stop_drop:self.r(0x14c),stop_input_limit:self.r64(0x158),cookie:self.r(0x150),dropped:self.r(0x140)as u64+self.r(0x144)as u64+self.r(0x160)as u64,running:status&1!=0,dma_busy:status&2!=0,drained:status&16!=0,fault:status&4!=0})}
 fn copy_full(&mut self,slot:usize,out:&mut[u8])->R<()>{if self.shift{
  let prev=(slot+511)&511;let pv=self.previous_view;let(a,rest)=out.split_at_mut(4352-pv);let(b,c)=rest.split_at_mut(pv);
  self.copy(0,prev*32768+28416+pv,a)?;
  if pv!=0{self.copy(1,prev*2048,b)?;}
  self.copy(0,slot*32768,c)?;
  self.previous_view=view::view_len(continuous::header_revision(out,3)?);
 }else{self.copy(0,slot*32768,out)?;}self.copied_bytes+=32768;Ok(())}
 fn copy_view(&mut self,slot:usize,out:&mut[u8])->R<()>{if self.shift{self.copy(0,slot*32768+28416,out)?;}else{self.copy(1,slot*2048,out)?;}self.view_bytes+=out.len()as u64;Ok(())}
 fn release(&mut self,run:u32,tail:u64)->R<()>{self.give_back()?;
  // Split544 needs the preceding physical slot until the next full record has
  // been copied. A logical cursor may lead the hardware release by exactly one.
  // The network never participates. After STOP/DRAINED flush the held slot.
  let safe_tail=if self.shift&&self.active{tail.checked_sub(1).ok_or("split tail underflow")?}else{tail};
  let physical=self.r64(0x110);if safe_tail<physical{return Err("backward physical release".into())}
  if safe_tail>physical{self.w(0xe8,safe_tail as u32);self.w(0xec,(safe_tail>>32)as u32);self.w(0xf0,run);self.w(0x54,self.valid|64);}
  self.logical_tail=Some(tail);self.max_release_gap_us=self.max_release_gap_us.max(self.last_release.elapsed().as_micros()as u64);self.last_release=Instant::now();Ok(())}
}
fn cpu()->f64{let mut r=std::mem::MaybeUninit::<libc::rusage>::zeroed();unsafe{libc::getrusage(libc::RUSAGE_SELF,r.as_mut_ptr());let r=r.assume_init();(r.ru_utime.tv_sec+r.ru_stime.tv_sec)as f64+(r.ru_utime.tv_usec+r.ru_stime.tv_usec)as f64/1e6}}
fn memory()->String{std::fs::read_to_string("/proc/self/status").unwrap_or_default().lines().filter(|s|s.starts_with("VmRSS:")||s.starts_with("VmHWM:")).collect::<Vec<_>>().join("\n")}
fn save(c:&Config,name:&str,v:&Value)->R<()>{std::fs::write(c.output.join(name),serde_json::to_vec_pretty(v).unwrap()).map_err(|e|e.to_string())}
fn mark(io:&mut Hardware,reader:&view::PairReader,start:Instant)->R<Value>{let regs=io.counters()?;Ok(json!({"time":start.elapsed().as_secs_f64(),"cpu_seconds":cpu(),"memory":memory(),"regs":regs,"full_read":reader.read,"consumed":reader.consumed,"full_bytes":io.copied_bytes,"view_bytes":io.view_bytes,"sync_ns":io.sync_ns.to_string(),"copy_ns":io.copy_ns.to_string()}))}
fn execute(c:&Config)->R<()>{
 if !(1..=1023).contains(&c.k)||c.mode>3||c.previous_mode>3||!c.seconds.is_finite()||c.seconds<=0.||c.seconds>120.||!c.warmup.is_finite()||c.warmup<0.||c.warmup>15.{return Err("bounded config".into())}
 std::fs::create_dir_all(&c.output).map_err(|e|e.to_string())?;
 let mut io=Hardware::open(c)?;
 save(c,"memory.json",&json!({"full_dma":io.info.full,"view_dma":io.info.view,"full_len":io.info.full_len,"view_len":io.info.view_len,"cached":io.info.cached,"cache_line":io.info.cache_line,"mmap":"read-only MAP_SHARED, Linux DMA API","lifetime":"module pinned until reboot","shift544":c.shift544}))?;
 save(c,"before.json",&io.counters()?)?;
 if c.command=="capture"{
  io.prepare(c)?;io.start()?;std::thread::sleep(Duration::from_secs_f64(c.seconds));io.stop();let t=Instant::now();while {let s=io.snapshot()?;s.dma_busy||!s.drained||s.committed!=s.reserved}{if t.elapsed()>Duration::from_secs(3){return Err("capture STOP timeout".into())}std::thread::sleep(Duration::from_millis(1));}
  save(c,"unreleased.json",&io.counters()?)?;
  // Heavy whole-ring evidence is outside every rate window, before ANY release.
  for(r,n)in[(0,0x1000000),(1,0x100000)]{let mut b=vec![0;n];io.copy(r,0,&mut b)?;std::fs::write(c.output.join(if r==0{"full-ring.bin"}else{"view-ring.bin"}),b).map_err(|e|e.to_string())?;io.give_back()?;}
  return Ok(())
 }
 if c.command!="rate"&&c.command!="drain"{return Err("command must be capture, drain, rate".into())}
 if c.discard_cold{let s=io.snapshot()?;if s.running||s.dma_busy||s.released!=0||s.committed==0{return Err("cold discard only stopped retained ordinal0".into())}io.release(s.cookie,1)?;}
 if c.command=="rate"{io.prepare(c)?;io.start()?;}
 let mut reader=view::PairReader::attach(io.snapshot()?)?;
 let start=Instant::now();let mut begin=None;let mut end=None;let mut records=Vec::<[u64;7]>::with_capacity(400000);let mut checksum=0u64;let mut max_service_us=0u64;let mut max_poll_us=0u64;let mut last_poll=Instant::now();let mut saved=false;
 let mut sink=|h:Header,full:&[u8],overview:&[u8]|->R<()>{
  if records.len()==400000{return Err("bounded metadata trace full".into())}
  let mut total=0u64;for w in full.chunks_exact(8){total=total.wrapping_add(u64::from_le_bytes(w.try_into().unwrap())&((1u64<<50)-1));}checksum=checksum.wrapping_add(std::hint::black_box(total));
  if c.check{let mut power=vec![0.;4096];view::decode(full,overview,&mut power)?;}
  if !saved&&c.save_ordinal==Some(h.ordinal){std::fs::write(c.output.join("record.bin"),full).map_err(|e|e.to_string())?;std::fs::write(c.output.join("record.view"),overview).map_err(|e|e.to_string())?;saved=true;}
  records.push([h.ordinal,h.first_seq,h.epoch,h.first_tick,h.k as u64,h.flags as u64,start.elapsed().as_nanos()as u64]);Ok(())
 };
 loop{
  if c.command=="rate"&&begin.is_none()&&start.elapsed().as_secs_f64()>=c.warmup{begin=Some(mark(&mut io,&reader,start)?);reader.high_water=0;io.physical_high_water=0;max_service_us=0;max_poll_us=0;io.max_release_gap_us=0;io.last_release=Instant::now();}
  if c.command=="rate"&&(start.elapsed().as_secs_f64()>=c.warmup+c.seconds||STOP.load(Ordering::Relaxed)){end=Some(mark(&mut io,&reader,start)?);break}
  let t=Instant::now();max_poll_us=max_poll_us.max(t.duration_since(last_poll).as_micros()as u64);last_poll=t;
  let s=reader.service_raw(&mut io,&mut sink)?;max_service_us=max_service_us.max(t.elapsed().as_micros()as u64);
  if c.command=="drain"&&s.committed==s.released{break}
  if s.committed==s.released{std::thread::sleep(Duration::from_micros(50));}
 }
 let measured=json!({"high_water":io.physical_high_water,"logical_high_water":reader.high_water,"max_service_us":max_service_us,"max_service_interval_us":max_poll_us,"max_release_interval_us":io.max_release_gap_us});
 io.stop();let stop=Instant::now();loop{let s=reader.service_raw(&mut io,&mut sink)?;if s.drained&&!s.dma_busy&&s.released==s.committed{break}if stop.elapsed()>Duration::from_secs(5){return Err("reader stop/drain timeout; allocation remains pinned".into())}}
 let s=io.snapshot()?;io.release(s.cookie,s.committed)?;
 drop(sink);
 let after=io.counters()?;let mut f=File::create(c.output.join("records.u64le")).map_err(|e|e.to_string())?;for row in &records{for x in row{f.write_all(&x.to_le_bytes()).map_err(|e|e.to_string())?;}}
 let result=json!({"begin":begin,"end":end,"measured":measured,"after":after,"full_read_total":reader.read,"consumed_total":reader.consumed,"trace_rows":records.len(),"checksum":checksum.to_string(),"consumer":"sum of low50 bits of ALL4096 u64 bins after full owned copy; no floats or reference check unless check=true","network_spectrum":false,"memory_final":memory(),"seconds_total":start.elapsed().as_secs_f64()});save(c,"result.json",&result)?;println!("{}",result);Ok(())
}
fn main(){unsafe{libc::signal(libc::SIGINT,signal as libc::sighandler_t);libc::signal(libc::SIGTERM,signal as libc::sighandler_t);}
 let args:Vec<_>=std::env::args().collect();if args.len()!=2{eprintln!("obs-dma-read config.json (explicit RAM-loader+DMA module prerequisites)");std::process::exit(2)}
 let c:Config=serde_json::from_slice(&std::fs::read(&args[1]).expect("config read")).expect("config JSON");if let Err(e)=execute(&c){let _=std::fs::write(c.output.join("ERROR.txt"),&e);eprintln!("{e}");std::process::exit(1)}
}
