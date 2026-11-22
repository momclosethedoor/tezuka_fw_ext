//! RX-only OBS007A delivery acceptance. No software video decoder is invoked.
#![allow(missing_docs)]
use anyhow::{Result,ensure};
use axum::{Router,Json,extract::State,routing::{get,post}};
use serde::Deserialize;
use serde_json::{Value,json};
use std::{fs::{File,OpenOptions},os::fd::AsRawFd,path::{Path,PathBuf},net::{SocketAddr,TcpStream},io::Write,sync::{Arc,Mutex,atomic::{AtomicBool,Ordering}},time::{Duration,Instant}};
#[derive(Deserialize)]#[serde(deny_unknown_fields)]
struct Config{setup:PathBuf,reader:Value,replay:Option<Replay>,#[serde(default)]tx_authorized:bool}
#[derive(Deserialize,Clone)]#[serde(deny_unknown_fields)]
struct Replay{file:PathBuf,metadata:PathBuf,destination:String,hz:u32,count:u32,#[serde(default="one")]targets:u32,#[serde(default)]burst:bool}
fn one()->u32{1}
struct Inner{running:AtomicBool,stop:AtomicBool,result:Mutex<Value>,delivery:Mutex<Value>,tx_authorized:bool}
type Shared=Arc<Inner>;
fn guard_ioctl(fd:&File,cmd:libc::c_ulong)->Result<Value>{let mut s=[0u32;128];ensure!(unsafe{libc::ioctl(fd.as_raw_fd(),cmd,s.as_mut_ptr())}==0,"guard ioctl {cmd:x}: {}",std::io::Error::last_os_error());ensure!(unsafe{libc::ioctl(fd.as_raw_fd(),0x4605,s.as_mut_ptr())}==0&&s[2]==0,"guard status/error");Ok(json!({"abi":s[0],"state":s[1],"error":s[2],"control":s[9],"tx_powerdown":s[112]}))}
async fn status(State(s):State<Shared>)->Json<Value>{Json(json!({"kind":"OBS007A-real8-byte-delivery","running":s.running.load(Ordering::Acquire),"reader":crate::system_reader::status(),"result":s.result.lock().unwrap().clone(),"delivery":s.delivery.lock().unwrap().clone(),"software_video_decoder":false,"tx_authorized":s.tx_authorized}))}
async fn stop(State(s):State<Shared>)->Json<Value>{s.stop.store(true,Ordering::Release);crate::system_reader::stop();Json(json!({"stop_requested":true}))}
pub(crate) fn mono_ns()->u64{let mut t=libc::timespec{tv_sec:0,tv_nsec:0};unsafe{libc::clock_gettime(libc::CLOCK_MONOTONIC,&mut t)};t.tv_sec as u64*1_000_000_000+t.tv_nsec as u64}
pub(crate) fn mono()->f64{mono_ns() as f64*1e-9}
fn replay(q:Replay,s:Shared,warmup:f64)->Result<Value>{
 ensure!((1..=20).contains(&q.hz)&&(1..=600).contains(&q.count)&&(1..=10).contains(&q.targets),"bounded replay load");
 let bytes=std::fs::read(&q.file)?;let base:Value=serde_json::from_slice(&std::fs::read(&q.metadata)?)?;
 ensure!(base["replay"]==true&&base["payload_bytes"].as_u64()==Some(bytes.len()as u64),"replay provenance/length");
 let mut conn=TcpStream::connect(&q.destination)?;conn.set_write_timeout(Some(Duration::from_millis(500)))?;conn.set_nodelay(true)?;
 while crate::system_reader::status()["time"].as_f64().unwrap_or(0.)<warmup{if !s.running.load(Ordering::Acquire)||s.stop.load(Ordering::Acquire){return Ok(json!({"aborted":true,"sent":0}))}std::thread::sleep(Duration::from_millis(2));}
 let start=Instant::now();let mut trace=Vec::new();let mut sent=0;let mut useful=0;
 for i in 0..q.count{
  let due=Duration::from_secs_f64(if q.burst{(i/q.targets) as f64}else{i as f64/q.hz as f64});while start.elapsed()<due{if s.stop.load(Ordering::Acquire){break}std::thread::sleep(due.saturating_sub(start.elapsed()).min(Duration::from_millis(2)));}
  if s.stop.load(Ordering::Acquire)||!s.running.load(Ordering::Acquire){break}
  let mut meta=base.clone();meta["delivery_id"]=json!(i+1);meta["replay_target"]=json!(i%q.targets);meta["target_hz"]=json!(base["target_hz"].as_f64().unwrap_or(0.)+(i%q.targets)as f64*100000.);meta["a9_send_monotonic_s"]=json!(mono());meta["concurrent_scan"]=crate::system_reader::status()["regs"]["fld"].clone();
  let header=serde_json::to_vec(&meta)?;let before=mono();conn.write_all(b"R8P1")?;conn.write_all(&(header.len()as u32).to_le_bytes())?;conn.write_all(&(bytes.len()as u32).to_le_bytes())?;conn.write_all(&header)?;
  for piece in bytes.chunks(65536){conn.write_all(piece)?;}
  sent+=1;useful+=bytes.len();trace.push(json!({"id":i+1,"send_start":before,"send_complete":mono(),"bytes":bytes.len()}));
  *s.delivery.lock().unwrap()=json!({"sent":sent,"bytes":useful,"replay":true});
 }
 Ok(json!({"sent":sent,"bytes":useful,"replay":true,"hz":q.hz,"chunk_bytes":65536,"seconds":start.elapsed().as_secs_f64(),"trace":trace}))
}
pub async fn run(path:&Path,listen:SocketAddr)->Result<()>{
 let listener=tokio::net::TcpListener::bind(listen).await?;
 let c:Config=serde_json::from_slice(&std::fs::read(path)?)?;
 let capture=c.reader["command"]=="capture";
 let view_mode=c.reader["mode"].as_u64();
 let bounded_obn3=c.reader["bench_obn3_layout"]==true || c.reader["stream_config"].is_number();
 ensure!(c.reader["hot"]==true&&c.reader["scan"]==true&&c.reader["k"]==2&&
        (view_mode==Some(1)||capture&&matches!(view_mode,Some(2|3))||bounded_obn3&&matches!(view_mode,Some(0|2|3))),
        "guarded K2 VIEW512, or offline reference capture in VIEW1024/2048");
 ensure!(std::fs::read_to_string("/sys/bus/iio/devices/iio:device0/out_altvoltage1_TX_LO_powerdown")?.trim()=="1","TX must be off");
 ensure!(c.reader["boot_id"].as_str()==Some(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim()),"boot mismatch");
 let guard=OpenOptions::new().read(true).write(true).open("/dev/obs_fl_guard")?;let mut setup=std::fs::read(&c.setup)?;
 ensure!(setup.len()==12488,"calibrated map layout");
 let rf_on=u32::from_le_bytes(setup[12332..12336].try_into().unwrap());
 if capture && view_mode!=Some(1) {
  ensure!(rf_on==0 && c.replay.is_none() && c.reader["udp"].is_null()
          && c.reader["seconds"].as_f64().is_some_and(|t| t>0. && t<=0.05),
          "reference capture requires RX only, no network output and at most 50 ms");
 }
 ensure!((rf_on==0&&!c.tx_authorized&&c.reader["tx_active"]!=true)||(rf_on==1&&c.tx_authorized&&c.reader["tx_active"]==true),"RF-on requires explicit operator-series authorization and declared reader mode");
 ensure!(unsafe{libc::ioctl(guard.as_raw_fd(),0x4601,setup.as_mut_ptr())}==0,"guard load");
 let taken=match guard_ioctl(&guard,0x4602){Ok(v)=>v,Err(e)=>{let _=guard_ioctl(&guard,0x4604);return Err(e)}};
 if taken["state"]!=2||(taken["control"]!=3&&taken["control"]!=7)||taken["tx_powerdown"]!=json!(if rf_on==1{0}else{1}){let _=guard_ioctl(&guard,0x4604);anyhow::bail!("guard take receipt: {taken}")}
 let state=Arc::new(Inner{running:AtomicBool::new(true),stop:AtomicBool::new(false),result:Mutex::new(json!({"guard_take":taken})),delivery:Mutex::new(json!({"sent":0,"replay":c.replay.is_some()})),tx_authorized:c.tx_authorized});
 let worker=state.clone();let data=serde_json::to_vec(&c.reader)?;let out=PathBuf::from(c.reader["output"].as_str().unwrap());let warmup=c.reader["warmup"].as_f64().unwrap_or(5.);
 let replay_thread=c.replay.map(|q|{let s=state.clone();std::thread::spawn(move||replay(q,s,warmup))});
 let acquisition=std::thread::spawn(move||{
  let result=crate::system_reader::run_json(&data);worker.running.store(false,Ordering::Release);
  let delivery=match replay_thread{Some(t)=>match t.join(){Ok(Ok(v))=>v,Ok(Err(e))=>json!({"error":e.to_string()}),Err(_)=>json!({"error":"replay panic"})},None=>json!({"sent":0,"bytes":0})};
  let handback=guard_ioctl(&guard,0x4604);let v=json!({"reader_outcome":result,"delivery":delivery,"guard_return":match handback{Ok(v)=>v,Err(e)=>json!({"error":e.to_string()})}});
  let _=std::fs::write(out.join("agent-result.json"),serde_json::to_vec_pretty(&v).unwrap());*worker.result.lock().unwrap()=v;
 });
 let app=Router::new().route("/api/system/field",get(crate::field1_control::get).post(crate::field1_control::post)).route("/api/system/status",get(status)).route("/api/system/stop",post(stop)).with_state(state.clone());
 let shutdown=state.clone();let served=axum::serve(listener,app).with_graceful_shutdown(async move{loop{if shutdown.stop.load(Ordering::Acquire)&&!shutdown.running.load(Ordering::Acquire){break}tokio::time::sleep(Duration::from_millis(20)).await;}}).await;
 crate::system_reader::stop();acquisition.join().map_err(|_|anyhow::anyhow!("reader panic"))?;served?;Ok(())
}
