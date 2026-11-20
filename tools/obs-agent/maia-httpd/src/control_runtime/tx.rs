//! Existing PS cyclic carrier and Nano TX gate. Called only by the mode owner.
use super::{FS, attr, int, set_attr, text_trim};
use anyhow::{Result, anyhow, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::{ErrorKind, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const IIO_TX: &str = "/usr/bin/iio_writedev";
const OFF: &str = "/usr/sbin/goofy_tx_off";
const DEVICE: &str = "cf-ad9361-dds-core-lpc";
const OFFSET: u64 = FS / 64;
const SAMPLE_COUNT: usize = 65536;

pub fn available() -> bool {
    [
        IIO_TX,
        OFF,
        "/etc/goofy/lease_enforce",
        "/run/goofy/goofy_lease.pid",
    ]
    .iter()
    .all(|p| Path::new(p).is_file())
}

pub fn lease_is_live() -> bool {
    available()
        && Path::new("/run/goofy/lease_ok").is_file()
        && text_trim("/run/goofy/goofy_lease.pid")
            .ok()
            .and_then(|p| p.parse::<i32>().ok())
            .is_some_and(|pid| pid > 1 && unsafe { libc::kill(pid, 0) } == 0)
}

fn gain_mdb() -> Result<u64> {
    let value = attr("out_voltage0_hardwaregain")?;
    let db = value
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("TX attenuation"))?
        .parse::<f64>()?;
    ensure!(
        db.is_finite() && (-89.75..=0.0).contains(&db),
        "TX attenuation out of range"
    );
    Ok((-db * 1000.).round() as u64)
}

pub fn readback(offset: u64) -> Result<Value> {
    let powerdown = attr("out_altvoltage1_TX_LO_powerdown")?.parse::<u64>()?;
    let pll = attr("out_altvoltage1_TX_LO_frequency")?.parse::<u64>()?;
    ensure!(powerdown <= 1, "TX powerdown readback");
    Ok(json!({"on":powerdown==0,"powerdown":powerdown,"pll_hz":pll,
        "frequency_hz":pll+offset,"attenuation_mdb":gain_mdb()?}))
}

fn tx_device() -> Result<PathBuf> {
    for node in fs::read_dir("/sys/bus/iio/devices")? {
        let node = node?.path();
        if text_trim(node.join("name")).as_deref().ok() == Some(DEVICE) {
            return Ok(node);
        }
    }
    Err(anyhow!("IIO TX device missing"))
}

fn buffer_enabled(device: &Path) -> bool {
    fs::read_dir(device).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            e.file_name().to_string_lossy().starts_with("buffer")
                && text_trim(e.path().join("enable")).as_deref().ok() == Some("1")
        })
    })
}

#[derive(Default)]
pub struct Carrier {
    child: Option<Child>,
    device: Option<PathBuf>,
    frequency_hz: u64,
}
impl Carrier {
    pub fn on(&self) -> bool {
        self.child.is_some()
    }

    pub fn off(&mut self) -> Result<Value> {
        // Gate before tearing down the buffer. Drop also closes a partially opened child.
        let gate = Command::new(OFF)
            .args(["scanner-control", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.device = None;
        ensure!(gate?.success(), "TX gate did not confirm");
        let result = readback(if self.frequency_hz == 0 { 0 } else { OFFSET })?;
        ensure!(
            result["powerdown"] == 1 && result["attenuation_mdb"] == 89750,
            "TX gate readback"
        );
        Ok(result)
    }

    pub fn apply(&mut self, command: &Value) -> Result<Value> {
        let on = command["on"] == true;
        if !on {
            return self.off();
        }
        let requested = int(command, "frequency_hz", 70_000_000 + OFFSET, 6_000_000_000)?;
        let attenuation = int(command, "attenuation_mdb", 0, 89750)?;
        ensure!(lease_is_live(), "TX lease not confirmed");
        self.off()?;
        self.frequency_hz = requested;
        let result = self.start(requested, attenuation, false);
        if result.is_err() {
            self.off()?;
        }
        result
    }

    pub fn prepare_guarded(&mut self, pll: u64) -> Result<()> {
        ensure!(lease_is_live(), "TX lease required before guarded scan");
        self.off()?;
        self.frequency_hz = pll + OFFSET;
        if let Err(e) = self.start(pll + OFFSET, 89750, true) {
            self.off()?;
            return Err(e);
        }
        Ok(())
    }

    pub fn buffer_live(&self) -> bool {
        self.child
            .as_ref()
            .is_some_and(|c| unsafe { libc::kill(c.id() as i32, 0) } == 0)
            && self.device.as_ref().is_some_and(|d| buffer_enabled(d))
    }

    fn start(&mut self, requested: u64, attenuation: u64, guarded: bool) -> Result<Value> {
        ensure!(
            attr("out_voltage_sampling_frequency")? == FS.to_string(),
            "TX sample rate differs"
        );
        let pll =
            set_attr("out_altvoltage1_TX_LO_frequency", requested - OFFSET)?.parse::<u64>()?;
        ensure!(
            (pll + OFFSET).abs_diff(requested) <= 10,
            "TX frequency readback"
        );
        let device = tx_device()?;
        ensure!(!buffer_enabled(&device), "IIO TX buffer owned");
        let mut cmd = Command::new(IIO_TX);
        cmd.args([
            "-u", "local:", "-T", "500", "-c", "-b", "65536", DEVICE, "voltage0", "voltage1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        let parent = unsafe { libc::getpid() };
        // If this worker is killed, release the cyclic buffer with the parent.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                    || libc::getppid() != parent
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        self.child = Some(cmd.spawn()?);
        self.device = Some(device);
        let child = self.child.as_mut().unwrap();
        let mut pipe = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("missing cyclic input"))?;
        ensure!(
            unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } == 0,
            "cyclic input setup"
        );
        // Same fixed OBS-007A I/Q block: 64 samples per period, 16-bit alignment.
        let mut period = [0; 256];
        for (i, sample) in period.chunks_exact_mut(4).enumerate() {
            let phase = std::f64::consts::TAU * i as f64 / 64.;
            for (byte, component) in sample.chunks_exact_mut(2).zip([phase.cos(), phase.sin()]) {
                byte.copy_from_slice(&((2047. * component).round() as i16 * 16).to_le_bytes());
            }
        }
        let mut block = period.repeat(SAMPLE_COUNT / 64);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut start = 0;
        while start < block.len() {
            ensure!(
                Instant::now() < deadline && child.try_wait()?.is_none(),
                "cyclic input timeout"
            );
            match pipe.write(&block[start..]) {
                Ok(n) if n > 0 => start += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(e) => return Err(e.into()),
                _ => return Err(anyhow!("cyclic input closed")),
            }
        }
        block.clear();
        while !buffer_enabled(self.device.as_ref().unwrap()) {
            ensure!(
                Instant::now() < deadline && child.try_wait()?.is_none(),
                "cyclic buffer enable timeout"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        ensure!(
            lease_is_live() && child.try_wait()?.is_none(),
            "TX lease lost before activation"
        );
        if guarded {
            let rb = readback(OFFSET)?;
            ensure!(
                rb["powerdown"] == 1 && rb["attenuation_mdb"] == 89750,
                "TX must be muted during handoff"
            );
            return Ok(rb);
        }
        set_attr(
            "out_voltage0_hardwaregain",
            format!("{:.3}", -(attenuation as f64) / 1000.0),
        )?;
        ensure!(
            gain_mdb()?.abs_diff(attenuation) <= 250,
            "TX attenuation readback"
        );
        ensure!(
            set_attr("out_altvoltage1_TX_LO_powerdown", 0)? == "0",
            "TX activation readback"
        );
        self.verify()
    }

    pub fn verify(&mut self) -> Result<Value> {
        ensure!(lease_is_live(), "TX lease lost");
        ensure!(
            self.child
                .as_mut()
                .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
                && self.device.as_ref().is_some_and(|d| buffer_enabled(d)),
            "cyclic carrier stopped"
        );
        let result = readback(OFFSET)?;
        ensure!(
            result["powerdown"] == 0
                && result["frequency_hz"]
                    .as_u64()
                    .is_some_and(|f| f.abs_diff(self.frequency_hz) <= 10),
            "TX readback changed"
        );
        Ok(result)
    }
}
impl Drop for Carrier {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.off();
        }
    }
}
