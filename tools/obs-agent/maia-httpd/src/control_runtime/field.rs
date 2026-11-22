//! Attach a single configured target to the existing FIELD1 watch transport.
use super::{FS, Registers, capture_enabled, put, put_hz};
use anyhow::{Result, anyhow, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub collector: PathBuf,
    pub target_hz: u64,
    pub timeout_ms: u64,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.collector.is_absolute()
                && self.collector.is_file()
                && (70_000_000..=6_000_000_000).contains(&self.target_hz)
                && (1..=1000).contains(&self.timeout_ms),
            "FIELD1 collector or target bounds"
        );
        Ok(())
    }
}

pub(super) struct Watch {
    child: Child,
    table: PathBuf,
    bytes: Vec<u8>,
    reported_exit: bool,
}

fn update(path: &Path, table: &[u8]) -> Result<()> {
    let pending = path.with_extension("pending");
    fs::write(&pending, table)?;
    fs::rename(&pending, path)?;
    Ok(())
}

fn target_position(rows: &[Value], rf: u64, phase: Option<&str>, used: &[(usize,u64,u64)]) -> Option<(usize,u64)> {
    rows.iter().enumerate().filter_map(|(i,p)| {
        let lo=p["rx_lo_hz"].as_u64()?;
        let covers=p["rf_ranges"].as_array()?.iter().any(|b|
            b[0].as_u64().zip(b[1].as_u64()).is_some_and(|(left,right)|left<=rf&&rf<right));
        (p["mixer"]==false && p["rx_available"]==true && p["transition_only"]!=true
         && phase.is_none_or(|want|p["sweep_phase"]==want) && covers && lo.abs_diff(rf)<=7_000_000
         && !used.iter().any(|(position,_,_)|*position==i)).then_some((i,lo))
    }).min_by_key(|(_,lo)|lo.abs_diff(rf))
}

fn target_assignments(rows:&[Value], pal_hz:u64, count:usize)->Result<Vec<(usize,u64,u64)>> {
    let mut assigned=Vec::with_capacity(count*2);
    let nominal=[180_000_000,790_000_000,1_420_000_000,2_030_000_000,2_660_000_000,
                 3_270_000_000,3_920_000_000,4_610_000_000,5_280_000_000];
    for i in 0..count {
        let center=if i==0{pal_hz}else{nominal[(i-1)*nominal.len()/(count-1)]};
        let candidates=std::iter::once(center).chain((1..=250).flat_map(|step|{
            let delta=step*100_000;
            [center-delta,center+delta]
        }));
        let (rf,a,b)=candidates.take(if i==0{9}else{501}).filter(|rf| *rf < 5_900_000_000).find_map(|rf|{
            Some((rf,target_position(rows,rf,Some("A"),&assigned)?,target_position(rows,rf,Some("B"),&assigned)?))
        }).ok_or_else(||anyhow!("no shared FIELD visit for {center} Hz"))?;
        assigned.extend([(a.0,a.1,rf),(b.0,b.1,rf)]);
    }
    Ok(assigned)
}

impl Watch {
    pub fn start(
        c: &Config,
        output: &Path,
        boot: &str,
        peer: SocketAddr,
        spectrum: &Value,
    ) -> Result<Option<(Self, Value)>> {
        let positions = spectrum["positions"]
            .as_array()
            .ok_or_else(|| anyhow!("FIELD1 positions"))?;
        let bench=std::env::var_os("OBS_MULTI_FIELD_WATCH_FILE");
        let targets=if let Some(path)=&bench {
            let count:usize=fs::read_to_string(path)?.trim().parse()?;
            ensure!(matches!(count,1|3|5|10)&&positions.len()<=512,"FIELD bench targets or map size");
            target_assignments(positions,c.target_hz,count)?
        } else if let Some((position,lo))=target_position(positions,c.target_hz,None,&[]) {
            vec![(position,lo,c.target_hz)]
        } else { return Ok(None) };
        let (position,_,reference_rf)=targets[0];
        let generation = spectrum["generation"]
            .as_u64()
            .ok_or_else(|| anyhow!("FIELD1 generation"))?;
        let version = spectrum["version"]
            .as_u64()
            .ok_or_else(|| anyhow!("FIELD1 map version"))?;
        let mut bytes = vec![0; 32 + 32 * positions.len()];
        for (offset, value) in [
            (0, 0x31544157),
            (4, 1),
            (8, version as u32),
            (12, positions.len() as u32),
            (24, 1),
            (28, 32),
        ] {
            put(&mut bytes, offset, value);
        }
        put_hz(&mut bytes, 16, 1);
        put(&mut bytes, 24, targets.len() as u32);
        for i in 0..positions.len() {
            put(&mut bytes, 32 + 32 * i + 4, i as u32);
        }
        for &(slot,lo,rf) in &targets {
            let nco=(((rf as i64-lo as i64)*(1<<28))/FS as i64) as u32 & 0x0fff_ffff;
            let offset=32+32*slot;
            put(&mut bytes,offset,1);
            put_hz(&mut bytes,offset+8,rf);
            put(&mut bytes,offset+16,nco);
            put(&mut bytes,offset+20,24);
            put_hz(&mut bytes,offset+24,generation);
        }
        let table = output.join("field-watch.bin");
        update(&table, &bytes)?;
        let mut collector=Command::new(&c.collector);
        if bench.is_some() {collector.env("OBS_FIELD1_UNQUALIFIED_RAW","1");}
        let spawned = collector
            .arg("--watch")
            .arg(output.join("field"))
            .arg(&table)
            .arg("0")
            .arg(c.timeout_ms.to_string())
            .arg("1")
            .env("OBS_FIELD1_BOOT", boot)
            .env("OBS_FIELD1_HOST", peer.ip().to_string())
            .env("OBS_FIELD1_PORT", peer.port().to_string())
            .env("OBS_TX_BLOCKS", "4")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let child = match spawned {
            Ok(x) => x,
            Err(e) => {
                let _ = fs::remove_file(&table);
                return Err(e.into());
            }
        };
        Ok(Some((
            Self {
                child,
                table,
                bytes,
                reported_exit: false,
            },
            json!({"target_hz":reference_rf,
            "position":position,"version":version,"generation":generation,"timeout_ms":c.timeout_ms,
            "targets":targets.iter().enumerate().map(|(i,(pos,_,rf))|
               json!({"rf_hz":rf,"position":pos,"signal_id":i/2,"phase":positions[*pos]["sweep_phase"]})).collect::<Vec<_>>() }),
        )))
    }

    pub fn exit_note(&mut self) -> Option<String> {
        if self.reported_exit {
            return None;
        }
        let status = match self.child.try_wait() {
            Ok(None) => return None,
            Ok(Some(status)) => format!("FIELD1 collector exited: {status}"),
            Err(e) => format!("FIELD1 collector readback: {e}"),
        };
        self.reported_exit = true;
        Some(status)
    }

    pub fn finish(mut self, regs: &Registers) -> Result<()> {
        put_hz(&mut self.bytes, 16, 2);
        put(&mut self.bytes, 24, 0);
        for i in 0..(self.bytes.len() - 32) / 32 {
            put(&mut self.bytes, 32 + 32 * i, 0);
        }
        let publication = update(&self.table, &self.bytes);
        let deadline = Instant::now() + Duration::from_millis(2300);
        let mut forced = publication.is_err();
        while !forced && self.child.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                forced = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(3));
        }
        if forced {
            let _ = self.child.kill();
            let _ = self.child.wait();
            regs.write(0x290, 2);
            regs.write(0x284, 0);
        }
        let _ = fs::remove_file(&self.table);
        let deadline = Instant::now() + Duration::from_millis(500);
        while regs.read(0x294) & 0xc1 != 0 || regs.read(0x284) != 0 || capture_enabled()? {
            ensure!(Instant::now() < deadline, "FIELD1 did not CANCEL/drain");
            std::thread::sleep(Duration::from_millis(3));
        }
        ensure!(
            !forced,
            "FIELD1 watchdog forced shutdown after failed drain request"
        );
        publication
    }
}
