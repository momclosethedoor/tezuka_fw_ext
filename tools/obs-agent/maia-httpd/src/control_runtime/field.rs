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
        let selected = positions
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                let lo = p["rx_lo_hz"].as_u64()?;
                let covers = p["rf_ranges"].as_array()?.iter().any(|w| {
                    w[0].as_u64().is_some_and(|f| f <= c.target_hz)
                        && w[1].as_u64().is_some_and(|f| c.target_hz < f)
                });
                (p["mixer"] == false
                    && p["rx_available"] == true
                    && covers
                    && lo.abs_diff(c.target_hz) <= 7_000_000)
                    .then_some((i, lo))
            })
            .min_by_key(|(_, lo)| lo.abs_diff(c.target_hz));
        let Some((position, lo)) = selected else {
            return Ok(None);
        };
        let generation = spectrum["generation"]
            .as_u64()
            .ok_or_else(|| anyhow!("FIELD1 generation"))?;
        let version = spectrum["version"]
            .as_u64()
            .ok_or_else(|| anyhow!("FIELD1 map version"))?;
        let nco = (((c.target_hz as i64 - lo as i64) * (1 << 28)) / FS as i64) as u32 & 0x0fff_ffff;
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
        for i in 0..positions.len() {
            put(&mut bytes, 32 + 32 * i + 4, i as u32);
        }
        let offset = 32 + 32 * position;
        put(&mut bytes, offset, 1);
        put_hz(&mut bytes, offset + 8, c.target_hz);
        put(&mut bytes, offset + 16, nco);
        put(&mut bytes, offset + 20, 24);
        put_hz(&mut bytes, offset + 24, generation);
        let table = output.join("field-watch.bin");
        update(&table, &bytes)?;
        let spawned = Command::new(&c.collector)
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
            json!({"target_hz":c.target_hz,
            "position":position,"version":version,"generation":generation,"timeout_ms":c.timeout_ms}),
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
