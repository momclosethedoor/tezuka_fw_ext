//! Opt-in bounded metadata instrumentation, no RF attribution from live MMIO.
#![allow(missing_docs)]
use std::{time::Instant,io::Write,path::Path};
pub const STAGES:[&str;13]=["arm","capture_wait","dma_copy","ram_assemble","validate_drain","validate_convert","normalize","scan_accept","scan_encode","scan_publish","gobs_encode","gobs_publish","rcs_tcp"];
pub struct Stamp(Instant,u64);
pub struct Diagnostic {start:Instant,cpu:fn()->u64,pub sink:bool,pub tracing:bool,records:Vec<[u64;8]>,used:usize,pub dropped:u64,pub wall:[u64;13],pub cpu_ns:[u64;13],pub calls:[u64;13],last_rcs:[u64;6],pub probe_ns:u64}
impl Diagnostic {
    pub fn new(sink:bool,tracing:bool,cpu:fn()->u64)->Self {
        let mut d=Self{start:Instant::now(),cpu,sink,tracing,records:vec![[0;8];65536],used:0,dropped:0,wall:[0;13],cpu_ns:[0;13],calls:[0;13],last_rcs:[u64::MAX;6],probe_ns:0};
        let probe=Instant::now();for _ in 0..1000{let s=d.stamp();d.finish(0,s);d.event(0,[0;6]);}d.probe_ns=probe.elapsed().as_nanos() as u64;
        d.used=0;d.wall=[0;13];d.cpu_ns=[0;13];d.calls=[0;13];d.start=Instant::now();d
    }
    pub fn stamp(&self)->Stamp {Stamp(Instant::now(),(self.cpu)())}
    pub fn finish(&mut self,stage:usize,s:Stamp){self.wall[stage]+=s.0.elapsed().as_nanos() as u64;self.cpu_ns[stage]+=(self.cpu)().saturating_sub(s.1);self.calls[stage]+=1;}
    pub fn event(&mut self,kind:u64,data:[u64;6]){if !self.tracing{return;}if self.used==self.records.len(){self.dropped+=1;return;}self.records[self.used]=[self.start.elapsed().as_nanos() as u64,kind,data[0],data[1],data[2],data[3],data[4],data[5]];self.used+=1;}
    pub fn rcs(&mut self,data:[u64;6]){if data!=self.last_rcs{self.event(5,data);self.last_rcs=data;}}
    pub fn write(&self,path:&Path)->std::io::Result<()> {
        std::fs::create_dir_all(path)?;let mut out=std::io::BufWriter::new(std::fs::File::create(path.join("trace.bin"))?);
        for row in &self.records[..self.used]{for v in row{out.write_all(&v.to_le_bytes())?;}}out.flush()?;
        let mut summary=std::fs::File::create(path.join("timings.json"))?;
        write!(summary,"{{\"version\":1,\"sink\":{},\"trace\":{},\"record_bytes\":64,\"capacity\":65536,\"used\":{},\"dropped\":{},\"probe_1000_stamp_finish_event_ns\":{},\"elapsed_ns\":{},\"cpu_clock\":\"CLOCK_PROCESS_CPUTIME_ID; concurrent server threads included by wall interval\",\"stages\":{{",self.sink,self.tracing,self.used,self.dropped,self.probe_ns,self.start.elapsed().as_nanos())?;
        for (i,name) in STAGES.iter().enumerate(){write!(summary,"{}\"{}\":{{\"wall_ns\":{},\"cpu_ns\":{},\"calls\":{}}}",if i==0{""}else{","},name,self.wall[i],self.cpu_ns[i],self.calls[i])?;}write!(summary,"}}}}\n")
    }
}
#[cfg(test)] mod tests {use super::*;
    #[test] fn trace_is_fixed_and_overflow_visible(){let mut d=Diagnostic::new(false,true,||0);let bytes=d.records.len()*64;for i in 0..70000{d.event(4,[i,0,0,0,0,0]);}assert_eq!(d.used,65536);assert_eq!(d.dropped,4464);assert_eq!(d.records.len()*64,bytes);}
    #[test] fn disabled_trace_preserves_stage_timing(){let mut d=Diagnostic::new(true,false,||10);let s=d.stamp();d.finish(2,s);d.event(4,[0;6]);assert_eq!(d.used,0);assert_eq!(d.calls[2],1);assert_eq!(d.cpu_ns[2],0);}
}
