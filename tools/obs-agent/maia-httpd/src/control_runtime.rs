//! Interactive owner for the row99 guarded RAM runtime.
#![allow(missing_docs)]

use anyhow::{Result, anyhow, bail, ensure};
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const FS: u64 = 61_440_000;
const PHY: &str = "/sys/bus/iio/devices/iio:device0";
const MAP: usize = 40 + 4096;
const NEXT: usize = MAP + 4096;
static STOP_RUNTIME: AtomicBool = AtomicBool::new(false);
extern "C" fn shutdown_signal(_: libc::c_int) {
    STOP_RUNTIME.store(true, Ordering::Release);
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    boot_id: String,
    setup: PathBuf,
    output: PathBuf,
    view: SocketAddr,
    video: SocketAddr,
    route: SocketAddr,
    positions: Vec<u64>,
    scan: Value,
    analysis: Value,
}

#[derive(Default)]
struct Inbox {
    next: Option<Value>,
    pending: Option<Value>,
    actual: Value,
    applied_id: Value,
    failed_id: Value,
    error: Value,
    receipt_ns: u64,
    route_pending: Value,
    metrics: Value,
}
struct Shared {
    inbox: Mutex<Inbox>,
    wake: Condvar,
    switch: AtomicBool,
    shutdown: AtomicBool,
}

fn int(v: &Value, k: &str, lo: u64, hi: u64) -> Result<u64> {
    v[k].as_u64()
        .filter(|n| (lo..=hi).contains(n))
        .ok_or_else(|| anyhow!("{k}: {lo}..{hi}"))
}
fn text_trim(path: impl AsRef<Path>) -> Result<String> {
    Ok(fs::read_to_string(path)?.trim().into())
}
fn attr(name: &str) -> Result<String> {
    text_trim(Path::new(PHY).join(name))
}
fn set_attr(name: &str, value: impl ToString) -> Result<String> {
    fs::write(Path::new(PHY).join(name), value.to_string())?;
    attr(name)
}
fn clock_ns() -> u64 {
    (crate::delivery_runtime::mono() * 1e9) as u64
}

fn validate(v: &Value) -> Result<()> {
    match v["op"].as_str() {
        Some("scan") => {
            int(v, "k", 1, 1023)?;
            let run = v["run"]
                .as_bool()
                .ok_or_else(|| anyhow!("run must be boolean"))?;
            let ranges = v["ranges"]
                .as_array()
                .filter(|r| (1..=16).contains(&r.len()))
                .ok_or_else(|| anyhow!("ranges: 1..16"))?;
            let mut enabled = false;
            for r in ranges {
                let lo = int(r, "lo_hz", 70_000_000, 18_000_000_000)?;
                int(r, "hi_hz", lo + 120_000, 18_000_000_000)?;
                enabled |= r["enabled"]
                    .as_bool()
                    .ok_or_else(|| anyhow!("enabled must be boolean"))?;
            }
            ensure!(!run || enabled, "no enabled range");
        }
        Some("analysis") | Some("video") => {
            int(v, "center_hz", 70_000_000, 6_000_000_000)?;
            int(
                v,
                "width_hz",
                120_000,
                if v["op"] == "video" {
                    12_000_000
                } else {
                    48_000_000
                },
            )?;
            if v["op"] == "analysis" {
                int(v, "k", 1, 1023)?;
                int(v, "gain_db", 0, 71)?;
                ensure!(
                    matches!(v["agc"].as_str(), Some("manual" | "slow_attack")),
                    "unsupported AGC"
                );
            }
        }
        Some("return") => {}
        Some("route") => {
            ensure!(
                v["kind"] == "direct",
                "mixer requires a verified calibrated TX/IF route in this RAM owner"
            );
            int(v, "antenna", 0, 7)?;
        }
        Some("tx") => {
            ensure!(
                v["on"] == false,
                "TX-on requires a calibrated carrier owner and expiring hardware lease"
            );
            int(v, "frequency_hz", 70_000_000, 6_000_000_000)?;
        }
        _ => bail!("unknown command"),
    }
    Ok(())
}

async fn status(State(shared): State<Arc<Shared>>) -> Json<Value> {
    let q = shared.inbox.lock().unwrap();
    let reader = crate::system_reader::status();
    Json(
        json!({"abi":1,"kind":"guarded-control","origin":"hardware","actual":q.actual,
        "pending_id":q.next.as_ref().or(q.pending.as_ref()).map(|v|&v["id"]),
        "applied_id":q.applied_id,"failed_id":q.failed_id,"error":q.error,
        "route_pending":q.route_pending,"arm_clock_ns":q.receipt_ns.to_string(),
        "capabilities":{"tx_on":false,"mixer":false,"finite_field":false},"metrics":{"owner":q.metrics,"reader":reader}}),
    )
}

async fn submit(
    State(shared): State<Arc<Shared>>,
    Json(cmd): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let id = &cmd["id"];
    if !id
        .as_str()
        .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid id"})));
    }
    if let Err(e) = validate(&cmd) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":e.to_string()})),
        );
    }
    let mut q = shared.inbox.lock().unwrap();
    if q.next.is_some() || q.pending.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"another hardware command is pending"})),
        );
    }
    if q.applied_id == *id || q.failed_id == *id {
        return (StatusCode::ACCEPTED, Json(json!({"accepted_id":id})));
    }
    let id = id.clone();
    q.next = Some(cmd);
    shared.switch.store(true, Ordering::Release);
    shared.wake.notify_one();
    (StatusCode::ACCEPTED, Json(json!({"accepted_id":id})))
}

fn receipt(shared: &Shared, state: &Value, id: &Value, error: Option<String>) {
    let mut q = shared.inbox.lock().unwrap();
    q.pending = None;
    q.actual = state.clone();
    q.receipt_ns = clock_ns();
    if let Some(e) = error {
        q.failed_id = id.clone();
        q.error = json!(e);
    } else {
        q.applied_id = id.clone();
        q.error = Value::Null;
    }
}

fn word(b: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(b[offset..offset + 4].try_into().unwrap())
}
fn put(b: &mut [u8], offset: usize, n: u32) {
    b[offset..offset + 4].copy_from_slice(&n.to_le_bytes());
}
fn plan(c: &Config, template: &[u8], state: &Value, marker: bool) -> Result<(Vec<u8>, Value)> {
    let mut bytes = template.to_vec();
    let mut locations = Vec::new();
    let mut rf_ranges = Vec::new();
    let mut indexes = Vec::new();
    for range in state["scan"]["ranges"]
        .as_array()
        .ok_or_else(|| anyhow!("scan ranges"))?
    {
        if range["enabled"] != true {
            continue;
        }
        let lo = int(range, "lo_hz", 70_000_000, 6_000_000_000)?;
        let hi = int(range, "hi_hz", lo + 120_000, 6_000_000_000)?;
        rf_ranges.push([lo, hi]);
        let mut covered = lo;
        for (idx, &f) in c.positions.iter().enumerate() {
            let (a, b) = (f.saturating_sub(23_800_000), f + 23_800_000);
            if b <= lo || a >= hi {
                continue;
            }
            ensure!(
                a <= covered,
                "range begins or crosses outside calibrated RX coverage"
            );
            covered = covered.max(b);
            if !indexes.contains(&idx) {
                indexes.push(idx);
            }
        }
        ensure!(covered >= hi, "range ends outside calibrated RX coverage");
    }
    ensure!(!indexes.is_empty(), "no calibrated scan position selected");
    let v = int(state, "map_version", 1, 0x7fff_fffe)? as u32;
    put(&mut bytes, 8, indexes.len() as u32);
    put(&mut bytes, 12, indexes.len() as u32);
    put(&mut bytes, 16, v);
    put(&mut bytes, 20, v + 1);
    bytes[MAP..NEXT + 4096].fill(0);
    for (i, &idx) in indexes.iter().enumerate() {
        let f = c.positions[idx];
        locations.push(json!({"rx_lo_hz":f,"rf_center_hz":f,"side_sign":1,"mixer":false}));
        for off in [MAP, NEXT] {
            let entry = off + 32 * i;
            bytes[entry..entry + 32]
                .copy_from_slice(&template[MAP + 32 * idx..MAP + 32 * (idx + 1)]);
            if marker && off == MAP && i == 0 {
                put(
                    &mut bytes,
                    entry + 4,
                    3 | ((state["route"]["antenna"].as_u64().unwrap() as u32) << 16),
                );
                put(&mut bytes, entry + 8, v);
            }
        }
    }
    Ok((bytes, json!({"rf_ranges":rf_ranges,"positions":locations})))
}

fn guard(fd: &File, cmd: libc::c_ulong, arg: *mut libc::c_void) -> Result<[u32; 128]> {
    let mut s = [0u32; 128];
    let arg = if arg.is_null() {
        s.as_mut_ptr().cast()
    } else {
        arg
    };
    ensure!(
        unsafe { libc::ioctl(fd.as_raw_fd(), cmd, arg) } == 0,
        "guard {cmd:x}: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        unsafe { libc::ioctl(fd.as_raw_fd(), 0x4605, s.as_mut_ptr()) } == 0 && s[2] == 0,
        "guard status/error"
    );
    Ok(s)
}
fn guard_cmd(fd: &File, cmd: libc::c_ulong) -> Result<[u32; 128]> {
    guard(fd, cmd, std::ptr::null_mut())
}

struct Route {
    stream: TcpStream,
    key: (u32, u64, u32, u32, u32),
    out: Vec<u8>,
    written: usize,
    input: Vec<u8>,
    since: Instant,
}
fn acknowledge(
    route: &mut Option<Route>,
    c: &Config,
    shared: &Shared,
    generation: u64,
    id: u32,
    epoch: u64,
    version: u32,
    position: u32,
    antenna: u32,
) -> std::result::Result<Option<u32>, String> {
    let key = (id, epoch, version, position, antenna);
    if route.as_ref().is_none_or(|r| r.key != key) {
        let addr = c.route;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_millis(50))
            .map_err(|e| e.to_string())?;
        stream.set_nonblocking(true).map_err(|e| e.to_string())?;
        let v = json!({"marker_id":id,"epoch":epoch,"version":version,"position":position,"route":antenna,"generation":generation});
        let mut out = serde_json::to_vec(&v).unwrap();
        out.push(b'\n');
        shared.inbox.lock().unwrap().route_pending = v;
        *route = Some(Route {
            stream,
            key,
            out,
            written: 0,
            input: Vec::new(),
            since: Instant::now(),
        });
    }
    let r = route.as_mut().unwrap();
    if r.since.elapsed() > Duration::from_secs(2) {
        return Err("route ACK timeout".into());
    }
    if r.written < r.out.len() {
        match r.stream.write(&r.out[r.written..]) {
            Ok(0) => return Err("route socket closed".into()),
            Ok(n) => r.written += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e.to_string()),
        }
    }
    let mut data = [0; 128];
    match r.stream.read(&mut data) {
        Ok(0) => return Err("route stream closed before current ACK".into()),
        Ok(n) => r.input.extend_from_slice(&data[..n]),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
        Err(e) => return Err(e.to_string()),
    }
    if r.input.len() > 512 {
        return Err("route ACK length".into());
    }
    let expected = format!("ACK {id}");
    while let Some(end) = r.input.iter().position(|b| *b == b'\n') {
        let ok = &r.input[..end] == expected.as_bytes();
        r.input.drain(..=end);
        if ok {
            shared.inbox.lock().unwrap().route_pending = Value::Null;
            return Ok(Some(id));
        }
    }
    Ok(None)
}

struct Registers(*mut u32);
impl Registers {
    fn open() -> Result<Self> {
        let fd = OpenOptions::new().read(true).write(true).open("/dev/mem")?;
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0x7c460000,
            )
        };
        ensure!(p != libc::MAP_FAILED, "Maia CSR mmap");
        Ok(Self(p.cast()))
    }
    fn read(&self, o: usize) -> u32 {
        unsafe { self.0.add(o / 4).read_volatile() }
    }
    fn write(&self, o: usize, v: u32) {
        unsafe { self.0.add(o / 4).write_volatile(v) };
    }
}
impl Drop for Registers {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.0.cast(), 4096);
        }
    }
}

fn tune(state: &mut Value) -> Result<()> {
    let mode = state["mode"].as_str().unwrap();
    let center = int(&state[mode], "center_hz", 70_000_000, 6_000_000_000)?;
    let width = int(&state[mode], "width_hz", 120_000, 48_000_000)?;
    ensure!(
        center - width / 2 >= 70_000_000 && center + width.div_ceil(2) <= 6_000_000_000,
        "selected RF interval not covered"
    );
    state["rx_gain_db"] = json!(apply_gain(&state["analysis"])?);
    let lo = set_attr("out_altvoltage0_RX_LO_frequency", center)?.parse::<u64>()?;
    ensure!(center.abs_diff(lo) <= 10, "RX LO readback");
    state["rx_lo_hz"] = json!(lo);
    state["if_hz"] = json!(center);
    state["ddc_hz"] = if mode == "video" {
        json!(center as i64 - lo as i64)
    } else {
        Value::Null
    };
    if mode == "analysis" {
        state["spectrum"] = json!({"rf_ranges":[[center-width/2,center+width.div_ceil(2)]],
            "positions":[{"rx_lo_hz":lo,"rf_center_hz":lo,"side_sign":1,"mixer":false}]});
    }
    Ok(())
}

fn apply_gain(settings: &Value) -> Result<f64> {
    let agc = settings["agc"]
        .as_str()
        .ok_or_else(|| anyhow!("AGC snapshot required"))?;
    ensure!(
        set_attr("in_voltage0_gain_control_mode", agc)? == agc,
        "AGC readback"
    );
    let read_gain = |s: &str| {
        s.split_whitespace()
            .next()
            .and_then(|v| v.parse::<f64>().ok())
            .ok_or_else(|| anyhow!("RX gain readback"))
    };
    if agc == "manual" {
        let gain = settings["gain_db"]
            .as_f64()
            .ok_or_else(|| anyhow!("manual gain"))?;
        ensure!(
            read_gain(&set_attr("in_voltage0_hardwaregain", gain)?)? == gain,
            "gain readback"
        );
    }
    read_gain(&attr("in_voltage0_hardwaregain")?)
}

fn continuous(
    c: &Config,
    shared: &Shared,
    state: &Value,
    id: &Value,
    regs: &Registers,
) -> Result<()> {
    ensure!(
        regs.read(0x64) == 16 && regs.read(0x1e0) == 0 && regs.read(0x284) == 0,
        "video needs STOP/DRAINED and released DDC"
    );
    let generation = state["generation"].as_u64().unwrap();
    let saved = [regs.read(0x30), regs.read(0x38), regs.read(0x40)];
    let nco = state["ddc_hz"].as_i64().unwrap();
    let phase = ((nco * (1i64 << 28)) / FS as i64) as u32 & 0x0fff_ffff;
    regs.write(0x30, phase);
    regs.write(0x38, 1);
    regs.write(0x40, 24);
    let spawned = Command::new("iio_readdev")
        .args(["-b", "8192", "cf-ad9361-lpc", "voltage0", "voltage1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let result = match spawned {
        Ok(mut child) => {
            let result = (|| {
                let mut stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| anyhow!("missing IIO stdout"))?;
                let fd = stdout.as_raw_fd();
                ensure!(
                    unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } == 0,
                    "nonblocking IIO pipe"
                );
                let start = Instant::now();
                while !capture_enabled()? {
                    ensure!(
                        start.elapsed() < Duration::from_secs(2)
                            && !shared.switch.load(Ordering::Acquire),
                        "IIO buffer enable timeout"
                    );
                    std::thread::sleep(Duration::from_millis(3));
                }
                regs.write(0x284, 1);
                ensure!(regs.read(0x284) == 1, "stream_run readback");
                let mut conn = TcpStream::connect_timeout(&c.video, Duration::from_millis(250))?;
                conn.set_write_timeout(Some(Duration::from_millis(150)))?;
                let (mut pos, mut seq) = (0u64, 0u32);
                let mut bytes = [0u8; 65536];
                let mut have = 0;
                while !shared.switch.load(Ordering::Acquire) {
                    match stdout.read(&mut bytes[have..]) {
                        Ok(0) => bail!("IIO real8 ended"),
                        Ok(n) => have += n,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if seq == 0 {
                                ensure!(
                                    start.elapsed() < Duration::from_secs(3),
                                    "first real8 timeout"
                                );
                            }
                            std::thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    }
                    if have < 16384 {
                        continue;
                    }
                    let len = have & !3;
                    let meta =
                        serde_json::to_vec(&json!({"generation":generation,"pos":pos,"seq":seq,
                        "code":24,"g":1,"reset":seq==0}))?;
                    if seq == 0 {
                        receipt(shared, state, id, None);
                    }
                    conn.write_all(b"R8V1")?;
                    conn.write_all(&(meta.len() as u32).to_le_bytes())?;
                    conn.write_all(&(len as u32).to_le_bytes())?;
                    conn.write_all(&meta)?;
                    conn.write_all(&bytes[..len])?;
                    bytes.copy_within(len..have, 0);
                    have -= len;
                    pos += len as u64;
                    seq = seq.wrapping_add(1);
                    if seq % 32 == 0 {
                        shared.inbox.lock().unwrap().metrics =
                            json!({"video_bytes":pos,"video_chunks":seq});
                    }
                }
                Ok(())
            })();
            regs.write(0x284, 0);
            let _ = child.kill();
            let _ = child.wait();
            result
        }
        Err(e) => Err(e.into()),
    };
    regs.write(0x38, 0);
    regs.write(0x30, saved[0]);
    regs.write(0x40, saved[2]);
    regs.write(0x38, saved[1]);
    result
}
fn capture_enabled() -> Result<bool> {
    for entry in fs::read_dir("/sys/bus/iio/devices")? {
        let dev = entry?.path();
        if text_trim(dev.join("name")).as_deref().ok() != Some("cf-ad9361-lpc") {
            continue;
        }
        for entry in fs::read_dir(dev)? {
            let e = entry?;
            if e.file_name().to_string_lossy().starts_with("buffer")
                && text_trim(e.path().join("enable")).as_deref().ok() == Some("1")
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn worker(
    c: Config,
    shared: Arc<Shared>,
    template: Vec<u8>,
    guard_fd: File,
    regs: Registers,
) -> Result<()> {
    let mut state = shared.inbox.lock().unwrap().actual.clone();
    while !shared.shutdown.load(Ordering::Acquire) {
        let cmd = {
            let mut q = shared.inbox.lock().unwrap();
            while q.next.is_none() && !shared.shutdown.load(Ordering::Acquire) {
                q = shared.wake.wait(q).unwrap();
            }
            let Some(cmd) = q.next.take() else { break };
            q.pending = Some(cmd.clone());
            shared.switch.store(false, Ordering::Release);
            cmd
        };
        let id = &cmd["id"];
        let before = state.clone();
        let op = cmd["op"].as_str().unwrap();
        let mut pending = state.clone();
        let previous_mode = pending["mode"].clone();
        let mut request = cmd.clone();
        request.as_object_mut().unwrap().remove("id");
        let generation = clock_ns() / 1000;
        pending["generation"] = json!(generation);
        pending["spectrum"] = Value::Null;
        pending["epoch"] = json!(0);
        pending["rx_lo_hz"] = Value::Null;
        pending["if_hz"] = Value::Null;
        pending["ddc_hz"] = Value::Null;
        pending["ddc_fs_hz"] = Value::Null;
        pending["waiting"] = Value::Null;
        let config_result = (|| {
            ensure!(
                previous_mode != "video" || op == "return" || op == "tx",
                "close video before changing mode"
            );
            match op {
                "scan" | "analysis" => {
                    pending["mode"] = json!(op);
                    pending[op] = request;
                }
                "video" => {
                    pending["return_mode"] = previous_mode.clone();
                    pending["mode"] = json!("video");
                    pending["video"] = request;
                }
                "return" => {
                    ensure!(previous_mode == "video", "no active video");
                    pending["mode"] = pending["return_mode"].take();
                    pending["video"] = Value::Null;
                }
                "route" => {
                    pending["route"] = json!({"kind":"direct","antenna":request["antenna"],"multiplier":1,"side":1});
                }
                "tx" => {
                    ensure!(
                        set_attr("out_altvoltage1_TX_LO_powerdown", 1)? == "1",
                        "TX LO powerdown readback"
                    );
                    let gain = set_attr("out_voltage0_hardwaregain", "-89.75")?;
                    ensure!(
                        gain.split_whitespace()
                            .next()
                            .and_then(|v| v.parse::<f64>().ok())
                            == Some(-89.75),
                        "TX attenuation readback"
                    );
                    pending["tx"] = json!({"on":false,"powerdown":1,"frequency_hz":attr("out_altvoltage1_TX_LO_frequency")?.parse::<u64>()?,"attenuation_mdb":89750});
                    if previous_mode == "video" {
                        pending["mode"] = pending["return_mode"].take();
                        pending["video"] = Value::Null;
                    }
                }
                _ => unreachable!(),
            };
            if pending["mode"] == "scan" {
                pending["map_version"] = json!((generation % 0x7fff_fffd) + 1);
            }
            let mode = pending["mode"].as_str().unwrap().to_owned();
            pending["k"] = pending[&mode]["k"].clone();
            pending["running"] = json!(mode != "scan" || pending["scan"]["run"] == true);
            if mode == "video" {
                pending["ddc_fs_hz"] = json!(FS / 4);
            }
            if mode == "scan" {
                ensure!(
                    !matches!(op, "route") || pending["running"] == true,
                    "start scan before switching antenna"
                );
                let (bytes, spectrum) = plan(&c, &template, &pending, op == "route")?;
                pending["rx_gain_db"] = json!(apply_gain(&pending["scan_rx"])?);
                if pending["running"] == true {
                    pending["spectrum"] = spectrum;
                }
                Ok::<_, anyhow::Error>(Some(bytes))
            } else {
                ensure!(
                    op != "route",
                    "antenna switch requires active scanner marker"
                );
                tune(&mut pending)?;
                Ok(None)
            }
        })();
        let mut guard_taken = false;
        let actual = RefCell::new(pending);
        let result = (|| {
            let mut setup = config_result?;
            let mode = actual.borrow()["mode"].as_str().unwrap().to_owned();
            if mode == "video" {
                return continuous(&c, &shared, &actual.borrow(), id, &regs);
            }
            if actual.borrow()["running"] != true {
                receipt(&shared, &actual.borrow(), id, None);
                return Ok(());
            }
            if let Some(setup) = setup.as_mut() {
                guard(&guard_fd, 0x4601, setup.as_mut_ptr().cast())?;
                guard_taken = true;
                let s = guard_cmd(&guard_fd, 0x4602)?;
                ensure!(s[1] == 2 && s[9] == 3 && s[112] == 1, "guard take readback");
            }
            let scan = mode == "scan";
            let work = c.output.join("reader");
            let k = actual.borrow()["k"].clone();
            let cfg = json!({"boot_id":c.boot_id,"output":work,"command":"rate","k":k,"mode":1,"previous_mode":1,
                "seconds":1.0,"warmup":0.0,"shift544":true,"settling":0,"batch_limit":64,"direct":true,"batch_sync":true,
                "neon":true,"hot":scan,"scan":scan,"udp":c.view.to_string(),"trace_limit":0,"send_after_release":true});
            let started = |h: crate::system_reader::Header| {
                let mut a = actual.borrow_mut();
                let version = a["map_version"].clone();
                let epoch = if scan {
                    u64::from(regs.read(0x238)) | (u64::from(regs.read(0x23c)) << 32)
                } else {
                    h.epoch
                };
                a["epoch"] = json!(epoch);
                let s = a["spectrum"].as_object_mut().unwrap();
                for (k, v) in [
                    ("generation", json!(generation)),
                    ("map_epoch", json!(epoch)),
                    ("version", version),
                    ("k", k.clone()),
                    ("scan", json!(scan)),
                    ("fs_hz", json!(FS)),
                    ("flags", json!(h.flags)),
                    ("TX_OFF", json!(true)),
                ] {
                    s.insert(k.into(), v);
                }
                receipt(&shared, &a, id, None);
            };
            let route = RefCell::new(None);
            let route_fn = |id, epoch, version, position, antenna| {
                acknowledge(
                    &mut route.borrow_mut(),
                    &c,
                    &shared,
                    generation,
                    id,
                    epoch,
                    version,
                    position,
                    antenna,
                )
            };
            let q = crate::system_reader::RunControl {
                generation,
                stop: &shared.switch,
                started: &started,
                route: &route_fn,
            };
            crate::system_reader::run_controlled(&serde_json::to_vec(&cfg)?, &q)
                .map_err(|e| anyhow!(e))
        })();
        let result = if guard_taken {
            let handback = guard_cmd(&guard_fd, 0x4604).and_then(|s| {
                ensure!(
                    s[1] == 3 && s[9] == 0 && s[112] == 1,
                    "guard return credential"
                );
                Ok(())
            });
            match handback {
                Ok(()) => result,
                Err(e) => {
                    let mut stopped = before;
                    stopped["running"] = json!(false);
                    stopped["spectrum"] = Value::Null;
                    stopped["generation"] = json!(generation);
                    receipt(&shared, &stopped, id, Some(e.to_string()));
                    return Err(e);
                }
            }
        } else {
            result
        };
        shared.inbox.lock().unwrap().route_pending = Value::Null;
        match result {
            Ok(()) => {
                state = actual.into_inner();
                if shared.inbox.lock().unwrap().pending.is_some() {
                    receipt(
                        &shared,
                        &state,
                        id,
                        Some("transition stopped before first valid observation".into()),
                    );
                }
            }
            Err(e) => {
                state = before;
                state["running"] = json!(false);
                state["spectrum"] = Value::Null;
                state["generation"] = json!(generation);
                receipt(&shared, &state, id, Some(e.to_string()));
            }
        }
    }
    Ok(())
}

/// Run the owner only on the prepared row99 RAM kit.
pub async fn run(path: &Path, listen: SocketAddr) -> Result<()> {
    let c: Config = serde_json::from_slice(&fs::read(path)?)?;
    validate(&c.scan)?;
    validate(&c.analysis)?;
    ensure!(
        c.scan["op"] == "scan" && c.analysis["op"] == "analysis",
        "initial profile modes"
    );
    ensure!(
        text_trim("/proc/sys/kernel/random/boot_id")? == c.boot_id
            && text_trim("/tmp/pl-uart-obs-session/phase")? == "OBS_READY",
        "prepared boot lease required"
    );
    ensure!(
        attr("in_voltage_sampling_frequency")? == FS.to_string(),
        "expected 61.44 MHz ADC"
    );
    ensure!(
        attr("out_altvoltage1_TX_LO_powerdown")? == "1",
        "TX must be off before startup"
    );
    let template = fs::read(&c.setup)?;
    ensure!(
        template.len() == 12488
            && word(&template, 0) == 0x3747534f
            && word(&template, 12328) == 0
            && word(&template, 12332) == 0,
        "RX-only guard template required"
    );
    ensure!(
        c.positions.len() == word(&template, 8) as usize
            && (1..=128).contains(&c.positions.len())
            && c.positions.windows(2).all(|p| p[0] < p[1]),
        "calibrated position registry"
    );
    for i in 0..c.positions.len() {
        ensure!(
            word(&template, MAP + 32 * i + 4) == 0 && word(&template, MAP + 32 * i + 12) == 0,
            "template cannot assign CAP2 or TX jobs"
        );
    }
    let regs = Registers::open()?;
    ensure!(
        regs.read(0x50) == 0x324e424f
            && regs.read(0x224) == 0x31444c46
            && Path::new("/dev/obs_cap2").exists(),
        "row99 CAP2/OBN2/FIELD1 devices required"
    );
    ensure!(
        regs.read(0x64) == 16
            && regs.read(0x1e0) == 0
            && regs.read(0x284) == 0
            && !capture_enabled()?,
        "exclusive ownership prerequisites"
    );
    let guard_fd = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/obs_fl_guard")?;
    ensure!(guard_cmd(&guard_fd, 0x4605)?[0] == 2, "guard ABI 2");
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open("/run/obs-scan-control.lock")?;
    ensure!(
        unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "control owner busy"
    );
    fs::create_dir_all(&c.output)?;
    let tx = attr("out_altvoltage1_TX_LO_frequency")?.parse::<u64>()?;
    let scan_agc = attr("in_voltage0_gain_control_mode")?;
    let scan_gain = attr("in_voltage0_hardwaregain")?
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("scan gain"))?
        .parse::<f64>()?;
    let initial = json!({"mode":"scan","generation":clock_ns()/1000,"map_version":1,"epoch":0,"boot":c.boot_id,
        "scan":c.scan,"analysis":c.analysis,"video":null,"return_mode":null,"running":false,"k":c.scan["k"],
        "fs_hz":FS,"ddc_fs_hz":null,"spectrum":null,"rx_lo_hz":null,"if_hz":null,"ddc_hz":null,
        "scan_rx":{"agc":scan_agc,"gain_db":scan_gain},"rx_gain_db":scan_gain,
        "route":{"kind":"direct","antenna":0,"multiplier":1,"side":1},"tx":{"on":false,"powerdown":1,"frequency_hz":tx,"attenuation_mdb":89750}});
    let listener = tokio::net::TcpListener::bind(listen).await?;
    unsafe {
        libc::signal(
            libc::SIGINT,
            shutdown_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            shutdown_signal as *const () as libc::sighandler_t,
        );
    }
    let shared = Arc::new(Shared {
        inbox: Mutex::new(Inbox {
            actual: initial,
            ..Inbox::default()
        }),
        wake: Condvar::new(),
        switch: AtomicBool::new(false),
        shutdown: AtomicBool::new(false),
    });
    let acquisition = std::thread::spawn({
        let shared = shared.clone();
        move || {
            let _lease = lease;
            let result = worker(c, shared, template, guard_fd, regs);
            STOP_RUNTIME.store(true, Ordering::Release);
            result
        }
    });
    let app = Router::new()
        .route("/api/system/control", get(status).post(submit))
        .route(
            "/api/system/field",
            get(crate::field1_control::get).post(crate::field1_control::post),
        )
        .with_state(shared.clone());
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            while !STOP_RUNTIME.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
    shared.shutdown.store(true, Ordering::Release);
    shared.switch.store(true, Ordering::Release);
    shared.wake.notify_one();
    acquisition
        .join()
        .map_err(|_| anyhow!("control worker panic"))??;
    served?;
    Ok(())
}

// Owner moves the register mapping to its sole acquisition worker.
unsafe impl Send for Registers {}
