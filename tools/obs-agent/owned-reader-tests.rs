#[path="maia-httpd/src/system_reader/continuous.rs"] mod continuous;
#[path="maia-httpd/src/system_reader/view.rs"] mod view;
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
 reader.service_released(&mut io,|p|{assert_eq!(phase.get(),0);read.set(read.get()+p.len());},|h,v|{assert_eq!(phase.get(),1);assert_eq!(h.ordinal,0);assert_eq!(v,expected);phase.set(2);Ok(())}).unwrap();
 assert_eq!(phase.get(),2);assert_eq!(reader.read,1);assert!(io.m.full.iter().all(|&b|b==0xcc));
}

#[path="maia-httpd/src/system_reader/view_batch.rs"] mod view_batch;
