//! OBN1-r1, authoritative OBS-003-PL commit 370d03a. No legacy decoder fallback.
//! The adapter never uses arrival time, current RF settings, legacy K(Fs) or nint scaling.
#![allow(missing_docs)]
use crate::observation::{Descriptor,Observation,NO_TAG};
pub const ABI:u32=0x314e424f;
pub const BYTES:usize=32768;
pub const BINS:usize=4096;
pub const SLOTS:usize=8;
pub const TICK_HZ:u64=62_500_000;
pub const COEFFICIENT_SUM:u64=750668;
pub const ENBW:f64=2.0050129965307675;
/// Experimental native mode has a static RF snapshot; legacy mode is untouched.
pub fn http_rejection(native:bool,method:&str,path:&str)->Option<u16>{
    if !native{return None;}
    if path=="/waterfall"{return Some(410);}
    if method=="POST" && path=="/api/resident/stop"{return None;}
    if method!="GET" && method!="HEAD"{return Some(423);}None
}
const MASK:u64=(1<<37)-1;
fn u16v(b:&[u8],i:usize)->u16{u16::from_le_bytes(b[i..i+2].try_into().unwrap())}
fn u32v(b:&[u8],i:usize)->u32{u32::from_le_bytes(b[i..i+4].try_into().unwrap())}
fn u64v(b:&[u8],i:usize)->u64{u64::from_le_bytes(b[i..i+8].try_into().unwrap())}
#[derive(Debug,Clone,Copy)]
pub struct Header{pub seq:u64,pub epoch:u64,pub first_tick:u64,pub clean:bool,pub use_ddc:bool}

/// Validate the whole native record before publishing any part of it.
pub fn validate(data:&[u8])->Result<Header,String>{
    if data.len()!=BYTES{return Err("OBN1 record must be exactly 32768 bytes".into());}
    let mut h=[0u8;64];
    for j in 0..32{h[2*j..2*j+2].copy_from_slice(&data[8*j+6..8*j+8]);}
    if u32v(&h,0)!=ABI || u16v(&h,4)!=1 || u16v(&h,6)!=64 || u32v(&h,32)!=BINS as u32 || u32v(&h,36)!=1 || u32v(&h,40)!=TICK_HZ as u32 || u32v(&h,48)!=1 || u32v(&h,52)!=BYTES as u32 || u64v(&h,56)!=0{return Err("OBN1 header ABI/constants".into());}
    let flags=u32v(&h,44);let epoch=u64v(&h,16);let tick=u64v(&h,24);
    if flags&!3!=0 || (flags&1!=0)!=(epoch!=0) || tick==u64::MAX{return Err("OBN1 clean/epoch/flags/tick".into());}
    for (j,b) in data.chunks_exact(8).enumerate(){let w=u64v(b,0);if ((w>>37)&7)>4 || (w>>40)&255!=0 || j>=32 && w>>48!=0{return Err("OBN1 payload exponent/reserved bits".into());}}
    Ok(Header{seq:u64v(&h,8),epoch,first_tick:tick,clean:flags&1!=0,use_ddc:flags&2!=0})
}

/// Immutable source/epoch bindings, installed before the first series.
/// This initial hardware profile is static raw input, one plane, nint=1.
pub struct Adapter{epochs:Vec<(u64,u16)>,dirty_id:u16,resident:Option<crate::resident_schedule::Map>}
impl Adapter{
    pub fn new(epochs:Vec<(u64,u16)>,dirty_id:u16)->Result<Self,String>{
        if epochs.is_empty() || epochs.len()>16 || dirty_id==0{return Err("context table bounds".into());}
        for (i,(e,d)) in epochs.iter().enumerate(){if *e==0 || *d==0 || epochs[..i].iter().any(|(old,_)|old==e){return Err("immutable context definitions".into());}}
        Ok(Self{epochs,dirty_id,resident:None})
    }
    pub fn resident(map:crate::resident_schedule::Map)->Self{Self{epochs:Vec::new(),dirty_id:1,resident:Some(map)}}
    pub fn descriptor(id:u16,source:u16,fs:f64,lo:f64)->Descriptor{Descriptor{id,sample_rate:fs,bin0_hz:lo-fs/2.,enbw_bins:ENBW,tick_hz:TICK_HZ,bins:BINS as u32,mode:1,source}}
    pub fn check_context(&self,h:Header)->Result<u16,String>{
        if h.use_ddc{return Err("first native live profile requires raw input; DDC gain/grid not installed".into());}
        if h.epoch==0{return Ok(self.dirty_id);}
        if let Some(map)=&self.resident{return Ok(map.resolve(h.epoch)?.descriptor);}
        self.epochs.iter().find(|(epoch,_)|*epoch==h.epoch).map(|(_,id)|*id).ok_or_else(||"unknown carried epoch; cannot use current RF state".into())
    }
    /// Reuse Observation.power. Native data is normalized and reordered exactly once.
    pub fn convert(&self,data:&[u8],out:&mut Observation)->Result<Header,String>{
        self.convert_observed(data,out,|_,_|{})
    }
    pub fn convert_observed<F:FnMut(usize,bool)>(&self,data:&[u8],out:&mut Observation,mut observe:F)->Result<Header,String>{
        observe(5,true);let h=validate(data)?;let id=self.check_context(h)?;observe(5,false);
        observe(6,true);
        if out.power.len()!=BINS{return Err("preallocate 4096 output bins".into());}
        let scale=1.0/(COEFFICIENT_SUM*COEFFICIENT_SUM) as f64;
        for k in 0..BINS{
            let j=((((k+BINS/2)&(BINS-1)) as u16).reverse_bits()>>4) as usize;
            let w=u64v(data,8*j);let value=(w&MASK)<<(2*((w>>37)&7));
            out.power[k]=(value as f64*scale) as f32;
        }
        out.descriptor_id=id;out.seq=h.seq;out.epoch=h.epoch;out.first_tick=h.first_tick;out.nint=1;out.tag=NO_TAG;observe(6,false);Ok(h)
    }
}
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct Snapshot{pub produced:u32,pub selected:u32,pub recorded:u32,pub lost:u32,pub last_slot:usize,pub last_seq:u64,pub status:u32}
impl Snapshot{
    pub fn idle(&self)->Result<bool,String>{if self.status&!15!=0 || self.status&12!=0 || self.last_slot>=SLOTS{return Err("native fault/rejected command/status/slot; coordinate system reset".into());}Ok(self.status&3==0)}
    /// Produced keeps advancing while idle; compare completion fields only.
    pub fn completion_eq(&self,other:&Self)->bool{self.selected==other.selected && self.recorded==other.recorded && self.lost==other.lost && self.last_slot==other.last_slot && self.last_seq==other.last_seq && self.status==other.status}
}
pub struct Series{base:Snapshot,count:u32}
#[derive(Debug,Clone,Copy)]
pub struct Complete{pub recorded:u32,pub lost:u32,pub first_slot:usize,pub last_seq:u64}
/// Implementations invalidate CPU cache and copy a stable completed DMA slot.
pub trait CompletedDma{fn copy_slot(&self,slot:usize,out:&mut[u8])->Result<(),String>;}
/// RAM instrumentation only: copy/quarantine exactly the proven completed
/// slots of a non-full/misaligned series. Never decode stale slots, join a
/// wrap, carry fragments, reset counters, or change the RCS1 schedule.
/// Return how many ordinary selected windows would next align the DMA pointer
/// to slot0 (zero means already aligned). Their records are also quarantined.
pub fn quarantine_ram<D:CompletedDma>(dma:&D,done:Complete,raw:&mut[u8])->Result<u32,String>{
    if raw.len()!=8*BYTES || done.first_slot>=8 || done.recorded>8 || done.lost>8 || done.recorded+done.lost>8{return Err("RAM quarantine completion/storage bounds".into());}
    if done.recorded==8 && done.lost==0 && done.first_slot==0{return Err("full aligned RAM series must use the normal decoder".into());}
    for i in 0..done.recorded as usize{dma.copy_slot((done.first_slot+i)%8,&mut raw[i*BYTES..(i+1)*BYTES])?;}
    let next=(done.first_slot+done.recorded as usize)%8;
    Ok(((8-next)%8) as u32)
}
/// Acceptance-only RAM reload correction. Each call is ONE proven contiguous,
/// completed physical extent. No retained suffix, modulo wrap or cross-series join.
pub fn assemble_ram_extent(raw:&[u8],out:&mut[Vec<u8>])->Result<usize,String>{
    if raw.len()%BYTES!=0 || raw.len()/BYTES>8 || raw.len()<BYTES{return Err("RAM extent must contain 1..8 whole completed adjacent slots".into());}
    let count=raw.len()/BYTES-1;
    if out.len()<count || out.iter().any(|b|b.len()!=BYTES){return Err("RAM assembly storage bounds".into());}
    let first=(BINS-544)*8;
    for (j,b) in out.iter_mut().take(count).enumerate(){b.copy_from_slice(&raw[first+j*BYTES..first+(j+1)*BYTES]);}
    Ok(count)
}
/// All eight physical slots belong to the current complete series. Its last
/// seven records fit linearly; the fragmented edge window is counted separately.
pub fn drain_ram<D:CompletedDma>(dma:&D,done:Complete,raw:&mut[u8],buffers:&mut[Vec<u8>],adapter:&Adapter,order:&crate::observation_live::Order)->Result<crate::observation_live::Order,String>{
    drain_ram_observed(dma,done,raw,buffers,adapter,order,|_,_|{})
}
pub fn drain_ram_observed<D:CompletedDma,F:FnMut(usize,bool)>(dma:&D,done:Complete,raw:&mut[u8],buffers:&mut[Vec<u8>],adapter:&Adapter,order:&crate::observation_live::Order,mut observe:F)->Result<crate::observation_live::Order,String>{
    if done.recorded!=8 || done.lost!=0 || done.first_slot!=0 || raw.len()!=8*BYTES{return Err("RAM mode requires one full loss-free completed ring starting at slot zero".into());}
    observe(2,true);for slot in 0..8{dma.copy_slot(slot,&mut raw[slot*BYTES..(slot+1)*BYTES])?;}observe(2,false);
    observe(3,true);let count=assemble_ram_extent(raw,buffers)?;observe(3,false);
    observe(4,true);let mut checked=order.clone();let mut previous=None;
    for b in buffers.iter().take(count){
        let h=validate(b)?;let id=adapter.check_context(h)?;
        if previous.is_some_and(|v:u64|v.checked_add(1)!=Some(h.seq)){return Err("nonconsecutive records inside one RAM series".into());}
        checked.accept(h.seq,h.epoch,id,h.first_tick)?;previous=Some(h.seq);
    }
    if previous!=Some(done.last_seq){return Err("RAM completion/header last_seq mismatch".into());}observe(4,false);Ok(checked)
}
/// Copy ALL slots before any consumer can publish or re-arm. Validate the whole
/// series transactionally; the caller's order is unchanged if any record is bad.
pub fn drain<D:CompletedDma>(dma:&D,done:Complete,buffers:&mut[Vec<u8>],adapter:&Adapter,order:&crate::observation_live::Order)->Result<crate::observation_live::Order,String>{
    if done.recorded>8 || buffers.len()!=8 || buffers.iter().any(|b|b.len()!=BYTES){return Err("bounded drain storage".into());}
    for (j,out) in buffers.iter_mut().take(done.recorded as usize).enumerate(){dma.copy_slot((done.first_slot+j)%8,out)?;}
    let mut checked=order.clone();let mut last=None;
    for b in buffers.iter().take(done.recorded as usize){let h=validate(b)?;let id=adapter.check_context(h)?;checked.accept(h.seq,h.epoch,id,h.first_tick)?;last=Some(h.seq);}
    if last.is_some_and(|seq|seq!=done.last_seq){return Err("native last_seq/header mismatch; discard series".into());}Ok(checked)
}
impl Series{
    pub fn new(base:Snapshot,count:u32)->Result<Self,String>{if !base.idle()? || !(1..=8).contains(&count){return Err("arm requires idle and count 1..8".into());}Ok(Self{base,count})}
    pub fn completion(&self,s:Snapshot)->Result<Option<Complete>,String>{
        let idle=s.idle()?;let selected=s.selected.wrapping_sub(self.base.selected);let recorded=s.recorded.wrapping_sub(self.base.recorded);let lost=s.lost.wrapping_sub(self.base.lost);
        if s.produced.wrapping_sub(self.base.produced)>=1<<31 || selected>self.count || recorded>selected || lost>selected || recorded+lost>selected{return Err("native counter rollback/inconsistent series".into());}
        if !idle || selected!=self.count || recorded+lost!=selected{return Ok(None);}
        Ok(Some(Complete{recorded,lost,first_slot:(s.last_slot+SLOTS+1-recorded as usize)%SLOTS,last_seq:s.last_seq}))
    }
}

/// /proc also contains self/thread-self aliases: only another numeric PID can own DMA.
pub fn is_other_process(name:&std::ffi::OsStr,current:u32)->bool{
    name.to_str().and_then(|s|s.parse::<u32>().ok()).is_some_and(|pid|pid!=0 && pid!=current)
}

#[cfg(test)]
mod tests{
    #[test] fn observer_preserves_exact_native_power_and_metadata(){
        let adapter=Adapter::new(vec![(7,1)],1).unwrap();let b=fixture("seq-0");let mut a=obs();let mut z=obs();
        adapter.convert(&b,&mut a).unwrap();let mut phases=Vec::new();adapter.convert_observed(&b,&mut z,|stage,begin|phases.push((stage,begin))).unwrap();
        assert_eq!((a.seq,a.epoch,a.first_tick,a.descriptor_id),(z.seq,z.epoch,z.first_tick,z.descriptor_id));assert_eq!(a.power,z.power);
        assert_eq!(phases,[(5,true),(5,false),(6,true),(6,false)]);
    }
    #[test]fn ram_loss_quarantine_then_ordinary_rearm_aligns_without_reset_or_stale_reads(){
        use std::cell::RefCell;
        struct Mock{slots:RefCell<Vec<usize>>}
        impl CompletedDma for Mock{fn copy_slot(&self,s:usize,out:&mut[u8])->Result<(),String>{self.slots.borrow_mut().push(s);out.fill(s as u8);Ok(())}}
        let dma=Mock{slots:RefCell::new(Vec::new())};let mut raw=vec![0;8*BYTES];
        let loss=Complete{recorded:7,lost:1,first_slot:0,last_seq:100};
        assert_eq!(quarantine_ram(&dma,loss,&mut raw).unwrap(),1);assert_eq!(*dma.slots.borrow(),vec![0,1,2,3,4,5,6]);
        dma.slots.borrow_mut().clear();let align=Complete{recorded:1,lost:0,first_slot:7,last_seq:101};
        assert_eq!(quarantine_ram(&dma,align,&mut raw).unwrap(),0);assert_eq!(*dma.slots.borrow(),vec![7]);
        let ready=Complete{recorded:8,lost:0,first_slot:0,last_seq:109};assert!(quarantine_ram(&dma,ready,&mut raw).is_err());
    }
    #[test]fn ram_quarantine_wrap_zero_record_and_bounds_are_explicit(){
        struct Mock;impl CompletedDma for Mock{fn copy_slot(&self,_:usize,_:&mut[u8])->Result<(),String>{Ok(())}}
        let mut raw=vec![0;8*BYTES];
        assert_eq!(quarantine_ram(&Mock,Complete{recorded:3,lost:5,first_slot:6,last_seq:1},&mut raw).unwrap(),7);
        assert_eq!(quarantine_ram(&Mock,Complete{recorded:0,lost:8,first_slot:7,last_seq:1},&mut raw).unwrap(),1);
        assert!(quarantine_ram(&Mock,Complete{recorded:7,lost:2,first_slot:0,last_seq:1},&mut raw).is_err());
        assert!(quarantine_ram(&Mock,Complete{recorded:1,lost:0,first_slot:8,last_seq:1},&mut raw).is_err());
    }
    #[test]fn ram_extent_does_not_carry_edges_or_join_modulo_slots(){
        let mut out=vec![vec![0;BYTES];8];
        assert_eq!(assemble_ram_extent(&vec![0;BYTES],&mut out).unwrap(),0);
        assert!(assemble_ram_extent(&vec![0;BYTES-1],&mut out).is_err());
        assert!(assemble_ram_extent(&vec![0;9*BYTES],&mut out).is_err());
        let first=(BINS-544)*8;let mut raw=vec![0;3*BYTES];
        for j in 0..2{raw[first+j*BYTES..first+(j+1)*BYTES].fill(j as u8+1);}
        assert_eq!(assemble_ram_extent(&raw,&mut out).unwrap(),2);
        assert!(out[0].iter().all(|b|*b==1));assert!(out[1].iter().all(|b|*b==2));
        // A fresh call has no preceding suffix, even at the same slot indices.
        assert_eq!(assemble_ram_extent(&vec![0;BYTES],&mut out).unwrap(),0);
    }
    #[test]fn ram_series_is_transactional_and_requires_current_full_completion(){
        struct Mock(Vec<u8>);
        impl CompletedDma for Mock{fn copy_slot(&self,s:usize,out:&mut[u8])->Result<(),String>{out.copy_from_slice(&self.0[s*BYTES..(s+1)*BYTES]);Ok(())}}
        let mut raw=vec![0;8*BYTES];let first=(BINS-544)*8;
        for seq in 1..=7u64{
            let mut b=fixture("seq-0");
            for (offset,v) in [(8,seq),(24,seq*8333)]{for (j,h) in v.to_le_bytes().chunks_exact(2).enumerate(){b[(offset/2+j)*8+6..(offset/2+j)*8+8].copy_from_slice(h);}}
            raw[first+(seq as usize-1)*BYTES..first+seq as usize*BYTES].copy_from_slice(&b);
        }
        let dma=Mock(raw);let mut scratch=vec![0;8*BYTES];let mut copied=vec![vec![0;BYTES];8];
        let a=Adapter::new(vec![(7,1)],1).unwrap();let order=crate::observation_live::Order::default();
        let done=Complete{recorded:8,lost:0,first_slot:0,last_seq:7};
        drain_ram(&dma,done,&mut scratch,&mut copied,&a,&order).unwrap();
        assert_eq!(validate(&copied[0]).unwrap().seq,1);assert_eq!(validate(&copied[6]).unwrap().seq,7);
        let bad=Complete{last_seq:8,..done};assert!(drain_ram(&dma,bad,&mut scratch,&mut copied,&a,&order).is_err());
        let wrap=Complete{first_slot:7,..done};assert!(drain_ram(&dma,wrap,&mut scratch,&mut copied,&a,&order).is_err());
        let loss=Complete{recorded:7,lost:1,..done};assert!(drain_ram(&dma,loss,&mut scratch,&mut copied,&a,&order).is_err());
        let mut corrupt=dma;corrupt.0[first+3*BYTES+6]^=1;
        assert!(drain_ram(&corrupt,done,&mut scratch,&mut copied,&a,&order).is_err());
    }
    #[test]fn native_owner_ignores_proc_aliases_but_keeps_other_pid(){
        use std::ffi::OsStr;
        for name in ["self","thread-self","net","42","0"]{assert!(!is_other_process(OsStr::new(name),42),"{name}");}
        assert!(is_other_process(OsStr::new("43"),42));
    }
    use super::*;use crate::observation::{Observation,NO_TAG};use crate::observation_live::Encoder;
    fn fixture(name:&str)->Vec<u8>{let s=std::fs::read_to_string(std::path::Path::new(env!("OBS_NATIVE_VECTOR_ROOT")).join(format!("{name}.hex"))).unwrap();s.split_whitespace().map(|s|u8::from_str_radix(s,16).unwrap()).collect()}
    fn obs()->Observation{Observation{descriptor_id:1,seq:0,epoch:0,first_tick:0,nint:1,tag:NO_TAG,power:vec![0.;BINS]}}
    #[test]fn shared_native_vectors_and_simulated_dma_to_gobs(){
        let adapter=Adapter::new(vec![(7,1),(8,2)],1).unwrap();
        for name in ["seq-0","seq-5","seq-6","dma-seq-0","dma-seq-1","dma-seq-2","dma-seq-3"]{
            let b=fixture(name);let mut o=obs();let h=adapter.convert(&b,&mut o).unwrap();assert_eq!((o.seq,o.epoch,o.first_tick,o.nint),(h.seq,h.epoch,h.first_tick,1));
            let session=if name.starts_with("dma-"){[0x44;16]}else{[0x33;16]};
            let mut e=Encoder::new_nint1(session,vec![Adapter::descriptor(1,1,62.5e6,100e6),Adapter::descriptor(2,1,62.5e6,100e6)]).unwrap();let mut frame=Vec::with_capacity(e.max_record());e.observation(&o,&mut frame).unwrap();
            if let Ok(dir)=std::env::var("OBS_NATIVE_OUTPUT_DIR"){let mut all=e.header().to_vec();all.extend_from_slice(&frame);std::fs::write(std::path::Path::new(&dir).join(format!("{name}.gobs")),all).unwrap();}
        }
    }
    #[test]fn native_sequence_gobs_with_gap_and_epoch_boundary(){
        let adapter=Adapter::new(vec![(7,1),(8,2)],1).unwrap();
        let mut e=Encoder::new_nint1([0x33;16],vec![Adapter::descriptor(1,1,62.5e6,100e6),Adapter::descriptor(2,1,62.5e6,100e6)]).unwrap();let mut stream=e.header().to_vec();let mut frame=Vec::with_capacity(e.max_record());let mut o=obs();
        for name in ["seq-0","seq-5","seq-6"]{adapter.convert(&fixture(name),&mut o).unwrap();e.observation(&o,&mut frame).unwrap();stream.extend_from_slice(&frame);}
        if let Ok(dir)=std::env::var("OBS_NATIVE_OUTPUT_DIR"){std::fs::write(std::path::Path::new(&dir).join("native-sequence.gobs"),stream).unwrap();}
    }
    #[test]fn malformed_native_no_legacy_fallback(){
        let b=fixture("seq-0");assert!(validate(&b[..BYTES-1]).is_err());
        for (offset,mask) in [(6,1),(22,1),(8*60+5,1),(8*100+7,1)]{let mut bad=b.clone();bad[offset]^=mask;assert!(validate(&bad).is_err(),"offset {offset}");}
        let mut bad=b.clone();bad[8*33+4]=(bad[8*33+4]&0x1f)|0xe0;assert!(validate(&bad).is_err());
        let mut bad=b.clone();bad[44/2*8+6]^=4;assert!(validate(&bad).is_err());
        let a=Adapter::new(vec![(77,1)],1).unwrap();assert!(a.convert(&b,&mut obs()).is_err());
        let mut h=validate(&b).unwrap();h.use_ddc=true;assert!(a.check_context(h).is_err());
    }
    fn baseline()->Snapshot{Snapshot{produced:100,selected:0,recorded:0,lost:0,last_slot:0,last_seq:0,status:0}}
    #[test]fn full_ring_count_not_irq_or_pointer_delta(){
        let s=Series::new(baseline(),8).unwrap();let mut n=baseline();n.produced=120;n.selected=8;n.recorded=8;
        let c=s.completion(n).unwrap().unwrap();assert_eq!((c.recorded,c.first_slot),(8,1));
        n.status=2;assert!(s.completion(n).unwrap().is_none());n.status=0;n.lost=1;assert!(s.completion(n).is_err());
    }
    #[test]fn loss_modulo_counters_and_delayed_completion(){
        let mut b=baseline();b.selected=u32::MAX-2;b.recorded=u32::MAX-2;b.produced=u32::MAX-2;
        let s=Series::new(b,6).unwrap();let mut n=b;n.selected=3;n.recorded=1;n.lost=2;n.produced=20;n.last_slot=4;
        let c=s.completion(n).unwrap().unwrap();assert_eq!((c.recorded,c.lost,c.first_slot),(4,2,1));
        n.recorded=0;assert!(s.completion(n).unwrap().is_none());n.status=4;assert!(s.completion(n).is_err());
        assert!(Series::new(b,0).is_err());assert!(Series::new(b,9).is_err());
    }
    #[test]fn transactional_dma_drain_full_ring_and_corruption(){
        use std::cell::RefCell;
        struct Mock{slots:Vec<Vec<u8>>,calls:RefCell<Vec<usize>>}
        impl CompletedDma for Mock{fn copy_slot(&self,slot:usize,out:&mut[u8])->Result<(),String>{self.calls.borrow_mut().push(slot);out.copy_from_slice(&self.slots[slot]);Ok(())}}
        let mut slots=vec![fixture("seq-0");8];
        // Synthetic native frames with monotonic producer IDs, spanning a ring wrap.
        for (j,seq) in (0..8u64).enumerate(){let slot=(j+6)%8;for (n,word) in seq.to_le_bytes().chunks_exact(2).enumerate(){slots[slot][(4+n)*8+6..(4+n)*8+8].copy_from_slice(word);}}
        let dma=Mock{slots,calls:RefCell::new(Vec::new())};let mut buffers=vec![vec![0;BYTES];8];let a=Adapter::new(vec![(7,1)],1).unwrap();let order=crate::observation_live::Order::default();let done=Complete{recorded:8,lost:0,first_slot:6,last_seq:7};
        let mut next=drain(&dma,done,&mut buffers,&a,&order).unwrap();assert_eq!(*dma.calls.borrow(),[6,7,0,1,2,3,4,5]);assert!(next.accept(7,7,1,1000).is_err());
        let mut dma=dma;dma.slots[1][8*50+5]=1;assert!(drain(&dma,done,&mut buffers,&a,&order).is_err());
        let mut unchanged=order.clone();unchanged.accept(0,7,1,1000).unwrap();
    }
    #[test]fn native_static_api_policy_does_not_affect_legacy(){
        assert_eq!(http_rejection(false,"PATCH","/api/ad9361"),None);
        assert_eq!(http_rejection(false,"GET","/waterfall"),None);
        assert_eq!(http_rejection(true,"GET","/waterfall"),Some(410));
        assert_eq!(http_rejection(true,"PATCH","/api/ddc/config"),Some(423));
        assert_eq!(http_rejection(true,"PUT","/api/ad9361"),Some(423));
        assert_eq!(http_rejection(true,"GET","/api"),None);
    }
}
