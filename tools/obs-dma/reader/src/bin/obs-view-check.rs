//! Offline r3 pair check. Never opens MMIO or starts DMA.
#[path="../continuous.rs"] mod continuous;
#[path="../view.rs"] mod view;
fn main(){
 let args:Vec<_>=std::env::args().collect();assert_eq!(args.len(),4,"obs-view-check full.bin view.bin power.f32");
 let full=std::fs::read(&args[1]).unwrap();let overview=std::fs::read(&args[2]).unwrap();let mut power=vec![0.;4096];
 let h=view::decode(&full,&overview,&mut power).unwrap();
 std::fs::write(&args[3],power.iter().flat_map(|v|v.to_le_bytes()).collect::<Vec<_>>()).unwrap();println!("PASS r3 pair all4096 {:?} view_bytes={} no hardware access",h,overview.len());
}
