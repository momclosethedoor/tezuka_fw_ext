//! FIELD1 timeout control and serial-bracketed readback. No frame ACK.
use axum::{Json,http::StatusCode};
use serde_json::{json,Value};
use std::{fs::OpenOptions,os::fd::AsRawFd,sync::Mutex};
static LOCK:Mutex<()>=Mutex::new(());
const RAW_POLICY:&str="/tmp/obs007a-field-system/unqualified-raw";
fn raw_policy()->Result<bool,String>{
 match std::fs::read_to_string(RAW_POLICY){
  Ok(s)=>match s.trim(){"0"=>Ok(false),"1"=>Ok(true),_=>Err("invalid raw policy".into())},
  Err(e) if e.kind()==std::io::ErrorKind::NotFound=>Ok(false),
  Err(e)=>Err(e.to_string())
 }
}
fn access(body:Option<Value>)->Result<Value,String>{
 let _lock=LOCK.lock().map_err(|_|"FIELD1 lock")?;
 let f=OpenOptions::new().read(true).write(true).open("/dev/mem").map_err(|e|e.to_string())?;
 let p=unsafe{libc::mmap(std::ptr::null_mut(),4096,libc::PROT_READ|libc::PROT_WRITE,libc::MAP_SHARED,f.as_raw_fd(),0x7c460000)};
 if p==libc::MAP_FAILED{return Err(std::io::Error::last_os_error().to_string())}
 let read=|o:usize|unsafe{std::ptr::read_volatile((p as *const u32).add(o/4))};
 let result=(||{
  if read(0x280)!=0x31444c46{return Err("FIELD1 ID mismatch".into())}
  if let Some(v)=body{
   let object=v.as_object().ok_or("control object required")?;
   if object.keys().any(|k|k!="preview_timeout_ms"&&k!="unqualified_raw"){return Err("unknown FIELD1 control".into())}
   let ms=object.get("preview_timeout_ms").map(|x|x.as_u64().filter(|m|(1..=68719).contains(m)).ok_or("timeout ms 1..68719")).transpose()?;
   let raw=object.get("unqualified_raw").map(|x|x.as_bool().ok_or("unqualified_raw must be boolean")).transpose()?;
   if let Some(ms)=ms{
    unsafe{std::ptr::write_volatile((p as *mut u32).add(0x28c/4),(ms*62500)as u32)};
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    if read(0x28c)!=ms as u32*62500{return Err("timeout readback mismatch".into())}
   }
   if let Some(raw)=raw{
    let pending=format!("{RAW_POLICY}.pending");
    std::fs::write(&pending,if raw{b"1\n"}else{b"0\n"}).map_err(|e|e.to_string())?;
    std::fs::rename(&pending,RAW_POLICY).map_err(|e|e.to_string())?;
   }
  }
  for _ in 0..16{
   let serial=read(0x298);let status=read(0x294);
   let v=json!({"id":read(0x280),"timeout_ticks":read(0x28c),"preview_timeout_ms":read(0x28c)as f64/62500.,"unqualified_raw":raw_policy()?,"raw_policy_scope":"A9 RAM; latched at attempt start","status":status,"serial":serial,"stream_tag":read(0x288),"result_tag":read(0x2a4),"epoch_lo":read(0x29c),"epoch_hi":read(0x2a0),"first":read(0x2a8),"end":read(0x2ac),"a9_monotonic_s":super::delivery_runtime::mono()});
   if serial==read(0x298)&&status==read(0x294){return Ok(v)}
  }
  Err("unstable FIELD1 metadata".into())
 })();
 unsafe{libc::munmap(p,4096)};result
}
pub async fn get()->Result<Json<Value>,(StatusCode,String)>{access(None).map(Json).map_err(|e|(StatusCode::CONFLICT,e))}
pub async fn post(Json(v):Json<Value>)->Result<Json<Value>,(StatusCode,String)>{access(Some(v)).map(Json).map_err(|e|(StatusCode::CONFLICT,e))}
