//! Bounded instrumental TCP fanout, shared by native and explicit synthetic sources.
#![allow(missing_docs)]
use crate::observation_live::{Cursor,Fanout};
use std::{io::{self,Write},net::{SocketAddr,TcpListener,TcpStream},sync::{Arc,atomic::{AtomicBool,Ordering}},thread::{self,JoinHandle},time::{Duration,Instant}};
#[derive(Default,Debug)]
pub struct Stats{pub sent:u64,pub missed:u64,pub partial_disconnects:u64,pub errors:u64,pub accepted:u64,pub rejected:u64}
impl Stats{fn add(&mut self,r:Self){self.sent+=r.sent;self.missed+=r.missed;self.partial_disconnects+=r.partial_disconnects;self.errors+=r.errors;}}
fn serve(mut socket:TcpStream,q:Arc<Fanout>,mut cursor:Cursor,header:Arc<Vec<u8>>,done:Arc<AtomicBool>,max:usize)->Stats{
    let mut result=Stats::default();let mut pending=Vec::with_capacity(max.max(header.len()));pending.extend_from_slice(&header);let mut offset=0;let mut record=false;let mut drain=None;
    if socket.set_nonblocking(true).is_err(){result.errors=1;return result;}
    let _=socket.set_nodelay(true);
    loop{
        if done.load(Ordering::Acquire) && drain.is_none(){drain=Some(Instant::now());}
        if drain.is_some_and(|t|t.elapsed()>Duration::from_secs(15)){if offset!=0 && offset<pending.len(){result.partial_disconnects+=1;}result.errors+=1;break;}
        if offset==pending.len(){
            if record{result.sent+=1;record=false;}
            if q.poll(&mut cursor,&mut pending){offset=0;record=true;}
            else if done.load(Ordering::Acquire){break;}
            else{thread::sleep(Duration::from_millis(1));continue;}
        }
        match socket.write(&pending[offset..]){
            Ok(0)=>{result.errors+=1;if offset!=0{result.partial_disconnects+=1;}break;},
            Ok(n)=>offset+=n,
            Err(e) if e.kind()==io::ErrorKind::WouldBlock=>thread::sleep(Duration::from_millis(1)),
            Err(e) if e.kind()==io::ErrorKind::Interrupted=>{},
            Err(_)=>{result.errors+=1;if offset!=0{result.partial_disconnects+=1;}break;}
        }
    }
    result.missed=cursor.missed;result
}
pub struct Server{listener:TcpListener,q:Arc<Fanout>,header:Arc<Vec<u8>>,done:Arc<AtomicBool>,max:usize,workers:Vec<Option<JoinHandle<Stats>>>,stats:Stats}
impl Server{
    pub fn bind(address:SocketAddr,q:Arc<Fanout>,header:Vec<u8>,max:usize,clients:usize)->io::Result<Self>{
        if !(1..=8).contains(&clients){return Err(io::Error::new(io::ErrorKind::InvalidInput,"client limit 1..8"));}
        let listener=TcpListener::bind(address)?;listener.set_nonblocking(true)?;
        Ok(Self{listener,q,header:Arc::new(header),done:Arc::new(AtomicBool::new(false)),max,workers:(0..clients).map(|_|None).collect(),stats:Stats::default()})
    }
    pub fn address(&self)->io::Result<SocketAddr>{self.listener.local_addr()}
    pub fn active(&self)->usize{self.workers.iter().filter(|s|s.is_some()).count()}
    pub fn poll_accept(&mut self)->io::Result<()>{
        for slot in &mut self.workers{if slot.as_ref().is_some_and(|h|h.is_finished()){match slot.take().unwrap().join(){Ok(s)=>self.stats.add(s),Err(_)=>self.stats.errors+=1}}}
        for _ in 0..8{match self.listener.accept(){
            Ok((socket,_))=>{if let Some(slot)=self.workers.iter_mut().find(|s|s.is_none()){
                let (q,h,d,max)=(self.q.clone(),self.header.clone(),self.done.clone(),self.max);let cursor=q.subscribe();*slot=Some(thread::spawn(move||serve(socket,q,cursor,h,d,max)));self.stats.accepted+=1;
            }else{self.stats.rejected+=1;}},
            Err(e) if e.kind()==io::ErrorKind::WouldBlock=>break,Err(e)=>return Err(e)
        }}Ok(())
    }
    pub fn finish(&mut self)->&Stats{
        self.done.store(true,Ordering::Release);
        for slot in &mut self.workers{if let Some(h)=slot.take(){match h.join(){Ok(r)=>self.stats.add(r),Err(_)=>self.stats.errors+=1}}}&self.stats
    }
}
impl Drop for Server{fn drop(&mut self){self.finish();}}
