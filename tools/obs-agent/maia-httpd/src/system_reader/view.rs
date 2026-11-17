//! OBN2 revision 3 core. No allocator is inferred from free memory or CSR bases.
//! A live adapter must own both DMA allocations and implement cache sync in copy.
#![allow(missing_docs)]
use super::continuous::{self as c, Header, Snapshot};
pub const SLOTS:usize=1024;
pub const VIEW_BYTES:usize=2048;
pub const FULL_LENGTH:usize=SLOTS*c::BYTES;
pub const VIEW_LENGTH:usize=SLOTS*VIEW_BYTES;

pub fn geometry(revision:u32,slots:u32,full:u32,view:u32)->Result<(),String>{
    if (revision,slots,full,view)!=(3,1024,32768,2048){return Err("OBN2 r3 geometry/revision mismatch before START".into())}Ok(())
}
/// Geometry alone is NOT allocation/ownership proof. Called only after a driver
/// has returned owned DMA addresses, exact lengths and valid mappings.
pub fn ranges(full:u64,full_len:usize,view:u64,view_len:usize)->Result<(),String>{
    if full==0||view==0||full%32768!=0||view%2048!=0||full_len!=FULL_LENGTH||view_len!=VIEW_LENGTH{return Err("DMA allocation size/alignment".into())}
    let end=full.checked_add(full_len as u64).ok_or("full overflow")?;
    let vend=view.checked_add(view_len as u64).ok_or("view overflow")?;
    if end>u32::MAX as u64||vend>u32::MAX as u64||!(end<=view||vend<=full){return Err("DMA allocation overlap/end overflow".into())}Ok(())
}
pub fn view_len(h:Header)->usize{match(h.flags>>3)&3{0=>0,1=>512,2=>1024,_=>2048}}
pub fn decode(full:&[u8],view:&[u8],power:&mut[f32])->Result<Header,String>{
    let h=c::header_revision(full,3)?;let n=view_len(h);
    if view.len()!=n{return Err("r3 active view length (ignore short-slot tail)".into())}
    c::normalize(full,h,1./750668f64.powi(2),power)?;
    if n!=0{
        for (j,group) in full.chunks_exact(8*(c::BINS/n)).enumerate(){
            let mut q=0;
            for word in group.chunks_exact(8){let w=u64::from_le_bytes(word.try_into().unwrap());let m=w&((1u64<<47)-1);let e=(w>>47)&7;
                let code=if m==0{0}else{64-m.leading_zeros()+2*e as u32};q=q.max(code as u8);}
            if view[j]!=q{return Err(format!("r3 overview mismatch point {j}: {} != {q}",view[j]))}
        }
    }Ok(h)
}
/// One owner. Both methods must invalidate the corresponding DMA cache range
/// and copy to owned memory. OFF never invokes copy_view.
pub trait PairIo{
    fn snapshot(&mut self)->Result<Snapshot,String>;
    fn fast_snapshot(&mut self)->Result<Snapshot,String>{self.snapshot()}
    fn batch_limit(&self)->usize{512}
    fn phase_timing(&self)->bool{false}
    fn full_read(&self)->bool{true}
    fn prepare_prefix(&mut self,_first:u64,_end:u64)->Result<(),String>{Ok(())}
    fn visit_pair<F:FnMut(Header,&[&[u8]],&[u8])->Result<(),String>>(&mut self,slot:usize,full:&mut[u8],view:&mut[u8],sink:&mut F)->Result<(),String>{
        self.copy_full(slot,full)?;let h=c::header_revision(full,3)?;let n=view_len(h);
        if n!=0{self.copy_view(slot,&mut view[..n])?;}sink(h,&[full],&view[..n])
    }
    fn copy_full(&mut self,slot:usize,out:&mut[u8])->Result<(),String>;
    fn copy_view(&mut self,slot:usize,out:&mut[u8])->Result<(),String>;
    fn release(&mut self,run:u32,tail:u64)->Result<(),String>;
}
/// Copy both products completely before releasing; callers only enqueue private data.
pub(super) fn copy_release<I:PairIo,F:FnOnce()>(io:&mut I,run:u32,ordinal:u64,full:&mut[u8],overview:&mut[u8],copied:F)->Result<Header,String>{
 let slot=ordinal as usize&(SLOTS-1);
 io.copy_full(slot,full)?;let h=c::header_revision(full,3)?;
 if h.ordinal!=ordinal||view_len(h)!=overview.len(){return Err("owned full/VIEW identity".into())}
 io.copy_view(slot,overview)?;copied();io.release(run,ordinal+1)?;Ok(h)
}
pub struct PairReader{pub full_read_ns:u128,pub view_own_ns:u128,pub after_release_ns:u128,pub(super) run:u32,pub(super) next:u64,full:Vec<u8>,view:Vec<u8>,power:Vec<f32>,pub read:u64,pub consumed:u64,pub visited:u64,pub high_water:u64}
impl PairReader{
    /// Finish all DMA reads, release, then expose only the small owned VIEW.
    /// No full-spectrum queue or checksum is part of this path.
    pub fn service_released<I:PairIo,Rd:FnMut(&[u8]),F:FnMut(Header,&[u8])->Result<(),String>>(&mut self,io:&mut I,mut read:Rd,mut sink:F)->Result<Snapshot,String>{
        let phase=io.phase_timing();
        let s=io.fast_snapshot()?;
        if s.cookie!=self.run||s.released!=self.next||s.committed<self.next||s.committed>s.reserved||s.reserved-self.next>SLOTS as u64{return Err("r3 cursor/owner violation".into())}
        self.high_water=self.high_water.max(s.reserved-self.next);
        let end=s.committed.min(self.next+io.batch_limit() as u64);
        io.prepare_prefix(self.next,end)?;
        let mut owned_view=[0u8;VIEW_BYTES];
        for ordinal in self.next..end{
            let mut header=None;let mut n=0;
            io.visit_pair(ordinal as usize&(SLOTS-1),&mut self.full,&mut self.view,&mut |h,parts,v|{
                if h.ordinal!=ordinal{return Err("r3 ordinal mismatch; no release".into())}
                let t=phase.then(std::time::Instant::now);
                n=v.len();owned_view[..n].copy_from_slice(v);header=Some(h);
                if let Some(t)=t{self.view_own_ns+=t.elapsed().as_nanos();}
                let t=phase.then(std::time::Instant::now);
                for part in parts{read(part);}
                if let Some(t)=t{self.full_read_ns+=t.elapsed().as_nanos();}
                Ok(())
            })?;
            io.release(self.run,ordinal+1)?;
            self.next=ordinal+1;self.visited+=1;
            if io.full_read(){self.read+=1;self.consumed+=1;}
            // The DMA slot may already be overwritten here.
            let t=phase.then(std::time::Instant::now);
            sink(header.ok_or("missing header")?,&owned_view[..n])?;
            if let Some(t)=t{self.after_release_ns+=t.elapsed().as_nanos();}
        }
        if s.fault{return Err("r3 fatal after committed prefix".into())}Ok(s)
    }
    /// Full-payload local consumer, without costly float conversion/reference
    /// comparison. Every counted record is copied in full into owned storage.
    pub fn service_raw<I:PairIo,F:FnMut(Header,&[&[u8]],&[u8])->Result<(),String>>(&mut self,io:&mut I,mut sink:F)->Result<Snapshot,String>{
        let s=io.snapshot()?;
        if s.cookie!=self.run||s.released!=self.next||s.committed<self.next||s.committed>s.reserved||s.reserved-self.next>SLOTS as u64{return Err("r3 cursor/owner violation".into())}
        self.high_water=self.high_water.max(s.reserved-self.next);
        let end=s.committed.min(self.next+io.batch_limit() as u64);
        io.prepare_prefix(self.next,end)?;
        for ordinal in self.next..end{
            let slot=ordinal as usize&(SLOTS-1);
            io.visit_pair(slot,&mut self.full,&mut self.view,&mut |h,parts,v|{
                if h.ordinal!=ordinal{return Err(format!("r3 ordinal mismatch expected {ordinal}, got {}; no release",h.ordinal))}
                sink(h,parts,v)
            })?;
            self.visited+=1;if io.full_read(){self.read+=1;self.consumed+=1;}
            io.release(self.run,ordinal+1)?;self.next=ordinal+1;
        }
        if s.fault{return Err("r3 fatal after committed prefix".into())}Ok(s)
    }
    pub fn attach(s:Snapshot)->Result<Self,String>{
        if s.cookie==0||s.fault||s.released>s.committed||s.committed>s.reserved||s.reserved-s.released>SLOTS as u64{return Err("r3 attach cursors".into())}
        Ok(Self{full_read_ns:0,view_own_ns:0,after_release_ns:0,run:s.cookie,next:s.released,full:vec![0;c::BYTES],view:vec![0;VIEW_BYTES],power:vec![0.;c::BINS],read:0,consumed:0,visited:0,high_water:s.reserved-s.released})
    }
    /// Sink consumes all 4096 bins synchronously or copies them into its own
    /// bounded storage. A socket client must never be this sink.
    pub fn service<I:PairIo,F:FnMut(Header,&[f32],&[u8])->Result<(),String>>(&mut self,io:&mut I,mut sink:F)->Result<Snapshot,String>{
        let s=io.snapshot()?;
        if s.cookie!=self.run||s.released!=self.next||s.committed<self.next||s.committed>s.reserved||s.reserved-self.next>SLOTS as u64{return Err("r3 cursor/owner violation".into())}
        self.high_water=self.high_water.max(s.reserved-self.next);
        for ordinal in self.next..s.committed{
            let slot=ordinal as usize&(SLOTS-1);io.copy_full(slot,&mut self.full)?;
            let h=c::header_revision(&self.full,3)?;
            if h.ordinal!=ordinal{return Err("r3 ordinal mismatch, no release".into())}
            let n=view_len(h);if n!=0{io.copy_view(slot,&mut self.view[..n])?;}
            decode(&self.full,&self.view[..n],&mut self.power)?;self.read+=1;
            sink(h,&self.power,&self.view[..n])?;self.consumed+=1;
            io.release(self.run,ordinal+1)?;self.next=ordinal+1;
        }
        if s.fault{return Err("r3 fatal after committed prefix; coordinated reset required".into())}Ok(s)
    }
}

#[cfg(test)]mod tests{
 use super::*;
 fn root()->std::path::PathBuf{std::env::var("OBS_VIEW_VECTOR_ROOT").unwrap_or_else(|_|"maia-httpd/tests/view".into()).into()}
 fn bytes(p:&std::path::Path)->Vec<u8>{let s=std::fs::read_to_string(p).unwrap();let s=s.split_whitespace().collect::<String>();(0..s.len()).step_by(2).map(|i|u8::from_str_radix(&s[i..i+2],16).unwrap()).collect()}
 #[test]fn all_paired_i5_vectors(){let mut count=0;for mode in 0..4{for entry in std::fs::read_dir(root().join(format!("mode{mode}"))).unwrap(){let p=entry.unwrap().path();if p.extension().is_none_or(|s|s!="hex"){continue}let full=bytes(&p);let view=bytes(&p.with_extension("viewhex"));let mut power=vec![0.;4096];let h=decode(&full,&view,&mut power).unwrap();assert_eq!((h.flags>>3)&3,mode);assert!(c::header(&full).is_err());if !view.is_empty(){let mut bad=view.clone();bad[0]^=1;assert!(decode(&full,&bad,&mut power).is_err());assert!(decode(&full,&view[..view.len()-1],&mut power).is_err());}count+=1;}}assert_eq!(count,16);}
 #[test]fn geometry_and_memory_gate(){assert!(geometry(3,1024,32768,2048).is_ok());assert!(geometry(2,8,32768,2048).is_err());assert!(ranges(0x18000000,FULL_LENGTH,0x1a000000,VIEW_LENGTH).is_ok());for(a,b,l)in[(0x18000001,0x1a000000,FULL_LENGTH),(0x18000000,0x18008000,FULL_LENGTH),(0x18000000,0x1a000000,262144)]{assert!(ranges(a,l,b,VIEW_LENGTH).is_err());}}
 #[test]fn wrap_and_mode_from_each_header(){
  struct Mock{full:Vec<u8>,view:Vec<u8>,s:Snapshot,vc:u64}
  impl PairIo for Mock{fn snapshot(&mut self)->Result<Snapshot,String>{Ok(self.s)}fn copy_full(&mut self,slot:usize,out:&mut[u8])->Result<(),String>{assert_eq!(slot,self.s.released as usize&1023);out.copy_from_slice(&self.full);Ok(())}fn copy_view(&mut self,slot:usize,out:&mut[u8])->Result<(),String>{assert_eq!(slot,self.s.released as usize&1023);assert_eq!(out.len(),self.view.len());out.copy_from_slice(&self.view);self.vc+=1;Ok(())}fn release(&mut self,run:u32,tail:u64)->Result<(),String>{assert_eq!(run,7);assert_eq!(tail,self.s.released+1);self.s.released=tail;Ok(())}}
  let mut io=Mock{full:vec![],view:vec![],s:Snapshot{cookie:7,committed:0,reserved:0,released:0,..Default::default()},vc:0};let mut r=PairReader::attach(io.s).unwrap();let mut expected_views=0;
  for ordinal in 0..2050u64{let mode=ordinal%4;let p=root().join(format!("mode{mode}/ordinal-0-k2.hex"));io.full=bytes(&p);io.view=bytes(&p.with_extension("viewhex"));for(i,b)in ordinal.to_le_bytes().iter().enumerate(){io.full[(56+i)*8+7]=*b;}io.s.committed+=1;io.s.reserved+=1;if mode!=0{expected_views+=1;}r.service(&mut io,|h,p,v|{assert_eq!(h.ordinal,ordinal);assert_eq!(p.len(),4096);assert_eq!(v.len(),view_len(h));Ok(())}).unwrap();assert_eq!(io.s.released,ordinal+1);}
  assert_eq!(r.consumed,514);assert_eq!(io.vc,expected_views);
 }
}
