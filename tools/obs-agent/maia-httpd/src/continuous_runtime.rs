//! Runtime for the continuous source, separate acquisition and preparation tasks.
//! No register addresses are guessed here. An accepted RingIo implementation and
//! pass resolver are supplied by the final PL integration.
#![allow(missing_docs)]
use crate::{continuous::{self as c,RingIo,Reader,OwnedQueue,Prepare,PartsHub,PreparedPart},scan_parts::{self as wire,Descriptor}};
use std::sync::{Arc,Mutex,atomic::{AtomicBool,Ordering}};
use std::time::Duration;
struct StopOnDrop(Arc<AtomicBool>);impl Drop for StopOnDrop{fn drop(&mut self){self.0.store(true,Ordering::Release);}}

pub trait Consumer:Send{fn consume(&mut self,p:&PreparedPart<'_>)->Result<(),String>;}
/// Current consumer is a hook, not a classifier. A future algorithm must own
/// its bounded state/queue; it must not hold DMA slots or network locks.
#[derive(Default)]pub struct CountingConsumer{pub parts:u64,pub contributing_ffts:u64}
impl Consumer for CountingConsumer{fn consume(&mut self,p:&PreparedPart<'_>)->Result<(),String>{self.parts+=1;self.contributing_ffts+=p.header.k as u64;Ok(())}}
pub trait PassResolver:Send{fn candidate(&mut self,h:c::Header)->Result<Option<u64>,String>;fn accepted(&mut self,h:c::Header,pass:u64);fn closed_through(&self)->u64;}
/// Explicit static-only pass assignment: one selected group is one row. It is
/// not a model of autonomous hops or measured RF pass boundaries.
#[derive(Default)]pub struct StaticPass{next:u64}
impl PassResolver for StaticPass{fn candidate(&mut self,_:c::Header)->Result<Option<u64>,String>{if self.next==u64::MAX{return Err("static pass exhaustion".into())}Ok(Some(self.next))}fn accepted(&mut self,_:c::Header,_:u64){self.next+=1;}fn closed_through(&self)->u64{self.next}}
#[derive(Debug)]pub struct Outcome{pub reader:c::ReaderStats,pub final_snapshot:c::Snapshot,pub prepare:c::PrepareStats,pub owned_dropped:u64,pub network_dropped:u64,pub publication_max_us:u64,pub stopped:bool}

pub async fn run<I:RingIo+Send+'static,C:Consumer,R:PassResolver>(mut io:I,mut prepare:Prepare,mut descriptor:Descriptor,hub:Arc<Mutex<PartsHub>>,stop:Arc<AtomicBool>,mut consumer:C,mut resolver:R)->Result<Outcome,String>{
    descriptor.validate()?;let mut frame=Vec::with_capacity(wire::MAX_WIRE);
    let _cancel=StopOnDrop(stop.clone());
    io.start()?;
    let reader_result=io.snapshot().and_then(|s|{descriptor.identity.run=s.cookie as u64;Reader::attach(s)});
    let mut reader=match reader_result{Ok(r)=>r,Err(e)=>{let _=io.stop();return Err(e)}};
    wire::descriptor(&descriptor,0,&mut frame)?;hub.lock().map_err(|_|"hub lock")?.descriptor(&frame)?;
    let queue=Arc::new(Mutex::new(OwnedQueue::new(8)?));let done=Arc::new(AtomicBool::new(false));
    let (q,s,d)=(queue.clone(),stop.clone(),done.clone());
    // This task owns the hardware reader. No normalization/consumer/WS work.
    let acquire=tokio::spawn(async move{
        let result=async{
            let mut stop_started=None;
            loop{
                if s.load(Ordering::Acquire){reader.stop(&mut io)?;let start=stop_started.get_or_insert_with(std::time::Instant::now);if start.elapsed()>Duration::from_secs(5){return Err("continuous stop/drain timeout; no automatic AXI reset".into())}}
                let snap={let mut q=q.lock().map_err(|_|"owned queue lock")?;reader.service(&mut io,&mut q)?};
                if reader.drained(snap){break}
                tokio::time::sleep(Duration::from_millis(1)).await;
            }Ok::<_,String>((reader.stats,io.snapshot()?))
        }.await;
        if result.is_err(){let _=io.stop();}
        d.store(true,Ordering::Release);result
    });
    let mut raw=vec![0;c::BYTES];let mut power=vec![0.;c::BINS];let mut seq=0u64;let network_dropped=0u64;let mut publication_max_us=0;
    let processing=async{
        loop{
            let got=queue.lock().map_err(|_|"owned queue lock")?.pop_swap(&mut raw);
            if !got{if done.load(Ordering::Acquire){break}tokio::time::sleep(Duration::from_millis(1)).await;continue}
            let h=c::header(&raw)?;
            let candidate=if h.clean(){resolver.candidate(h)?}else{None};
            if let Some(p)=prepare.accept_if(&raw,&mut power,|_,_|candidate.is_some())?{
                let pass=candidate.unwrap();resolver.accepted(h,pass);
                consumer.consume(&p)?; // the same product, before WS serialization
                seq=seq.checked_add(1).ok_or("wire sequence exhaustion")?;
                wire::part(&descriptor,seq,pass,&p,&mut frame)?;
                // Only bounded memory copies hold this mutex. Socket sends are
                // outside it; acquisition owns a different queue and DMA lease.
                // Brief contention must not discard an already prepared row.
                publication_max_us=publication_max_us.max(c::publish_ready(&hub,&frame)?);
            }
            tokio::task::yield_now().await;
        }Ok::<_,String>(())
    }.await;
    if processing.is_err(){stop.store(true,Ordering::Release)}
    let (reader,final_snapshot)=acquire.await.map_err(|e|e.to_string())??;
    processing?;
    seq=seq.checked_add(1).ok_or("wire sequence exhaustion")?;
    wire::progress(descriptor.identity,seq,resolver.closed_through(),true,&mut frame);
    hub.lock().map_err(|_|"hub lock")?.publish(&frame)?;
    let owned_dropped=queue.lock().map_err(|_|"owned queue lock")?.dropped;
    Ok(Outcome{reader,final_snapshot,prepare:prepare.stats,owned_dropped,network_dropped,publication_max_us,stopped:true})
}
