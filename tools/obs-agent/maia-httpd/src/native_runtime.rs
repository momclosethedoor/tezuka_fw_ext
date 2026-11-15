//! Opt-in static raw OBN1 source. No PL load/reset, RF retune, TX or legacy fallback.
#![allow(missing_docs)]
use crate::{app::AppState,native_observation::{self as native,Adapter,Series,Snapshot},observation::{Observation,NO_TAG},observation_live::{Encoder,Fanout,Order},observation_transport::Server};
use anyhow::{Result,bail,ensure};
use serde::Deserialize;
use crate::observation_accumulator::Accumulator;
use std::{fs::{File,OpenOptions},io::Write,net::SocketAddr,os::fd::AsRawFd,path::Path,sync::{Arc,atomic::Ordering},time::{Duration,Instant}};

#[derive(Debug,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config{
    pub capture_id:String,
    pub coordinated_boot_id:String,
    pub externally_settled:bool,
    pub sample_rate:u32,
    pub rx_lo_hz:u64,
    pub epoch:u64,
    pub series_windows:u32,
    pub run_seconds:f64,
    pub bind:SocketAddr,
    pub diagnostic_after_series:Option<u64>,
    pub diagnostic_next_epoch:Option<u64>,
    #[serde(default)]
    pub ram_load_test:bool,
    pub accumulate_windows:Option<u32>,
    pub derived_capture_id:Option<String>,
    pub resident:Option<crate::scan_runtime::Config>,
    pub diagnostics:Option<DiagnosticConfig>,
}
#[derive(Debug,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticConfig {pub mode:String,pub trace:bool}
fn process_cpu_ns()->u64 {let mut t:libc::timespec=unsafe{std::mem::zeroed()};let ok=unsafe{libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID,&mut t)};if ok!=0{return 0;}t.tv_sec as u64*1_000_000_000+t.tv_nsec as u64}
fn observer<'a>(diag:&'a mut Option<crate::gap_diagnostics::Diagnostic>)->impl FnMut(usize,bool)+'a {
    let mut stamp=None;move |stage,begin|{if let Some(d)=diag.as_mut(){if begin{stamp=Some(d.stamp());}else if let Some(s)=stamp.take(){d.finish(stage,s);}}}
}
fn uuid(s:&str)->Result<[u8;16]>{
    let s=s.replace('-',"");ensure!(s.len()==32 && s.is_ascii(),"UUID format");let mut id=[0;16];for (i,x) in id.iter_mut().enumerate(){*x=u8::from_str_radix(&s[2*i..2*i+2],16)?;}ensure!(id!=[0;16],"zero UUID");Ok(id)
}
impl Config{
    pub fn load(path:&Path)->Result<Self>{let c:Self=serde_json::from_slice(&std::fs::read(path)?)?;c.validate()?;Ok(c)}
    fn validate(&self)->Result<()>{
        uuid(&self.capture_id)?;uuid(&self.coordinated_boot_id)?;
        if let Some(d)=&self.diagnostics{ensure!(matches!(d.mode.as_str(),"scan_ws"|"sink") && self.resident.is_some() && self.ram_load_test,"diagnostics require resident RAM mode scan_ws or sink");}
        if let Some(r)=&self.resident{let map=r.map()?;crate::scan_frame::Assembler::new(map,self.sample_rate).map_err(anyhow::Error::msg)?;ensure!(self.accumulate_windows.is_none() && self.diagnostic_after_series.is_none(),"resident scan and static accumulator/diagnostic are separate modes");}
        ensure!(self.externally_settled,"operator must establish static RF settling before asserting validity");
        ensure!(self.sample_rate>0 && self.rx_lo_hz>0 && self.epoch!=0 && (1..=8).contains(&self.series_windows),"native static profile parameters");
        ensure!(self.run_seconds.is_finite() && (0.01..=240.).contains(&self.run_seconds),"source duration must be <=240 seconds");
        ensure!(!self.ram_load_test || self.series_windows==8,"RAM-load test requires full eight-window series");
        match(self.accumulate_windows,&self.derived_capture_id){
            (None,None)=>{},
            (Some(n),Some(id)) if (1..=65536).contains(&n)=>{ensure!(uuid(id)?!=uuid(&self.capture_id)?,"derived capture UUID must differ from raw source UUID");},
            _=>bail!("avg/peak requires explicit accumulate_windows and a separate derived_capture_id"),
        }
        match (self.diagnostic_after_series,self.diagnostic_next_epoch){(None,None)=>{},(Some(n),Some(e)) if n>0 && e!=0 && e!=self.epoch=>{},_=>bail!("diagnostic requires one distinct next epoch and a positive series boundary")}
        Ok(())
    }
}
/// Retained per-boot marker refuses blind process restart/attach. No automatic reset.
pub struct Guard{_lease:File,marker:String}
impl Guard{
    pub fn acquire(c:&Config)->Result<Self>{
        let boot=std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        ensure!(uuid(boot.trim())?==uuid(&c.coordinated_boot_id)?,"requires the explicitly coordinated system boot; stale boot ID");
        for entry in std::fs::read_dir("/proc")?{
            let p=entry?.path();if !native::is_other_process(p.file_name().unwrap(),std::process::id()){continue;}
            if let Ok(name)=std::fs::read_to_string(p.join("comm")){ensure!(name.trim()!="maia-httpd","stop the stock DMA owner and its auto-restart before native startup");}
            if c.resident.is_some(){
                if let Ok(args)=std::fs::read(p.join("cmdline")){
                    let controller=args.split(|b|*b==0).any(|arg|std::str::from_utf8(arg).ok().is_some_and(|s|["api_controller.sh","pl_video_ctl.sh","sweep.sh","sweep_stop.sh"].iter().any(|name|s.rsplit('/').next()==Some(*name))));
                    if controller {let status=std::fs::read_to_string(p.join("status"))?;let paused=status.lines().find_map(|line|line.strip_prefix("State:")).and_then(|s|s.trim().chars().next()).is_some_and(|s|matches!(s,'T'|'t'|'Z'));ensure!(paused,"active legacy RF controller competes with RCS1; use the established coordinated loader/quiescence procedure");}
                }
            }
        }
        let lease=OpenOptions::new().read(true).write(true).create(true).truncate(false).open("/run/maia-observation.lock")?;
        ensure!(unsafe{libc::flock(lease.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}==0,"native DMA owner already holds lease");
        Ok(Self{_lease:lease,marker:format!("/run/maia-observation-{}",boot.trim())})
    }
    fn claim(&self,c:&Config)->Result<()>{
        let mut f=OpenOptions::new().write(true).create_new(true).open(&self.marker)?;
        writeln!(f,"OBN1-r1 capture={} coordinated_boot={}",c.capture_id,c.coordinated_boot_id)?;f.sync_all()?;Ok(())
    }
}
pub struct Runtime{state:AppState,config:Config,_guard:Guard,adapter:Adapter,encoder:Encoder,q:Arc<Fanout>,server:Server,accumulator:Option<Accumulator>,resident:Option<crate::scan_runtime::Resident>,diag:Option<crate::gap_diagnostics::Diagnostic>}
impl std::fmt::Debug for Runtime{fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{f.debug_struct("NativeRuntime").field("config",&self.config).finish_non_exhaustive()}}
impl Runtime{
    pub async fn new(state:AppState,c:Config,guard:Guard)->Result<Self>{
        {let a=state.ad9361().lock().await;ensure!(a.get_sampling_frequency().await?==c.sample_rate && a.get_rx_lo_frequency().await?==c.rx_lo_hz,"immutable static RF descriptor does not match IIO readback");}
        let base=state.ip_core().lock().unwrap().native_snapshot();
        ensure!(base.idle().map_err(anyhow::Error::msg)? && base.selected==0 && base.recorded==0 && base.lost==0,"native attach requires unused counters after coordinated full reset; never reset SDR-only AXI here");
        let mut contexts=vec![(c.epoch,1)];let mut ds=vec![Adapter::descriptor(1,3,c.sample_rate as f64,c.rx_lo_hz as f64)];
        if let Some(epoch)=c.diagnostic_next_epoch{contexts.push((epoch,2));ds.push(Adapter::descriptor(2,3,c.sample_rate as f64,c.rx_lo_hz as f64));}
        let adapter=if let Some(r)=&c.resident{let map=r.map()?;ds=map.entries.iter().enumerate().map(|(i,e)|Adapter::descriptor(i as u16+1,3,c.sample_rate as f64,e.lo as f64)).collect();Adapter::resident(map)}else{Adapter::new(contexts,1).map_err(anyhow::Error::msg)?};
        let accumulator=c.accumulate_windows.map(|n|Accumulator::new(n,ds.clone())).transpose().map_err(anyhow::Error::msg)?;
        let encoder=if let Some(a)=&accumulator{Encoder::new(uuid(c.derived_capture_id.as_ref().unwrap())?,a.output_descriptors())}else{Encoder::new_nint1(uuid(&c.capture_id)?,ds)}.map_err(anyhow::Error::msg)?;
        let q=Arc::new(Fanout::new(8,encoder.max_record()).map_err(anyhow::Error::msg)?);
        let server=Server::bind(c.bind,q.clone(),encoder.header().to_vec(),encoder.max_record(),2)?;
        guard.claim(&c)?;
        state.ip_core().lock().unwrap().native_invalid();
        let mut resident=if let Some(r)=&c.resident{Some(crate::scan_runtime::Resident::new(&state,r,c.sample_rate,uuid(&c.capture_id)?).await?)}else{None};
        tracing::info!(abi="OBN1-r1",expected_pl_commit=if c.resident.is_some(){"c117a1f90774098da49bd73022e01c72c712435e"}else{"370d03a02fface2a287d8d7cafb6cee7422217ee"},capture=%c.capture_id,derived_capture=?c.derived_capture_id,accumulate_windows=?c.accumulate_windows,ram_load_test=c.ram_load_test,boot=%c.coordinated_boot_id,fs=c.sample_rate,lo=c.rx_lo_hz,enbw=native::ENBW,address=%server.address()?,"native hardware source READY; raw input, frozen static/resident context, not RF calibration");
        let diag=c.diagnostics.as_ref().map(|d|crate::gap_diagnostics::Diagnostic::new(d.mode=="sink",d.trace,process_cpu_ns));
        if let Some(r)=resident.as_mut(){r.suppress_publication=diag.as_ref().is_some_and(|d|d.sink);}
        Ok(Self{state,config:c,_guard:guard,adapter,encoder,q,server,accumulator,resident,diag})
    }
    fn poll_resident(&mut self)->Result<bool>{
        let stamp=self.diag.as_ref().map(|d|d.stamp());let stop=if let Some(r)=self.resident.as_mut(){r.poll(&self.state)?}else{false};
        if let Some(d)=self.diag.as_mut(){d.finish(12,stamp.unwrap());if let Some(r)=&self.resident{let q=&r.link;d.rcs([r.diagnostic_status[0] as u64,r.diagnostic_status[1] as u64,q.requests,q.diagnostic_event(),q.last_reply.0,(q.last_reply.1 as u64)|((q.last_reply.2 as u64)<<1)]);}}Ok(stop)
    }
    fn snapshot(&self)->Snapshot{self.state.ip_core().lock().unwrap().native_snapshot()}
    pub async fn run(mut self)->Result<()>{
        let result=self.run_inner().await;
        if let Err(error)=&result{tracing::error!(%error,"native acquisition failed; logging before source teardown");}
        result
    }
    async fn run_inner(&mut self)->Result<()>{
        let mut copied:Vec<Vec<u8>>=(0..8).map(|_|vec![0;native::BYTES]).collect();
        let mut ram=if self.config.ram_load_test{vec![0;8*native::BYTES]}else{Vec::new()};
        let mut observation=Observation{descriptor_id:1,seq:0,epoch:0,first_tick:0,nint:1,tag:NO_TAG,power:vec![0.;native::BINS]};
        let mut reduced=Observation{descriptor_id:1,seq:0,epoch:0,first_tick:0,nint:1,tag:NO_TAG,power:Vec::with_capacity(if self.accumulator.is_some(){2*native::BINS}else{0})};
        let mut frame=Vec::with_capacity(self.encoder.max_record());let mut source_order=Order::default();
        let start=Instant::now();let mut base=self.snapshot();let initial=base;let mut series_count=0u64;let mut recorded=0u64;let mut lost=0u64;let mut ram_edges=0u64;let mut decoded=0u64;let mut output_records=0u64;
        let mut stopping=false;let mut wait_us=0u128;let mut drain_us=0u128;let mut decode_us=0u128;
        let mut ram_align_count=0u32;let mut ram_quarantined=0u64;let mut ram_align_recorded=0u64;let mut ram_discard_series=0u64;let mut consecutive_discard=0u32;
        if let Some(r)=&self.resident{r.start(&self.state)?;}
        loop{
            self.server.poll_accept()?;
            if self.poll_resident()? && !stopping{self.resident.as_ref().unwrap().stop(&self.state)?;stopping=true;}
            let epoch=if self.config.diagnostic_after_series.is_some_and(|n|series_count>=n){self.config.diagnostic_next_epoch.unwrap()}else{self.config.epoch};
            let capture_count=if self.config.ram_load_test && ram_align_count>0{ram_align_count}else{self.config.series_windows};
            let series=Series::new(base,capture_count).map_err(anyhow::Error::msg)?;
            if let Some(d)=self.diag.as_mut(){d.event(1,[series_count,base.produced as u64,base.last_seq,capture_count as u64,0,0]);}
            let arm_stamp=self.diag.as_ref().map(|d|d.stamp());self.state.ip_core().lock().unwrap().native_arm(epoch,capture_count)?;
            if let Some(d)=self.diag.as_mut(){d.finish(0,arm_stamp.unwrap());d.event(7,[series_count,0,0,0,0,0]);}
            let wait_stamp=self.diag.as_ref().map(|d|d.stamp());
            let waiting=Instant::now();
            let (complete,next)=loop{
                self.server.poll_accept()?;
                if self.resident.is_some() && (self.poll_resident()? || start.elapsed().as_secs_f64()>=self.config.run_seconds) && !stopping{self.resident.as_ref().unwrap().stop(&self.state)?;stopping=true;}
                let a=self.snapshot();
                if let Some(done)=series.completion(a).map_err(anyhow::Error::msg)?{
                    // Completion counters and last_seq can cross RegisterCDC independently.
                    let b=self.snapshot();if a.completion_eq(&b){break (done,b);}
                }
                ensure!(waiting.elapsed()<Duration::from_secs(5),"native series timeout; no reset/rearm, coordinate system recovery");
                tokio::time::sleep(Duration::from_millis(1)).await;
            };
            if let Some(d)=self.diag.as_mut(){d.finish(1,wait_stamp.unwrap());d.event(2,[series_count,complete.recorded as u64,complete.lost as u64,complete.first_slot as u64,complete.last_seq,next.produced as u64]);}
            wait_us+=waiting.elapsed().as_micros();let drain_start=Instant::now();
            // Never re-arm until ALL previous records have been invalidated/copied.
            let quarantine=self.config.ram_load_test && !(complete.recorded==8 && complete.lost==0 && complete.first_slot==0);
            let checked={let core=self.state.ip_core().lock().unwrap();
                if quarantine{
                    ram_align_count=native::quarantine_ram(&*core,complete,&mut ram).map_err(anyhow::Error::msg)?;
                    if capture_count==8{ram_quarantined+=complete.recorded as u64;}else{ram_align_recorded+=complete.recorded as u64;}
                    ram_discard_series+=1;consecutive_discard+=1;ensure!(consecutive_discard<=16,"RAM alignment unable to recover after 16 bounded series; no reset attempted");
                    tracing::warn!(selected=capture_count,recorded=complete.recorded,lost=complete.lost,first_slot=complete.first_slot,next_alignment_windows=ram_align_count,"RAM-only whole series quarantined; no stale-slot decode, schedule/counters retained");
                    Ok(source_order.clone())
                }
                else if self.config.ram_load_test{ram_align_count=0;consecutive_discard=0;native::drain_ram_observed(&*core,complete,&mut ram,&mut copied,&self.adapter,&source_order,observer(&mut self.diag))}
                else{native::drain(&*core,complete,&mut copied,&self.adapter,&source_order)}
            }.map_err(anyhow::Error::msg)?;
            let after=self.snapshot();ensure!(next.completion_eq(&after),"DMA completion changed while draining; competing owner/reset");
            source_order=checked;
            drain_us+=drain_start.elapsed().as_micros();let decode_start=Instant::now();
            let count=if quarantine{0}else if self.config.ram_load_test{ram_edges+=1;7}else{complete.recorded as usize};
            if let Some(d)=self.diag.as_mut(){d.event(3,[series_count,count as u64,quarantine as u64,ram_align_count as u64,0,0]);}
            for bytes in copied.iter().take(count){
                self.adapter.convert_observed(bytes,&mut observation,observer(&mut self.diag)).map_err(anyhow::Error::msg)?;
                decoded+=1;
                if let Some(r)=self.resident.as_mut(){let before=r.assembler.stats.selected;let old_lap=r.assembler.lap;let old_frames=r.assembler.stats.frames;
                    r.consume_observed(&observation,&self.state,observer(&mut self.diag))?;
                    if let Some(d)=self.diag.as_mut(){let outcome=if observation.epoch==0{3}else if r.assembler.stats.selected>before{1}else{2};d.event(4,[series_count,observation.seq,observation.epoch,observation.first_tick,outcome,0]);if r.assembler.stats.frames!=old_frames{d.event(6,[old_lap.unwrap_or(u64::MAX),r.assembler.stats.frames,0,0,0,0]);}}

                    // An unclean moving-RF window has no authenticated grid.
                    // Keep/count it at the native diagnostic boundary, never
                    // publish it using the first visit's RF descriptor.
                    if observation.epoch==0{continue;}
                }
                if self.diag.as_ref().is_some_and(|d|d.sink){continue;}
                if let Some(a)=self.accumulator.as_mut(){
                    if a.push(&observation,&mut reduced).map_err(anyhow::Error::msg)?{
                        self.encoder.observation(&reduced,&mut frame).map_err(anyhow::Error::msg)?;self.q.publish(&frame).map_err(anyhow::Error::msg)?;output_records+=1;
                    }
                }else{
                    let enc=self.diag.as_ref().map(|d|d.stamp());self.encoder.observation(&observation,&mut frame).map_err(anyhow::Error::msg)?;if let Some(d)=self.diag.as_mut(){d.finish(10,enc.unwrap());}
                    let pub_s=self.diag.as_ref().map(|d|d.stamp());self.q.publish(&frame).map_err(anyhow::Error::msg)?;if let Some(d)=self.diag.as_mut(){d.finish(11,pub_s.unwrap());}output_records+=1;
                }
            }
            series_count+=1;recorded+=complete.recorded as u64;lost+=complete.lost as u64;base=after;
            decode_us+=decode_start.elapsed().as_micros();
            if series_count==1 || series_count%1000==0{tracing::info!(series=series_count,native_recorded=recorded,native_lost=lost,last_seq=base.last_seq,"native bounded acquisition");}
            if stopping || start.elapsed().as_secs_f64()>=self.config.run_seconds{
                if let Some(r)=self.resident.as_mut(){r.stop(&self.state)?;r.poll(&self.state)?;ensure!(self.state.ip_core().lock().unwrap().resident_read(0xac)&1==0,"stop not acknowledged");r.close(&self.state,false,observation.first_tick);tracing::info!(internal_clean_test_consumer=r.clean_consumer,scan=?r.assembler.stats,scan_storage=r.assembler.storage_bytes(),scan_wire=r.wire.len(),publish_skipped=r.publish_skipped,route_requests=r.link.requests,route_stale=r.link.stale,route_failed=r.link.failed,"resident accounting; test hook is not a classifier; FFT input times are not exact pass boundaries");}
                break;
            }
        }
        self.state.ip_core().lock().unwrap().native_invalid();
        tracing::info!(capture_wait_us=wait_us,drain_copy_us=drain_us,decode_publish_us=decode_us,ram_quarantined_recorded=ram_quarantined,ram_alignment_recorded=ram_align_recorded,ram_discard_series,ws_sent=self.state.scan().sent.load(Ordering::Relaxed),ws_missed=self.state.scan().missed.load(Ordering::Relaxed),ws_contention=self.state.scan().contention.load(Ordering::Relaxed),"bounded phase and scan transport accounting");
        if let Some(a)=self.accumulator.as_mut(){if a.finish(&mut reduced).map_err(anyhow::Error::msg)?{
            self.encoder.observation(&reduced,&mut frame).map_err(anyhow::Error::msg)?;self.q.publish(&frame).map_err(anyhow::Error::msg)?;output_records+=1;
        }}
        tracing::info!(native_decoded=decoded,ram_edge_records=ram_edges,ram_edge_bytes=ram_edges*native::BYTES as u64,ram_copy_capacity=ram.len(),accumulator_bytes=self.accumulator.as_ref().map(|a|a.storage_bytes()).unwrap_or(0),reduced_power_capacity=reduced.power.capacity(),output_records,accumulator=?self.accumulator.as_ref().map(|a|&a.stats),"native input/reduction accounting; RAM edge exclusion is not native lost");
        let net=self.server.finish();
        tracing::info!(series=series_count,produced_delta=base.produced.wrapping_sub(initial.produced),selected=recorded+lost,native_recorded=recorded,native_lost=lost,published=self.q.published(),contention_drops=self.q.contention_drops.load(Ordering::Relaxed),client_missed=net.missed,sent=net.sent,sender_errors=net.errors,partial_disconnects=net.partial_disconnects,queue_capacity=self.q.allocated_payload_bytes(),native_copy_capacity=8*native::BYTES,power_capacity=observation.power.capacity(),frame_capacity=frame.capacity(),elapsed=start.elapsed().as_secs_f64(),"native source stopped; retained lifetime marker requires coordinated new boot for another capture");
        if let Some(d)=self.diag.as_ref(){let path=Path::new("/tmp/obs005-gap-diagnostics");d.write(path)?;std::fs::write(path.join("config.json"),serde_json::to_vec_pretty(&serde_json::json!({"capture":self.config.capture_id,"boot":self.config.coordinated_boot_id,"config_source":"externally saved exact native.json","trace_schema":{"1":"before arm: series,prev produced,prev selected seq,count","7":"after arm: series","2":"completion: series,recorded,lost,first_slot,lastseq,produced","3":"drained: series,decodecount,quarantine,alignment","4":"decoded: series,seq,epoch,inputtick,outcome1selected2repeat3dirty","5":"RCS sampled: status,visit,requests,eventid,replyid,reply_ok+accepted*2; never record RF attribution","6":"closed frame: oldlap,frames"}}))?)?;}
        Ok(())
    }
}
impl native::CompletedDma for crate::fpga::IpCore{fn copy_slot(&self,slot:usize,out:&mut[u8])->Result<(),String>{self.native_copy(slot,out).map_err(|e|e.to_string())}}
impl Drop for Runtime{fn drop(&mut self){if let Ok(core)=self.state.ip_core().lock(){if let Some(r)=&self.resident{let _=crate::resident_schedule::stop(&*core,r.map.repeat);}core.native_invalid();tracing::info!(snapshot=?core.native_snapshot(),"native validity disabled; no SDR-only reset attempted");}}}

#[cfg(test)]
mod tests{
    use super::*;
    fn config()->Config{serde_json::from_value(serde_json::json!({
        "capture_id":"11111111-1111-4111-8111-111111111111",
        "coordinated_boot_id":"22222222-2222-4222-8222-222222222222",
        "externally_settled":true,"sample_rate":30720000,"rx_lo_hz":2400000000u64,
        "epoch":7,"series_windows":8,"run_seconds":90.,"bind":"0.0.0.0:8765"
    })).unwrap()}
    #[test]fn default_is_uncompensated_single_and_ram_requires_full_series(){
        let mut c=config();c.validate().unwrap();assert!(!c.ram_load_test);assert!(c.accumulate_windows.is_none());
        c.ram_load_test=true;c.series_windows=7;assert!(c.validate().is_err());c.series_windows=8;c.validate().unwrap();
    }
    #[test]fn explicit_accumulation_has_separate_identity_and_bounded_target(){
        let mut c=config();c.accumulate_windows=Some(4);assert!(c.validate().is_err());
        c.derived_capture_id=Some(c.capture_id.clone());assert!(c.validate().is_err());
        c.derived_capture_id=Some("33333333-3333-4333-8333-333333333333".into());c.validate().unwrap();
        for n in [0,65537]{c.accumulate_windows=Some(n);assert!(c.validate().is_err());}
    }
}
