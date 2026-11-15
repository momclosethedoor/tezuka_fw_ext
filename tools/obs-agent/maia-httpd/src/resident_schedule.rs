//! RCS1 frozen resident map. No MMIO position is used to label a result.
#![allow(missing_docs)]
pub const ABI:u32=0x31534352;
#[derive(Clone,Debug)]
pub struct Entry {pub slot:u8,pub marker:bool,pub last:u8,pub route:u32,pub settle:u32,pub dwell:u32,pub context:u32,pub lo:u64}
impl Entry {pub fn words(&self)->[u32;4]{[self.slot as u32|((self.marker as u32)<<3)|((self.last as u32)<<4)|(self.route<<8),self.settle,self.dwell,self.context]}}
#[derive(Clone,Debug)]
pub struct Map {pub run:u32,pub map_id:u64,pub initial_slot:u8,pub initial_route:u32,pub repeat:bool,pub entries:Vec<Entry>}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub struct Context {pub visit:u32,pub phase:u8,pub position:usize,pub lap:u64,pub route:u32,pub descriptor:u16}
impl Map {
    pub fn validate(&self,last_run:u32,slot:u8)->Result<(),String>{
        let n=self.entries.len();
        if self.run==0 || self.run<=last_run || self.map_id==0 || !(1..=8).contains(&n) || slot>7 || slot!=self.initial_slot || self.initial_route>0xffffff {return Err("run/map/length/initial physical slot".into());}
        let mut prev=slot;let mut zone=None;
        for (i,e) in self.entries.iter().enumerate(){
            if e.slot>7 || !(e.slot==prev || e.slot==(prev+1)%8) || e.lo==0 || e.context==0 || e.route>0xffffff || e.last>7 {return Err("slot must hold/+1 modulo8; RF context/route".into());}
            if e.marker {if (e.last as usize)<i || e.last as usize>=n || zone.is_some_and(|z|i<=z){return Err("overlapping/backwards external transition zone".into());}zone=Some(e.last as usize);}
            else if e.last!=0 || e.route!=0 {return Err("non-marker reserved zone/route must be zero".into());}
            prev=e.slot;
        }
        if self.repeat && !(self.entries[0].slot==prev || self.entries[0].slot==(prev+1)%8){return Err("repeat closure must hold/+1 modulo8".into());}
        Ok(())
    }
    pub fn resolve(&self,epoch:u64)->Result<Context,String>{
        let visit=((epoch as u32)>>1) as u32;let phase=(epoch&1) as u8;
        if (epoch>>32)!=self.run as u64 || visit==0 {return Err("unknown run/visit epoch".into());}
        let n=self.entries.len();let position=(visit as usize-1)%n;let lap=(visit as u64-1)/n as u64;
        if !self.repeat && lap!=0{return Err("visit beyond one-shot map".into());}
        // A phase1 can only result from a marker in this lap/zone, at or before position.
        if phase!=0 && !self.entries.iter().enumerate().any(|(i,e)|e.marker && i<=position && position<=e.last as usize){return Err("phase1 outside a response zone".into());}
        let mut route=self.initial_route;
        if lap>0 {if let Some(e)=self.entries.iter().rev().find(|e|e.marker){route=e.route;}}
        for e in self.entries.iter().take(position+1){if e.marker {route=e.route;}}
        Ok(Context{visit,phase,position,lap,route,descriptor:position as u16+1})
    }
}
pub trait Registers {fn read(&self,offset:usize)->u32;fn write(&self,offset:usize,value:u32);}
pub fn check<R:Registers>(r:&R)->Result<(),String>{if r.read(0x50)!=crate::native_observation::ABI{return Err("OBN1 required before RCS1".into());}if r.read(0x80)!=ABI{return Err("RCS1 ID mismatch".into());}Ok(())}
pub fn load<R:Registers>(r:&R,map:&Map)->Result<(),String>{
    check(r)?;let status=r.read(0xac);if status&0x13!=0{return Err("load requires stopped/nonfaulted RCS1".into());}
    map.validate(r.read(0xd4),((status>>6)&7) as u8)?;
    for (i,e) in map.entries.iter().enumerate(){r.write(0x90,i as u32);for (j,w) in e.words().iter().enumerate(){r.write(0x94+4*j,*w);}r.write(0x84,9);}
    r.write(0x88,map.entries.len() as u32);r.write(0x8c,map.run);Ok(())
}
pub fn start<R:Registers>(r:&R,map:&Map)->Result<(),String>{check(r)?;map.validate(r.read(0xd4),((r.read(0xac)>>6)&7) as u8)?;r.write(0x84,3|if map.repeat{64}else{0});Ok(())}
pub fn stop<R:Registers>(r:&R,repeat:bool)->Result<(),String>{check(r)?;r.write(0x84,5|if repeat{64}else{0});Ok(())}
pub fn pending<R:Registers>(r:&R)->Result<Option<(u64,u32)>,String>{
    check(r)?;if r.read(0xac)&2==0{return Ok(None);}
    let lo=r.read(0xc0);let hi=r.read(0xc4);let info=r.read(0xc8);
    if r.read(0xac)&2==0{return Ok(None);}Ok(Some(((hi as u64)<<32|lo as u64,info)))
}
pub fn response<R:Registers>(r:&R,id:u64,ok:bool,repeat:bool)->Result<(),String>{
    if pending(r)?.map(|e|e.0)!=Some(id){return Err("stale/duplicate applied ID".into());}
    r.write(0xa4,id as u32);r.write(0xa8,(id>>32) as u32);r.write(0x84,17|if ok{32}else{0}|if repeat{64}else{0});Ok(())
}
#[cfg(test)] mod tests {
    use super::*;
    fn map()->Map{Map{run:7,map_id:u64::MAX,initial_slot:0,initial_route:3,repeat:true,entries:(1..=8).map(|i|Entry{slot:i%8,marker:i==1,last:if i==1{1}else{0},route:if i==1{9}else{0},settle:10,dwell:20,context:i as u32,lo:100+i as u64}).collect()}}
    #[test] fn circle_hold_and_initial_slot(){let mut m=map();m.validate(6,0).unwrap();assert!(m.validate(7,0).is_err());assert!(m.validate(6,1).is_err());m.entries.truncate(3);assert!(m.validate(0,0).is_err());m.repeat=false;m.validate(0,0).unwrap();m.entries[1].slot=1;m.entries[2].slot=2;m.validate(0,0).unwrap();m.entries[2].slot=7;assert!(m.validate(0,0).is_err());}
    #[test] fn immutable_context_late_lap_and_phase(){let m=map();let c=m.resolve((7u64<<32)|19).unwrap();assert_eq!((c.visit,c.lap,c.position,c.phase,c.route),(9,1,0,1,9));assert_eq!(m.resolve((7u64<<32)|2).unwrap().lap,0);assert!(m.resolve((8u64<<32)|2).is_err());assert!(m.resolve((7u64<<32)|7).is_err());}
    #[test] fn zones(){let mut m=map();m.entries[1].marker=true;m.entries[1].last=2;assert!(m.validate(0,0).is_err());}
    #[test] fn register_commands_ids_stop_and_stale(){
        use std::cell::RefCell;
        struct Mock{data:RefCell<[u32;64]>,writes:RefCell<Vec<(usize,u32)>>}
        impl Registers for Mock{fn read(&self,o:usize)->u32{self.data.borrow()[o/4]}fn write(&self,o:usize,v:u32){self.writes.borrow_mut().push((o,v));}}
        let r=Mock{data:RefCell::new([0;64]),writes:RefCell::new(Vec::new())};let m=map();
        assert!(load(&r,&m).is_err());assert!(r.writes.borrow().is_empty());
        r.data.borrow_mut()[0x50/4]=crate::native_observation::ABI;assert!(load(&r,&m).is_err());
        r.data.borrow_mut()[0x80/4]=ABI;load(&r,&m).unwrap();start(&r,&m).unwrap();assert_eq!(r.writes.borrow().last(),Some(&(0x84,0x43)));
        let id=(7u64<<32)|2;r.data.borrow_mut()[0xac/4]=6; // pending/HOLD model
        r.data.borrow_mut()[0xc0/4]=id as u32;r.data.borrow_mut()[0xc4/4]=(id>>32) as u32;
        let count=r.writes.borrow().len();assert!(response(&r,id+2,true,true).is_err());assert_eq!(count,r.writes.borrow().len());
        response(&r,id,false,true).unwrap();assert_eq!(r.writes.borrow().last(),Some(&(0x84,0x51)));
        response(&r,id,true,true).unwrap();assert_eq!(r.writes.borrow().last(),Some(&(0x84,0x71))); // late valid applied releases held model
        r.data.borrow_mut()[0xac/4]=0;assert!(response(&r,id,true,true).is_err());stop(&r,true).unwrap();assert_eq!(r.writes.borrow().last(),Some(&(0x84,0x45)));
    }
}
