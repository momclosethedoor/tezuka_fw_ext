//! Static OBN2 r2 binding, contract 449b79e839010089836e8202cdae9c6b77d12492.
//! No PL load, RF hop or automatic RF retune. RAM layout is explicitly opt-in.
#![allow(missing_docs)]
use crate::{app::AppState,continuous::{self as c,RingIo,Snapshot},continuous_runtime::{self,CountingConsumer,StaticPass},scan_parts::{Descriptor,Entry,Identity}};
use anyhow::{Result,ensure};
use serde::Deserialize;
use std::{path::Path,fs::{File,OpenOptions},os::fd::AsRawFd,sync::{Arc,Mutex,atomic::{AtomicBool,Ordering}},time::Duration};
#[derive(Clone,Debug,Deserialize)]#[serde(deny_unknown_fields)]pub struct Config{
    pub capture_id:String,pub coordinated_boot_id:String,pub sample_rate:u32,pub rx_lo_hz:u64,pub epoch:u64,
    #[serde(default="two")]pub k:u32,#[serde(default)]pub peak:bool,#[serde(default="stride")]pub publish_stride:u32,
    pub externally_settled:bool,pub controllers_quiesced:bool,pub run_seconds:f64,
    #[serde(default)]pub ram_split544:bool,#[serde(default)]pub first_per_epoch:bool,
}
fn two()->u32{2}fn stride()->u32{250}
pub fn http_rejection(method:&str,path:&str)->bool{!matches!(method,"GET"|"HEAD")&&(path.starts_with("/api/ad9361")||path.starts_with("/api/spectrometer"))}
pub fn uuid(s:&str)->Result<[u8;16]>{let s=s.replace('-',"");ensure!(s.len()==32&&s.is_ascii(),"UUID format");let mut id=[0;16];for(i,b)in id.iter_mut().enumerate(){*b=u8::from_str_radix(&s[i*2..i*2+2],16)?;}ensure!(id!=[0;16],"zero UUID");Ok(id)}
impl Config{pub fn load(p:&Path)->Result<Self>{let c:Self=serde_json::from_slice(&std::fs::read(p)?)?;c.validate()?;Ok(c)}pub fn validate(&self)->Result<()>{uuid(&self.capture_id)?;uuid(&self.coordinated_boot_id)?;ensure!(self.externally_settled&&self.controllers_quiesced,"static RF/owner prerequisites must be established");ensure!(self.sample_rate>0&&self.rx_lo_hz>0&&self.epoch!=0&&(1..=1023).contains(&self.k)&&self.publish_stride>0,"static OBN2 configuration");ensure!(self.run_seconds.is_finite()&&(0.01..=240.).contains(&self.run_seconds),"bounded capture duration");Ok(())}}
/// Same device lease as the older diagnostic reader, without a per-boot marker.
pub struct Guard{_file:File}
impl Guard{pub fn acquire(c:&Config)->Result<Self>{ensure!(uuid(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim())?==uuid(&c.coordinated_boot_id)?,"coordinated boot mismatch");for p in std::fs::read_dir("/proc")?{let p=p?.path();if !crate::native_observation::is_other_process(p.file_name().unwrap(),std::process::id()){continue}if let Ok(name)=std::fs::read_to_string(p.join("comm")){ensure!(name.trim()!="maia-httpd","another maia-httpd owns the hardware");}if let Ok(args)=std::fs::read(p.join("cmdline")){let controller=args.split(|b|*b==0).any(|a|std::str::from_utf8(a).ok().is_some_and(|s|["api_controller.sh","pl_video_ctl.sh","sweep.sh","sweep_stop.sh"].iter().any(|n|s.rsplit('/').next()==Some(*n))));if controller{let status=std::fs::read_to_string(p.join("status"))?;let paused=status.lines().find_map(|l|l.strip_prefix("State:")).and_then(|l|l.trim().chars().next()).is_some_and(|s|matches!(s,'T'|'t'|'Z'));ensure!(paused,"legacy RF controller still active");}}}let file=OpenOptions::new().read(true).write(true).create(true).truncate(false).open("/run/maia-observation.lock")?;ensure!(unsafe{libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}==0,"spectral owner lease busy");Ok(Self{_file:file})}}
pub struct Hardware{state:AppState,config:Config,control:u32,active:bool,pub last:Option<Snapshot>}
impl Drop for Hardware{fn drop(&mut self){if self.active{if let Ok(core)=self.state.ip_core().lock(){core.stream_write(0x54,4);}}}}
impl Hardware{
    fn snapshot_locked(&self)->Result<Snapshot,String>{let core=self.state.ip_core().lock().map_err(|_|"IpCore lock")?;core.stream_write(0x54,self.control|32);let read64=|a|core.stream_read(a)as u64|((core.stream_read(a+4)as u64)<<32);let status=core.stream_read(0x154);if status&8!=0{return Err("OBN2 rejected command sticky".into())}Ok(Snapshot{input_seq:read64(0x118),finished:read64(0x128),unselected:read64(0x138),dirty_drop:core.stream_read(0x148),stop_drop:core.stream_read(0x14c),stop_input_limit:read64(0x158),cookie:core.stream_read(0x150),committed:read64(0x100),reserved:read64(0x108),released:read64(0x110),selected:read64(0x130),dropped:core.stream_read(0x140)as u64+core.stream_read(0x144)as u64+core.stream_read(0x160)as u64,running:status&1!=0,dma_busy:status&2!=0,drained:status&16!=0,fault:status&4!=0})}
}
impl RingIo for Hardware{
    fn snapshot(&mut self)->Result<Snapshot,String>{let s=self.snapshot_locked()?;self.last=Some(s);Ok(s)}
    fn start(&mut self)->Result<(),String>{let before=self.snapshot_locked()?;if before.fault||before.running||before.dma_busy||!before.drained||before.reserved!=before.committed||before.released!=before.committed{return Err("OBN2 START needs stopped drained released nonfatal source".into())}if self.config.ram_split544&&before.committed==0&&!self.config.first_per_epoch{return Err("cold RAM ordinal0 requires first_per_epoch at fixed epoch before continuous START".into())}if before.cookie==u32::MAX{return Err("run counter exhausted".into())}{let core=self.state.ip_core().lock().map_err(|_|"IpCore lock")?;let mode=(self.config.k<<1)|((self.config.peak as u32)<<15);core.stream_write(0x20,mode);core.stream_write(0x58,self.config.epoch as u32);core.stream_write(0x5c,(self.config.epoch>>32)as u32);core.stream_write(0x60,self.config.publish_stride);self.control=1|if self.config.first_per_epoch{16}else{0};self.active=true;core.stream_write(0x54,self.control|2);}let after=self.snapshot()?;if after.cookie!=before.cookie+1||!after.running{return Err("OBN2 START not accepted".into())}Ok(())}
    fn copy_slot(&mut self,s:usize,b:&mut[u8])->Result<(),String>{self.state.ip_core().lock().map_err(|_|"IpCore lock")?.native_copy(s,b).map_err(|e|e.to_string())}
    fn copy_record(&mut self,ordinal:u64,b:&mut[u8])->Result<(),String>{
        if !self.config.ram_split544{return self.copy_slot((ordinal%8)as usize,b)}
        let core=self.state.ip_core().lock().map_err(|_|"IpCore lock")?;
        c::copy_committed(ordinal,c::CopyLayout::Ram544,b,|slot,range,out|core.continuous_copy_range(slot,range,out).map_err(|e|e.to_string()))
    }
    fn release(&mut self,cookie:u32,tail:u64)->Result<(),String>{let core=self.state.ip_core().lock().map_err(|_|"IpCore lock")?;core.stream_write(0xe8,tail as u32);core.stream_write(0xec,(tail>>32)as u32);core.stream_write(0xf0,cookie);core.stream_write(0x54,self.control|64);if core.stream_read(0x64)&8!=0{return Err("OBN2 release rejected".into())}Ok(())}
    fn stop(&mut self)->Result<(),String>{self.control=0;self.state.ip_core().lock().map_err(|_|"IpCore lock")?.stream_write(0x54,4);self.active=false;Ok(())}
}
pub struct Runtime{state:AppState,config:Config,_guard:Guard}
impl std::fmt::Debug for Runtime{fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{f.debug_struct("ContinuousRuntime").field("config",&self.config).finish_non_exhaustive()}}
impl Runtime{
    pub async fn new(state:AppState,config:Config,guard:Guard)->Result<Self>{let a=state.ad9361().lock().await;ensure!(a.get_sampling_frequency().await?==config.sample_rate&&a.get_rx_lo_frequency().await?==config.rx_lo_hz,"immutable RF descriptor differs from actual IIO");drop(a);Ok(Self{state,config,_guard:guard})}
    pub async fn run(self)->Result<()>{
        let c=&self.config;let flags=if c.peak{4}else{0};let descriptor=Descriptor{identity:Identity{capture:uuid(&c.capture_id)?,map:1,run:1},fs:c.sample_rate as f64,bin0:c.rx_lo_hz as f64-c.sample_rate as f64/2.,step:c.sample_rate as f64/4096.,bins:4096,tick_hz:c::TICK_HZ,enbw:crate::native_observation::ENBW,entries:vec![Entry{epoch:c.epoch,lo:c.rx_lo_hz,offset:0,context:1,route:0,flags}]};
        let prepare=c::Prepare::new(vec![c::Context{epoch:c.epoch,position:0,flags,scale:1./750668f64.powi(2)}],0).map_err(anyhow::Error::msg)?;
        let hub=Arc::new(Mutex::new(c::PartsHub::new(8,crate::scan_parts::MAX_WIRE).map_err(anyhow::Error::msg)?));self.state.set_parts(hub.clone());self.state.scan().stop.store(false,Ordering::Release);
        let io=Hardware{state:self.state.clone(),config:c.clone(),control:0,active:false,last:None};let stop=Arc::new(AtomicBool::new(false));let timer_stop=stop.clone();let state=self.state.clone();let duration=Duration::from_secs_f64(c.run_seconds);
        let timer=tokio::spawn(async move{let start=std::time::Instant::now();loop{if start.elapsed()>=duration||state.scan().stop.load(Ordering::Acquire){timer_stop.store(true,Ordering::Release);break}tokio::time::sleep(Duration::from_millis(1)).await;}});
        *self.state.scan().status.lock().unwrap()=format!("{{\"version\":2,\"kind\":\"static-continuous\",\"running\":true,\"k\":{},\"stride\":{},\"autonomous_rf\":false}}",c.k,c.publish_stride);
        let result=continuous_runtime::run(io,prepare,descriptor,hub,stop,CountingConsumer::default(),StaticPass::default()).await;timer.abort();
        // Hardware and owned queue are already drained. App::run cancels HTTP
        // when this future returns; give fast clients a bounded final flush.
        // A slow socket cannot extend this deadline or retain any DMA slot.
        // One second exceeds the 8-row / 30 Hz fast-client backlog budget.
        if result.is_ok(){tokio::time::sleep(Duration::from_secs(1)).await;}
        match result{Ok(outcome)=>{tracing::info!(?outcome,"OBN2 stopped/drained; static passes, no RF acceptance");*self.state.scan().status.lock().unwrap()=format!("{{\"version\":2,\"kind\":\"static-continuous\",\"running\":false,\"copied\":{},\"normalized\":{},\"owned_drop\":{},\"network_drop\":{},\"high_water\":{}}}",outcome.reader.copied,outcome.prepare.normalized,outcome.owned_dropped,outcome.network_dropped,outcome.reader.high_water);Ok(())},Err(e)=>{tracing::error!(error=%e,"OBN2 error; no implicit AXI reset or RAM correction");Err(anyhow::anyhow!(e))}}
    }
}
