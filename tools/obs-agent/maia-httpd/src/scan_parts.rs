//! SCN1 v2 ARM-to-host parts. Software transport, independent of PL CSR layout.
//! Each call flushes already prepared parts immediately; count=1 is normal.
#![allow(missing_docs)]
use crate::continuous::{PreparedPart,BINS};
pub const COMMON:usize=64;
pub const MAX_PARTS:usize=4;
pub const PART_BYTES:usize=64+4*BINS;
pub const MAX_WIRE:usize=COMMON+MAX_PARTS*PART_BYTES;
#[derive(Clone,Copy,Debug,PartialEq,Eq)]pub struct Identity{pub capture:[u8;16],pub map:u64,pub run:u64}
#[derive(Clone,Debug)]pub struct Entry{pub epoch:u64,pub lo:u64,pub offset:u32,pub context:u32,pub route:u32,pub flags:u32}
#[derive(Clone,Debug)]pub struct Descriptor{pub identity:Identity,pub fs:f64,pub bin0:f64,pub step:f64,pub bins:u32,pub tick_hz:u64,pub enbw:f64,pub entries:Vec<Entry>}
impl Descriptor{
    pub fn validate(&self)->Result<(),String>{
        if self.identity.capture==[0;16]||self.identity.map==0||self.identity.run==0||!self.fs.is_finite()||self.fs<=0.||!self.bin0.is_finite()||!self.step.is_finite()||self.step<=0.||self.tick_hz!=62_500_000||!self.enbw.is_finite()||self.enbw<=0.||!(4096..=32768).contains(&self.bins)||self.entries.is_empty()||self.entries.len()>8||(self.step-self.fs/4096.).abs()>self.step*1e-10{return Err("SCN1 descriptor bounds/grid".into())}
        for(i,e)in self.entries.iter().enumerate(){if e.epoch==0||e.lo==0||e.context==0||e.flags&!6!=0||e.offset as u64+4096>self.bins as u64||e.route>0xffffff||self.entries[..i].iter().any(|p|p.epoch==e.epoch)||((e.lo as f64-self.fs/2.)-(self.bin0+e.offset as f64*self.step)).abs()>self.step*1e-6{return Err("SCN1 entry provenance/placement".into())}}
        Ok(())
    }
}
fn common(out:&mut Vec<u8>,id:Identity,kind:u16,count:u32,seq:u64,floor:u64){out.clear();out.extend_from_slice(b"SCN1");out.extend_from_slice(&2u16.to_le_bytes());out.extend_from_slice(&kind.to_le_bytes());out.extend_from_slice(&0u32.to_le_bytes());out.extend_from_slice(&count.to_le_bytes());out.extend_from_slice(&id.capture);for v in[id.map,id.run,seq,floor]{out.extend_from_slice(&v.to_le_bytes())}}
fn finish(out:&mut[u8]){let n=out.len()as u32;out[8..12].copy_from_slice(&n.to_le_bytes())}
pub fn descriptor(d:&Descriptor,seq:u64,out:&mut Vec<u8>)->Result<(),String>{d.validate()?;common(out,d.identity,1,d.entries.len()as u32,seq,0);for v in[d.fs,d.bin0,d.step]{out.extend_from_slice(&v.to_le_bytes())}out.extend_from_slice(&d.bins.to_le_bytes());out.extend_from_slice(&(d.entries.len()as u32).to_le_bytes());out.extend_from_slice(&d.tick_hz.to_le_bytes());out.extend_from_slice(&d.enbw.to_le_bytes());for e in &d.entries{out.extend_from_slice(&e.epoch.to_le_bytes());out.extend_from_slice(&e.lo.to_le_bytes());for v in[e.offset,e.context,e.route,e.flags]{out.extend_from_slice(&v.to_le_bytes())}}finish(out);Ok(())}
/// One ready part. No normalization, accumulation, geometry or wait here.
pub fn part(d:&Descriptor,seq:u64,pass:u64,p:&PreparedPart<'_>,out:&mut Vec<u8>)->Result<(),String>{
    let e=d.entries.get(p.position as usize).ok_or("part position")?;let h=p.header;
    if !h.clean()||h.epoch!=e.epoch||h.flags&6!=e.flags||p.power.len()!=4096||p.power.iter().any(|x|!x.is_finite()||*x<0.){return Err("prepared part/descriptor mismatch".into())}
    common(out,d.identity,2,1,seq,pass);out.extend_from_slice(&pass.to_le_bytes());out.extend_from_slice(&p.position.to_le_bytes());out.extend_from_slice(&h.k.to_le_bytes());for v in[h.first_seq,h.last_seq,h.epoch,h.first_tick]{out.extend_from_slice(&v.to_le_bytes())}out.extend_from_slice(&h.flags.to_le_bytes());out.extend_from_slice(&4096u32.to_le_bytes());out.extend_from_slice(&h.ordinal.to_le_bytes());for v in p.power{out.extend_from_slice(&v.to_le_bytes())}finish(out);Ok(())
}
/// Ordered input progress: passes below floor cannot receive further parts.
/// Static/replay assigns passes explicitly; not a claim about RF pass boundaries.
pub fn progress(id:Identity,seq:u64,floor:u64,terminal:bool,out:&mut Vec<u8>){common(out,id,3,terminal as u32,seq,floor);finish(out)}
/// Combine only parts which are already ready. Never wait to fill this batch.
pub fn coalesce(frames:&[&[u8]],out:&mut Vec<u8>)->Result<(),String>{
    if frames.is_empty()||frames.len()>4{return Err("batch count".into())}
    let first=frames[0];if first.len()!=COMMON+PART_BYTES{return Err("part size".into())}out.clear();out.extend_from_slice(&first[..COMMON]);
    for f in frames{if f.len()!=COMMON+PART_BYTES||f[..8]!=first[..8]||f[12..16]!=1u32.to_le_bytes()||f[16..48]!=first[16..48]{return Err("batch identity/type".into())}out.extend_from_slice(&f[COMMON..]);}out[12..16].copy_from_slice(&(frames.len()as u32).to_le_bytes());finish(out);Ok(())
}
#[cfg(test)]mod tests{
    use super::*;use crate::continuous::Header;
    #[test]fn cross_language_ready_parts(){let d=Descriptor{identity:Identity{capture:[0x66;16],map:31,run:9},fs:61_440_000.,bin0:2_400_000_000.-30_720_000.,step:15_000.,bins:8192,tick_hz:62_500_000,enbw:2.0050129965307675,entries:vec![Entry{epoch:7,lo:2_400_000_000,offset:0,context:1,route:0,flags:0},Entry{epoch:8,lo:2_461_440_000,offset:4096,context:2,route:0,flags:0}]};let mut desc=Vec::new();descriptor(&d,0,&mut desc).unwrap();assert_eq!(desc.len(),176);let p=vec![0.125;4096];let h=Header{first_seq:1,last_seq:2,epoch:7,first_tick:1000,k:2,flags:1,ordinal:0};let mut a=Vec::new();part(&d,1,0,&PreparedPart{header:h,position:0,power:&p},&mut a).unwrap();assert_eq!(a.len(),COMMON+PART_BYTES);let mut b=Vec::new();part(&d,2,0,&PreparedPart{header:Header{epoch:8,first_seq:3,last_seq:4,first_tick:10000,..h},position:1,power:&p},&mut b).unwrap();let mut batch=Vec::new();coalesce(&[&a,&b],&mut batch).unwrap();assert_eq!(batch.len(),COMMON+2*PART_BYTES);if let Ok(dir)=std::env::var("OBS_CONTINUOUS_OUTPUT_DIR"){std::fs::create_dir_all(&dir).unwrap();for(n,v)in[("descriptor.scn",desc),("part0.scn",a),("part1.scn",b),("batch.scn",batch)]{std::fs::write(std::path::Path::new(&dir).join(n),v).unwrap()}}}
}
