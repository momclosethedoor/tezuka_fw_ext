#[path="maia-httpd/src/system_reader/continuous.rs"] mod continuous;
#[path="maia-httpd/src/system_reader/view.rs"] mod view;
#[path="maia-httpd/src/system_reader/obn3.rs"] mod obn3;
#[path="maia-httpd/src/system_reader/roi.rs"] mod roi;
#[path="maia-httpd/src/control_runtime/plan_math.rs"] mod scan_plan;
use view::PairIo;
struct Mock { full:Vec<u8>, view:Vec<u8>, released:usize, fail_view:bool }
impl PairIo for Mock {
 fn snapshot(&mut self)->Result<continuous::Snapshot,String>{unimplemented!()}
 fn copy_full(&mut self,_:usize,out:&mut[u8])->Result<(),String>{out.copy_from_slice(&self.full);Ok(())}
 fn copy_view(&mut self,_:usize,out:&mut[u8])->Result<(),String>{if self.fail_view{return Err("injected incomplete VIEW copy".into())}out.copy_from_slice(&self.view);Ok(())}
 fn release(&mut self,_:u32,_:u64)->Result<(),String>{self.released+=1;self.full.fill(0xcc);self.view.fill(0xdd);Ok(())}
}
fn hex(path:&str)->Vec<u8>{let s=std::fs::read_to_string(path).unwrap().split_whitespace().collect::<String>();(0..s.len()).step_by(2).map(|i|u8::from_str_radix(&s[i..i+2],16).unwrap()).collect()}
fn mock()->Mock{Mock{full:hex("tools/obs-agent/maia-httpd/tests/view/mode1/ordinal-0-k2.hex"),view:hex("tools/obs-agent/maia-httpd/tests/view/mode1/ordinal-0-k2.viewhex"),released:0,fail_view:false}}
#[test] fn owned_boundary_survives_immediate_dma_reuse_and_wrap(){
 for ordinal in [0u64,1,510,511,512,513,1022,1023,1024,1025,2047,2048]{
  let mut io=mock();for(i,b)in ordinal.to_le_bytes().iter().enumerate(){io.full[(56+i)*8+7]=*b;}
  let expected=io.full.clone();let expected_view=io.view.clone();let mut full=vec![0;32768];let mut v=vec![0;512];let mut copied=false;
  let h=view::copy_release(&mut io,7,ordinal,&mut full,&mut v,||copied=true).unwrap();
  assert!(copied);assert_eq!(io.released,1);assert_eq!(h.ordinal,ordinal);assert_eq!(full,expected);assert_eq!(v,expected_view);
  assert!(io.full.iter().all(|&b|b==0xcc));let mut bins=vec![0.;4096];view::decode(&full,&v,&mut bins).unwrap();
 }
}
#[test] fn owned_boundary_incomplete_copy_keeps_dma_ownership(){
 let mut io=mock();io.fail_view=true;let mut full=vec![0;32768];let mut v=vec![0;512];
 assert!(view::copy_release(&mut io,7,0,&mut full,&mut v,||panic!("incomplete copy")).is_err());assert_eq!(io.released,0);
}
#[test] fn owned_boundary_wrong_ordinal_keeps_dma_ownership(){
 let mut io=mock();let mut full=vec![0;32768];let mut v=vec![0;512];
 assert!(view::copy_release(&mut io,7,1,&mut full,&mut v,||panic!("wrong identity")).is_err());assert_eq!(io.released,0);
}
#[test] fn owned_boundary_direct_read_releases_before_sink_and_preserves_view(){
 use std::{rc::Rc,cell::Cell};
 struct Direct{m:Mock,read:Rc<Cell<usize>>,phase:Rc<Cell<u32>>}
 impl PairIo for Direct{
  fn snapshot(&mut self)->Result<continuous::Snapshot,String>{Ok(continuous::Snapshot{cookie:7,committed:1,reserved:1,..Default::default()})}
  fn copy_full(&mut self,_:usize,_:&mut[u8])->Result<(),String>{panic!("no full copy in direct path")}
  fn copy_view(&mut self,_:usize,_:&mut[u8])->Result<(),String>{panic!("borrowed view")}
  fn visit_pair<F:FnMut(continuous::Header,&[&[u8]],&[u8])->Result<(),String>>(&mut self,_:usize,_:&mut[u8],_:&mut[u8],sink:&mut F)->Result<(),String>{sink(continuous::header_revision(&self.m.full,3)?,&[&self.m.full],&self.m.view)}
  fn release(&mut self,run:u32,tail:u64)->Result<(),String>{assert_eq!(self.read.get(),32768);assert_eq!(self.phase.get(),0);self.m.release(run,tail)?;self.phase.set(1);Ok(())}
 }
 let read=Rc::new(Cell::new(0));let phase=Rc::new(Cell::new(0));let m=mock();let expected=m.view.clone();
 let mut io=Direct{m,read:read.clone(),phase:phase.clone()};let mut reader=view::PairReader::attach(io.snapshot().unwrap()).unwrap();
 reader.service_released(&mut io,|p|{assert_eq!(phase.get(),0);read.set(read.get()+p.len());},|h,v,raw|{assert_eq!(phase.get(),1);assert_eq!(h.ordinal,0);assert_eq!(v,expected);assert!(raw.is_empty());phase.set(2);Ok(())}).unwrap();
 assert_eq!(phase.get(),2);assert_eq!(reader.read,1);assert!(io.m.full.iter().all(|&b|b==0xcc));
}

#[path="maia-httpd/src/system_reader/view_batch.rs"] mod view_batch;
#[path="maia-httpd/src/system_reader/view_pace.rs"] mod view_pace;
#[path="maia-httpd/src/system_reader/timing.rs"] mod timing;
#[path="maia-httpd/src/system_reader/sweep_delivery.rs"] mod sweep_delivery;
use continuous::Header;
use view_batch::UdpBatch;
type R<T> = Result<T,String>;
#[path="maia-httpd/src/control_runtime/mixer_math.rs"] mod mixer_math;
#[path="maia-httpd/src/control_runtime/generated/tx_schedule.rs"] mod tx_schedule;
#[path="maia-httpd/src/system_reader/progress.rs"] mod progress;
#[path="maia-httpd/src/control_runtime/video_egress.rs"] mod video_egress;

#[test]
fn view_only_wrap_and_short_after_long_survive_immediate_dma_reuse() {
 use std::{cell::Cell,rc::Rc};
 struct MockView{header:continuous::Header,view:Vec<u8>,snap:continuous::Snapshot,released:Rc<Cell<u64>>}
 impl PairIo for MockView{
  fn snapshot(&mut self)->Result<continuous::Snapshot,String>{Ok(self.snap)}
  fn full_read(&self)->bool{false}
  fn copy_full(&mut self,_:usize,_:&mut[u8])->Result<(),String>{panic!("VIEW consumer requested full DMA")}
  fn copy_view(&mut self,_:usize,_:&mut[u8])->Result<(),String>{panic!("test expects borrowed DMA")}
  fn visit_pair<F:FnMut(continuous::Header,&[&[u8]],&[u8])->Result<(),String>>(
   &mut self,slot:usize,_:&mut[u8],_:&mut[u8],sink:&mut F
  )->Result<(),String>{
   assert_eq!(slot,self.snap.released as usize%1024);
   sink(self.header,&[],&self.view)
  }
  fn release(&mut self,run:u32,tail:u64)->Result<(),String>{
   assert_eq!(run,23);
   assert_eq!(tail,self.snap.released+1);
   self.snap.released=tail;
   self.view.fill(0);
   self.released.set(tail);
   Ok(())
  }
 }
 let released=Rc::new(Cell::new(1022));
 let mut io=MockView{
  header:continuous::header_revision(&mock().full,3).unwrap(),view:vec![],
  snap:continuous::Snapshot{cookie:23,committed:1022,reserved:1022,released:1022,..Default::default()},
  released:released.clone(),
 };
 let mut reader=view::PairReader::attach(io.snap).unwrap();
 for (ordinal,bins) in [512,1024,2048,512].into_iter().enumerate(){
  let ordinal=ordinal as u64+1022;
  let mode=match bins{512=>1,1024=>2,_=>3};
  io.header.ordinal=ordinal;
  io.header.flags=(mode<<3)|1;
  io.view=(0..bins).map(|i| ((i+ordinal as usize)%251+1)as u8).collect();
  let expected=io.view.clone();
  io.snap.committed+=1;
  io.snap.reserved+=1;
  reader.service_released(&mut io,|_|panic!("VIEW read of native bins"),|header,data,_|{
   assert_eq!(released.get(),ordinal+1);
   assert_eq!(header.ordinal,ordinal);
   assert_eq!(view::view_len(header),bins);
   assert_eq!(data,expected);
   Ok(())
  }).unwrap();
 }
 assert_eq!(reader.visited,4);
 assert_eq!(reader.read,0);
}

#[test]
fn compact_tcp_sink_reads_the_owned_copy_only_after_release() {
    struct Owned { dma:Vec<u8>, released:bool }
    impl PairIo for Owned {
        fn snapshot(&mut self)->R<continuous::Snapshot>{Ok(continuous::Snapshot{
            cookie:3,committed:1,reserved:1,..Default::default()})}
        fn owned_full_bytes(&self)->usize{obn3::COMPACT_BYTES}
        fn copy_full(&mut self,_:usize,_:&mut[u8])->R<()>{unreachable!()}
        fn copy_view(&mut self,_:usize,_:&mut[u8])->R<()>{unreachable!()}
        fn visit_pair<F:FnMut(Header,&[&[u8]],&[u8])->R<()>>(
            &mut self,_:usize,full:&mut[u8],_:&mut[u8],sink:&mut F
        )->R<()> {
            full[..self.dma.len()].copy_from_slice(&self.dma);
            sink(Header{first_seq:10,last_seq:11,epoch:1,first_tick:5,bins:4096,k:2,flags:1,ordinal:0},&[],&[])
        }
        fn release(&mut self,run:u32,tail:u64)->R<()>{
            assert_eq!((run,tail),(3,1));
            self.dma.fill(0);
            self.released=true;
            Ok(())
        }
    }
    let mut io=Owned{dma:vec![42;obn3::COMPACT_BYTES],released:false};
    let mut reader=view::PairReader::attach(io.snapshot().unwrap()).unwrap();
    reader.service_released(&mut io,|_|panic!("full payload must not be processed while owned"),|_,view,raw|{
        assert!(view.is_empty());
        assert_eq!(raw.len(),obn3::COMPACT_BYTES);
        assert!(raw.iter().all(|&value|value==42));
        Ok(())
    }).unwrap();
    assert!(io.released);
    assert!(io.dma.iter().all(|&value|value==0));
}

#[test]
fn roi_header_only_records_keep_order_and_release_without_exposing_old_payload() {
    struct Ring { tail:u64, copies:u64 }
    impl PairIo for Ring {
        fn snapshot(&mut self)->R<continuous::Snapshot>{
            Ok(continuous::Snapshot{cookie:1,released:self.tail,committed:1026,reserved:1026,..Default::default()})
        }
        fn owned_full_bytes(&self)->usize{obn3::COMPACT_BYTES}
        fn copy_full(&mut self,_:usize,_:&mut[u8])->R<()>{unreachable!()}
        fn copy_view(&mut self,_:usize,_:&mut[u8])->R<()>{unreachable!()}
        fn visit_for_release<F:FnMut(Header,&[&[u8]],&[u8])->R<()>>(
            &mut self,slot:usize,full:&mut[u8],_:&mut[u8],keep:Option<&mut dyn FnMut(Header)->bool>,sink:&mut F
        )->R<usize>{
            assert_eq!(slot,self.tail as usize%1024);
            let h=Header{first_seq:0,last_seq:1,epoch:self.tail,first_tick:0,bins:4096,k:2,flags:1,ordinal:self.tail};
            let bytes=if keep.unwrap()(h){
                self.copies+=1;
                full[..obn3::COMPACT_BYTES].fill(42);
                obn3::COMPACT_BYTES
            }else{0};
            sink(h,&[],&[])?;
            Ok(bytes)
        }
        fn release(&mut self,run:u32,tail:u64)->R<()>{
            assert_eq!((run,tail),(1,self.tail+1));
            self.tail=tail;
            Ok(())
        }
    }
    let mut ring=Ring{tail:1023,copies:0};
    let mut reader=view::PairReader::attach(ring.snapshot().unwrap()).unwrap();
    let mut received=Vec::new();
    reader.service_filtered(&mut ring,Some(&mut |h|h.ordinal==1024),|_|panic!("unexpected DMA view"),|h,v,raw|{
        assert!(v.is_empty());
        assert!(raw.iter().all(|&b|b==42));
        received.push((h.ordinal,raw.len()));
        Ok(())
    }).unwrap();
    assert_eq!(received,[(1023,0),(1024,obn3::COMPACT_BYTES),(1025,0)]);
    assert_eq!((ring.tail,ring.copies,reader.visited,reader.read,reader.payload_skipped),(1026,1,3,1,2));
}
