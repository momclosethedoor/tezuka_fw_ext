//! Interactive owner for the row99 guarded RAM runtime.
#![allow(missing_docs)]
mod field;
mod plan_math;
mod tx;

use anyhow::{Result, anyhow, bail, ensure};
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::BTreeMap,
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
    #[serde(default)]
    external_mixer: Option<Mixer>,
    #[serde(default)]
    scan_tx: Vec<ScanTxProfile>,
    #[serde(default)]
    antenna_bands: BTreeMap<u64, Vec<[u64; 2]>>,
    #[serde(default = "filter_limit")]
    rf_filter_max_hz: u64,
    #[serde(default)]
    field_preview: Option<field::Config>,
}

fn filter_limit() -> u64 {
    plan_math::DEFAULT_FILTER_LIMIT
}
fn scan_limit(c: &Config) -> u64 {
    let band_max = c.antenna_bands.values().flatten().map(|band| band[1]).max();
    let fallback = if c.scan_tx.is_empty() {
        plan_math::DIRECT_LIMIT
    } else {
        c.rf_filter_max_hz
    };
    band_max.unwrap_or(fallback).min(c.rf_filter_max_hz)
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScanTxProfile {
    pll_hz: u64,
    multiplier: u64,
    antenna: u64,
}
impl ScanTxProfile {
    fn tone(&self) -> u64 {
        self.pll_hz + FS / 64
    }
    fn offset(&self) -> i64 {
        (self.multiplier * self.tone()) as i64
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Mixer {
    tone_hz: u64,
    attenuation_mdb: u64,
    multiplier: u64,
    side: i64,
    antenna: u64,
}
impl Mixer {
    fn confirm_tx(&self) -> Result<Value> {
        ensure!(
            (70_000_000..=6_000_000_000).contains(&self.tone_hz)
                && self.attenuation_mdb <= 89750
                && (1..=2).contains(&self.multiplier)
                && matches!(self.side, -1 | 1)
                && self.antenna <= 7,
            "invalid mixer fixture"
        );
        let pll = attr("out_altvoltage1_TX_LO_frequency")?.parse::<u64>()?;
        ensure!(
            (pll + FS / 64).abs_diff(self.tone_hz) <= 10,
            "mixer carrier readback"
        );
        ensure!(
            attr("out_altvoltage1_TX_LO_powerdown")? == "0",
            "mixer LO off"
        );
        let attenuation = attr("out_voltage0_hardwaregain")?
            .split_whitespace()
            .next()
            .ok_or_else(|| anyhow!("TX attenuation"))?
            .parse::<f64>()?;
        ensure!(
            (attenuation * 1000.0).round() == -(self.attenuation_mdb as f64),
            "mixer attenuation readback"
        );
        Ok(
            json!({"on":true,"powerdown":0,"frequency_hz":self.tone_hz,"pll_hz":pll,"attenuation_mdb":self.attenuation_mdb}),
        )
    }
    fn route(&self) -> Value {
        json!({"kind":"mixer","antenna":self.antenna,"multiplier":self.multiplier,"side":self.side,
               "owner":"operator + leased IIO adapter"})
    }
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
    queued_ns: u64,
    received_ns: u64,
    command_timing: Value,
    capabilities: Value,
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
                let lo = int(r, "lo_hz", 70_000_000, plan_math::MAX_RF)?;
                int(r, "hi_hz", lo + 120_000, plan_math::MAX_RF)?;
                enabled |= r["enabled"]
                    .as_bool()
                    .ok_or_else(|| anyhow!("enabled must be boolean"))?;
                if !r["tx_slot"].is_null() && r["tx_slot"] != "auto" {
                    int(r, "tx_slot", 0, 7)?;
                }
            }
            ensure!(!run || enabled, "no enabled range");
        }
        Some("analysis") | Some("video") => {
            int(v, "center_hz", 70_000_000, 18_000_000_000)?;
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
                matches!(v["kind"].as_str(), Some("direct" | "mixer")),
                "route kind"
            );
            int(v, "antenna", 0, 7)?;
            if v["kind"] == "mixer" {
                int(v, "multiplier", 1, 2)?;
                ensure!(matches!(v["side"].as_i64(), Some(-1 | 1)), "route side");
            }
        }
        Some("tx") => {
            int(v, "frequency_hz", 70_000_000, 6_000_000_000)?;
            int(v, "attenuation_mdb", 0, 89750)?;
            ensure!(v["on"].is_boolean(), "TX on must be boolean");
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
        "command_timing":q.command_timing,"capabilities":q.capabilities,"metrics":{"owner":q.metrics,"reader":reader}}),
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
    q.queued_ns = clock_ns();
    shared.switch.store(true, Ordering::Release);
    shared.wake.notify_one();
    (StatusCode::ACCEPTED, Json(json!({"accepted_id":id})))
}

fn receipt(shared: &Shared, state: &Value, id: &Value, error: Option<String>) {
    let mut q = shared.inbox.lock().unwrap();
    q.pending = None;
    q.actual = state.clone();
    q.receipt_ns = clock_ns();
    if !id.is_null() {
        q.command_timing = json!({"id":id,"received_ns":q.received_ns.to_string(),
            "completed_ns":q.receipt_ns.to_string()});
    }
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
fn put_hz(b: &mut [u8], offset: usize, n: u64) {
    b[offset..offset + 8].copy_from_slice(&n.to_le_bytes());
}

#[derive(Clone, Copy)]
struct Visit {
    index: usize,
    slot: Option<usize>,
    antenna: u64,
    side: i64,
    rf: [u64; 2],
    transition_only: bool,
    marker_route: Option<u64>,
    boundary: bool,
}

fn plan(c: &Config, template: &[u8], state: &Value, marker: bool) -> Result<(Vec<u8>, Value)> {
    use plan_math::{Candidate, Segment};
    let direct_antenna = state["route"]["antenna"].as_u64().unwrap_or(0);
    let bands = |antenna| c.antenna_bands.get(&antenna).map_or(&[][..], Vec::as_slice);
    let direct_antenna_ids = if c.antenna_bands.is_empty() {
        vec![direct_antenna]
    } else {
        c.antenna_bands.keys().copied().collect::<Vec<_>>()
    };
    let direct = direct_antenna_ids
        .iter()
        .map(|&antenna| Candidate {
            offset: 0,
            slot: None,
            antenna,
            antenna_bands: bands(antenna),
        })
        .collect::<Vec<_>>();
    let mixer = c
        .scan_tx
        .iter()
        .enumerate()
        .filter_map(|(slot, p)| {
            if !c.antenna_bands.is_empty() && !c.antenna_bands.contains_key(&p.antenna) {
                return None;
            }
            Some(Candidate {
                offset: p.offset(),
                slot: Some(slot),
                antenna: p.antenna,
                antenna_bands: bands(p.antenna),
            })
        })
        .collect::<Vec<_>>();
    let candidate_for = |segment: &Segment| {
        let group = if segment.slot.is_some() {
            &mixer
        } else {
            &direct
        };
        group
            .iter()
            .find(|p| p.slot == segment.slot && p.antenna == segment.antenna)
            .unwrap()
    };
    let mut sections = Vec::new();
    let mut intervals = Vec::new();
    let mut previous = Some((None, direct_antenna));
    let max_rf = scan_limit(c);
    for row in state["scan"]["ranges"]
        .as_array()
        .ok_or_else(|| anyhow!("scan ranges"))?
    {
        if row["enabled"] != true {
            continue;
        }
        let lo = int(row, "lo_hz", 70_000_000, max_rf)?;
        let hi = int(row, "hi_hz", lo + 120_000, max_rf)?;
        let mut list = Vec::new();
        if row["tx_slot"] == "auto" {
            list = plan_math::automatic(&c.positions, &mixer, previous, lo, hi)
                .ok_or_else(|| anyhow!("RF {lo}..{hi} Hz: no complete mixer coverage"))?;
        } else if let Some(slot) = row["tx_slot"].as_u64() {
            let profile = mixer
                .iter()
                .find(|p| p.slot == Some(slot as usize))
                .ok_or_else(|| {
                    anyhow!("TX slot {slot} is unavailable for the configured antennas")
                })?;
            list.push(
                plan_math::cover(&c.positions, profile, lo, hi).ok_or_else(|| {
                    anyhow!("RF {lo}..{hi} Hz: no complete coverage with TX slot {slot}")
                })?,
            );
        } else {
            let direct_hi = hi.min(plan_math::DIRECT_LIMIT);
            if lo < direct_hi {
                list = plan_math::automatic(&c.positions, &direct, previous, lo, direct_hi)
                    .ok_or_else(|| {
                        anyhow!("RF {lo}..{direct_hi} Hz: no complete calibrated direct coverage")
                    })?;
            }
            if hi > plan_math::DIRECT_LIMIT {
                let mixer_lo = lo.max(plan_math::DIRECT_LIMIT);
                let next = plan_math::automatic(
                    &c.positions,
                    &mixer,
                    list.last().map(|s| (s.slot, s.antenna)).or(previous),
                    mixer_lo,
                    hi,
                )
                .ok_or_else(|| {
                    anyhow!("RF {mixer_lo}..{hi} Hz: no complete calibrated mixer coverage")
                })?;
                list.extend(next);
            }
        }
        intervals.push([lo, hi]);
        previous = list.last().map(|s| (s.slot, s.antenna)).or(previous);
        sections.extend(list.into_iter().map(|s| ([lo, hi], s)));
    }
    ensure!(!sections.is_empty(), "no calibrated scan position selected");
    let mut visits = Vec::new();
    let mut planned = Vec::new();
    let mut overlaps = Vec::new();
    for (i, (range, section)) in sections.iter().enumerate() {
        let ([prev_lo, prev_hi], prior) = &sections[(i + sections.len() - 1) % sections.len()];
        let candidate = candidate_for(section);
        let needs_route = prior.antenna != section.antenna
            || i == 0 && (marker || section.antenna != direct_antenna);
        let before = visits.len();
        if needs_route {
            let handover = if i > 0 && prior.hi == section.lo {
                plan_math::handover(
                    &c.positions,
                    candidate_for(prior),
                    candidate,
                    [range[0].min(*prev_lo), range[1].max(*prev_hi)],
                    section.lo,
                )
            } else {
                None
            };
            if let Some((rf, old_idx, old_side, new_idx, new_side)) = handover {
                visits.push(Visit {
                    index: old_idx,
                    slot: prior.slot,
                    antenna: prior.antenna,
                    side: old_side,
                    rf,
                    transition_only: true,
                    marker_route: Some(section.antenna),
                    boundary: false,
                });
                visits.push(Visit {
                    index: new_idx,
                    slot: section.slot,
                    antenna: section.antenna,
                    side: new_side,
                    rf,
                    transition_only: true,
                    marker_route: None,
                    boundary: true,
                });
                overlaps.push(json!({"from":prior.antenna,"to":section.antenna,"rf_hz":rf,
                    "notify_position":before,"wait_position":before+1,
                    "old_if_hz":c.positions[old_idx],"new_if_hz":c.positions[new_idx]}));
            }
        }
        let needs_immediate_hold = needs_route && visits.len() == before;
        let rf = [section.lo, section.hi];
        for (pos, &index) in section.indexes.iter().enumerate() {
            let boundary = needs_immediate_hold && pos == 0;
            visits.push(Visit {
                index,
                slot: section.slot,
                antenna: section.antenna,
                side: section.side,
                rf,
                transition_only: false,
                marker_route: boundary.then_some(section.antenna),
                boundary,
            });
        }
        let image = section.slot.map(|_| {
            [
                2 * candidate.offset - section.hi as i64,
                2 * candidate.offset - section.lo as i64,
            ]
        });
        let image_in_band = section
            .slot
            .and_then(|_| plan_math::image_overlap(candidate, section.lo, section.hi));
        planned.push(json!({"rf_hz":rf,"tx_slot":section.slot,"side_sign":section.side,
            "antenna":section.antenna,"mixer_lo_hz":section.slot.map(|_|candidate.offset),
            "image_rf_hz":image,"image_overlap_hz":image_in_band,"rx_positions":section.indexes.len(),
            "transition":if !needs_route {"none"} else if needs_immediate_hold {"hold_at_entry"} else {"overlap"}}));
    }
    ensure!(
        (1..=128).contains(&visits.len()),
        "compiled primary plus overlap map exceeds 128 positions"
    );
    let version = int(state, "map_version", 1, 0x7fff_fffe)? as u32;
    let mut bytes = template.to_vec();
    put(&mut bytes, 8, visits.len() as u32);
    put(&mut bytes, 12, visits.len() as u32);
    put(&mut bytes, 16, version);
    put(&mut bytes, 20, version + 1);
    bytes[MAP..NEXT + 4096].fill(0);
    let mut locations = Vec::with_capacity(visits.len());
    for (i, v) in visits.iter().enumerate() {
        let intermediate = c.positions[v.index];
        let profile = v.slot.map(|slot| &c.scan_tx[slot]);
        let offset = profile.map_or(0, ScanTxProfile::offset);
        let rf = offset + v.side * intermediate as i64;
        locations.push(json!({"rx_lo_hz":intermediate,"rf_center_hz":rf,"side_sign":v.side,
            "mixer":v.slot.is_some(),"antenna":v.antenna,"rf_ranges":[v.rf],
            "tx_slot":v.slot,"tx_pll_hz":profile.map(|p|p.pll_hz),"tx_tone_hz":profile.map(ScanTxProfile::tone),
            "image_rf_center_hz":v.slot.map(|_|2*offset-rf),"rx_available":v.slot.is_none()&&!v.transition_only,
            "transition_only":v.transition_only,"marker_route":v.marker_route,"hold_boundary":v.boundary}));
        let flags = v.slot.map_or(0, |slot| 4 | ((slot as u32) << 8))
            | v.marker_route.map_or(0, |ant| 1 | ((ant as u32) << 16))
            | if v.boundary { 2 } else { 0 };
        for page in [MAP, NEXT] {
            let entry = page + 32 * i;
            let previous = MAP + 32 * v.index;
            bytes[entry..entry + 32].copy_from_slice(&template[previous..previous + 32]);
            put(&mut bytes, entry + 4, flags);
            put(
                &mut bytes,
                entry + 8,
                if v.marker_route.is_some() {
                    version.wrapping_add(i as u32)
                } else {
                    0
                },
            );
            put_hz(&mut bytes, entry + 16, v.rf[0]);
            put_hz(&mut bytes, entry + 24, v.rf[1]);
        }
    }
    let first = visits.iter().find_map(|v| v.slot);
    put(&mut bytes, 12328, first.is_some() as u32);
    // The accepted guard can exercise fastlock without authorizing RF emission.
    put(&mut bytes, 12332, 0);
    if let Some(first) = first {
        put(&mut bytes, 12340, first as u32);
    }
    Ok((
        bytes,
        json!({"rf_ranges":intervals,"positions":locations,"planned_ranges":planned,
        "overlap_profiles":overlaps}),
    ))
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
    source: &str,
) -> std::result::Result<Option<u32>, String> {
    let key = (id, epoch, version, position, antenna);
    if route.as_ref().is_none_or(|r| r.key != key) {
        let addr = c.route;
        let stream = TcpStream::connect_timeout(&addr, Duration::from_millis(50))
            .map_err(|e| e.to_string())?;
        stream.set_nonblocking(true).map_err(|e| e.to_string())?;
        let v = json!({"marker_id":id,"epoch":epoch,"version":version,"position":position,"route":antenna,"generation":generation,"source":source});
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
    let video = state["mode"] == "video";
    let settings = &state[if video { "video" } else { "analysis" }];
    let center = int(settings, "center_hz", 70_000_000, 18_000_000_000)?;
    let width = int(settings, "width_hz", 120_000, 48_000_000)?;
    let mixer = state["route"]["kind"] == "mixer";
    let side = state["route"]["side"].as_i64().unwrap_or(1);
    let offset = if mixer {
        (state["route"]["multiplier"].as_u64().unwrap()
            * state["tx"]["frequency_hz"].as_u64().unwrap()) as i64
    } else {
        0
    };
    let to_if = |rf: u64| (rf as i64 - offset) * side;
    let intermediate = to_if(center);
    let a = to_if(center - width / 2);
    let b = to_if(center + width.div_ceil(2));
    ensure!(
        a.min(b) >= 70_000_000 && a.max(b) <= 6_000_000_000,
        "selected RF interval not covered"
    );
    state["rx_gain_db"] = json!(apply_gain(&state["analysis"])?);
    let lo = set_attr("out_altvoltage0_RX_LO_frequency", intermediate)?.parse::<u64>()?;
    ensure!(intermediate.abs_diff(lo as i64) <= 10, "RX LO readback");
    state["rx_lo_hz"] = json!(lo);
    state["if_hz"] = json!(intermediate);
    state["ddc_hz"] = if video {
        json!(intermediate - lo as i64)
    } else {
        Value::Null
    };
    if !video {
        state["spectrum"] = json!({"rf_ranges":[[center-width/2,center+width.div_ceil(2)]],
            "positions":[{"rx_lo_hz":lo,"rf_center_hz":offset+side*lo as i64,"side_sign":side,"mixer":mixer}]});
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
    verify_tx: &mut dyn FnMut() -> Result<()>,
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
                let mut lo_check = Instant::now();
                while !shared.switch.load(Ordering::Acquire) {
                    if lo_check.elapsed() >= Duration::from_millis(200) {
                        verify_tx()?;
                        lo_check = Instant::now();
                    }
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
    let manual = state["manual_tx_available"] == true;
    let mut carrier = tx::Carrier::default();
    while !shared.shutdown.load(Ordering::Acquire) {
        let cmd = {
            let mut q = shared.inbox.lock().unwrap();
            if q.next.is_none() && !shared.shutdown.load(Ordering::Acquire) {
                q = shared
                    .wake
                    .wait_timeout(q, Duration::from_millis(200))
                    .unwrap()
                    .0;
            }
            q.next.take().map(|cmd| {
                q.received_ns = q.queued_ns;
                q.pending = Some(cmd.clone());
                shared.switch.store(false, Ordering::Release);
                cmd
            })
        };
        let Some(cmd) = cmd else {
            if carrier.on() {
                if let Err(e) = carrier.verify() {
                    state["tx"] = carrier.off()?;
                    state["waiting"] = json!(e.to_string());
                    receipt(&shared, &state, &Value::Null, Some(e.to_string()));
                }
            }
            continue;
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
        pending["guard_tx"] = Value::Null;
        pending["field_watch"] = Value::Null;
        let config_result = (|| {
            ensure!(
                previous_mode != "video" || op == "return" || op == "tx",
                "close video before changing mode"
            );
            match op {
                "scan" | "analysis" => {
                    if op == "scan" && carrier.on() {
                        pending["tx"] = carrier.off()?;
                    }
                    if previous_mode != op {
                        if op == "scan" {
                            pending["analysis_route"] = pending["route"].clone();
                            pending["route"] = json!({"kind":"direct","antenna":pending["scan_antenna"],"multiplier":1,"side":1});
                        } else {
                            pending["route"] = pending["analysis_route"].clone();
                        }
                    }
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
                    ensure!(c.external_mixer.is_none(), "external fixture owns route");
                    let scan = pending["mode"] == "scan";
                    ensure!(
                        !scan || request["kind"] == "direct",
                        "select calibrated TX profiles in the scan map"
                    );
                    if !scan || pending["scan"]["run"] != true {
                        let mut socket = None;
                        let version = pending["map_version"].as_u64().unwrap() as u32;
                        let marker = generation as u32;
                        let antenna = request["antenna"].as_u64().unwrap() as u32;
                        loop {
                            if acknowledge(
                                &mut socket,
                                &c,
                                &shared,
                                generation,
                                marker,
                                0,
                                version,
                                0,
                                antenna,
                                "manual",
                            )
                            .map_err(|e| anyhow!(e))?
                            .is_some()
                            {
                                break;
                            }
                            ensure!(!shared.shutdown.load(Ordering::Acquire), "route cancelled");
                            std::thread::sleep(Duration::from_millis(2));
                        }
                    }
                    pending["route"] = json!({"kind":request["kind"],"antenna":request["antenna"],
                        "multiplier":if request["kind"]=="direct" {json!(1)} else {request["multiplier"].clone()},
                        "side":if request["kind"]=="direct" {json!(1)} else {request["side"].clone()}});
                    if scan {
                        pending["scan_antenna"] = request["antenna"].clone();
                    } else {
                        pending["analysis_route"] = pending["route"].clone();
                    }
                }
                "tx" => {
                    ensure!(
                        pending["mode"] != "scan",
                        "manual TX locked while scanner owns profiles"
                    );
                    ensure!(
                        manual && c.external_mixer.is_none(),
                        "TX owned by external adapter or unavailable"
                    );
                    pending["tx"] = carrier.apply(&request)?;
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
            if let Some(mixer) = &c.external_mixer {
                ensure!(
                    mode != "scan" && op != "route",
                    "mixer bench supports static analysis/video under PS ownership"
                );
                pending["tx"] = mixer.confirm_tx()?;
            }
            if mode != "scan" && pending["route"]["kind"] == "mixer" && pending["tx"]["on"] != true
            {
                ensure!(mode != "video", "mixer LO off");
                pending["running"] = json!(false);
                pending["waiting"] = json!("mixer LO off");
                return Ok(None);
            }
            if mode == "video" {
                pending["ddc_fs_hz"] = json!(FS / 4);
            }
            if mode == "scan" {
                let (bytes, spectrum) = plan(&c, &template, &pending, op == "route")?;
                pending["rx_gain_db"] = json!(apply_gain(&pending["scan_rx"])?);
                if pending["running"] == true {
                    pending["spectrum"] = spectrum;
                }
                if word(&bytes, 12328) == 1 {
                    pending["waiting"] = json!("TX profiles loaded, mixer RF off");
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
        let watch = RefCell::new(None);
        let result = (|| {
            let mut setup = config_result?;
            let mode = actual.borrow()["mode"].as_str().unwrap().to_owned();
            if actual.borrow()["running"] != true {
                receipt(&shared, &actual.borrow(), id, None);
                return Ok(());
            }
            if mode == "video" {
                return continuous(&c, &shared, &actual.borrow(), id, &regs, &mut || {
                    if carrier.on() {
                        carrier.verify()?;
                    }
                    if let Some(mixer) = &c.external_mixer {
                        mixer.confirm_tx()?;
                    }
                    Ok(())
                });
            }
            if let Some(setup) = setup.as_mut() {
                let expected_control = 3 | (word(setup, 12328) << 2);
                guard(&guard_fd, 0x4601, setup.as_mut_ptr().cast())?;
                guard_taken = true;
                let s = guard_cmd(&guard_fd, 0x4602)?;
                ensure!(
                    s[1] == 2 && s[9] == expected_control && s[112] == 1,
                    "guard take readback"
                );
                actual.borrow_mut()["guard_tx"] =
                    json!({"executor":s[9]&4!=0,"powerdown":s[112],"attenuation_mdb":s[113]});
            }
            let scan = mode == "scan";
            let tx_active = actual.borrow()["tx"]["on"] == true;
            let work = c.output.join("reader");
            let k = actual.borrow()["k"].clone();
            let cfg = json!({"boot_id":c.boot_id,"output":work,"command":"rate","k":k,"mode":1,"previous_mode":1,
                "seconds":1.0,"warmup":0.0,"shift544":true,"settling":0,"batch_limit":64,"direct":true,"batch_sync":true,
                "neon":true,"hot":scan,"scan":scan,"tx_active":tx_active,
                "udp":c.view.to_string(),"trace_limit":0,"send_after_release":true});
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
                    ("fft_bins", json!(4096)),
                    (
                        "view_bins",
                        json!([0, 512, 1024, 2048][((h.flags >> 3) & 3) as usize]),
                    ),
                    ("TX_OFF", json!(!tx_active)),
                ] {
                    s.insert(k.into(), v);
                }
                if scan && let Some(field) = &c.field_preview {
                    match field::Watch::start(field, &c.output, &c.boot_id, c.video, &a["spectrum"])
                    {
                        Ok(Some((child, identity))) => {
                            *watch.borrow_mut() = Some(child);
                            a["field_watch"] = identity;
                        }
                        Ok(None) => {
                            a["field_watch"] =
                                json!({"waiting":"target outside usable direct capture coverage"})
                        }
                        Err(e) => a["field_watch"] = json!({"error":e.to_string()}),
                    }
                }
                receipt(&shared, &a, id, None);
            };
            let route = RefCell::new(None);
            let route_fn = |id, epoch, version, position, antenna| {
                let anchor = u64::from(regs.read(0x238)) | (u64::from(regs.read(0x23c)) << 32);
                let count = actual.borrow()["spectrum"]["positions"]
                    .as_array()
                    .map_or(0, Vec::len);
                let marker_position = if count > 0 && epoch >= anchor {
                    ((epoch - anchor) % count as u64) as u32
                } else {
                    position
                };
                acknowledge(
                    &mut route.borrow_mut(),
                    &c,
                    &shared,
                    generation,
                    id,
                    epoch,
                    version,
                    marker_position,
                    antenna,
                    "PL",
                )
            };
            let q = crate::system_reader::RunControl {
                generation,
                stop: &shared.switch,
                started: &started,
                route: &route_fn,
                check: &|| {
                    if let Some(note) = watch
                        .borrow_mut()
                        .as_mut()
                        .and_then(field::Watch::exit_note)
                    {
                        shared.inbox.lock().unwrap().actual["field_watch"]["error"] = json!(note);
                    }
                    if tx_active && c.external_mixer.is_none() {
                        if !tx::lease_is_live()
                            || tx::readback(FS / 64).map_err(|e| e.to_string())?["on"] != true
                        {
                            return Err("manual TX lease or carrier lost".into());
                        }
                    }
                    if let Some(mixer) = &c.external_mixer {
                        mixer.confirm_tx().map_err(|e| e.to_string())?;
                    }
                    Ok(())
                },
            };
            crate::system_reader::run_controlled(&serde_json::to_vec(&cfg)?, &q)
                .map_err(|e| anyhow!(e))
        })();
        let field_stop = watch
            .into_inner()
            .map_or(Ok(()), |field| field.finish(&regs));
        let result = result.and(field_stop);
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
                if carrier.on() {
                    state["tx"] = carrier.off()?;
                    if state["route"]["kind"] == "mixer" {
                        state["waiting"] = json!("mixer LO off");
                    }
                }
                if c.external_mixer.is_some()
                    && attr("out_altvoltage1_TX_LO_powerdown").as_deref().ok() == Some("1")
                {
                    state["tx"]["on"] = json!(false);
                    state["tx"]["powerdown"] = json!(1);
                    state["waiting"] = json!("mixer LO off");
                }
                receipt(&shared, &state, id, Some(e.to_string()));
            }
        }
    }
    Ok(())
}

/// Run the owner only on the prepared row99 RAM kit.
pub async fn run(path: &Path, listen: SocketAddr) -> Result<()> {
    let mut c: Config = serde_json::from_slice(&fs::read(path)?)?;
    if let Some(preview) = &c.field_preview {
        preview.validate()?;
    }
    ensure!(
        (70_120_000..=plan_math::MAX_RF).contains(&c.rf_filter_max_hz),
        "RF filter limit"
    );
    for (&antenna, bands) in &mut c.antenna_bands {
        ensure!(
            antenna <= 7
                && !bands.is_empty()
                && bands.len() <= 16
                && bands
                    .iter()
                    .all(|[lo, hi]| *lo >= 70_000_000 && lo < hi && *hi <= plan_math::MAX_RF),
            "invalid calibrated RF bands for antenna {antenna}"
        );
        bands.sort_unstable();
    }
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
    if c.external_mixer.is_none() {
        ensure!(
            attr("out_altvoltage1_TX_LO_powerdown")? == "1",
            "TX must be off before startup"
        );
    }
    let template = fs::read(&c.setup)?;
    ensure!(
        template.len() == 12488 && word(&template, 0) == 0x3747534f && word(&template, 12332) == 0,
        "guard template must start RF off"
    );
    if !c.scan_tx.is_empty() {
        ensure!(
            c.scan_tx.len() == 8
                && c.scan_tx.iter().enumerate().all(|(slot, p)| {
                    (70_000_000..=6_000_000_000 - FS / 64).contains(&p.pll_hz)
                        && (1..=2).contains(&p.multiplier)
                        && p.antenna <= 7
                        && template[12352 + 16 * slot..12368 + 16 * slot]
                            .iter()
                            .any(|b| *b != 0)
                }),
            "eight calibrated TX fastlock slots and metadata required"
        );
    } else {
        ensure!(
            word(&template, 12328) == 0,
            "TX calibration metadata missing"
        );
    }
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
    let manual = c.external_mixer.is_none() && tx::available();
    let scan_agc = attr("in_voltage0_gain_control_mode")?;
    let scan_gain = attr("in_voltage0_hardwaregain")?
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("scan gain"))?
        .parse::<f64>()?;
    let initial_mode = if c.external_mixer.is_some() {
        "analysis"
    } else {
        "scan"
    };
    let tx_readback = match &c.external_mixer {
        Some(mixer) => mixer.confirm_tx()?,
        None => tx::readback(0)?,
    };
    let route = match &c.external_mixer {
        Some(mixer) => mixer.route(),
        None => json!({"kind":"direct","antenna":0,"multiplier":1,"side":1}),
    };
    let initial = json!({"mode":initial_mode,"generation":clock_ns()/1000,"map_version":1,"epoch":0,"boot":c.boot_id,
        "scan":c.scan,"analysis":c.analysis,"video":null,"return_mode":null,"running":false,"k":c.scan["k"],
        "fs_hz":FS,"ddc_fs_hz":null,"spectrum":null,"rx_lo_hz":null,"if_hz":null,"ddc_hz":null,
        "scan_rx":{"agc":scan_agc,"gain_db":scan_gain},"rx_gain_db":scan_gain,
        "route":route,"analysis_route":route,"scan_antenna":0,"tx":tx_readback,"manual_tx_available":manual});
    let profiles = c
        .scan_tx
        .iter()
        .enumerate()
        .map(|(slot, p)| {
            json!({"slot":slot,"pll_hz":p.pll_hz,"tone_hz":p.tone(),
        "multiplier":p.multiplier,"antenna":p.antenna})
        })
        .collect::<Vec<_>>();
    let capabilities = json!({"tx_on":manual,"mixer":c.external_mixer.is_none(),"finite_field":c.field_preview.is_some(),
        "scan_tx_profiles":profiles,"scan_tx_rf":false,"scan_max_rf_hz":scan_limit(&c),
        "scan_min_rf_hz":c.antenna_bands.values().flatten().map(|b|b[0]).min().unwrap_or(70_000_000),
        "rf_filter_max_hz":c.rf_filter_max_hz,"antenna_bands":c.antenna_bands});
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
            capabilities,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_reuses_one_slot_for_each_range_and_mirrors_the_if() {
        let p = ScanTxProfile {
            pll_hz: 2_250_000_000,
            multiplier: 2,
            antenna: 3,
        };
        let c = Config {
            boot_id: String::new(),
            setup: "setup".into(),
            output: "out".into(),
            view: "127.0.0.1:1".parse().unwrap(),
            video: "127.0.0.1:2".parse().unwrap(),
            route: "127.0.0.1:3".parse().unwrap(),
            positions: vec![
                1_339_080_000,
                1_351_080_000,
                1_363_080_000,
                1_375_080_000,
                1_387_080_000,
            ],
            scan: Value::Null,
            analysis: Value::Null,
            external_mixer: None,
            scan_tx: vec![p; 8],
            antenna_bands: BTreeMap::new(),
            rf_filter_max_hz: filter_limit(),
            field_preview: None,
        };
        let input = json!({"scan":{"ranges":[
            {"lo_hz":5_841_000_000u64,"hi_hz":5_889_000_000u64,"enabled":true,"tx_slot":5},
            {"lo_hz":3_114_840_000u64,"hi_hz":3_162_840_000u64,"enabled":true,"tx_slot":"auto"}]},
            "map_version":3,"route":{"antenna":0}});
        let (map, desc) = plan(&c, &vec![0u8; 12488], &input, false).unwrap();
        assert_eq!(desc["planned_ranges"][0]["side_sign"], 1);
        assert_eq!(desc["planned_ranges"][1]["side_sign"], -1);
        assert_eq!(
            desc["planned_ranges"][0]["image_rf_hz"],
            desc["planned_ranges"][1]["rf_hz"]
        );
        assert_eq!(desc["planned_ranges"][1]["tx_slot"], 5);
        assert_eq!(word(&map, 12328), 1);
        assert_eq!(word(&map, 12332), 0);
        assert_eq!(word(&map, 12340), 5);
        let first_len = desc["planned_ranges"][0]["rx_positions"].as_u64().unwrap() as usize;
        assert!(first_len > 1);
        assert_eq!(desc["positions"][first_len]["rx_lo_hz"], 1_387_080_000u64);
        for i in 0..word(&map, 8) as usize {
            assert_eq!(word(&map, MAP + 32 * i + 4) & 0x704, 0x504);
            assert_eq!(word(&map, NEXT + 32 * i + 4) & 0x704, 0x504);
        }
        let mut crosses = input.clone();
        crosses["scan"]["ranges"][0]["lo_hz"] = json!(4_501_000_000u64);
        crosses["scan"]["ranges"][0]["hi_hz"] = json!(4_503_000_000u64);
        assert!(plan(&c, &vec![0u8; 12488], &crosses, false).is_err());
    }
}
