//! One bounded assembly buffer and one wire scratch buffer. No GOBS epoch fusion.
#![allow(missing_docs)]
use crate::{observation::Observation,resident_schedule::Map};
use std::sync::{Mutex,atomic::{AtomicBool,AtomicU64,AtomicUsize,Ordering}};
#[derive(Debug,Default)]
struct Publication {bytes:Vec<u8>,ordinal:u64}
#[derive(Debug,Default)]
pub struct Hub {latest:Mutex<Publication>,pub stop:AtomicBool,pub clients:AtomicUsize,pub missed:AtomicU64,pub sent:AtomicU64,pub contention:AtomicU64,pub status:Mutex<String>}
impl Hub {
    pub fn prepare(&self,max:usize){self.latest.lock().unwrap().bytes=Vec::with_capacity(max);}
    pub fn publish(&self,bytes:&[u8]){if let Ok(mut p)=self.latest.try_lock(){p.bytes.clear();p.bytes.extend_from_slice(bytes);p.ordinal+=1;}else{self.contention.fetch_add(1,Ordering::Relaxed);}}
    pub fn poll(&self,last:&mut u64,out:&mut Vec<u8>)->bool {let p=self.latest.lock().unwrap();if p.ordinal==*last{return false;}self.missed.fetch_add(p.ordinal.saturating_sub(*last+1),Ordering::Relaxed);*last=p.ordinal;out.clear();out.extend_from_slice(&p.bytes);true}
}
#[derive(Clone,Debug)]
pub struct Layout {pub bin0:f64,pub step:f64,pub owners:Vec<(usize,usize)>}
impl Layout {
    pub fn new(map:&Map,fs:u32)->Result<Self,String>{
        if fs==0 || map.entries.is_empty(){return Err("scan layout parameters".into());}
        let step=fs as f64/4096.;let bin0=map.entries.iter().map(|e|e.lo).min().unwrap() as f64-fs as f64/2.;
        let offsets:Vec<usize>=map.entries.iter().map(|e|{
            let offset=(e.lo as f64-fs as f64/2.-bin0)/step;
            if (offset-offset.round()).abs()>1e-6 || offset<0. || offset>32768. {return Err("resident centers must share the FFT grid; maximum scan span 32768 bins".to_string());}Ok(offset.round() as usize)
        }).collect::<Result<_,_>>()?;
        let n=offsets.iter().max().unwrap()+4096;if n>32768{return Err("scan span exceeds 32768 bins".into());}
        let mut owners=vec![(usize::MAX,0);n];
        // Lower table position owns overlap, independent of data arrival/validity.
        for (pos,offset) in offsets.into_iter().enumerate(){for k in 0..4096 {if owners[offset+k].0==usize::MAX{owners[offset+k]=(pos,k);}}}
        Ok(Self{bin0,step,owners})
    }
}
#[derive(Clone,Copy,Debug,Default)]
pub struct Part {pub valid:bool,pub seq:u64,pub epoch:u64,pub tick:u64}
#[derive(Default,Debug)]
pub struct Stats {pub clean:u64,pub dirty:u64,pub selected:u64,pub duplicates:u64,pub late:u64,pub frames:u64,pub skipped_laps:u64}
pub struct Assembler {pub map:Map,pub layout:Layout,pub lap:Option<u64>,pub parts:Vec<Part>,pub power:Vec<f32>,pub stats:Stats,pub first_tick:u64,pub end_tick:u64,fs:u32}
impl Assembler {
    pub fn new(map:Map,fs:u32)->Result<Self,String>{let layout=Layout::new(&map,fs)?;Ok(Self{parts:vec![Part::default();map.entries.len()],power:vec![f32::NAN;layout.owners.len()],map,layout,lap:None,stats:Stats::default(),first_tick:0,end_tick:0,fs})}
    /// Caller closes the old lap BEFORE accepting a newer carried epoch. Native
    /// results are ordered; only this FFT-input watermark, or a fully drained
    /// stop, proves that late DMA for the old lap cannot still arrive.
    pub fn incoming_lap(&self,o:&Observation)->Result<Option<u64>,String>{if o.epoch==0{return Ok(None);}Ok(Some(self.map.resolve(o.epoch)?.lap))}
    pub fn begin(&mut self,lap:u64){if let Some(old)=self.lap{self.stats.skipped_laps+=lap.saturating_sub(old+1);}self.lap=Some(lap);self.parts.fill(Part::default());self.power.fill(f32::NAN);self.first_tick=0;self.end_tick=0;}
    pub fn accept(&mut self,o:&Observation)->Result<(),String>{
        if o.epoch==0{self.stats.dirty+=1;return Ok(());}
        let c=self.map.resolve(o.epoch)?;
        if o.nint!=1 || o.power.len()!=4096 || o.power.iter().any(|v|!v.is_finite()||*v<0.){return Err("scan requires suitable nint1 power".into());}
        self.stats.clean+=1;
        match self.lap {None=>self.begin(c.lap),Some(l) if c.lap<l=>{self.stats.late+=1;return Ok(());},Some(l) if c.lap>l=>return Err("close previous lap at ordered input watermark first".into()),_=>{}}
        let end=o.first_tick.checked_add((4096.*62_500_000./self.fs as f64).ceil() as u64).ok_or("tick overflow")?;
        if self.first_tick==0 || o.first_tick<self.first_tick{self.first_tick=o.first_tick;}self.end_tick=self.end_tick.max(end);
        if self.parts[c.position].valid{self.stats.duplicates+=1;return Ok(());}
        self.parts[c.position]=Part{valid:true,seq:o.seq,epoch:o.epoch,tick:o.first_tick};
        for (k,(owner,src)) in self.layout.owners.iter().enumerate(){if *owner==c.position{self.power[k]=o.power[*src];}}
        self.stats.selected+=1;Ok(())
    }
    /// SCN1 v1: fixed header, <=8 part records, one f32 power plane. All IDs
    /// remain binary u64; holes are canonical NaN and part validity is explicit.
    pub fn encode(&mut self,capture:[u8;16],closed:bool,watermark:u64,out:&mut Vec<u8>){
        out.clear();out.extend_from_slice(b"SCN1");out.extend_from_slice(&1u16.to_le_bytes());out.extend_from_slice(&112u16.to_le_bytes());
        out.extend_from_slice(&capture);out.extend_from_slice(&self.map.map_id.to_le_bytes());out.extend_from_slice(&(self.map.run as u64).to_le_bytes());
        let lap=self.lap.unwrap_or(0);out.extend_from_slice(&lap.to_le_bytes());out.extend_from_slice(&lap.to_le_bytes());
        out.extend_from_slice(&self.first_tick.to_le_bytes());out.extend_from_slice(&self.end_tick.to_le_bytes());out.extend_from_slice(&watermark.to_le_bytes());
        out.extend_from_slice(&self.layout.bin0.to_le_bytes());out.extend_from_slice(&self.layout.step.to_le_bytes());
        out.extend_from_slice(&(self.power.len() as u32).to_le_bytes());out.extend_from_slice(&(self.parts.len() as u32).to_le_bytes());
        out.extend_from_slice(&(closed as u32).to_le_bytes());out.extend_from_slice(&62_500_000u32.to_le_bytes());
        for (i,p) in self.parts.iter().enumerate(){
            out.extend_from_slice(&(p.valid as u32).to_le_bytes());out.extend_from_slice(&(i as u32).to_le_bytes());out.extend_from_slice(&p.seq.to_le_bytes());out.extend_from_slice(&p.epoch.to_le_bytes());out.extend_from_slice(&p.tick.to_le_bytes());
            let e=&self.map.entries[i];let route=if p.valid{self.map.resolve(p.epoch).unwrap().route}else{0};
            for v in [e.context,e.slot as u32,route,e.settle,e.dwell,0]{out.extend_from_slice(&v.to_le_bytes());}out.extend_from_slice(&e.lo.to_le_bytes());
        }
        for (owner,src) in &self.layout.owners {out.extend_from_slice(&(*owner as u16).to_le_bytes());out.extend_from_slice(&(*src as u16).to_le_bytes());}
        for v in &self.power {out.extend_from_slice(&v.to_le_bytes());}self.stats.frames+=1;
    }
    pub fn max_wire(&self)->usize{112+64*self.parts.len()+8*self.power.len()}
    pub fn storage_bytes(&self)->usize{self.parts.capacity()*std::mem::size_of::<Part>()+self.power.capacity()*4+self.layout.owners.capacity()*std::mem::size_of::<(usize,usize)>()}
}
#[cfg(test)] mod tests {
    use super::*;use crate::resident_schedule::Entry;
    fn assembler()->Assembler {Assembler::new(Map{run:0xf0000000,map_id:u64::MAX,initial_slot:0,initial_route:0,repeat:true,entries:(0..2).map(|i|Entry{slot:0,marker:false,last:0,route:0,settle:0,dwell:100,context:i+1,lo:100_000_000+2048*i as u64}).collect()},4096).unwrap()}
    fn obs(visit:u64,value:f32)->Observation {Observation{descriptor_id:1,seq:visit,epoch:(0xf0000000u64<<32)|(visit<<1),first_tick:visit*100,nint:1,tag:crate::observation::NO_TAG,power:vec![value;4096]}}
    #[test] fn first_fft_overlap_no_arithmetic_and_missing(){let mut a=assembler();a.accept(&obs(1,3.)).unwrap();a.accept(&obs(1,99.)).unwrap();a.accept(&obs(2,5.)).unwrap();assert!(a.power[..4096].iter().all(|p|*p==3.));assert!(a.power[4096..].iter().all(|p|*p==5.));assert_eq!(a.stats.selected,2);let mut wire=Vec::new();a.encode([1;16],true,300,&mut wire);assert_eq!(wire.len(),a.max_wire());assert_eq!(&wire[24..32],&u64::MAX.to_le_bytes());a.begin(1);a.accept(&obs(3,7.)).unwrap();assert!(a.power[4096..].iter().all(|p|p.is_nan()));assert!(!a.parts[1].valid);a.accept(&obs(1,9.)).unwrap();assert_eq!(a.stats.late,1);}
    #[test] fn dirty_and_next_lap_require_watermark(){let mut a=assembler();let mut o=obs(1,2.);o.epoch=0;a.accept(&o).unwrap();assert!(a.lap.is_none());a.accept(&obs(1,2.)).unwrap();assert!(a.accept(&obs(3,4.)).is_err());assert_eq!(a.incoming_lap(&obs(3,4.)).unwrap(),Some(1));let capacity=a.storage_bytes();for lap in 1..10000{a.begin(lap);a.accept(&obs(lap*2+1,2.)).unwrap();}assert_eq!(capacity,a.storage_bytes());}
    #[test] fn cross_language_wire(){let mut a=assembler();a.accept(&obs(1,3.)).unwrap();a.accept(&obs(2,5.)).unwrap();let mut wire=Vec::with_capacity(a.max_wire());a.encode([0x55;16],true,400,&mut wire);if let Ok(dir)=std::env::var("OBS_NATIVE_OUTPUT_DIR"){std::fs::write(std::path::Path::new(&dir).join("scan-frame.scn"),wire).unwrap();}}
    #[test] fn scan_publication_slow_cursor_repeat_and_contention_are_bounded(){
        let h=Hub::default();h.prepare(64);let mut frame=vec![0;64];let mut last=0;let mut out=Vec::with_capacity(64);
        for id in 0..10000u64{frame[..8].copy_from_slice(&id.to_le_bytes());h.publish(&frame);}
        assert!(h.poll(&mut last,&mut out));assert_eq!(u64::from_le_bytes(out[..8].try_into().unwrap()),9999);
        assert_eq!(h.missed.load(Ordering::Relaxed),9999);assert!(!h.poll(&mut last,&mut out));assert_eq!(h.latest.lock().unwrap().bytes.capacity(),64);assert_eq!(out.capacity(),64);
        let lock=h.latest.lock().unwrap();h.publish(&frame);drop(lock);assert_eq!(h.contention.load(Ordering::Relaxed),1);
    }
}
