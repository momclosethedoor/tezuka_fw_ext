//! Integrated OBS007A RAM scanner/preview mode of the main maia-httpd ELF.
//! Hardware ownership is exclusive; legacy App initialization is not run in this mode.
#![allow(missing_docs)]
use anyhow::{Result,ensure};
use axum::{Router,Json,extract::State,routing::{get,post},http::StatusCode,response::IntoResponse};
use serde::Deserialize;
use serde_json::{Value,json};
use std::{ffi::CString,fs::{File,OpenOptions},os::fd::AsRawFd,path::{Path,PathBuf},net::SocketAddr,sync::{Arc,Mutex,atomic::{AtomicBool,AtomicU64,Ordering}},time::Duration};
unsafe extern "C" {fn obs_preview_run(argc:libc::c_int,argv:*mut *mut libc::c_char)->libc::c_int;fn obs_preview_cancel();}
#[derive(Deserialize)]#[serde(deny_unknown_fields)]
struct Config{setup:PathBuf,reader:Value,preview_root:PathBuf,preview_version:u32,preview_position:u32,#[serde(default)]preview_armed:bool,#[serde(default="normal")]preview_test:String}
struct Inner{busy:AtomicBool,running:AtomicBool,stop:AtomicBool,serial:AtomicU64,result:Mutex<Value>,last_dir:Mutex<Option<PathBuf>>,config:Config,guard:File}
type Shared=Arc<Inner>;
fn guard_ioctl(fd:&File,cmd:libc::c_ulong)->Result<Value>{let mut s=[0u32;128];ensure!(unsafe{libc::ioctl(fd.as_raw_fd(),cmd,s.as_mut_ptr())}==0,"guard ioctl {cmd:x}: {}",std::io::Error::last_os_error());ensure!(unsafe{libc::ioctl(fd.as_raw_fd(),0x4605,s.as_mut_ptr())}==0,"guard status");ensure!(s[2]==0,"guard error {}",s[2]);Ok(json!({"abi":s[0],"state":s[1],"error":s[2],"denied":s[3],"control":s[9],"fld":&s[16..34],"tx_powerdown":s[112],"tx_atten_mdb":s[113]}))}
async fn status(State(s):State<Shared>)->Json<Value>{Json(json!({"kind":"OBS007A-integrated-maia-httpd","running":s.running.load(Ordering::Acquire),"preview_busy":s.busy.load(Ordering::Acquire),"reader":crate::system_reader::status(),"result":s.result.lock().unwrap().clone(),"preview":{"version":s.config.preview_version,"position":s.config.preview_position,"armed":s.config.preview_armed,"trigger":"held FLD1 map marker; no HTTP request","data_ready":"completed owned IIO DMA block"},"tx_authorized":false}))}
#[derive(Deserialize)]#[serde(deny_unknown_fields)]struct Request{version:u32,position:u32,#[serde(default="normal")]test:String}
fn normal()->String{"normal".into()}
fn call_native(args:&[String])->Result<i32>{let c=args.iter().map(|x|CString::new(x.as_str())).collect::<std::result::Result<Vec<_>,_>>()?;let mut p=c.iter().map(|x|x.as_ptr() as *mut libc::c_char).collect::<Vec<_>>();Ok(unsafe{obs_preview_run(p.len() as i32,p.as_mut_ptr())})}
pub fn check_field(input:&Path,output:&Path)->Result<()>{ensure!(call_native(&["maia-httpd".into(),"--offline".into(),input.display().to_string(),output.display().to_string()])?==0,"native field check failed");Ok(())}
fn start_map_preview(s:&Shared,q:Request)->Result<Value>{
 ensure!(s.running.load(Ordering::Acquire)&&q.version==s.config.preview_version&&q.position==s.config.preview_position&&matches!(q.test.as_str(),"normal"|"stale"|"timeout"|"delayed"),"running target/version/test mismatch");
 ensure!(!s.busy.swap(true,Ordering::AcqRel),"IIO/preview already owned");
 let id=s.serial.fetch_add(1,Ordering::AcqRel)+1;let out=s.config.preview_root.join(format!("attempt-{id}"));
 let response=json!({"armed":true,"request_id":id,"version":q.version,"position":q.position,"trigger":"FLD1 marker pending"});
 let worker=s.clone();tokio::task::spawn_blocking(move||{
  let answer=(||->Result<Value>{
   // Stage removal of the one-shot marker for the next complete map boundary.
   let publication=guard_ioctl(&worker.guard,0x4603)?;
   let rc=call_native(&["maia-httpd".into(),out.display().to_string(),q.version.to_string(),q.position.to_string(),q.test])?;
   ensure!(rc==0,"in-process preview failed rc={rc}");
   let result:Value=serde_json::from_slice(&std::fs::read(out.join("result.json"))?)?;
   Ok(json!({"request_id":id,"publication":publication,"preview_result":result}))
  })();
  *worker.last_dir.lock().unwrap()=Some(out);
  *worker.result.lock().unwrap()=match answer{Ok(v)=>v,Err(e)=>json!({"request_id":id,"preview_error":e.to_string()})};worker.busy.store(false,Ordering::Release);
 });
 Ok(response)
}
async fn field(State(s):State<Shared>)->impl IntoResponse{let path=s.last_dir.lock().unwrap().clone();if let Some(p)=path{if let Ok(b)=std::fs::read(p.join("field.pgm")){return(StatusCode::OK,[("content-type","image/x-portable-graymap")],b);}}(StatusCode::NOT_FOUND,[("content-type","text/plain")],b"No owned successful field".to_vec())}
async fn stop(State(s):State<Shared>)->Json<Value>{s.stop.store(true,Ordering::Release);crate::system_reader::stop();unsafe{obs_preview_cancel()};Json(json!({"stop_requested":true,"drained":false}))}
async fn next(State(s):State<Shared>)->impl IntoResponse{match guard_ioctl(&s.guard,0x4603){Ok(v)=>(StatusCode::ACCEPTED,Json(v)),Err(e)=>(StatusCode::CONFLICT,Json(json!({"error":e.to_string()})))}}
pub async fn run(path:&Path,listen:SocketAddr)->Result<()>{
 let listener=tokio::net::TcpListener::bind(listen).await?;
 let c:Config=serde_json::from_slice(&std::fs::read(path)?)?;
 ensure!(c.reader["hot"]==true&&c.reader["scan"]==true&&c.reader["k"]==2&&c.reader["tx_active"]!=true,"integrated mode requires guarded RX-only K2 scan");
 ensure!(c.preview_version==7401&&c.preview_position==58,"this acceptance binary is bound to the calibrated direct-map video target");
 ensure!(matches!(c.preview_test.as_str(),"normal"|"stale"|"timeout"|"delayed"),"unknown preview test");
 ensure!(std::fs::read_to_string("/sys/bus/iio/devices/iio:device0/out_altvoltage1_TX_LO_powerdown")?.trim()=="1","TX must be physically off before guard");
 ensure!(std::fs::read_to_string("/sys/bus/iio/devices/iio:device0/in_voltage_sampling_frequency")?.trim()=="61440000","Fs prerequisite");
 ensure!(c.reader["boot_id"].as_str()==Some(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim()),"boot lease mismatch");
 std::fs::create_dir_all(&c.preview_root)?;
 let guard=OpenOptions::new().read(true).write(true).open("/dev/obs_fl_guard")?;
 let mut setup=std::fs::read(&c.setup)?;ensure!(setup.len()==12488&&setup[12328..12336]==[0;8],"guard setup size / TX disabled");
 let map_version=u32::from_le_bytes(setup[16..20].try_into().unwrap());
 ensure!(c.preview_armed==(map_version==c.preview_version),"marked preview map and armed A9 consumer must agree");
 ensure!(unsafe{libc::ioctl(guard.as_raw_fd(),0x4601,setup.as_mut_ptr())}==0,"guard load: {}",std::io::Error::last_os_error());
 let taken=guard_ioctl(&guard,0x4602)?;ensure!(taken["state"]==2&&taken["control"]==3,"guard RX-only ownership");
 let state=Arc::new(Inner{busy:AtomicBool::new(false),running:AtomicBool::new(true),stop:AtomicBool::new(false),serial:AtomicU64::new(0),result:Mutex::new(json!({"guard_take":taken})),last_dir:Mutex::new(None),config:c,guard});
 // The map is the request. The in-process worker waits for the latched marker,
 // checks the held target/epoch, then consumes completed DMA blocks. HTTP only
 // exposes status/results and administrative stop/map publication.
 if state.config.preview_armed{start_map_preview(&state,Request{version:state.config.preview_version,position:state.config.preview_position,test:state.config.preview_test.clone()})?;}
 let worker=state.clone();let data=serde_json::to_vec(&state.config.reader)?;
 let acquisition=std::thread::spawn(move||{
  let result=crate::system_reader::run_json(&data);worker.running.store(false,Ordering::Release);unsafe{obs_preview_cancel()};
  let started=std::time::Instant::now();while worker.busy.load(Ordering::Acquire)&&started.elapsed()<Duration::from_secs(2){std::thread::sleep(Duration::from_millis(2));}
  let handback=if worker.busy.load(Ordering::Acquire){Err(anyhow::anyhow!("preview did not drain; SPI lease remains pinned"))}else{guard_ioctl(&worker.guard,0x4604)};
  let mut last=worker.result.lock().unwrap();last["reader_outcome"]=json!(result);last["guard_return"]=match handback{Ok(v)=>v,Err(e)=>json!({"error":e.to_string()})};
 });
 let app=Router::new().route("/api/system/status",get(status)).route("/api/system/preview/image",get(field)).route("/api/system/map/next",post(next)).route("/api/system/stop",post(stop)).with_state(state.clone());
 let shutdown=state.clone();let served=axum::serve(listener,app).with_graceful_shutdown(async move{loop{if shutdown.stop.load(Ordering::Acquire)&&!shutdown.running.load(Ordering::Acquire)&&!shutdown.busy.load(Ordering::Acquire){break;}tokio::time::sleep(Duration::from_millis(20)).await;}}).await;
 crate::system_reader::stop();unsafe{obs_preview_cancel()};acquisition.join().map_err(|_|anyhow::anyhow!("reader panic"))?;served?;Ok(())
}
