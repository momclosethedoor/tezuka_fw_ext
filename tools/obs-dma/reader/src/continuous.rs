//! Continuous owned-copy reader. Hardware binding requires the versioned PL contract.
//! OBN2 r2/encoding2 contract: goofy_docs 449b79e8. Golden acceptance is separate.
#![allow(missing_docs)]
use std::collections::VecDeque;
pub const BYTES:usize=32768;
pub const BINS:usize=4096;
pub const SLOTS:usize=8;
pub const ABI:u32=0x324e424f;
pub const TICK_HZ:u64=62_500_000;
/// Explicit diagnostic DDR layout, selected only at committed -> owned copy.
#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]pub enum CopyLayout{#[default]Normal,Ram544}
/// No magic search. ordinal is the absolute DMA cursor, never the START/run id.
/// The cold ordinal0 prefix must be protected with first_per_epoch until copied.
pub fn copy_committed<F:FnMut(usize,std::ops::Range<usize>,&mut[u8])->Result<(),String>>(ordinal:u64,layout:CopyLayout,out:&mut[u8],mut copy:F)->Result<(),String>{
    if out.len()!=BYTES{return Err("committed copy bounds".into())}
    let slot=(ordinal%SLOTS as u64)as usize;
    match layout{
        CopyLayout::Normal=>copy(slot,0..BYTES,out),
        CopyLayout::Ram544=>{let prefix=544*8;let suffix=BYTES-prefix;let previous=(slot+SLOTS-1)%SLOTS;let offset=if ordinal==0{0}else{suffix};let(a,b)=out.split_at_mut(prefix);copy(previous,offset..offset+prefix,a)?;copy(slot,0..suffix,b)}
    }
}

#[cfg(test)]mod copy_tests{
    use super::*;
    #[test]fn explicit_segments_cold_then_wrap(){
        for ordinal in 0..26u64{for layout in [CopyLayout::Normal,CopyLayout::Ram544]{
            let expected:Vec<u8>=(0..BYTES).map(|j|((j*31+j/257+ordinal as usize)%251)as u8).collect();let mut ring=vec![0;BYTES*SLOTS];let mut spans=Vec::new();let mut out=vec![0;BYTES];
            copy_committed(ordinal,layout,&mut out,|slot,range,b|{spans.push((slot,range.start,range.end));Ok(())}).unwrap();
            let mut offset=0;for &(slot,start,end)in &spans{let n=end-start;ring[slot*BYTES+start..slot*BYTES+end].copy_from_slice(&expected[offset..offset+n]);offset+=n;}assert_eq!(offset,BYTES);
            copy_committed(ordinal,layout,&mut out,|slot,range,b|{b.copy_from_slice(&ring[slot*BYTES+range.start..slot*BYTES+range.end]);Ok(())}).unwrap();assert_eq!(out,expected);
            if layout==CopyLayout::Ram544{assert_eq!(spans,vec![(((ordinal+7)%8)as usize,if ordinal==0{0}else{28416},if ordinal==0{4352}else{32768}),((ordinal%8)as usize,0,28416)]);}
        }}
    }
    /// Local physical bytes stay private. Execute the CI-built Windows test binary
    /// with OBS_RAM_CAPTURE_ROOT after independent Python prepared each case.
    #[test]fn saved_physical_captures(){
        let Some(root)=std::env::var_os("OBS_RAM_CAPTURE_ROOT")else{return};let mut count=0;
        for entry in std::fs::read_dir(root).unwrap(){let p=entry.unwrap().path();if !p.is_dir(){continue}let name=p.file_name().unwrap().to_str().unwrap();let mut fields=name.split('-');let layout=if fields.next()==Some("normal"){CopyLayout::Normal}else{CopyLayout::Ram544};let ordinal:u64=fields.next().unwrap().parse().unwrap();let ring=std::fs::read(p.join("ring.bin")).unwrap();let expected=std::fs::read(p.join("record.bin")).unwrap();let mut out=vec![0;BYTES];copy_committed(ordinal,layout,&mut out,|slot,range,b|{b.copy_from_slice(&ring[slot*BYTES+range.start..slot*BYTES+range.end]);Ok(())}).unwrap();assert_eq!(out,expected,"{name}");let h=header(&out).unwrap();assert_eq!(h.ordinal,ordinal);let mut bins=vec![0.;BINS];normalize(&out,h,1./750668f64.powi(2),&mut bins).unwrap();let f32bytes:Vec<u8>=bins.iter().flat_map(|v|v.to_le_bytes()).collect();assert_eq!(f32bytes,std::fs::read(p.join("power.f32")).unwrap(),"all4096 {name}");count+=1;
        }assert!(count>=19);println!("physical capture cases={count}, exact records/metadata/all4096 f32 PASS");
    }
}
fn u16v(b:&[u8],i:usize)->u16{u16::from_le_bytes(b[i..i+2].try_into().unwrap())}
fn u32v(b:&[u8],i:usize)->u32{u32::from_le_bytes(b[i..i+4].try_into().unwrap())}
fn u64v(b:&[u8],i:usize)->u64{u64::from_le_bytes(b[i..i+8].try_into().unwrap())}
#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct Header{pub first_seq:u64,pub last_seq:u64,pub epoch:u64,pub first_tick:u64,pub k:u32,pub flags:u32,pub ordinal:u64}
impl Header{pub fn clean(self)->bool{self.flags&1!=0}pub fn peak(self)->bool{self.flags&4!=0}}
/// Cheap metadata validation, before payload normalization or serialization.
pub fn header(b:&[u8])->Result<Header,String>{
    header_revision(b,2)
}
pub fn header_revision(b:&[u8],revision:u16)->Result<Header,String>{
    if !matches!(revision,2|3){return Err("unsupported OBN2 revision".into())}
    if b.len()!=BYTES{return Err("OBN2 slot length".into())}
    let mut h=[0u8;64];for j in 0..64{h[j]=b[j*8+7];}
    if u32v(&h,0)!=ABI || u16v(&h,4)!=revision || u16v(&h,6)!=64 || u32v(&h,32)!=4096 || u32v(&h,40)!=TICK_HZ as u32 || u32v(&h,48)!=2 || u32v(&h,52)!=BYTES as u32{return Err("OBN2 ABI/revision/encoding/constants".into())}
    let k=u32v(&h,36);if !(1..=1023).contains(&k){return Err("OBN2 actualK".into())}
    let first_seq=u64v(&h,8);let last_seq=first_seq.checked_add(k as u64-1).ok_or("OBN2 sequence exhaustion")?;
    let x=Header{first_seq,epoch:u64v(&h,16),first_tick:u64v(&h,24),k,flags:u32v(&h,44),last_seq,ordinal:u64v(&h,56)};
    if x.flags&!(if revision==3{31}else{7})!=0 || !x.clean() || x.epoch==0 || x.first_tick==u64::MAX{return Err("OBN2 count/flags/continuous clean group".into())}
    Ok(x)
}
/// All bins in encoding2 are already natural fftshifted. No bit reversal.
pub fn normalize(b:&[u8],h:Header,scale:f64,out:&mut[f32])->Result<(),String>{
    if b.len()!=BYTES || out.len()!=BINS || !scale.is_finite() || scale<=0.{return Err("OBN2 conversion bounds/scale".into())}
    // Validate the complete payload before changing the prepared product.
    for (j,w) in b.chunks_exact(8).enumerate(){let w=u64v(w,0);if (w>>47)&7>4 || (w>>50)&63!=0 || j>=64 && w>>56!=0{return Err("OBN2 M47E3/reserved".into())}}
    let scale=scale/if h.peak(){1.}else{h.k as f64};
    for (dst,w) in out.iter_mut().zip(b.chunks_exact(8)){let w=u64v(w,0);*dst=(((w&((1u64<<47)-1)) as f64)*(1u64<<(2*((w>>47)&7))) as f64*scale) as f32;if !dst.is_finite(){return Err("OBN2 power overflow".into())}}
    Ok(())
}
#[derive(Clone,Copy,Debug,Default)]
pub struct Snapshot{pub input_seq:u64,pub finished:u64,pub unselected:u64,pub dirty_drop:u32,pub stop_drop:u32,pub stop_input_limit:u64,pub cookie:u32,pub committed:u64,pub reserved:u64,pub released:u64,pub selected:u64,pub dropped:u64,pub running:bool,pub dma_busy:bool,pub drained:bool,pub fault:bool}
/// Implement only from i5's final CONTRACT. copy_slot includes cache invalidation.
pub trait RingIo{fn start(&mut self)->Result<(),String>;fn snapshot(&mut self)->Result<Snapshot,String>;fn copy_slot(&mut self,slot:usize,out:&mut[u8])->Result<(),String>;fn copy_record(&mut self,ordinal:u64,out:&mut[u8])->Result<(),String>{self.copy_slot((ordinal%SLOTS as u64)as usize,out)}fn release(&mut self,cookie:u32,tail:u64)->Result<(),String>;fn stop(&mut self)->Result<(),String>;}
#[derive(Default,Debug)]pub struct ReaderStats{pub copied:u64,pub serviced:u64,pub high_water:u64,pub max_service_us:u64,pub max_interval_us:u64}
pub struct Reader{cookie:u32,next:u64,stopping:bool,previous:Snapshot,last_service:std::time::Instant,scratch:Vec<u8>,pub stats:ReaderStats}
impl Reader{
    pub fn attach(s:Snapshot)->Result<Self,String>{if s.cookie==0 || s.fault || s.released>s.committed || s.reserved<s.committed || s.reserved-s.released>SLOTS as u64{return Err("ring attach snapshot".into())}Ok(Self{cookie:s.cookie,next:s.released,stopping:false,previous:s,last_service:std::time::Instant::now(),scratch:vec![0;BYTES],stats:Default::default()})}
    fn checked(&self,s:Snapshot)->Result<(),String>{
        if s.cookie!=self.cookie || s.released!=self.next || s.committed<self.previous.committed || s.selected<self.previous.selected || s.dropped<self.previous.dropped || s.committed<s.released || s.reserved<s.committed || s.reserved-s.released>SLOTS as u64{return Err("ring lifetime/cursor/ownership/fault".into())}Ok(())
    }
    /// Drain what is ready now, never wait for a full ring/batch. Release each
    /// owned record before queueing; processing receives no borrowed DMA memory.
    pub fn service<I:RingIo>(&mut self,io:&mut I,q:&mut OwnedQueue)->Result<Snapshot,String>{
        let now=std::time::Instant::now();self.stats.max_interval_us=self.stats.max_interval_us.max(now.duration_since(self.last_service).as_micros()as u64);self.last_service=now;let s=io.snapshot()?;self.checked(s)?;self.stats.high_water=self.stats.high_water.max(s.reserved-self.next);
        for tail in self.next..s.committed{
            io.copy_record(tail,&mut self.scratch)?;
            if header(&self.scratch)?.ordinal!=tail{return Err("committed slot ordinal mismatch; no release".into())}
            io.release(self.cookie,tail+1)?;self.next=tail+1;
            q.push_swap(&mut self.scratch);self.stats.copied+=1;
        }
        self.previous=s;self.stats.serviced+=1;self.stats.max_service_us=self.stats.max_service_us.max(now.elapsed().as_micros()as u64);if s.fault{return Err("PL fatal after draining valid committed prefix".into())}Ok(s)
    }
    pub fn stop<I:RingIo>(&mut self,io:&mut I)->Result<(),String>{if !self.stopping{io.stop()?;self.stopping=true;}Ok(())}
    pub fn drained(&self,s:Snapshot)->bool{self.stopping&&s.drained&&!s.running&&!s.dma_busy&&self.next==s.committed&&s.reserved==s.committed}
}
/// Fixed pool; overwrite only an owned, unprocessed record, with visible loss.
pub struct OwnedQueue{slots:Vec<Vec<u8>>,head:usize,len:usize,pub dropped:u64}
impl OwnedQueue{
    pub fn new(cap:usize)->Result<Self,String>{if !(1..=8).contains(&cap){return Err("owned queue capacity".into())}Ok(Self{slots:(0..cap).map(|_|vec![0;BYTES]).collect(),head:0,len:0,dropped:0})}
    fn push_swap(&mut self,b:&mut Vec<u8>){if self.len==self.slots.len(){self.head=(self.head+1)%self.slots.len();self.len-=1;self.dropped+=1;}let i=(self.head+self.len)%self.slots.len();std::mem::swap(&mut self.slots[i],b);self.len+=1;}
    pub fn pop_swap(&mut self,out:&mut Vec<u8>)->bool{assert_eq!(out.len(),BYTES);if self.len==0{return false}std::mem::swap(out,&mut self.slots[self.head]);self.head=(self.head+1)%self.slots.len();self.len-=1;true}
    pub fn storage_bytes(&self)->usize{self.slots.iter().map(|s|s.capacity()).sum()}
}
#[derive(Clone,Debug)]pub struct Context{pub epoch:u64,pub position:u32,pub flags:u32,pub scale:f64}
#[derive(Default,Debug)]pub struct PrepareStats{pub dirty:u64,pub duplicate:u64,pub unselected:u64,pub normalized:u64,pub gaps:u64}
/// The selector updates acceptance only after full validation/normalization.
/// tick_period limits publication, not K. Zero means no temporal thinning.
pub struct Prepare{contexts:Vec<Context>,last:Option<Header>,last_selected:Vec<Option<Header>>,pub stats:PrepareStats,pub tick_period:u64}
pub struct PreparedPart<'a>{pub header:Header,pub position:u32,pub power:&'a[f32]}
impl Prepare{
    pub fn new(contexts:Vec<Context>,tick_period:u64)->Result<Self,String>{
        if contexts.is_empty()||contexts.len()>8{return Err("context bound".into())}
        for(i,c)in contexts.iter().enumerate(){if c.epoch==0||c.position as usize!=i||c.flags&!6!=0||!c.scale.is_finite()||c.scale<=0.||contexts[..i].iter().any(|p|p.epoch==c.epoch){return Err("immutable context".into())}}
        let n=contexts.len();Ok(Self{contexts,last:None,last_selected:vec![None;n],stats:Default::default(),tick_period})
    }
    pub fn accept<'a>(&mut self,b:&[u8],power:&'a mut[f32])->Result<Option<PreparedPart<'a>>,String>{
        self.accept_if(b,power,|_,_|true)
    }
    pub fn accept_if<'a,F:FnOnce(Header,&Context)->bool>(&mut self,b:&[u8],power:&'a mut[f32],selected:F)->Result<Option<PreparedPart<'a>>,String>{
        let h=header(b)?;
        if let Some(prev)=self.last{
            if h==prev{self.stats.duplicate+=1;return Ok(None)}
            if h.first_seq<=prev.last_seq||h.first_tick<=prev.first_tick{return Err("input group reset/order/conflicting repeat".into())}
            self.stats.gaps+=h.first_seq-prev.last_seq-1;
        }
        self.last=Some(h);
        if !h.clean(){self.stats.dirty+=1;return Ok(None)}
        let c=self.contexts.iter().find(|c|c.epoch==h.epoch).ok_or("unknown carried epoch")?;
        if h.flags&6!=c.flags{return Err("carried source/mode differs from descriptor".into())}
        let i=c.position as usize;
        if !selected(h,c){self.stats.unselected+=1;return Ok(None)}
        if self.last_selected[i].is_some_and(|p|h.first_tick-p.first_tick<self.tick_period){self.stats.unselected+=1;return Ok(None)}
        normalize(b,h,c.scale,power)?;self.last_selected[i]=Some(h);self.stats.normalized+=1;
        Ok(Some(PreparedPart{header:h,position:c.position,power}))
    }
}

/// Shared bounded batch hub. Descriptor is a mandatory reconnect preamble;
/// it is not evicted by data. No acquisition lock is held during socket sends.
#[derive(Debug)]pub struct PartsHub{generation:u64,descriptor:Vec<u8>,batches:VecDeque<(u64,Vec<u8>)>,next:u64,capacity:usize,max:usize,pub evicted:u64}
#[derive(Default)]pub struct Cursor{generation:u64,last:u64,pub missed:u64}
pub fn publish_ready(hub:&std::sync::Mutex<PartsHub>,data:&[u8])->Result<u64,String>{
    let begin=std::time::Instant::now();
    hub.lock().map_err(|_|"hub lock poisoned")?.publish(data)?;
    Ok(begin.elapsed().as_micros() as u64)
}
impl PartsHub{
    pub fn new(capacity:usize,max:usize)->Result<Self,String>{if !(1..=8).contains(&capacity)||max>262144||max<64{return Err("batch hub bound".into())}Ok(Self{generation:0,descriptor:Vec::new(),batches:VecDeque::with_capacity(capacity),next:0,capacity,max,evicted:0})}
    pub fn descriptor(&mut self,data:&[u8])->Result<(),String>{if data.len()>self.max{return Err("descriptor maximum".into())}self.generation=self.generation.checked_add(1).ok_or("generation exhaustion")?;self.descriptor.clear();self.descriptor.extend_from_slice(data);self.batches.clear();Ok(())}
    pub fn publish(&mut self,data:&[u8])->Result<(),String>{if self.generation==0||data.len()>self.max{return Err("missing descriptor/batch maximum".into())}self.next=self.next.checked_add(1).ok_or("batch exhaustion")?;if self.batches.len()==self.capacity{self.batches.pop_front();self.evicted+=1;}self.batches.push_back((self.next,data.to_vec()));Ok(())}
    pub fn poll(&self,c:&mut Cursor,out:&mut Vec<u8>)->bool{
        if c.generation!=self.generation{if self.generation==0{return false}c.generation=self.generation;c.last=self.batches.front().map(|x|x.0-1).unwrap_or(self.next);out.clear();out.extend_from_slice(&self.descriptor);return true}
        if let Some((n,b))=self.batches.iter().find(|(n,_)|*n>c.last){c.missed+=n.saturating_sub(c.last+1);c.last=*n;out.clear();out.extend_from_slice(b);return true}false
    }
    pub fn storage_bytes(&self)->usize{self.descriptor.capacity()+self.batches.iter().map(|(_,b)|b.capacity()).sum::<usize>()}
}

#[cfg(test)]mod tests{
    use super::*;
    #[test]fn publication_waits_for_bounded_contention(){
        let hub=std::sync::Arc::new(std::sync::Mutex::new(PartsHub::new(8,64).unwrap()));
        hub.lock().unwrap().descriptor(&[9;64]).unwrap();let mut cursor=Cursor::default();let mut out=Vec::new();hub.lock().unwrap().poll(&mut cursor,&mut out);
        let guard=hub.lock().unwrap();let h=hub.clone();let(tx,rx)=std::sync::mpsc::channel();
        let producer=std::thread::spawn(move||{tx.send(()).unwrap();publish_ready(&h,&[7;64]).unwrap()});
        rx.recv().unwrap();std::thread::sleep(std::time::Duration::from_millis(10));drop(guard);producer.join().unwrap();
        assert!(hub.lock().unwrap().poll(&mut cursor,&mut out));assert_eq!(out,[7;64]);assert_eq!(cursor.missed,0);
    }
    #[test]fn exact_i5_golden_all_bins(){
        let root=std::env::var("OBS_OBN2_VECTOR_ROOT").unwrap_or_else(|_|"maia-httpd/tests/observation-continuous".into());
        for(i,(k,peak,name))in [(1,false,"layout-k1-sum.bin"),(2,false,"layout-k2-sum.bin"),(3,true,"layout-k3-peak.bin"),(1023,false,"layout-k1023-sum.bin")].into_iter().enumerate(){
            let hex=std::fs::read_to_string(std::path::Path::new(&root).join(name.replace(".bin",".hex"))).unwrap();let text=hex.split_whitespace().collect::<String>();let b:Vec<u8>=(0..text.len()).step_by(2).map(|i|u8::from_str_radix(&text[i..i+2],16).unwrap()).collect();let h=header(&b).unwrap();assert_eq!(h.k,k);assert_eq!(h.peak(),peak);assert_eq!(h.ordinal,0x100000000+i as u64);assert_eq!(h.first_seq,0x1234567800000000+i as u64*1024);assert_eq!(h.last_seq,h.first_seq+k as u64-1);
            let mut p=vec![0.;4096];normalize(&b,h,1./750668f64.powi(2),&mut p).unwrap();
            for j in 0..4096{let m=if j==0{0}else if j==1{(1u64<<47)-1}else{((j as u64+1)*0x123456789ab+i as u64*0xffeeddccbb)&((1u64<<47)-1)};let e=(j+i)%5;let want=((m as f64*(1u64<<(2*e))as f64)/750668f64.powi(2)/if peak{1.}else{k as f64})as f32;assert!((p[j]-want).abs()<=want.abs()*f32::EPSILON,"bin {j} {name}");}
            if let Ok(dir)=std::env::var("OBS_CONTINUOUS_OUTPUT_DIR"){std::fs::create_dir_all(&dir).unwrap();let bytes:Vec<u8>=p.iter().flat_map(|v|v.to_le_bytes()).collect();std::fs::write(std::path::Path::new(&dir).join(format!("{name}.f32")),bytes).unwrap();}
        }
    }
    fn record(seq:u64,epoch:u64,k:u32,peak:bool)->Vec<u8>{let mut h=[0u8;64];h[0..4].copy_from_slice(&ABI.to_le_bytes());h[4..6].copy_from_slice(&2u16.to_le_bytes());h[6..8].copy_from_slice(&64u16.to_le_bytes());for(o,v)in[(8,seq),(16,epoch),(24,1000+seq*100),(56,seq)]{h[o..o+8].copy_from_slice(&v.to_le_bytes())}for(o,v)in[(32,4096),(36,k),(40,TICK_HZ as u32),(44,(epoch!=0)as u32|((peak as u32)<<2)),(48,2),(52,BYTES as u32)]{h[o..o+4].copy_from_slice(&v.to_le_bytes())}let mut b=vec![0;BYTES];for j in 0..BINS{let e=j%5;let w=(j as u64*19+7)|((e as u64)<<47)|if j<64{(h[j]as u64)<<56}else{0};b[j*8..j*8+8].copy_from_slice(&w.to_le_bytes());}b}
    #[test]fn all_bins_sum_peak_and_selection(){let scale=1./750668f64.powi(2);for k in[1,2,3,1023]{for peak in[false,true]{let b=record(1,7,k,peak);let h=header(&b).unwrap();let mut p=vec![0.;4096];normalize(&b,h,scale,&mut p).unwrap();for j in 0..4096{assert_eq!(p[j],((j as u64*19+7)as f64*(1u64<<(2*(j%5)))as f64*scale/if peak{1.}else{k as f64})as f32)}}}let b=record(1,7,2,false);let mut p=vec![0.;4096];let mut prep=Prepare::new(vec![Context{epoch:7,position:0,flags:0,scale}],1000).unwrap();assert!(prep.accept(&b,&mut p).unwrap().is_some());assert!(prep.accept(&b,&mut p).unwrap().is_none());let mut skip=record(3,7,2,false);skip[4095*8+6]=255;assert!(prep.accept(&skip,&mut p).unwrap().is_none());assert_eq!(prep.stats.normalized,1);assert!(prep.accept(&record(5,0,2,false),&mut p).is_err());assert!(prep.accept(&record(7,8,2,false),&mut p).is_err());}
    struct Mock{data:Vec<Vec<u8>>,snap:Snapshot,copies:u64,releases:u64,stops:u64}
    impl RingIo for Mock{fn start(&mut self)->Result<(),String>{self.snap.running=true;Ok(())}fn snapshot(&mut self)->Result<Snapshot,String>{Ok(self.snap)}fn copy_slot(&mut self,s:usize,b:&mut[u8])->Result<(),String>{b.copy_from_slice(&self.data[s]);self.copies+=1;Ok(())}fn release(&mut self,c:u32,t:u64)->Result<(),String>{assert_eq!(c,self.snap.cookie);assert!(t<=self.snap.committed);self.snap.released=t;self.releases+=1;Ok(())}fn stop(&mut self)->Result<(),String>{self.stops+=1;self.snap.running=false;self.snap.drained=true;Ok(())}}
    #[test]fn wrap_full_release_before_processing_stop_reset(){let s=Snapshot{cookie:7,committed:0,reserved:0,released:0,selected:0,dropped:0,running:true,dma_busy:false,drained:false,fault:false,..Snapshot::default()};let mut io=Mock{data:(0..8).map(|_|vec![0;BYTES]).collect(),snap:s,copies:0,releases:0,stops:0};let mut r=Reader::attach(s).unwrap();let mut q=OwnedQueue::new(3).unwrap();for batch in 0..100{for j in 0..8{io.data[j]=record((batch*8+j)as u64,7,1,false)}io.snap.committed+=8;io.snap.reserved=io.snap.committed;io.snap.selected+=8;r.service(&mut io,&mut q).unwrap();assert_eq!(io.snap.released,io.snap.committed);}assert_eq!(r.stats.copied,800);assert_eq!(q.dropped,797);assert_eq!(q.storage_bytes(),3*BYTES);let mut out=vec![0;BYTES];assert!(q.pop_swap(&mut out));assert_eq!(header(&out).unwrap().ordinal,797);r.stop(&mut io).unwrap();r.stop(&mut io).unwrap();let s=r.service(&mut io,&mut q).unwrap();assert!(r.drained(s));assert_eq!(io.stops,1);io.snap.cookie+=1;assert!(r.service(&mut io,&mut q).is_err());}
    #[test]fn slow_client_gets_descriptor_then_visible_gap(){let mut h=PartsHub::new(2,64).unwrap();h.descriptor(&[1;64]).unwrap();let mut slow=Cursor::default();let mut o=Vec::new();assert!(h.poll(&mut slow,&mut o));for i in 0..10000{h.publish(&[i as u8;64]).unwrap();}assert!(h.storage_bytes()<=3*64);assert!(h.poll(&mut slow,&mut o));assert_eq!(slow.missed,9998);let mut reconnect=Cursor::default();assert!(h.poll(&mut reconnect,&mut o));assert_eq!(o,[1;64]);h.descriptor(&[2;64]).unwrap();assert!(h.poll(&mut slow,&mut o));assert_eq!(o,[2;64]);}
    #[test]fn reject_reserved_payload_and_malformed_groups(){let mut b=record(4,7,2,false);b[600*8+6]=4;let mut out=vec![0.;4096];assert!(normalize(&b,header(&b).unwrap(),1.,&mut out).is_err());let mut b=record(4,7,2,false);b[36*8+7]=0;assert!(header(&b).is_err());}
}
