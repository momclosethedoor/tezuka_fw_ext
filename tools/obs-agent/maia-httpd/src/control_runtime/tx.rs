//! Existing PS cyclic carrier and Nano TX gate. Called only by the mode owner.
use super::{FS, attr, int, set_attr, text_trim};
use anyhow::{Result, anyhow, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, ErrorKind, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}},
    time::{Duration, Instant},
};

const PS_RF_CLAIM: libc::c_ulong = 0x460c;
const IIO_TX: &str = "/usr/bin/iio_writedev";
const OFF: &str = "/usr/sbin/goofy_tx_off";
const DEVICE: &str = "cf-ad9361-dds-core-lpc";
const OFFSET: u64 = FS / 64;
const SAMPLE_COUNT: usize = 65536;

pub fn lock_radio(cancel: &AtomicBool) -> Result<File> {
    let file = OpenOptions::new().write(true).create(true).truncate(false)
        .open("/run/goofy/radio.lock")?;
    loop {
        ensure!(!cancel.load(Ordering::Acquire),"preparation cancelled while waiting for radio");
        if unsafe { libc::flock(file.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB) }==0 {
            return Ok(file);
        }
        let err=std::io::Error::last_os_error();
        if err.kind()!=ErrorKind::WouldBlock {return Err(err.into());}
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn child_follows_owner(cmd: &mut Command) {
    let owner = unsafe { libc::getpid() };
    // IIO buffer descriptors belong to the ARM process.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != owner {
                return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

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

// The stock lease daemon timestamps its decisions with /proc/uptime. A
// present lease_ok can outlive a stalled telemetry read. Expire its last
// positive decision using that same board clock before renewing physical RF.
struct LeaseDecisions {
    child: Child,
    expiry_ms: Arc<AtomicU64>,
}

impl LeaseDecisions {
    fn start() -> Result<Self> {
        let mut child = Command::new("mosquitto_sub")
            .args(["-h", "127.0.0.1", "-R", "-t", "state/goofy/lease"])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let pipe = child.stdout.take().unwrap();
        let expiry_ms = Arc::new(AtomicU64::new(0));
        let published = expiry_ms.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(std::result::Result::ok) {
                let expiry = serde_json::from_str::<Value>(&line).ok().and_then(|v| {
                    if v["alive"] != true || v["enforce"] != 1 || v["lease"] != 1
                        || v["released"] != 0 || v["lease_ms"] != 5000 { return None; }
                    let up = v["up_ms"].as_u64()?;
                    up.checked_sub(v["age_ms"].as_u64()?)?.checked_add(5000)
                }).unwrap_or(0);
                published.store(expiry, Ordering::Release);
            }
            published.store(0, Ordering::Release);
        });
        Ok(Self { child, expiry_ms })
    }

    fn live(&self) -> bool {
        let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut t) } != 0 { return false; }
        let now = t.tv_sec as u64 * 1000 + t.tv_nsec as u64 / 1_000_000;
        let until = self.expiry_ms.load(Ordering::Acquire);
        until > now && until - now <= 5000 && lease_is_live()
    }
}

impl Drop for LeaseDecisions {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct ProcessOwner {
    guard: File,
    radio: File,
}

#[derive(Default)]
pub struct Carrier {
    process_owner: Option<ProcessOwner>,
    lease: Option<LeaseDecisions>,
    child: Option<Child>,
    device: Option<PathBuf>,
    frequency_hz: u64,
}
impl Carrier {
    pub fn on(&self) -> bool {
        self.child.is_some()
    }

    pub fn process_owned(&self) -> bool {
        self.process_owner.is_some()
    }

    pub fn radio_lock(&self, cancel: &AtomicBool) -> Result<File> {
        match &self.process_owner {
            Some(owner) => Ok(owner.radio.try_clone()?),
            None => lock_radio(cancel),
        }
    }

    pub fn take_process_ownership(&mut self, radio: &File) -> Result<Value> {
        if self.process_owner.is_none() {
            let guard = OpenOptions::new().read(true).write(true).open("/dev/obs_fl_guard")?;
            if unsafe { libc::ioctl(guard.as_raw_fd(), PS_RF_CLAIM) } != 0 {
                return Err(anyhow!("PS RF owner claim: {}", std::io::Error::last_os_error()));
            }
            self.process_owner = Some(ProcessOwner { guard, radio: radio.try_clone()? });
        }
        self.verify()
    }

    pub fn off(&mut self) -> Result<Value> {
        // Gate before tearing down the buffer. Drop also closes a partially opened child.
        let gate = Command::new(OFF)
            .args(["scanner-control", "0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let closed = self.close_buffer(false);
        if let Some(ProcessOwner { guard, radio }) = self.process_owner.take() {
            // Close the safety FD while legacy writers are still excluded.
            drop(guard);
            drop(radio);
        }
        closed?;
        ensure!(gate?.success(), "TX gate did not confirm");
        self.muted_readback()
    }

    pub fn after_guard(&mut self) -> Result<Value> {
        // The guard already confirmed the physical mute and returned PS SPI.
        // Closing this owned cyclic buffer needs no second PS gate sequence.
        self.close_buffer(true)?;
        self.muted_readback()
    }

    fn close_buffer(&mut self, guard_muted: bool) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            if !guard_muted {
                // Give the manual carrier its usual libiio signal cleanup.
                unsafe { libc::kill(child.id() as i32, libc::SIGTERM); }
                let deadline = Instant::now() + Duration::from_millis(500);
                while child.try_wait()?.is_none() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            if child.try_wait()?.is_none() {
                child.kill()?;
                child.wait()?;
            }
            if let Some(device) = &self.device
                && buffer_enabled(device)
            {
                // This owner opened buffer 0 after checking it was idle.
                let en = device.join("buffer0/enable");
                fs::write(
                    if en.exists() {
                        en
                    } else {
                        device.join("buffer/enable")
                    },
                    "0",
                )?;
                ensure!(
                    !buffer_enabled(device),
                    "cyclic buffer remains enabled after TX off"
                );
            }
        }
        self.device = None;
        Ok(())
    }

    fn muted_readback(&self) -> Result<Value> {
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
        let result = self.start(requested, attenuation, false, |_| {});
        if result.is_err() {
            self.off()?;
        }
        result
    }

    pub fn guarded_lease_live(&self) -> bool {
        self.lease.as_ref().is_some_and(LeaseDecisions::live)
    }

    pub fn prepare_guarded(&mut self, pll: u64, cancel: &AtomicBool, trace: impl Fn(&'static str)) -> Result<()> {
        self.off()?;
        trace("carrier_muted");
        if self.lease.is_none() {
            self.lease = Some(LeaseDecisions::start()?);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while !self.guarded_lease_live() {
            ensure!(!cancel.load(Ordering::Acquire), "preparation cancelled");
            ensure!(Instant::now() < deadline, "fresh TX lease required before guarded scan");
            std::thread::sleep(Duration::from_millis(20));
        }
        trace("carrier_lease_ready");
        self.frequency_hz = pll + OFFSET;
        if let Err(e) = self.start(pll + OFFSET, 89750, true, trace) {
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

    fn start(&mut self, requested: u64, attenuation: u64, guarded: bool, trace: impl Fn(&'static str)) -> Result<Value> {
        let sample_rate=attr("out_voltage_sampling_frequency")?;
        ensure!(
            sample_rate == FS.to_string(),
            "TX sample rate {sample_rate:?}, expected {FS}"
        );
        let pll =
            set_attr("out_altvoltage1_TX_LO_frequency", requested - OFFSET)?.parse::<u64>()?;
        ensure!(
            (pll + OFFSET).abs_diff(requested) <= 10,
            "TX frequency readback"
        );
        trace("carrier_pll_ready");
        let device = tx_device()?;
        ensure!(!buffer_enabled(&device), "IIO TX buffer owned");
        let mut cmd = Command::new(IIO_TX);
        cmd.args([
            "-u", "local:", "-T", "500", "-c", "-b", "65536", DEVICE, "voltage0", "voltage1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        child_follows_owner(&mut cmd);
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
        trace("carrier_buffer_ready");
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
        let local = self.process_owner.is_some();
        ensure!(local || lease_is_live(), "TX lease lost");
        ensure!(
            self.child
                .as_mut()
                .is_some_and(|c| matches!(c.try_wait(), Ok(None)))
                && self.device.as_ref().is_some_and(|d| buffer_enabled(d)),
            "cyclic carrier stopped"
        );
        let mut result = readback(OFFSET)?;
        ensure!(
            result["powerdown"] == 0
                && result["frequency_hz"]
                    .as_u64()
                    .is_some_and(|f| f.abs_diff(self.frequency_hz) <= 10),
            "TX readback changed"
        );
        result["owner"] = json!(if local { "arm_process" } else { "client_lease" });
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
