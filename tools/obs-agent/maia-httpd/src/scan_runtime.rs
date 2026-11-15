//! Resident mode plumbing in the existing ARM application. Host GPIO is external.
#![allow(missing_docs)]
use crate::{app::AppState,observation::Observation,resident_schedule::{self as rcs,Registers,Map,Entry},scan_frame::Assembler};
use anyhow::{Result,ensure};
use serde::Deserialize;
use std::{io::{Read,Write},net::{SocketAddr,TcpListener,TcpStream},sync::atomic::Ordering,time::{Duration,Instant}};

#[derive(Debug,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {pub slot:u8,pub lo_hz:u64,pub bytes:[u8;16]}
#[derive(Debug,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {pub slot:u8,pub marker:bool,pub last:u8,pub route:u32,pub settling_ticks:u32,pub dwell_ticks:u32,pub context:u32}
#[derive(Debug,Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub run_id:u32,pub map_id:u64,pub initial_slot:u8,pub initial_route:u32,pub repeat:bool,
    pub profiles:Vec<Profile>,pub entries:Vec<Record>,pub route_bind:SocketAddr,pub publish_period_ms:u64,
    pub exclusive_controllers_quiesced:bool,pub ps_fastlock_outputs_zero:bool,pub coordinated_external_slot:bool,
}
impl Config {
    pub fn map(&self)->Result<Map>{
        ensure!(self.exclusive_controllers_quiesced && self.ps_fastlock_outputs_zero && self.coordinated_external_slot,"quiesce legacy controller/PS OR outputs and establish external sweeper slot before RCS1");
        ensure!((1..=8).contains(&self.profiles.len()) && (10..=60_000).contains(&self.publish_period_ms),"profile count/publication period");
        for (i,p) in self.profiles.iter().enumerate(){ensure!(p.slot<8 && p.lo_hz>0 && !self.profiles[..i].iter().any(|q|q.slot==p.slot),"unique resident profiles");}
        let entries=self.entries.iter().map(|e|Ok(Entry{slot:e.slot,marker:e.marker,last:e.last,route:e.route,settle:e.settling_ticks,dwell:e.dwell_ticks,context:e.context,lo:self.profiles.iter().find(|p|p.slot==e.slot).ok_or_else(||anyhow::anyhow!("table references unloaded profile"))?.lo_hz})).collect::<Result<Vec<_>>>()?;
        ensure!(self.profiles.iter().any(|p|p.slot==self.initial_slot),"load the initial physical slot too");
        let map=Map{run:self.run_id,map_id:self.map_id,initial_slot:self.initial_slot,initial_route:self.initial_route,repeat:self.repeat,entries};map.validate(0,self.initial_slot).map_err(anyhow::Error::msg)?;Ok(map)
    }
}
pub use crate::scan_frame::Hub;
impl Registers for crate::fpga::IpCore {
    fn read(&self,offset:usize)->u32 {self.resident_read(offset)}
    fn write(&self,offset:usize,value:u32){self.resident_write(offset,value)}
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Applied {version:u32,kind:String,id:String,applied:bool,model:bool}
pub struct RouteLink {listener:TcpListener,socket:Option<TcpStream>,pending:Vec<u8>,offset:usize,input:Vec<u8>,last_event:Option<u64>,pub stale:u64,pub failed:u64,pub requests:u64,pub last_reply:(u64,bool,bool)}
impl RouteLink {
    pub fn diagnostic_event(&self)->u64{self.last_event.unwrap_or(0)}
    pub fn new(bind:SocketAddr)->Result<Self>{let listener=TcpListener::bind(bind)?;listener.set_nonblocking(true)?;Ok(Self{listener,socket:None,pending:Vec::with_capacity(512),offset:0,input:Vec::with_capacity(512),last_event:None,stale:0,failed:0,requests:0,last_reply:(0,false,false)})}
    pub fn poll(&mut self,state:&AppState,map:&Map)->Result<()> {
        if self.socket.is_none(){match self.listener.accept(){Ok((s,_))=>{s.set_nonblocking(true)?;self.socket=Some(s);},Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>{},Err(e)=>return Err(e.into())}}
        let event={let core=state.ip_core().lock().unwrap();rcs::pending(&*core).map_err(anyhow::Error::msg)?};
        if let Some((id,bits))=event {if self.last_event!=Some(id){
            ensure!(self.offset==self.pending.len(),"previous route request not delivered before another marker");
            self.pending.clear();self.pending.extend_from_slice(format!("{{\"version\":1,\"kind\":\"route-request\",\"id\":\"{id}\",\"route\":{},\"marker\":{},\"last\":{},\"model_required\":true}}\n",bits>>8,bits&7,(bits>>3)&7).as_bytes());self.offset=0;self.last_event=Some(id);self.requests+=1;
        }}
        let Some(socket)=self.socket.as_mut() else{return Ok(());};
        if self.offset<self.pending.len(){match socket.write(&self.pending[self.offset..]){Ok(0)=>anyhow::bail!("route TCP closed; no partial request replay"),Ok(n)=>self.offset+=n,Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>{},Err(e)=>return Err(e.into())}}
        let mut buf=[0;512];match socket.read(&mut buf){
            Ok(0)=>anyhow::bail!("route TCP closed; quiesce before a new run, no request replay"),
            Ok(n)=>{ensure!(self.input.len()+n<=512,"bounded applied message exceeded");self.input.extend_from_slice(&buf[..n]);
                while let Some(end)=self.input.iter().position(|b|*b==b'\n'){
                    let reply=serde_json::from_slice::<Applied>(&self.input[..end]);
                    match reply {
                        Ok(a) if a.version==1 && a.kind=="applied" && a.model=>{
                            if let Ok(id)=a.id.parse::<u64>() {let core=state.ip_core().lock().unwrap();
                                let accepted=rcs::response(&*core,id,a.applied,map.repeat).is_ok();self.last_reply=(id,a.applied,accepted);if !accepted{self.stale+=1;}else if !a.applied{self.failed+=1;}
                            }else{self.stale+=1;}
                        },_=>self.stale+=1,
                    }
                    self.input.drain(..=end);
                }
            },Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>{},Err(e)=>return Err(e.into())
        }Ok(())
    }
}
pub struct Resident {pub map:Map,pub assembler:Assembler,pub wire:Vec<u8>,pub link:RouteLink,pub clean_consumer:u64,pub publish_skipped:u64,last_publish:Instant,period:Duration,pub capture:[u8;16],pub diagnostic_status:[u32;4],pub suppress_publication:bool}
impl Resident {
    pub async fn new(state:&AppState,config:&Config,fs:u32,capture:[u8;16])->Result<Self>{
        let map=config.map()?;let assembler=Assembler::new(map.clone(),fs).map_err(anyhow::Error::msg)?;
        {let core=state.ip_core().lock().unwrap();rcs::check(&*core).map_err(anyhow::Error::msg)?;ensure!(core.read(0xac)&0x13==0 && core.read(0xd4)==0 && ((core.read(0xac)>>6)&7)==map.initial_slot as u32,"fresh coordinated sweeper lifetime required");}
        state.ad9361().lock().await.load_resident_profiles(&config.profiles,map.initial_slot).await?;
        let wire=Vec::with_capacity(assembler.max_wire());state.scan().prepare(assembler.max_wire());
        let link=RouteLink::new(config.route_bind)?;
        {let core=state.ip_core().lock().unwrap();rcs::load(&*core,&map).map_err(anyhow::Error::msg)?;}
        Ok(Self{map,assembler,wire,link,clean_consumer:0,publish_skipped:0,last_publish:Instant::now()-Duration::from_secs(60),period:Duration::from_millis(config.publish_period_ms),capture,diagnostic_status:[0;4],suppress_publication:false})
    }
    pub fn start(&self,state:&AppState)->Result<()>{let core=state.ip_core().lock().unwrap();rcs::start(&*core,&self.map).map_err(anyhow::Error::msg)}
    pub fn poll(&mut self,state:&AppState)->Result<bool>{
        self.link.poll(state,&self.map)?;
        let core=state.ip_core().lock().unwrap();let status=core.read(0xac);ensure!(status&16==0,"RCS1 fault");
        self.diagnostic_status=[status,core.read(0xb8),core.read(0xbc),core.read(0xcc)];
        *state.scan().status.lock().unwrap()=format!("{{\"version\":1,\"kind\":\"resident-status\",\"run_id\":\"{}\",\"map_id\":\"{}\",\"status\":{},\"visit\":{},\"laps\":{},\"hold_ticks\":{},\"slot\":{},\"route_model\":true}}",self.map.run,self.map.map_id,status,self.diagnostic_status[1],self.diagnostic_status[2],self.diagnostic_status[3],(status>>6)&7);
        Ok(state.scan().stop.load(Ordering::Relaxed))
    }
    /// Internal consumer hook runs on EACH decoded clean FFT, before display
    /// selection. This counter is a test consumer, not a classifier algorithm.
    pub fn consume(&mut self,o:&Observation,state:&AppState)->Result<()> {
        self.consume_observed(o,state,|_,_|{})
    }
    pub fn consume_observed<F:FnMut(usize,bool)>(&mut self,o:&Observation,state:&AppState,mut observe:F)->Result<()> {
        if o.epoch!=0{self.map.resolve(o.epoch).map_err(anyhow::Error::msg)?;self.clean_consumer+=1;}
        if let Some(lap)=self.assembler.incoming_lap(o).map_err(anyhow::Error::msg)? {
            if self.assembler.lap.is_some_and(|old|lap>old){self.close_observed(state,true,o.first_tick,&mut observe);self.assembler.begin(lap);}
        }
        observe(7,true);let result=self.assembler.accept(o).map_err(anyhow::Error::msg);observe(7,false);result
    }
    pub fn close(&mut self,state:&AppState,closed:bool,watermark:u64){self.close_observed(state,closed,watermark,&mut |_,_|{});}
    pub fn close_observed<F:FnMut(usize,bool)>(&mut self,state:&AppState,closed:bool,watermark:u64,observe:&mut F){if self.assembler.lap.is_none(){return;}observe(8,true);self.assembler.encode(self.capture,closed,watermark,&mut self.wire);observe(8,false);observe(9,true);if self.last_publish.elapsed()>=self.period{if !self.suppress_publication{state.scan().publish(&self.wire);}self.last_publish=Instant::now();}else{self.publish_skipped+=1;}observe(9,false);}
    pub fn stop(&self,state:&AppState)->Result<()>{let core=state.ip_core().lock().unwrap();rcs::stop(&*core,self.map.repeat).map_err(anyhow::Error::msg)}
}
