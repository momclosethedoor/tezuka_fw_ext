//! Bounded reduction of normalized nint=1 observations. No level conversion.
#![allow(missing_docs)]
use crate::observation::{self,Descriptor,Observation,NO_TAG};
use crate::observation_live::Order;

#[derive(Default,Debug)]
pub struct Stats{pub inputs:u64,pub accepted:u64,pub unusable:u64,pub input_gaps:u64,pub groups:u64,pub partial_groups:u64,pub contributing_ffts:u64}
pub struct Accumulator{
    descriptors:Vec<Descriptor>,target:u32,sum:Vec<f64>,peak:Vec<f32>,count:u32,
    id:u16,epoch:u64,first_tick:u64,bins:usize,last_seq:Option<u64>,order:Order,
    next_seq:u64,pub stats:Stats,
}
impl Accumulator{
    pub fn new(target:u32,descriptors:Vec<Descriptor>)->Result<Self,String>{
        if !(1..=65536).contains(&target) || descriptors.is_empty() || descriptors.len()>16{return Err("accumulation bounds".into());}
        let mut max=0;
        for (i,d) in descriptors.iter().enumerate(){
            observation::descriptor_ok(d)?;
            if d.mode!=1 || d.source==2 || d.tick_hz==0 || descriptors[..i].iter().any(|p|p.id==d.id){return Err("accumulator requires immutable normalized single-plane source descriptors".into());}
            max=max.max(d.bins as usize);
        }
        Ok(Self{descriptors,target,sum:vec![0.;max],peak:vec![0.;max],count:0,id:0,epoch:0,first_tick:0,bins:0,last_seq:None,order:Order::default(),next_seq:0,stats:Stats::default()})
    }
    pub fn output_descriptors(&self)->Vec<Descriptor>{self.descriptors.iter().cloned().map(|mut d|{d.mode=3;d}).collect()}
    pub fn storage_bytes(&self)->usize{self.sum.capacity()*8+self.peak.capacity()*4}
    /// At most one output per input: after a boundary a new group has one FFT;
    /// for target=1 there cannot be a pending previous group.
    pub fn push(&mut self,o:&Observation,out:&mut Observation)->Result<bool,String>{
        let d=self.descriptors.iter().find(|d|d.id==o.descriptor_id).ok_or("unknown accumulator descriptor")?;
        observation::observation_ok(o,d)?;
        if o.nint!=1{return Err("accumulator accepts nint=1 only".into());}
        let bins=d.bins as usize;
        let mut checked=self.order.clone();checked.accept(o.seq,o.epoch,o.descriptor_id,o.first_tick)?;
        let gap=self.last_seq.is_some_and(|last|last.checked_add(1)!=Some(o.seq));
        if let Some(last)=self.last_seq{self.stats.input_gaps+=o.seq-last-1;}
        self.order=checked;self.last_seq=Some(o.seq);self.stats.inputs+=1;
        let usable=o.epoch!=0 && o.power.iter().all(|p|p.is_finite());
        let boundary=gap || !usable || self.id!=o.descriptor_id || self.epoch!=o.epoch;
        let mut emitted=if boundary{self.finish(out)?}else{false};
        if !usable{self.stats.unusable+=1;return Ok(emitted);}
        if self.count==0{
            self.id=o.descriptor_id;self.epoch=o.epoch;self.first_tick=o.first_tick;self.bins=bins;
            self.sum[..bins].fill(0.);self.peak[..bins].fill(0.);
        }
        for (j,p) in o.power.iter().enumerate(){self.sum[j]+=*p as f64;self.peak[j]=self.peak[j].max(*p);}
        self.count+=1;self.stats.accepted+=1;
        if self.count==self.target{debug_assert!(!emitted);emitted=self.finish(out)?;}
        Ok(emitted)
    }
    pub fn finish(&mut self,out:&mut Observation)->Result<bool,String>{
        if self.count==0{return Ok(false);}
        let next=self.next_seq.checked_add(1).ok_or("derived sequence exhausted; allocate new capture")?;
        out.descriptor_id=self.id;out.seq=self.next_seq;out.epoch=self.epoch;out.first_tick=self.first_tick;out.nint=self.count;out.tag=NO_TAG;
        out.power.resize(2*self.bins,0.);
        for j in 0..self.bins{out.power[j]=(self.sum[j]/self.count as f64) as f32;out.power[self.bins+j]=self.peak[j];}
        self.stats.groups+=1;self.stats.partial_groups+=u64::from(self.count<self.target);self.stats.contributing_ffts+=self.count as u64;
        self.count=0;self.next_seq=next;Ok(true)
    }
}

#[cfg(test)]
mod tests{
    use super::*;
    fn desc(id:u16,source:u16)->Descriptor{Descriptor{id,sample_rate:8.,bin0_hz:10.,enbw_bins:1.,tick_hz:100,bins:2,mode:1,source}}
    fn frame(seq:u64,epoch:u64,id:u16,p:[f32;2])->Observation{Observation{descriptor_id:id,seq,epoch,first_tick:seq*25,nint:1,tag:NO_TAG,power:p.to_vec()}}
    fn output()->Observation{frame(0,0,1,[0.,0.])}
    #[test]fn mean_and_true_peak_same_windows(){
        let mut a=Accumulator::new(3,vec![desc(1,3)]).unwrap();let mut out=output();
        assert!(!a.push(&frame(0,7,1,[1.,4.]),&mut out).unwrap());
        assert!(!a.push(&frame(1,7,1,[100.,1.]),&mut out).unwrap());
        assert!(a.push(&frame(2,7,1,[4.,7.]),&mut out).unwrap());
        assert_eq!(out.power,vec![35.,4.,100.,7.]);assert_eq!((out.seq,out.nint,out.epoch,out.first_tick),(0,3,7,0));
        if let Ok(dir)=std::env::var("OBS_NATIVE_OUTPUT_DIR"){
            let mut e=crate::observation_live::Encoder::new([0x55;16],a.output_descriptors()).unwrap();
            let mut wire=Vec::new();e.observation(&out,&mut wire).unwrap();
            let mut bytes=e.header().to_vec();bytes.extend_from_slice(&wire);
            std::fs::write(std::path::Path::new(&dir).join("avg-peak.gobs"),bytes).unwrap();
        }
        assert_eq!(a.stats.contributing_ffts,3);assert!(!a.finish(&mut out).unwrap());
    }
    #[test]fn gaps_dirty_epoch_descriptor_source_and_final_partial(){
        let mut a=Accumulator::new(4,vec![desc(1,3),desc(2,1)]).unwrap();let mut out=output();
        a.push(&frame(10,7,1,[1.,3.]),&mut out).unwrap();
        assert!(a.push(&frame(12,7,1,[2.,4.]),&mut out).unwrap());assert_eq!((out.nint,out.first_tick),(1,250));
        assert!(a.push(&frame(13,0,1,[99.,99.]),&mut out).unwrap());assert_eq!((out.seq,out.nint),(1,1));
        assert!(!a.push(&frame(14,8,1,[6.,8.]),&mut out).unwrap());
        assert!(a.push(&frame(15,9,2,[9.,10.]),&mut out).unwrap());assert_eq!((out.epoch,out.nint),(8,1));
        assert!(a.finish(&mut out).unwrap());assert_eq!((out.descriptor_id,out.seq,out.nint),(2,3,1));
        assert_eq!((a.stats.inputs,a.stats.accepted,a.stats.unusable,a.stats.input_gaps),(5,4,1,1));
    }
    #[test]fn explicit_one_window_duplicates_invalid_input_and_bounded_storage(){
        let mut a=Accumulator::new(1,vec![desc(1,3)]).unwrap();let mut out=output();let storage=a.storage_bytes();
        for seq in 0..10000{assert!(a.push(&frame(seq,7,1,[seq as f32,1.]),&mut out).unwrap());assert_eq!(out.nint,1);}
        assert_eq!(a.storage_bytes(),storage);assert!(a.push(&frame(9999,7,1,[1.,1.]),&mut out).is_err());
        let mut bad=frame(10000,7,1,[1.,1.]);bad.nint=2;assert!(a.push(&bad,&mut out).is_err());
        assert_eq!(a.stats.groups,10000);assert!(Accumulator::new(0,vec![desc(1,3)]).is_err());
    }
    #[test]fn nan_window_ends_group_and_source_end_flushes(){
        let mut a=Accumulator::new(4,vec![desc(1,3)]).unwrap();let mut out=output();
        a.push(&frame(0,7,1,[2.,8.]),&mut out).unwrap();
        assert!(a.push(&frame(1,7,1,[f32::NAN,5.]),&mut out).unwrap());
        assert_eq!(out.nint,1);assert_eq!(out.power,vec![2.,8.,2.,8.]);
        assert!(!a.finish(&mut output()).unwrap());assert_eq!(a.stats.unusable,1);
        // A source lifetime is an accumulator instance; finish before replacement.
        let mut next=Accumulator::new(4,vec![desc(1,3)]).unwrap();
        next.push(&frame(0,9,1,[5.,6.]),&mut out).unwrap();
        assert!(next.finish(&mut out).unwrap());assert_eq!((out.seq,out.epoch,out.nint),(0,9,1));
    }
}
