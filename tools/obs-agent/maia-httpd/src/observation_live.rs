//! Bounded GOBS1 stream codec and fanout. No native PL/DMA ABI is assumed here.
//! Offline Capture remains separate: live delivery is strictly producer ordered.
#![allow(missing_docs)]
use crate::observation::{self, Descriptor, Observation, MAGIC, UNKNOWN_TICK};
use std::io::{self, Read};
use std::sync::{Mutex, TryLockError};
use std::sync::atomic::{AtomicU64, Ordering};
pub const MAX_DESCRIPTORS: usize = 16;
pub const MAX_RECORD: usize = 40 + 8 * 65536;

fn number<const N:usize>(b:&[u8],i:usize)->[u8;N]{b[i..i+N].try_into().unwrap()}
fn u16v(b:&[u8],i:usize)->u16{u16::from_le_bytes(number(b,i))}
fn u32v(b:&[u8],i:usize)->u32{u32::from_le_bytes(number(b,i))}
fn u64v(b:&[u8],i:usize)->u64{u64::from_le_bytes(number(b,i))}
fn invalid(s:&str)->io::Error{io::Error::new(io::ErrorKind::InvalidData,s)}

/// Constant-size monotonic live state. No lifetime seq or epoch collection.
#[derive(Default, Clone)]
pub struct Order { last:Option<u64>, epoch:u64, descriptor:u16, tick:Option<u64> }
impl Order {
    pub fn accept(&mut self,seq:u64,epoch:u64,id:u16,tick:u64)->Result<(),String>{
        if self.last.is_some_and(|v|seq<=v){return Err("non-increasing live seq; reset needs new capture".into());}
        if epoch!=0 && epoch==self.epoch && self.descriptor!=id{return Err("live epoch binding".into());}
        if tick!=UNKNOWN_TICK && self.tick.is_some_and(|v|tick<v){return Err("source counter reset needs new capture".into());}
        self.last=Some(seq);
        if epoch!=0 {self.epoch=epoch;self.descriptor=id;}
        if tick!=UNKNOWN_TICK {self.tick=Some(tick);}
        Ok(())
    }
}

/// Stable descriptors are checked once at installation. Caller owns a reusable frame.
pub struct Encoder { descriptors:Vec<Descriptor>, header:Vec<u8>, order:Order, max:usize, nint1:bool }
impl Encoder {
    pub fn new(session:[u8;16],descriptors:Vec<Descriptor>)->Result<Self,String>{
        if session==[0;16] || descriptors.is_empty() || descriptors.len()>MAX_DESCRIPTORS {return Err("capture/descriptor bounds".into());}
        let mut header=Vec::with_capacity(24+48*descriptors.len());header.extend_from_slice(MAGIC);header.extend_from_slice(&session);
        let mut max=0;
        for (i,d) in descriptors.iter().enumerate(){
            observation::descriptor_ok(d)?;
            if descriptors[..i].iter().any(|x|x.id==d.id){return Err("descriptor redefinition".into());}
            max=max.max(40+4*d.bins as usize*if d.mode==3 {2}else{1});
            header.extend_from_slice(&48u32.to_le_bytes());header.extend_from_slice(&1u16.to_le_bytes());header.extend_from_slice(&d.id.to_le_bytes());
            for x in [d.sample_rate,d.bin0_hz,d.enbw_bins]{header.extend_from_slice(&if x.is_nan(){0x7ff8000000000000u64}else{x.to_bits()}.to_le_bytes());}
            header.extend_from_slice(&d.tick_hz.to_le_bytes());header.extend_from_slice(&d.bins.to_le_bytes());header.extend_from_slice(&d.mode.to_le_bytes());header.extend_from_slice(&d.source.to_le_bytes());
        }
        Ok(Self{descriptors,header,order:Order::default(),max,nint1:false})
    }
    pub fn new_nint1(session:[u8;16],descriptors:Vec<Descriptor>)->Result<Self,String>{let mut e=Self::new(session,descriptors)?;e.require_nint1_profile()?;e.nint1=true;Ok(e)}
    /// First integration profile; the generic codec also supports old golden mode 3.
    pub fn require_nint1_profile(&self)->Result<(),String>{
        let first=&self.descriptors[0];
        if self.descriptors.iter().any(|d|d.mode==3 || d.source==2 || d.tick_hz==0 || d.tick_hz!=first.tick_hz || d.source!=first.source){return Err("live source requires one plane, one provenance and one source clock domain".into());}Ok(())
    }
    pub fn header(&self)->&[u8]{&self.header}
    pub fn max_record(&self)->usize{self.max}
    /// Append exactly one complete record, without decoding any capture/history.
    pub fn observation(&mut self,o:&Observation,out:&mut Vec<u8>)->Result<(),String>{
        let d=self.descriptors.iter().find(|d|d.id==o.descriptor_id).ok_or("missing descriptor")?;
        if self.nint1 && o.nint!=1{return Err("source profile requires nint=1".into());}
        observation::observation_ok(o,d)?;
        self.order.accept(o.seq,o.epoch,o.descriptor_id,o.first_tick)?;
        out.clear();out.extend_from_slice(&(40u32+4*o.power.len() as u32).to_le_bytes());out.extend_from_slice(&2u16.to_le_bytes());out.extend_from_slice(&o.descriptor_id.to_le_bytes());
        for x in [o.seq,o.epoch,o.first_tick]{out.extend_from_slice(&x.to_le_bytes());}
        out.extend_from_slice(&o.nint.to_le_bytes());out.extend_from_slice(&o.tag.to_le_bytes());
        for x in &o.power{out.extend_from_slice(&x.to_bits().to_le_bytes());}Ok(())
    }
}

/// Borrowed payload, valid until the reader is advanced. No owned payload per record.
pub struct Frame<'a>{pub descriptor_id:u16,pub seq:u64,pub epoch:u64,pub first_tick:u64,pub nint:u32,pub tag:u32,pub payload:&'a[u8]}
impl Frame<'_>{pub fn power(&self)->impl Iterator<Item=f32>+'_ {self.payload.chunks_exact(4).map(|x|f32::from_le_bytes(x.try_into().unwrap()))}}
/// Incremental bounded reader. Descriptors must be installed before observations.
pub struct Reader<R>{input:R,pub session:[u8;16],pub descriptors:Vec<Descriptor>,scratch:Vec<u8>,order:Order,started:bool}
impl<R:Read> Reader<R>{
    pub fn new(mut input:R)->io::Result<Self>{
        let mut h=[0;24];input.read_exact(&mut h)?;
        if &h[..8]!=MAGIC || h[8..]==[0;16]{return Err(invalid("capture magic/UUID"));}
        Ok(Self{input,session:h[8..].try_into().unwrap(),descriptors:Vec::with_capacity(MAX_DESCRIPTORS),scratch:vec![0;MAX_RECORD],order:Order::default(),started:false})
    }
    pub fn next_observation(&mut self)->io::Result<Option<Frame<'_>>>{
        loop {
            let b=&mut self.scratch;
            // EOF is legal only on a record boundary. read_exact handles short reads.
            loop {match self.input.read(&mut b[..1]){Ok(0)=>return Ok(None),Ok(_)=>break,Err(e) if e.kind()==io::ErrorKind::Interrupted=>continue,Err(e)=>return Err(e)}}
            self.input.read_exact(&mut b[1..8])?;
            let size=u32v(b,0) as usize;let kind=u16v(b,4);let id=u16v(b,6);
            if !(8..=MAX_RECORD).contains(&size) || id==0 {return Err(invalid("record bounds"));}
            match kind {
                1=>{
                    if size!=48 || self.started || self.descriptors.len()==MAX_DESCRIPTORS || self.descriptors.iter().any(|d|d.id==id){return Err(invalid("descriptor bounds/order/redefinition"));}
                    self.input.read_exact(&mut b[8..size])?;
                    let mut values=[0.;3];
                    for (j,x) in values.iter_mut().enumerate(){let bits=u64v(b,8+8*j);*x=f64::from_bits(bits);if x.is_nan() && bits!=0x7ff8000000000000{return Err(invalid("noncanonical NaN"));}}
                    let d=Descriptor{id,sample_rate:values[0],bin0_hz:values[1],enbw_bins:values[2],tick_hz:u64v(b,32),bins:u32v(b,40),mode:u16v(b,44),source:u16v(b,46)};
                    observation::descriptor_ok(&d).map_err(|e|invalid(&e))?;self.descriptors.push(d);
                },
                2=>{
                    let d=self.descriptors.iter().find(|d|d.id==id).ok_or_else(||invalid("missing descriptor"))?;
                    if size!=40+4*d.bins as usize*if d.mode==3{2}else{1}{return Err(invalid("observation size"));}
                    self.input.read_exact(&mut b[8..size])?;
                    let (seq,epoch,tick,nint,tag)=(u64v(b,8),u64v(b,16),u64v(b,24),u32v(b,32),u32v(b,36));
                    if nint==0 && d.source!=2 || (d.tick_hz==0)!=(tick==UNKNOWN_TICK) || d.source==2 && (epoch!=0 || tag>7) || d.source!=2 && tag!=u32::MAX{return Err(invalid("observation metadata"));}
                    for x in b[40..size].chunks_exact(4){let bits=u32::from_le_bytes(x.try_into().unwrap());let v=f32::from_bits(bits);if v.is_infinite() || v<0. || v.is_nan() && bits!=0x7fc00000{return Err(invalid("power/NaN"));}}
                    self.order.accept(seq,epoch,id,tick).map_err(|e|invalid(&e))?;self.started=true;
                    return Ok(Some(Frame{descriptor_id:id,seq,epoch,first_tick:tick,nint,tag,payload:&self.scratch[40..size]}));
                },_=>return Err(invalid("unknown record kind"))
            }
        }
    }
}

struct Ring {slots:Vec<Vec<u8>>,head:u64}
/// Fixed slots; acquisition never waits for a reader or its socket.
pub struct Fanout {ring:Mutex<Ring>,head:AtomicU64,pub contention_drops:AtomicU64,max:usize}
/// One bounded subscriber cursor; ordinal is internal and never replaces source seq.
#[derive(Default)]
pub struct Cursor {next:u64,pub missed:u64}
impl Fanout{
    pub fn new(capacity:usize,max:usize)->Result<Self,String>{
        if !(1..=1024).contains(&capacity) || !(40..=MAX_RECORD).contains(&max) || capacity*max>16*1024*1024{return Err("queue bounds (16 MiB maximum payload slots)".into());}
        Ok(Self{ring:Mutex::new(Ring{slots:(0..capacity).map(|_|Vec::with_capacity(max)).collect(),head:0}),head:AtomicU64::new(0),contention_drops:AtomicU64::new(0),max})
    }
    pub fn publish(&self,frame:&[u8])->Result<bool,String>{
        if frame.len()>self.max || frame.len()<40 || u32v(frame,0) as usize!=frame.len() || u16v(frame,4)!=2{return Err("publish requires one complete observation".into());}
        let mut r=match self.ring.try_lock(){Ok(r)=>r,Err(TryLockError::WouldBlock)=>{self.contention_drops.fetch_add(1,Ordering::Relaxed);return Ok(false)},Err(_)=>return Err("poisoned ring".into())};
        let i=r.head as usize%r.slots.len();r.slots[i].clear();r.slots[i].extend_from_slice(frame);r.head=r.head.checked_add(1).ok_or("ring ordinal exhausted")?;self.head.store(r.head,Ordering::Release);Ok(true)
    }
    pub fn subscribe(&self)->Cursor{Cursor{next:self.head.load(Ordering::Acquire),missed:0}}
    /// Copy before socket I/O. No reference to an overwritten slot escapes the lock.
    pub fn poll(&self,c:&mut Cursor,out:&mut Vec<u8>)->bool{
        let r=self.ring.lock().unwrap();let oldest=r.head.saturating_sub(r.slots.len() as u64);
        if c.next<oldest{c.missed+=oldest-c.next;c.next=oldest;}
        if c.next==r.head{return false;}
        out.clear();out.extend_from_slice(&r.slots[c.next as usize%r.slots.len()]);c.next+=1;true
    }
    pub fn published(&self)->u64{self.head.load(Ordering::Acquire)}
    pub fn allocated_payload_bytes(&self)->usize{self.ring.lock().unwrap().slots.iter().map(|v|v.capacity()).sum()}
}

#[cfg(test)]
mod tests{
    use super::*;use crate::observation::{decode,Record,NO_TAG};
    fn golden()->Vec<u8>{let s:String=include_str!("../tests/observation-v1/golden.hex").split_whitespace().collect();(0..s.len()).step_by(2).map(|i|u8::from_str_radix(&s[i..i+2],16).unwrap()).collect()}
    #[test]fn incremental_golden_bytes_and_values(){
        let wire=golden();let c=decode(&wire).unwrap();let ds=c.records.iter().filter_map(|r|if let Record::Descriptor(d)=r{Some(d.clone())}else{None}).collect();
        let mut e=Encoder::new(c.session,ds).unwrap();let mut out=e.header().to_vec();let mut scratch=Vec::with_capacity(e.max_record());
        for r in &c.records{if let Record::Observation(o)=r{e.observation(o,&mut scratch).unwrap();out.extend_from_slice(&scratch);}}
        assert_eq!(out,wire);
        let mut reader=Reader::new(&wire[..]).unwrap();let mut values=Vec::new();
        while let Some(f)=reader.next_observation().unwrap(){values.push((f.seq,f.epoch,f.nint,f.power().count()));}
        assert_eq!(values,[(0,7,1,8),(1,7,3,8),(2,8,1,8),(3,8,2,8),(5,8,1,8),(6,0,1,8)]);
        if let Ok(p)=std::env::var("OBS_RUST_STREAMED"){std::fs::write(p,out).unwrap();}
    }
    #[test]fn order_reset_and_epoch_binding(){let mut s=Order::default();s.accept(8,7,1,100).unwrap();assert!(s.accept(8,7,1,100).is_err());assert!(s.accept(9,7,2,101).is_err());s.accept(9,0,2,101).unwrap();s.accept(10,8,2,102).unwrap();assert!(s.accept(11,8,1,103).is_err());assert!(s.accept(11,9,1,0).is_err());}
    #[test]fn partial_eof_and_malformed_prefix(){let w=golden();for end in [1,23,25,71,121,551]{let result=Reader::new(&w[..end]);if let Ok(mut r)=result{assert!(loop{match r.next_observation(){Ok(Some(_))=>{},Ok(None)=>break false,Err(_)=>break true}});}}let mut w=w;w[24..28].copy_from_slice(&u32::MAX.to_le_bytes());assert!(Reader::new(&w[..]).unwrap().next_observation().is_err());}
    #[test]fn million_observations_bounded_and_slow_consumer(){
        let d=Descriptor{id:1,sample_rate:64.,bin0_hz:100.,enbw_bins:1.,tick_hz:64,bins:64,mode:1,source:1};let mut e=Encoder::new([3;16],vec![d]).unwrap();e.require_nint1_profile().unwrap();
        let q=Fanout::new(8,e.max_record()).unwrap();let budget=q.allocated_payload_bytes();let mut c=q.subscribe();let mut frame=Vec::with_capacity(e.max_record());let mut pending=Vec::with_capacity(e.max_record());
        let mut o=Observation{descriptor_id:1,seq:0,epoch:1,first_tick:0,nint:1,tag:NO_TAG,power:vec![0.;64]};let mut received=0;let mut prev=None;
        for seq in 0..1_000_000u64{o.seq=seq;o.first_tick=seq*64;o.epoch=1+seq/200000;for (i,x) in o.power.iter_mut().enumerate(){*x=((seq%1024)*16+i as u64%16) as f32/16384.;}e.observation(&o,&mut frame).unwrap();assert!(q.publish(&frame).unwrap());
            if seq%1000==999{while q.poll(&mut c,&mut pending){let seq=u64v(&pending,8);assert!(prev.is_none_or(|v|seq>v));assert_eq!(u64v(&pending,16),1+seq/200000);assert_eq!(u64v(&pending,24),seq*64);assert_eq!(f32::from_le_bytes(number(&pending,40)),((seq%1024)*16) as f32/16384.);prev=Some(seq);received+=1;}}
        }
        assert_eq!(received,8000);assert_eq!(c.missed,992000);assert_eq!(received+c.missed,q.published());assert_eq!(budget,q.allocated_payload_bytes());assert_eq!(frame.capacity(),e.max_record());assert_eq!(pending.capacity(),e.max_record());assert_eq!(o.power.capacity(),64);
        // Deliberate lock contention drops a whole record without blocking acquisition.
        let guard=q.ring.lock().unwrap();assert!(!q.publish(&frame).unwrap());drop(guard);assert_eq!(q.contention_drops.load(Ordering::Relaxed),1);
    }
}
