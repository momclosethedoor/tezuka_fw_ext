//! Explicit synthetic transport exerciser. Never opens DMA/IIO or controls RF.
//! This is not a native PL producer and is not started by maia-httpd.
#![allow(dead_code)]
#[path="../observation.rs"] mod observation;
#[path="../observation_live.rs"] mod observation_live;
#[path="../observation_transport.rs"] mod observation_transport;
use observation::{Descriptor,Observation,NO_TAG};
use observation_live::{Encoder,Fanout};
use observation_transport::Server;
use std::io::{self,Write};
use std::sync::{Arc,atomic::Ordering};
use std::thread;
use std::time::{Duration,Instant};

fn main(){if let Err(e)=run(){eprintln!("{e}");std::process::exit(1);}}
fn run()->Result<(),Box<dyn std::error::Error>>{
    let args:Vec<String>=std::env::args().skip(1).collect();
    if !args.iter().any(|s|s=="--synthetic"){return Err("SYNTHETIC ONLY: --synthetic and --capture-id UUID required; no hardware fallback".into());}
    let value=|key:&str,default:&str|->String{args.iter().position(|s|s==key).map(|i|args.get(i+1).expect("missing option value").clone()).unwrap_or_else(||default.into())};
    let known=["--capture-id","--bind","--seconds","--rate","--bins","--queue","--clients","--wait-clients","--context-every","--unclean-every"];
    let mut i=0;while i<args.len(){if args[i]=="--synthetic"{i+=1;}else if known.contains(&args[i].as_str()) && i+1<args.len(){i+=2;}else{return Err(format!("unknown/missing argument {}",args[i]).into());}}
    let id=value("--capture-id","").replace('-',"");
    if id.len()!=32{return Err("explicit fresh capture UUID is required".into());}
    let mut session=[0;16];for (i,x) in session.iter_mut().enumerate(){*x=u8::from_str_radix(&id[2*i..2*i+2],16)?;}
    let seconds:f64=value("--seconds","30").parse()?;let rate:f64=value("--rate","1000").parse()?;
    let bins:u32=value("--bins","4096").parse()?;let capacity:usize=value("--queue","8").parse()?;let clients:usize=value("--clients","2").parse()?;let wait:usize=value("--wait-clients","0").parse()?;
    let context:u64=value("--context-every","10000").parse()?;let unclean:u64=value("--unclean-every","0").parse()?;
    if !seconds.is_finite() || !(0.01..=240.).contains(&seconds) || !rate.is_finite() || !(1.0..=100000.).contains(&rate) || !(1..=8).contains(&clients) || wait>clients || context==0{return Err("duration/rate/clients/context bounds".into());}
    let ds=(1..=2).map(|id|Descriptor{id,sample_rate:4e6,bin0_hz:id as f64*100e6,enbw_bins:1.5,tick_hz:4_000_000,bins,mode:1,source:1}).collect();
    let mut encoder=Encoder::new_nint1(session,ds)?;let max=encoder.max_record();let header=encoder.header().to_vec();
    let hub=Arc::new(Fanout::new(capacity,max)?);
    let mut server=Server::bind(value("--bind","127.0.0.1:0").parse()?,hub.clone(),header,max,clients)?;
    println!("SYNTHETIC ONLY; no PL/DMA/RF access");println!("READY {}",server.address()?);io::stdout().flush()?;
    let waiting=Instant::now();
    while server.active()<wait{server.poll_accept()?;if waiting.elapsed()>Duration::from_secs(15){return Err("synthetic startup client timeout".into());}thread::sleep(Duration::from_millis(1));}
    let start=Instant::now();let mut frame=Vec::with_capacity(max);let mut o=Observation{descriptor_id:1,seq:0,epoch:1,first_tick:0,nint:1,tag:NO_TAG,power:vec![0.;bins as usize]};
    let mut produced=0u64;
    while start.elapsed().as_secs_f64()<seconds{
        server.poll_accept()?;
        let due=produced as f64/rate;let elapsed=start.elapsed().as_secs_f64();
        if due>elapsed{thread::sleep(Duration::from_secs_f64((due-elapsed).min(0.001)));continue;}
        o.seq=produced;o.epoch=1+produced/context;o.descriptor_id=1+((o.epoch-1)%2) as u16;o.first_tick=produced.checked_mul(bins as u64).ok_or("synthetic tick overflow")?;
        if unclean!=0 && produced%unclean==0{o.epoch=0;}
        for (k,x) in o.power.iter_mut().enumerate(){*x=((produced%1024)*16+k as u64%16) as f32/16384.;}
        encoder.observation(&o,&mut frame)?;hub.publish(&frame)?;produced+=1;
    }
    let total=server.finish();let accepted=total.accepted;let rejected=total.rejected;
    println!("{{\"source\":\"synthetic\",\"produced\":{produced},\"published\":{},\"contention_drops\":{},\"client_missed\":{},\"sent_records\":{},\"accepted\":{accepted},\"rejected\":{rejected},\"partial_disconnects\":{},\"sender_errors\":{},\"queue_payload_capacity\":{},\"frame_capacity\":{},\"power_capacity\":{},\"max_clients\":{clients},\"seconds\":{}}}",hub.published(),hub.contention_drops.load(Ordering::Relaxed),total.missed,total.sent,total.partial_disconnects,total.errors,hub.allocated_payload_bytes(),frame.capacity(),o.power.capacity(),start.elapsed().as_secs_f64());
    Ok(())
}
