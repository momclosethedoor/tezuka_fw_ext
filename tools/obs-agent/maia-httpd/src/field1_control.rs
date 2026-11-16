//! FIELD1 timeout control and serial-bracketed readback. No frame ACK.
use axum::{Json,http::StatusCode};
use serde_json::{json,Value};
use std::{fs::OpenOptions,os::fd::AsRawFd,sync::Mutex};
static LOCK:Mutex<()>=Mutex::new(());
fn access(body:Option<Value>)->Result<Value,String>{
 let _lock=LOCK.lock().map_err(|_|"FIELD1 lock")?;
 let f=OpenOptions::new().read(true).write(true).open("/dev/mem").map_err(|e|e.to_string())?;
 let p=unsafe{libc::mmap(std::ptr::null_mut(),4096,libc::PROT_READ|libc::PROT_WRITE,libc::MAP_SHARED,f.as_raw_fd(),0x7c460000)};
 if p==libc::MAP_FAILED{return Err(std::io::Error::last_os_error().to_string())}
 let read=|o:usize|unsafe{std::ptr::read_volatile((p as *const u32).add(o/4))};
 let result=(||{
  if read(0x280)!=0x31444c46{return Err("FIELD1 ID mismatch".into())}
  if let Some(v)=body{
   let ms=v["preview_timeout_ms"].as_u64().filter(|m|(1..=68719).contains(m)).ok_or("timeout ms 1..68719")?;
   unsafe{std::ptr::write_volatile((p as *mut u32).add(0x28c/4),(ms*62500)as u32)};
   std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
   if read(0x28c)!=ms as u32*62500{return Err("timeout readback mismatch".into())}
  }
  for _ in 0..16{
   let serial=read(0x298);let status=read(0x294);
   let v=json!({"id":read(0x280),"timeout_ticks":read(0x28c),"preview_timeout_ms":read(0x28c)as f64/62500.,"status":status,"serial":serial,"stream_tag":read(0x288),"result_tag":read(0x2a4),"epoch_lo":read(0x29c),"epoch_hi":read(0x2a0),"first":read(0x2a8),"end":read(0x2ac),"a9_monotonic_s":super::delivery_runtime::mono()});
   if serial==read(0x298)&&status==read(0x294){return Ok(v)}
  }
  Err("unstable FIELD1 metadata".into())
 })();
 unsafe{libc::munmap(p,4096)};result
}
pub async fn get()->Result<Json<Value>,(StatusCode,String)>{access(None).map(Json).map_err(|e|(StatusCode::CONFLICT,e))}
pub async fn post(Json(v):Json<Value>)->Result<Json<Value>,(StatusCode,String)>{access(Some(v)).map(Json).map_err(|e|(StatusCode::CONFLICT,e))}
