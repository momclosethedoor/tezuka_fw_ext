//! Bind persistent FIELD targets to the already calibrated physical A/B map.
use super::{FS, Registers, put, put_hz, word};
use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    collector: PathBuf,
    timeout_ms: u32,
}

impl Config {
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(self.collector.is_absolute() && self.collector.is_file()
            && (1..=1000).contains(&self.timeout_ms), "resident FIELD collector or timeout");
        Ok(())
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Target {
    target_id: u32,
    lo_hz: u64,
    hi_hz: u64,
    generation: u64,
}

pub(super) fn targets(value: &Value) -> Result<Vec<Target>> {
    if value.is_null() { return Ok(Vec::new()); }
    let targets: Vec<Target> = serde_json::from_value(value.clone()).context("field_targets")?;
    let mut ids = BTreeSet::new();
    ensure!(targets.len() <= 10 && targets.iter().all(|t| t.target_id > 0 && ids.insert(t.target_id)
        && t.generation > 0 && (70_000_000..t.hi_hz).contains(&t.lo_hz)
        && t.hi_hz <= super::plan_math::MAX_RF), "resident FIELD targets, unique IDs and RF bounds");
    Ok(targets)
}

pub(super) struct Plan {
    bytes: Vec<u8>,
    summary: Value,
}

impl Plan {
    pub(super) fn compile(rows: &mut [u8], desc: &Value, targets: &[Target], revision: u64) -> Result<Self> {
        let positions = desc["positions"].as_array().ok_or_else(|| anyhow!("FIELD positions missing"))?;
        ensure!(rows.len() == 32 * positions.len() && (1..=65535).contains(&positions.len()), "FIELD map length");
        let version = desc["version"].as_u64().ok_or_else(|| anyhow!("FIELD map version"))?;
        ensure!((1..=u32::MAX as u64).contains(&version), "FIELD map version bounds");
        let mut table = vec![0; 32 + rows.len()];
        for (offset, value) in [(0,0x31544157),(4,1),(8,version as u32),(12,positions.len() as u32),
            (24,(targets.len()*2) as u32),(28,32)] {
            put(&mut table, offset, value);
        }
        put_hz(&mut table, 16, revision);
        for index in 0..positions.len() { put(&mut table, 32+index*32+4, index as u32); }
        let mut assignments = Vec::with_capacity(targets.len()*2);
        let mut occupied = BTreeSet::new();
        for target in targets {
            let hz = target.lo_hz + (target.hi_hz - target.lo_hz)/2;
            for phase in ["A", "B"] {
                let (_, index, offset) = positions.iter().enumerate().filter_map(|(index, row)| {
                    if row["rx_available"]!=true || row["transition_only"]==true || row["sweep_phase"]!=phase
                        || occupied.contains(&index) { return None; }
                    let covers = row["rf_ranges"].as_array()?.iter().any(|bounds|
                        bounds[0].as_u64().is_some_and(|lo| lo <= target.lo_hz)
                         && bounds[1].as_u64().is_some_and(|hi| hi >= target.hi_hz));
                    let offset = row["side_sign"].as_i64()? * (hz as i64-row["rf_center_hz"].as_i64()?);
                    covers.then_some((offset.unsigned_abs(),index,offset))
                }).min().ok_or_else(|| anyhow!("target {} has no free usable {phase} visit",target.target_id))?;
                occupied.insert(index);
                let nco = (offset * (1 << 28) / FS as i64) as u32 & 0x0fff_ffff;
                let at=32+index*32;
                put(&mut table,at,1);
                put_hz(&mut table,at+8,hz);
                put(&mut table,at+16,nco);
                put(&mut table,at+20,24);
                put_hz(&mut table,at+24,target.generation);
                let row=index*32;
                let flags=word(rows,row+4);
                put(rows,row+4,flags|32);
                if flags&8==0 { put(rows,row+12,nco); }
                assignments.push(json!({"target_id":target.target_id,"lo_hz":target.lo_hz,
                    "hi_hz":target.hi_hz,"target_hz":hz,"position":index,"phase":phase,
                    "nco":nco,"generation":target.generation}));
            }
        }
        Ok(Self{bytes:table,summary:json!({"version":version,"revision":revision,
            "positions":positions.len(),"assignments":assignments})})
    }

    pub(super) fn save(&self, directory: &Path) -> Result<Value> {
        let version=self.summary["version"].as_u64().unwrap();
        fs::write(directory.join(format!("WAT1-{version}.bin")),&self.bytes)?;
        fs::write(directory.join(format!("WAT1-{version}.json")),serde_json::to_vec(&self.summary)?)?;
        Ok(self.summary.clone())
    }
}

pub(super) struct Session {
    child: Child,
    reported_exit: bool,
}

impl Session {
    pub(super) fn start(config: &Config, directory: &Path, peer: SocketAddr, regs: &Registers,
        mut renew: impl FnMut() -> Result<()>) -> Result<Self> {
        let log=directory.join("collector.log");
        let mut command=Command::new(&config.collector);
        command.arg(directory).arg(config.timeout_ms.to_string())
            .env("OBS_FFM1_HOST",peer.ip().to_string()).env("OBS_FFM1_PORT",peer.port().to_string());
        #[cfg(feature="scan-stream-bench")]
        if std::env::var_os("OBS_FFM1_TRACE").is_some() { command.env("OBS_FFM1_TRACE","1"); }
        let mut child=command
            .stdin(Stdio::null()).stdout(File::create(directory.join("receipts.jsonl"))?)
            .stderr(File::create(&log)?).spawn()?;
        let ready=(|| {
            let start=Instant::now();
            loop {
                ensure!(child.try_wait()?.is_none(), "FFM1 collector exited before SESSION (see collector.log)");
                if regs.read(0x354)==1 { return Ok(()); }
                ensure!(start.elapsed()<Duration::from_millis(1500), "FFM1 SESSION readiness timeout");
                renew()?;
                std::thread::sleep(Duration::from_millis(10));
            }
        })();
        if let Err(error)=ready {
            let _=terminate(&mut child,Duration::from_millis(1500));
            return Err(error);
        }
        Ok(Self{child,reported_exit:false})
    }

    pub(super) fn exit_note(&mut self) -> Option<String> {
        if self.reported_exit { return None; }
        let note=match self.child.try_wait() {
            Ok(None)=>return None,
            Ok(Some(status))=>format!("FFM1 collector exited: {status}"),
            Err(error)=>format!("FFM1 collector readback: {error}"),
        };
        self.reported_exit=true;
        Some(note)
    }

    pub(super) fn finish(mut self, regs: &Registers, renew: impl FnMut() -> Result<()>) -> Result<()> {
        let result=terminate_with(&mut self.child,Duration::from_millis(1600),renew);
        if result.is_err() {
            regs.write(0x354,0);
            regs.write(0x290,2);
            regs.write(0x284,0);
        }
        result?;
        ensure!(regs.read(0x354)==0 && regs.read(0x294)&0xc1==0 && regs.read(0x35c)&0x1ff==0,
            "FFM1 tail or session remains active");
        Ok(())
    }
}

fn terminate(child: &mut Child, budget: Duration) -> Result<()> {
    terminate_with(child,budget,||Ok(()))
}

fn terminate_with(child: &mut Child, budget: Duration, mut renew: impl FnMut() -> Result<()>) -> Result<()> {
    if child.try_wait()?.is_none() {
        unsafe { libc::kill(child.id() as i32,libc::SIGTERM); }
    }
    let start=Instant::now();
    loop {
        if let Some(status)=child.try_wait()? {
            ensure!(status.success(), "FFM1 collector exit {status}");
            return Ok(());
        }
        if start.elapsed()>=budget {
            child.kill()?;
            child.wait()?;
            return Err(anyhow!("FFM1 collector drain timeout"));
        }
        let _=renew();
        std::thread::sleep(Duration::from_millis(20));
    }
}
