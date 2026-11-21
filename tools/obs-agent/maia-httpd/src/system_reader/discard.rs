//! Diagnostic read/release. No spectrum copy, sum, payload file, network or rows.
use super::*;
fn mark(io:&mut Hardware,start:Instant,read:u64)->R<Value>{Ok(json!({"time":start.elapsed().as_secs_f64(),"full_read":read,"cpu_seconds":cpu(),"memory":memory(),"regs":io.counters()?,"sync_ns":io.sync_ns.to_string(),"sync_calls":io.sync_calls}))}
pub(super) fn execute(c:&Config)->R<()>{
 let scope=c.diagnostic_sync.as_deref().unwrap();
 if !matches!(scope,"none"|"header"|"all")||c.mode!=1||!c.shift544||c.previous_mode!=1||c.udp.is_some()||c.save_ring||c.owned_pipeline{return Err("discard diagnostic: fixed split544 VIEW512, no network/ring export/private queue".into())}
 std::fs::create_dir_all(&c.output).map_err(|e|e.to_string())?;
 let mut io=Hardware::open(c,false)?;io.prepare(c)?;io.start()?;
 let initial=io.snapshot()?;let run=initial.cookie;let mut next=initial.released;let mut read=0u64;
 let start=Instant::now();let mut begin=None;let mut end=None;let mut begin_epoch=u64::MAX;let mut end_epoch=u64::MAX;
 let mut valid=0u64;let mut stale=0u64;let mut invalid=0u64;let mut reversed=0u64;let mut last:Option<Header>=None;let mut unique=0u64;let mut last_counted=None;let mut duplicates=0u64;let mut inside_hold_missing=0u64;let mut first_gap=None;
 let mut per_position=[0u64;64];let mut anchor=0;let mut version=0;let mut stopped=None;let mut status_due=Instant::now();
 loop{
  if begin.is_none()&&start.elapsed().as_secs_f64()>=c.warmup{let b=mark(&mut io,start,read)?;begin_epoch=b["regs"]["visit_epoch"].as_u64().unwrap();anchor=b["regs"]["fld"]["map_epoch"].as_u64().unwrap();version=b["regs"]["fld"]["version"].as_u64().unwrap();begin=Some(b);io.physical_high_water=0;io.max_release_gap_us=0;io.last_release=Instant::now();}
  if stopped.is_none()&&(begin.as_ref().is_some_and(|b|start.elapsed().as_secs_f64()>=b["time"].as_f64().unwrap()+c.seconds)||STOP.load(Ordering::Relaxed)){let e=mark(&mut io,start,read)?;end_epoch=e["regs"]["visit_epoch"].as_u64().unwrap();end=Some(e);io.stop();stopped=Some(Instant::now());}
  if status_due.elapsed()>=Duration::from_millis(100){*STATUS.lock().unwrap()=mark(&mut io,start,read)?;status_due=Instant::now();}
  let s=io.snapshot()?;
  if s.cookie!=run||s.released!=next||s.committed<next||s.committed>s.reserved||s.fault{return Err("discard cursor/fault".into())}
  let until=s.committed.min(next+c.batch_limit as u64);
  if scope=="all"{io.prepare_prefix(next,until)?;}
  for ordinal in next..until{
   let slot=(ordinal&1023)as usize;let prev=(slot+1023)&1023;let prefix=prev*32768+28928;
   // Only the second diagnostic invalidates the eight header cache lines.
   if scope=="header"{io.sync(0,prefix as u32,512,true)?;}
   let mut header=[0u8;512];
   unsafe{
    // Preserve just metadata before release; never export or retain bins.
    std::ptr::copy_nonoverlapping(io.full.ptr.add(prefix),header.as_mut_ptr(),512);
    full_read_without_sum(std::slice::from_raw_parts(io.full.ptr.add(prefix),3840));
    full_read_without_sum(std::slice::from_raw_parts(io.overview.ptr.add(prev*2048),512));
    full_read_without_sum(std::slice::from_raw_parts(io.full.ptr.add(slot*32768),28416));
    full_read_without_sum(std::slice::from_raw_parts(io.full.ptr.add(slot*32768+28416),512));
   }
   io.release(run,ordinal+1)?;
   next=ordinal+1;read+=1;
   // Everything below is aggregate loss accounting, AFTER release.
   match continuous::header_prefix(&header,3){
    Ok(h)if h.ordinal==ordinal&&h.k==2&&h.flags==9=>{
     valid+=1;
     if let Some(previous)=last{if h.epoch<previous.epoch{reversed+=1;}if h.epoch==previous.epoch{duplicates+=1;inside_hold_missing+=h.first_seq.saturating_sub(previous.first_seq).saturating_div(2).saturating_sub(1);}if h.epoch>previous.epoch+1&&first_gap.is_none(){first_gap=Some(json!({"before_epoch":previous.epoch,"after_epoch":h.epoch,"before_tick":previous.first_tick,"after_tick":h.first_tick,"ordinal":ordinal}));}}
     if h.epoch>begin_epoch&&h.epoch<end_epoch&&last_counted!=Some(h.epoch){unique+=1;if h.epoch>=anchor{per_position[((h.epoch-anchor)%64)as usize]+=1;}last_counted=Some(h.epoch);}
     last=Some(h);
    },Ok(_)=>stale+=1,Err(_)=>invalid+=1,
   }
  }
  if let Some(t)=stopped{if s.drained&&!s.dma_busy&&next==s.committed{break}if t.elapsed()>Duration::from_secs(5){return Err("discard drain timeout".into())}}
  if next==s.committed{std::hint::spin_loop();} // Diagnostic CSR-only polling; no timer delay.
 }
 let s=io.snapshot()?;io.release(run,s.committed)?;let after=io.counters()?;
 // A boundary visit may have been counted just before the end snapshot.
 if last_counted==Some(end_epoch){unique=unique.saturating_sub(1);if end_epoch>=anchor{let p=((end_epoch-anchor)%64)as usize;per_position[p]=per_position[p].saturating_sub(1);}}
 let expected=end_epoch.saturating_sub(begin_epoch).saturating_sub(1);
 save(c,"result.json",&json!({"kind":"diagnostic-read-release-discard","sync_scope":scope,"payload_valid":false,"metadata_coherent_by_contract":scope!="none","begin":begin,"end":end,"after":after,"full_read_total":read,"bytes_read":read*33280,"valid_metadata":valid,"stale_metadata":stale,"invalid_metadata":invalid,"reversed_epochs":reversed,"useful_expected":expected,"useful_received":unique,"useful_missing":if scope!="none"&&stale==0&&invalid==0&&reversed==0{Some(expected.saturating_sub(unique))}else{None},"untrusted_missing_from_observed_headers":expected.saturating_sub(unique),"map_version":version,"map_epoch":anchor,"useful_per_position":per_position.as_slice(),"held_repeated_records":duplicates,"inside_hold_missing_bracketed":inside_hold_missing,"first_gap":first_gap,"measured":{"high_water":io.physical_high_water,"max_release_interval_us":io.max_release_gap_us},"sync_calls":io.sync_calls,"sync_ns":io.sync_ns.to_string(),"payload_saved":false,"payload_sent":false,"spectrum_rows_saved":false,"checksum":false}))?;
 Ok(())
}
