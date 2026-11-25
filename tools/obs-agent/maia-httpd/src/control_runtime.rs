//! Interactive owner for the row99 guarded RAM runtime.
#![allow(missing_docs)]
mod calibration;
mod field;
mod resident_field;
mod generated;
mod guard_take;
mod loop_grid;
mod mixer_math;
mod plan_math;
mod tx;
mod video_egress;
mod video_profile;

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
const DEFAULT_ANALYSIS_K: u32 = 500;
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
    #[serde(default)]
    bench_split544: bool,
    #[serde(default)]
    bench_obn3_layout: bool,
    #[serde(default)]
    bench_obn3_view_only: bool,
    #[serde(default)]
    setup: PathBuf,
    output: PathBuf,
    view: SocketAddr,
    video: SocketAddr,
    route: SocketAddr,
    #[serde(default)]
    positions: Vec<u64>,
    scan: Value,
    analysis: Value,
    #[serde(default)]
    external_mixer: Option<Mixer>,
    #[serde(default)]
    scan_tx: Vec<ScanTxProfile>,
    #[serde(default)]
    antenna_bands: AntennaBands,
    #[serde(default)]
    field_preview: Option<field::Config>,
    #[serde(default)]
    resident_field: Option<resident_field::Config>,
    #[serde(default)]
    roi: Option<SocketAddr>,
    #[serde(default)]
    bench_roi: Option<SocketAddr>,
    #[serde(default)]
    bench_full_compact_tcp: bool,
}

type AntennaBands = BTreeMap<u64, Vec<[u64; 2]>>;
fn parse_antenna_bands(source: &Value) -> Result<AntennaBands> {
    if source.is_null() {
        return Ok(BTreeMap::new());
    }
    let mut bands: AntennaBands = serde_json::from_value(source.clone())?;
    ensure!(
        !bands.is_empty() && bands.len() <= 8,
        "antenna_bands must specify 1…8 antennas"
    );
    for (&antenna, intervals) in &mut bands {
        ensure!(
            antenna <= 7
                && !intervals.is_empty()
                && intervals.len() <= 16
                && intervals
                    .iter()
                    .all(|[lo, hi]| *lo > 0 && lo < hi && *hi <= plan_math::MAX_RF),
            "invalid RF bands for antenna {antenna}"
        );
        intervals.sort_unstable();
    }
    Ok(bands)
}
fn scan_bounds(c: &Config, bands: &AntennaBands) -> Result<[u64; 2]> {
    let entries = bands.values().flatten();
    let [rf_min, rf_max] = if c.scan["planner_version"] == 2 {
        generated::scan_rf_bounds()?
    } else {
        [70_000_000, plan_math::MAX_RF]
    };
    let lower = entries.clone().map(|b| b[0]).min().unwrap_or(rf_min).max(rf_min);
    let upper = entries.map(|b| b[1]).max().unwrap_or_else(|| {
        if c.scan["planner_version"] == 2 {
            return rf_max;
        }
        c.scan_tx
            .iter()
            .map(|p| p.offset() as u64 + plan_math::DIRECT_LIMIT)
            .max()
            .unwrap_or(plan_math::DIRECT_LIMIT)
            .min(plan_math::MAX_RF)
    });
    Ok([lower, upper.min(rf_max)])
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
    phase: &'static str,
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
    route_last: Value,
    metrics: Value,
    operation: Value,
    progress: Value,
    last_progress: Value,
    progress_failure: Value,
    last_confirmed: Value,
    worker_error: Option<String>,
    take_cancel: Option<(File, u32)>,
    next_map_request: Option<(Value, u64)>,
    roi: Option<crate::system_reader::RoiRequest>,
    roi_id: Value,
    roi_generation: u64,
}
struct Shared {
    clock_session: String,
    inbox: Mutex<Inbox>,
    loop_config: Option<Mutex<loop_grid::Store>>,
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
    radio_access("read", name, || text_trim(Path::new(PHY).join(name)))
}
fn set_attr(name: &str, value: impl ToString) -> Result<String> {
    radio_access("write", name, || Ok(fs::write(Path::new(PHY).join(name), value.to_string())?))?;
    attr(name)
}
std::thread_local! {
    static RADIO_TRACE: RefCell<Option<(Value, u64, u8)>> = const { RefCell::new(None) };
}

fn trace_radio<T>(id: &Value, generation: u64, run: impl FnOnce() -> Result<T>) -> Result<T> {
    if !control_trace_enabled() { return run(); }
    RADIO_TRACE.with(|state| *state.borrow_mut() = Some((id.clone(),generation,64)));
    let result = run();
    RADIO_TRACE.with(|state| *state.borrow_mut() = None);
    result
}

fn radio_access<T>(direction: &str, attribute: &str, access: impl FnOnce() -> Result<T>) -> Result<T> {
    let stamp = RADIO_TRACE.with(|state| {
        let mut state = state.borrow_mut();
        let (id, generation, remaining) = state.as_mut()?;
        *remaining = remaining.checked_sub(1)?;
        Some(json!({"id":id,"generation":generation,"direction":direction,"attribute":attribute,
            "start_ns":clock_ns().to_string()}))
    });
    if let Some(stamp) = &stamp { trace_control("sysfs_enter", || stamp.clone()); }
    let result = access();
    if let Some(mut stamp) = stamp {
        stamp["errno"] = json!(result.as_ref().err().and_then(|e| e.downcast_ref::<std::io::Error>())
            .and_then(std::io::Error::raw_os_error));
        trace_control("sysfs_exit", || stamp);
    }
    result
}
fn clock_ns() -> u64 {
    crate::delivery_runtime::mono_ns()
}

fn view_mode(settings: &Value) -> Result<u32> {
    let bins = settings.get("view_bins").map(|_| int(settings,"view_bins",0,4096))
        .transpose()?.unwrap_or(512);
    let fft = settings.get("fft_bins").map(|_| int(settings,"fft_bins",1024,4096))
        .transpose()?.unwrap_or(4096);
    ensure!(matches!(fft,1024|2048|4096),"fft_bins must be 1024, 2048 or 4096");
    [0,fft/8,fft/4,fft/2,fft].iter().position(|&n|n==bins)
        .map(|mode| mode as u32).ok_or_else(||anyhow!("view_bins must be off or FFT bins divided by 1, 2, 4 or 8"))
}

fn check_spectrum_settings(settings: &Value, obn5: bool) -> Result<()> {
    for key in ["fft_bins", "hold_k", "view_bins", "spectrum_detector"] {
        if settings.get(key).is_some() {
            ensure!(matches!(settings["op"].as_str(),Some("scan"|"analysis")),"{key} requires scan or analysis");
        }
    }
    if let Some(detector) = settings.get("spectrum_detector") {
        ensure!(obn5, "spectrum_detector requires OBN5");
        ensure!(matches!(detector.as_str(), Some("avg" | "peak")), "spectrum_detector must be avg or peak");
    }
    if settings.get("hold_k").is_some() { int(settings,"hold_k",1,1023)?; }
    if matches!(settings["op"].as_str(),Some("scan"|"analysis")) {
        let mode=view_mode(settings)?;
        ensure!(obn5 || (settings.get("fft_bins").is_none() && settings.get("hold_k").is_none() && mode<4),
            "fft_bins, hold_k and VIEW /1 require OBN5");
    }
    Ok(())
}

fn roi_request(cmd:&Value,generation:u64)->Result<Option<crate::system_reader::RoiRequest>> {
    use crate::system_reader::{LEGACY_ROI_WIDTH_HZ,RoiRequest,RoiSource};
    let ranged=["lo_hz","hi_hz","view_bins","source"].iter().any(|key|cmd.get(*key).is_some());
    if ranged {
        ensure!(cmd["center_hz"].is_null(),"specify ROI limits or a legacy center");
        let low=int(cmd,"lo_hz",70_000_000,plan_math::MAX_RF)? as i64;
        let high=int(cmd,"hi_hz",70_000_000,plan_math::MAX_RF)? as i64;
        ensure!(low<high,"ROI limits must be increasing");
        let view_bins=cmd["view_bins"].as_u64().filter(|n|matches!(*n,512|1024|2048|4096))
            .ok_or_else(||anyhow!("ROI view_bins must be 512, 1024, 2048 or 4096"))? as u16;
        let source=match cmd.get("source") {
            None => None,
            Some(v) if v == "full" => Some(RoiSource::Full),
            Some(v) if v == "view" => Some(RoiSource::View),
            _ => bail!("ROI source must be full or view"),
        };
        return Ok(Some(RoiRequest{range:[low,high],view_bins,generation,legacy_full_record:false,source}));
    }
    if cmd["center_hz"].is_null(){return Ok(None);}
    let center=int(cmd,"center_hz",70_000_000,plan_math::MAX_RF)? as i64;
    let half=LEGACY_ROI_WIDTH_HZ/2;
    Ok(Some(RoiRequest{range:[center-half,center+half],view_bins:4096,generation,legacy_full_record:true,source:None}))
}

fn roi_source(spectrum: &Value, request: crate::system_reader::RoiRequest) -> Option<crate::system_reader::RoiSource> {
    request.resolve_source(spectrum["fft_bins"].as_u64().unwrap_or(0) as u16,
                           spectrum["view_bins"].as_u64().unwrap_or(0) as u16)
}

fn validate(v: &Value) -> Result<()> {
    ensure!(v.get("loop").is_none(), "loop requires a running v2 scan with grid policy support");
    ensure!(v["op"] == "scan" || v.get("roi").is_none(), "embedded ROI requires scan");
    if v.get("tx_attenuation_mdb").is_some() {
        ensure!(v["op"] == "scan" && v["planner_version"] == 2,
            "tx_attenuation_mdb requires v2 scan");
        ensure!(int(v, "tx_attenuation_mdb", 0, 89_750)? % 250 == 0,
            "tx_attenuation_mdb must be a multiple of 250");
    }
    if v["op"]=="roi" {
        roi_request(v,0)?;
        return Ok(());
    }
    if v.get("field_targets").is_some() {
        ensure!(v["op"] == "scan" && v["planner_version"] == 2, "field_targets requires v2 scan");
        resident_field::targets(&v["field_targets"])?;
    }
    check_spectrum_settings(v,true)?;
    match v["op"].as_str() {
        Some("scan_ranges") => {
            ensure!(v.as_object().is_some_and(|o| o.keys().all(|key| matches!(key.as_str(), "op" | "id" | "ranges"))),
                "scan_ranges changes only ranges");
            validate(&json!({"op":"scan","planner_version":2,"k":1,"run":false,"ranges":v["ranges"]}))?;
        }
        Some("scan") => {
            if let Some(delivery) = v.get("view_delivery") {
                ensure!(matches!(delivery.as_str(),Some("stream" | "pass")),
                    "unsupported view_delivery");
            }
            if let Some(rev) = v.get("path_ack_version") {
                ensure!(rev == 1 && v["planner_version"] == 2, "unsupported path_ack_version");
            }
            if let Some(v) = v.get("planner_version") {
                ensure!(
                    matches!(v.as_u64(), Some(1 | 2)),
                    "unsupported planner_version"
                );
            }
            int(v, "k", 1, 1023)?;
            let run = v["run"]
                .as_bool()
                .ok_or_else(|| anyhow!("run must be boolean"))?;
            let ranges = v["ranges"]
                .as_array()
                .filter(|r| (1..=16).contains(&r.len()))
                .ok_or_else(|| anyhow!("ranges: 1..16"))?;
            let mut enabled = false;
            let [rf_min, rf_max] = if v["planner_version"] == 2 {
                generated::scan_rf_bounds()?
            } else {
                [70_000_000, plan_math::MAX_RF]
            };
            for r in ranges {
                let lo = int(r, "lo_hz", rf_min, rf_max)?;
                int(
                    r,
                    "hi_hz",
                    lo + if v["planner_version"] == 2 {
                        1
                    } else {
                        120_000
                    },
                    rf_max,
                )?;
                enabled |= r["enabled"]
                    .as_bool()
                    .ok_or_else(|| anyhow!("enabled must be boolean"))?;
                if !r["tx_slot"].is_null() && r["tx_slot"] != "auto" {
                    int(r, "tx_slot", 0, 7)?;
                }
            }
            ensure!(!run || enabled, "no enabled range");
            if let Some(roi) = v.get("roi") {
                ensure!(run && v["planner_version"] == 2, "embedded ROI requires a running v2 scan");
                let roi = roi_request(roi, 0)?.ok_or_else(|| anyhow!("embedded ROI requires explicit RF limits"))?;
                ensure!(roi.source == Some(crate::system_reader::RoiSource::View)
                    && v["view_bins"].as_u64() == Some(u64::from(roi.view_bins)),
                    "embedded ROI requires the requested VIEW grid");
                ensure!(ranges.iter().any(|row| row["enabled"] == true &&
                    row["lo_hz"].as_i64().is_some_and(|lo| lo < roi.range[1]) &&
                    row["hi_hz"].as_i64().is_some_and(|hi| hi > roi.range[0])),
                    "embedded ROI must overlap the scan range");
            }
            if let Some(bands) = v.get("antenna_bands") {
                ensure!(!bands.is_null(), "antenna_bands must be an object");
                parse_antenna_bands(bands)?;
            }
            if v.get("gain_db").is_some() {
                int(v, "gain_db", 0, 71)?;
            }
            if let Some(ranges) = v.get("next_ranges") {
                ensure!(run && v["planner_version"] == 2,"NEXT preparation requires v2 scan run");
                let mut second = v.clone();
                second.as_object_mut().unwrap().remove("next_ranges");
                second["ranges"] = ranges.clone();
                validate(&second)?;
            }
        }
        Some("next") => {
            int(v,"source_generation",1,u64::MAX)?;
            int(v,"next_version",1,0x7fff_fffe)?;
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
                    56_000_000
                },
            )?;
            if v["op"] == "analysis" {
                int(v, "k", 1, 1023)?;
                int(v, "gain_db", 0, 71)?;
                ensure!(
                    matches!(v["agc"].as_str(), Some("manual" | "slow_attack")),
                    "unsupported AGC"
                );
            } else if v.get("spectrum_k").is_some() {
                int(v, "spectrum_k", 1, 1023)?;
            }
        }
        Some("return") => {}
        Some("antennas") => {
            ensure!(!parse_antenna_bands(&v["antenna_bands"])?.is_empty(), "antenna table required");
        }
        Some("alternative") => {
            int(v, "source_generation", 1, u64::MAX)?;
            int(v, "source_position", 0, 65534)?;
            let [rf_min, rf_max] = generated::scan_rf_bounds()?;
            let lo = int(v, "lo_hz", rf_min, rf_max)?;
            int(v, "hi_hz", lo + 1, rf_max)?;
        }
        Some("cancel_alternative") => {
            ensure!(
                v["request_id"].as_str().is_some_and(|s| s.len() == 32),
                "alternative request_id"
            );
        }
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
    let loop_config = shared.loop_config.as_ref().map(|config| config.lock().unwrap().status());
    let q = shared.inbox.lock().unwrap();
    let reader = crate::system_reader::status();
    Json(
        json!({"abi":1,"kind":"guarded-control","origin":"hardware","actual":q.actual,
        "pending_id":q.next.as_ref().or(q.pending.as_ref()).map(|v|&v["id"])
            .or(q.next_map_request.as_ref().map(|(v,_)|&v["id"])),
        "applied_id":q.applied_id,"failed_id":q.failed_id,"error":q.error,
        "route_pending":q.route_pending,"route_last":q.route_last,"arm_clock_ns":q.receipt_ns.to_string(),
        "arm_now_ns":clock_ns().to_string(),"boot_id":q.actual["boot"],"clock_session":shared.clock_session,
        "phase":q.phase,"command_timing":q.command_timing,"capabilities":q.capabilities,"loop_config":loop_config,
        "operation":q.operation,"progress":q.progress,"last_progress":q.last_progress,"progress_failure":q.progress_failure,
        "roi":{"id":q.roi_id,"generation":q.roi_generation,"center_hz":q.roi.map(|r|r.range[0]+(r.range[1]-r.range[0])/2),
               "range_hz":q.roi.map(|r|r.range),"view_bins":q.roi.map(|r|r.view_bins)},
        "last_confirmed":q.last_confirmed,"worker_error":q.worker_error,
        "metrics":{"owner":q.metrics,"reader":reader}}),
    )
}

async fn submit(
    State(shared): State<Arc<Shared>>,
    Json(mut cmd): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if cmd["op"] == "analysis" && cmd.get("k").is_none() {
        cmd["k"] = json!(DEFAULT_ANALYSIS_K);
    }
    let id = &cmd["id"];
    if !id
        .as_str()
        .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"invalid id"})));
    }
    if cmd["op"] == "loop_config" {
        if shared.loop_config.is_none() {
            return (StatusCode::CONFLICT, Json(json!({"error":"ARM loop config is unavailable"})));
        }
        let policy = match loop_grid::Policy::parse(cmd["config"].clone()) {
            Ok(policy) => policy,
            Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({"error":error.to_string()}))),
        };
        let id = id.as_str().unwrap().to_owned();
        let save_id = id.clone();
        let saved = tokio::task::spawn_blocking(move || {
            shared.loop_config.as_ref().unwrap().lock().unwrap().save(&save_id, policy)
        }).await;
        return match saved {
            Ok(Ok(())) => (StatusCode::ACCEPTED, Json(json!({"accepted_id":id}))),
            Ok(Err(error)) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":error.to_string()}))),
            Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error":error.to_string()}))),
        };
    }
    if cmd.get("loop").is_some() && (cmd["op"] != "scan" || shared.loop_config.is_none()) {
        return (StatusCode::CONFLICT, Json(json!({"error":"configurable loop requires a supported scan"})));
    }
    if cmd["op"] == "scan" && cmd["planner_version"] == 2 && let Some(config) = &shared.loop_config {
        let policy = config.lock().unwrap().effective();
        let resolved = generated::scan_rf_bounds().and_then(|bounds| policy.resolve_scan(&mut cmd, bounds));
        if let Err(error) = resolved {
            return (StatusCode::BAD_REQUEST, Json(json!({"error":error.to_string()})));
        }
    }
    let id = &cmd["id"];
    if let Err(e) = validate(&cmd) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":e.to_string()})),
        );
    }
    let mut q = shared.inbox.lock().unwrap();
    if cmd["op"]!="roi" && let Err(error)=check_spectrum_settings(&cmd,q.capabilities["spectrum_abi"]=="OBN5") {
        return (StatusCode::BAD_REQUEST,Json(json!({"error":error.to_string()})));
    }
    if let Some(error) = &q.worker_error {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"RF owner unavailable","worker_error":error})));
    }
    if cmd.get("roi").is_some() && q.capabilities["scan_roi_only"] != true {
        return (StatusCode::CONFLICT, Json(json!({"error":"ROI-only scan delivery is unavailable"})));
    }
    if cmd["op"]=="roi" {
        let bench = cfg!(feature="scan-stream-bench") && q.capabilities["bench_roi"]==true;
        if (!bench && q.capabilities["roi"]!=true) || q.actual["mode"]!="scan" || q.actual["running"]!=true {
            return (StatusCode::CONFLICT, Json(json!({"error":"active ROI scan required"})));
        }
        if q.actual["spectrum"]["overview_enabled"] == false {
            return (StatusCode::CONFLICT, Json(json!({"error":"change ROI-only delivery with a scan command"})));
        }
        if q.roi_id != *id {
            let generation=q.roi_generation+1;
            let Ok(request)=roi_request(&cmd,generation) else {
                return (StatusCode::BAD_REQUEST,Json(json!({"error":"invalid ROI configuration"})));
            };
            if !bench && request.is_some_and(|r|r.legacy_full_record) {
                return (StatusCode::BAD_REQUEST, Json(json!({"error":"ROI requires explicit RF limits and bin count"})));
            }
            if q.actual["view_bins"]!=0 && request.is_some_and(|r|r.legacy_full_record) {
                return (StatusCode::CONFLICT,Json(json!({"error":"full-record ROI requires VIEW OFF"})));
            }
            if q.actual["spectrum"]["fft_bins"]!=4096 && request.is_some_and(|r|r.legacy_full_record) {
                return (StatusCode::CONFLICT,Json(json!({"error":"full-record ROI requires FFT4096"})));
            }
            if q.capabilities["roi_exact_bins"] == true &&
                request.is_some_and(|r| roi_source(&q.actual["spectrum"], r).is_none())
            {
                return (StatusCode::CONFLICT, Json(json!({
                    "error": "ROI requires the active PL FFT or VIEW grid",
                    "fft_bins": q.actual["spectrum"]["fft_bins"],
                    "view_bins": q.actual["spectrum"]["view_bins"],
                })));
            }
            q.roi_generation=generation;
            q.roi=request;
            q.roi_id=id.clone();
        }
        return (StatusCode::ACCEPTED,Json(json!({"accepted_id":id,"roi_generation":q.roi_generation})));
    }
    if cmd["op"]=="analysis" && cmd["width_hz"].as_u64()>q.capabilities["analysis_max_width_hz"].as_u64() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"analysis width exceeds the spectral ABI limit"})));
    }
    if cmd["op"] == "scan_ranges" && q.actual["mode"] != "analysis" {
        return (StatusCode::CONFLICT, Json(json!({"error":"scan_ranges requires analysis"})));
    }
    if cmd["op"]=="video" && let Err(error)=video_profile::Profile::parse(&cmd,q.capabilities["video_profiles"]==true) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":error.to_string()})));
    }
    if cmd["op"] == "video" && cmd.get("spectrum_k").is_some() && q.capabilities["video_spectrum"] != true {
        return (StatusCode::BAD_REQUEST, Json(json!({"error":"video spectrum requires OBN5 r2"})));
    }
    if q.applied_id == *id
        || q.failed_id == *id
        || q.pending.as_ref().is_some_and(|p| p["id"] == *id)
        || q.next.as_ref().is_some_and(|p| p["id"] == *id)
        || q.next_map_request.as_ref().is_some_and(|(p,_)| p["id"] == *id)
    {
        return (StatusCode::ACCEPTED, Json(json!({"accepted_id":id})));
    }
    if cmd["op"] == "next" {
        if q.next.is_some() || q.pending.is_some() || q.next_map_request.is_some()
            || q.actual["running"] != true || q.actual["generation"] != cmd["source_generation"]
            || q.actual["prepared_next_version"] != cmd["next_version"] {
            return (StatusCode::CONFLICT,Json(json!({"error":"NEXT version unavailable or already pending"})));
        }
        q.next_map_request = Some((cmd.clone(),clock_ns()));
        return (StatusCode::ACCEPTED,Json(json!({"accepted_id":id})));
    }
    if cmd["op"] == "scan" && cmd["run"] == false && q.pending.is_some() && q.next.is_none() {
        if let Some((fd, cookie)) = &q.take_cancel {
            let _ = guard_take::cancel(fd, *cookie);
        }
        q.next = Some(cmd.clone());
        q.queued_ns = clock_ns();
        shared.switch.store(true, Ordering::Release);
        shared.wake.notify_one();
        return (StatusCode::ACCEPTED, Json(json!({"accepted_id":id})));
    }
    if cmd["op"] == "alternative" {
        let source = &q.actual;
        let p = &source["spectrum"]["positions"][cmd["source_position"].as_u64().unwrap() as usize];
        let lo = cmd["lo_hz"].as_u64().unwrap();
        let hi = cmd["hi_hz"].as_u64().unwrap();
        if source["generation"] != cmd["source_generation"]
            || source["running"] != true
            || source["scan"]["planner_version"] != 2
            || source["mode"] != "scan"
            || !source["alternative"].is_null()
            || !p["rf_ranges"].as_array().is_some_and(|rs| {
                rs.iter().any(|r| {
                    r[0].as_u64().is_some_and(|a| a <= lo) && r[1].as_u64().is_some_and(|b| hi <= b)
                })
            })
        {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error":"stale source or target outside source visit"})),
            );
        }
    }
    if cmd["op"] == "cancel_alternative" {
        let preparing = q
            .pending
            .as_ref()
            .is_some_and(|p| p["op"] == "alternative" && p["id"] == cmd["request_id"]);
        if !preparing && q.actual["alternative"]["request_id"] != cmd["request_id"] {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error":"unmatched alternative request"})),
            );
        }
        if preparing && q.next.is_none() {
            if let Some((fd, cookie)) = &q.take_cancel {
                let _ = guard_take::cancel(fd, *cookie);
            }
            q.next = Some(cmd.clone());
            q.queued_ns = clock_ns();
            shared.switch.store(true, Ordering::Release);
            shared.wake.notify_one();
            return (StatusCode::ACCEPTED, Json(json!({"accepted_id":id})));
        }
    }
    if q.next.is_some() || q.pending.is_some() || (q.next_map_request.is_some()
            && !(cmd["op"]=="scan" && cmd["run"]==false)) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"another hardware command is pending"})),
        );
    }
    if let Some(reason) = tx_conflict(&cmd, &q.actual, q.capabilities["tx_on"] == true) {
        return (StatusCode::CONFLICT, Json(json!({"id":id,"error":reason})));
    }
    let id = id.clone();
    q.next = Some(cmd);
    q.phase = "accepted";
    q.queued_ns = clock_ns();
    shared.switch.store(true, Ordering::Release);
    shared.wake.notify_one();
    (StatusCode::ACCEPTED, Json(json!({"accepted_id":id})))
}

fn tx_conflict(cmd: &Value, state: &Value, available: bool) -> Option<&'static str> {
    if cmd["op"] != "tx" {
        return None;
    }
    if state["mode"] == "scan" {
        return Some("manual TX locked while scanner owns profiles");
    }
    if !available {
        return Some("TX owned by external adapter or unavailable");
    }
    if state["mode"] == "video" {
        let video = &state["video"];
        if state["manual_demod_tx_available"] != true || video.get("spectrum_k").is_none() {
            return Some("manual TX in video requires demod support");
        }
        if state["route"]["kind"] == "mixer" && cmd["on"] == true {
            let center = video["center_hz"].as_u64().unwrap();
            let width = video["width_hz"].as_u64().unwrap();
            let lo = cmd["frequency_hz"].as_u64().unwrap() as i64;
            let (offset, side) = mixer_transform(&state["route"], lo);
            let a = (center.saturating_sub(width / 2) as i64 - offset) * side;
            let b = ((center + width.div_ceil(2)) as i64 - offset) * side;
            if a.min(b) < 70_000_000 || a.max(b) > 6_000_000_000 {
                return Some("TX frequency leaves the demod channel outside RX coverage");
            }
        }
    }
    None
}

fn progress_matches(sample: &Value, state: &Value) -> bool {
    state["running"] == true && !sample["generation"].is_null()
        && sample["generation"] == state["generation"]
        && sample["version"] == state["spectrum"]["version"]
        && sample["map_epoch"] == state["spectrum"]["map_epoch"]
        && sample["run"] == state["dma_run"]
}

fn receipt(shared: &Shared, state: &Value, id: &Value, error: Option<String>) {
    let mut q = shared.inbox.lock().unwrap();
    q.pending = None;
    let was_roi_only = q.actual["mode"] == "scan" && q.actual["scan"].get("roi").is_some();
    q.actual = state.clone();
    let next_roi = if error.is_none() && state["mode"] == "scan" && state["running"] == true {
        state["scan"].get("roi").and_then(|command| roi_request(command, q.roi_generation + 1).ok().flatten())
    } else { None };
    if let Some(request) = next_roi {
        q.roi_generation = request.generation;
        q.roi = Some(request);
        q.roi_id = id.clone();
    } else if q.capabilities["roi_exact_bins"] == true && q.roi.is_some_and(|r|
        was_roi_only || state["mode"] != "scan" || state["running"] != true ||
        roi_source(&state["spectrum"], r).is_none())
    {
        q.roi = None;
        q.roi_id = Value::Null;
        q.roi_generation += 1;
    }
    if !progress_matches(&q.progress, state) { q.progress = Value::Null; }
    q.phase = if error.is_some() {
        "failed"
    } else if state["running"] == true {
        "running"
    } else {
        "stopped"
    };
    q.receipt_ns = clock_ns();
    if !id.is_null() {
        q.command_timing = json!({"id":id,"received_ns":q.received_ns.to_string(),
            "completed_ns":q.receipt_ns.to_string()});
    }
    if let Some(e) = error {
        q.failed_id = id.clone();
        if q.progress_failure["id"] == *id && q.progress_failure["recovery"] == "repreparing" {
            q.progress_failure["recovery"] = json!("failed");
            q.progress_failure["recovery_error"] = json!(e);
        }
        q.error = json!(e);
    } else {
        q.applied_id = id.clone();
        q.error = Value::Null;
        if state["running"] == true {
            if q.progress_failure["id"] == *id && q.progress_failure["recovery"] == "repreparing" {
                q.progress_failure["recovery"] = json!("recovered");
                q.progress_failure["recovery_generation"] = state["generation"].clone();
            }
            q.last_confirmed = json!({"id":id,"generation":state["generation"],"mode":state["mode"],
                "version":state["spectrum"]["version"],"map_epoch":state["spectrum"]["map_epoch"],"run":state["dma_run"],
                "path":state["spectrum"]["path_confirmation"],"confirmed_arm_ns":q.receipt_ns.to_string()});
        }
    }
}

fn word(b: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(b[offset..offset + 4].try_into().unwrap())
}

fn trace_control(stage: &str, fields: impl FnOnce() -> Value) {
    if control_trace_enabled() {
        eprintln!("{}", json!({"control_stage":stage,"arm_ns":clock_ns(),"detail":fields()}));
    }
}

fn control_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("OBS_CONTROL_TRACE").is_some())
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
    plan_positions(c, &c.positions, template, state, marker)
}

fn plan_positions(
    c: &Config,
    positions: &[u64],
    template: &[u8],
    state: &Value,
    marker: bool,
) -> Result<(Vec<u8>, Value)> {
    use plan_math::{Candidate, Segment};
    let direct_antenna = state["route"]["antenna"].as_u64().unwrap_or(0);
    let received = parse_antenna_bands(&state["scan"]["antenna_bands"])?;
    let bands = |antenna| received.get(&antenna).map_or(&[][..], Vec::as_slice);
    let direct_antenna_ids = if received.is_empty() {
        vec![direct_antenna]
    } else {
        received.keys().copied().collect::<Vec<_>>()
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
            if !received.is_empty() && !received.contains_key(&p.antenna) {
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
    let [min_rf, max_rf] = scan_bounds(c, &received)?;
    for row in state["scan"]["ranges"]
        .as_array()
        .ok_or_else(|| anyhow!("scan ranges"))?
    {
        if row["enabled"] != true {
            continue;
        }
        let lo = int(row, "lo_hz", min_rf, max_rf)?;
        let hi = int(
            row,
            "hi_hz",
            lo + if state["scan"]["planner_version"] == 2 {
                1
            } else {
                120_000
            },
            max_rf,
        )?;
        let mut list = Vec::new();
        if row["tx_slot"] == "auto" {
            list = plan_math::automatic(positions, &mixer, previous, lo, hi)
                .ok_or_else(|| anyhow!("RF {lo}..{hi} Hz: no complete mixer coverage"))?;
        } else if let Some(slot) = row["tx_slot"].as_u64() {
            let profile = mixer
                .iter()
                .find(|p| p.slot == Some(slot as usize))
                .ok_or_else(|| {
                    anyhow!("TX slot {slot} is unavailable for the configured antennas")
                })?;
            list.push(plan_math::cover(positions, profile, lo, hi).ok_or_else(|| {
                anyhow!("RF {lo}..{hi} Hz: no complete coverage with TX slot {slot}")
            })?);
        } else {
            let direct_hi = hi.min(plan_math::DIRECT_LIMIT);
            if lo < direct_hi {
                list = plan_math::automatic(positions, &direct, previous, lo, direct_hi)
                    .ok_or_else(|| {
                        anyhow!("RF {lo}..{direct_hi} Hz: no complete calibrated direct coverage")
                    })?;
            }
            if hi > plan_math::DIRECT_LIMIT {
                let mixer_lo = lo.max(plan_math::DIRECT_LIMIT);
                let next = plan_math::automatic(
                    positions,
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
                    positions,
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
                    "old_if_hz":positions[old_idx],"new_if_hz":positions[new_idx]}));
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
        let intermediate = positions[v.index];
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

fn guard_raw(fd: &File) -> Option<[u32;128]> {
    let mut status = [0u32;128];
    (unsafe {libc::ioctl(fd.as_raw_fd(),0x4605,status.as_mut_ptr())}==0).then_some(status)
}

const XTG1: u32 = 0x31475458;

fn gate_receipt(words: &[u32; 128], spectrum: &Value) -> Value {
    let status = words[118];
    let epoch = (u64::from(words[124]) << 32) | u64::from(words[123]);
    let position = words[121] as usize;
    let expected = &spectrum["positions"][position];
    let count = spectrum["positions"].as_array().map_or(0, Vec::len);
    let on = status & 2 != 0;
    let slot = (status >> 3) & 7;
    let bound = words[117] == XTG1 && status & 0x1c5 == 0x45
        && spectrum["version"] == words[120] && count > 0
        && spectrum["map_epoch"].as_u64().is_some_and(|start|
            epoch.checked_sub(start).is_some_and(|e| e % count as u64 == position as u64))
        && expected["tx_gate_requested"] == on
        && expected["tx_slot"].as_u64().is_none_or(|s| s == u64::from(slot));
    json!({"capability":words[117],"raw_status":status,"duration_ticks":words[119],
        "version":words[120],"position":position,"pass":words[122],"epoch":epoch,
        "on":bound.then_some(on),"slot":bound.then_some(slot),"reason":(status>>9)&7})
}

const ACK_ATTEMPTS: u8 = 5;
const ACK_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(1);

struct Route {
    stream: Option<TcpStream>,
    key: (u64, u32, u64, u32, u32, u32),
    out: Vec<u8>,
    written: usize,
    input: Vec<u8>,
    since: Instant,
    attempt: u8,
    detected_ns: u64,
    send_started_ns: u64,
    sent_ns: u64,
    trace: bool,
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
    if shared.switch.load(Ordering::Acquire) || shared.shutdown.load(Ordering::Acquire) {
        return Ok(None);
    }
    let key = (generation, id, epoch, version, position, antenna);
    let initial_path = source.strip_prefix("startup_");
    let visit_path = source.strip_prefix("visit_");
    let manual_path = source.strip_prefix("manual_");
    if route.as_ref().is_none_or(|r| r.key != key) {
        static SAMPLES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let trace = initial_path.is_some() || SAMPLES.fetch_add(1, Ordering::Relaxed) < 256;
        if trace {
            trace_control("route_detected", ||json!([generation,version,epoch,position,id,source]));
        }
        let detected_ns = clock_ns();
        let mut v = json!({"marker_id":id,"epoch":epoch,"version":version,"position":position,"route":antenna,"generation":generation,
            "boot_id":c.boot_id,"clock_session":shared.clock_session,
            "source":if source=="PLv2" {"PL"} else {source},"ack_version":if source=="PLv2" {2}else{1}});
        if let Some(path) = manual_path {
            v["source"] = json!("manual");
            v["required_path"] = json!(path);
        }
        if let Some(path) = initial_path {
            v["event"] = json!("prepare_path");
            v["source"] = json!("ARM");
            v["required_path"] = json!(path);
            v["boot_id"] = json!(c.boot_id);
            v["ack_version"] = json!(3);
        }
        if let Some(path) = visit_path {
            v["source"] = json!("PL");
            v["event"] = json!("visit_path");
            v["required_path"] = json!(path);
            v["boot_id"] = json!(c.boot_id);
            v["ack_version"] = json!(4);
        }
        let mut out = serde_json::to_vec(&v).unwrap();
        out.push(b'\n');
        v["detected_arm_ns"] = json!(detected_ns.to_string());
        shared.inbox.lock().unwrap().route_pending = v;
        *route = Some(Route {
            stream: None,
            key,
            out,
            written: 0,
            input: Vec::new(),
            since: Instant::now(),
            attempt: 0,
            detected_ns,
            send_started_ns: 0,
            sent_ns: 0,
            trace,
        });
    }
    let r = route.as_mut().unwrap();
    if r.attempt == 0 || r.since.elapsed() >= ACK_ATTEMPT_TIMEOUT {
        if r.attempt == ACK_ATTEMPTS {
            return Err("route ACK timeout after retry".into());
        }
        r.attempt += 1;
        r.since = Instant::now();
        r.written = 0;
        r.send_started_ns = 0;
        r.sent_ns = 0;
        r.input.clear();
        r.stream = TcpStream::connect_timeout(&c.route, Duration::from_millis(50)).ok();
        if let Some(stream) = &r.stream {
            stream.set_nonblocking(true).map_err(|e| e.to_string())?;
        }
        let mut q = shared.inbox.lock().unwrap();
        q.route_pending["attempt"] = json!(r.attempt);
        q.route_pending["send_started_arm_ns"] = Value::Null;
        q.route_pending["sent_arm_ns"] = Value::Null;
    }
    let Some(stream) = &mut r.stream else { return Ok(None) };
    if r.written < r.out.len() {
        if r.send_started_ns == 0 {
            r.send_started_ns = clock_ns();
            shared.inbox.lock().unwrap().route_pending["send_started_arm_ns"] = json!(r.send_started_ns.to_string());
        }
        match stream.write(&r.out[r.written..]) {
            Ok(0) => { r.stream = None; return Ok(None); }
            Ok(n) => {
                r.written += n;
                if r.written == r.out.len() {
                    r.sent_ns = clock_ns();
                    shared.inbox.lock().unwrap().route_pending["sent_arm_ns"] = json!(r.sent_ns.to_string());
                    if r.trace {
                        trace_control("route_sent", ||json!([generation,version,epoch,position,id,r.attempt]));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(_) => { r.stream = None; return Ok(None); }
        }
    }
    let mut data = [0; 128];
    match stream.read(&mut data) {
        Ok(0) => { r.stream = None; return Ok(None); }
        Ok(n) => r.input.extend_from_slice(&data[..n]),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
        Err(_) => { r.stream = None; return Ok(None); }
    }
    if r.input.len() > 512 {
        return Err("route ACK length".into());
    }
    let expected = if let Some(path) = initial_path {
        format!("ACK3 {} {generation} {version} {antenna} {path}", c.boot_id)
    } else if let Some(path) = visit_path {
        format!("ACK4 {} {generation} {version} {epoch} {position} {id} {antenna} {path}", c.boot_id)
    } else if source == "PLv2" {
        format!("ACK2 {generation} {version} {epoch} {position} {id} {antenna}")
    } else {
        format!("ACK {id}")
    };
    while let Some(end) = r.input.iter().position(|b| *b == b'\n') {
        let ok = &r.input[..end] == expected.as_bytes();
        r.input.drain(..=end);
        if ok && r.since.elapsed() < ACK_ATTEMPT_TIMEOUT
            && !shared.switch.load(Ordering::Acquire) && !shared.shutdown.load(Ordering::Acquire) {
            if r.trace {
                trace_control("route_ack", ||json!([generation,version,epoch,position,id,r.attempt]));
            }
            let mut q = shared.inbox.lock().unwrap();
            let mut done = std::mem::take(&mut q.route_pending);
            done["ack_arm_ns"] = json!(clock_ns().to_string());
            done["detected_arm_ns"] = json!(r.detected_ns.to_string());
            q.route_last = done;
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

fn mixer_transform(route: &Value, tx_hz: i64) -> (i64, i64) {
    let side = route["side"].as_i64().unwrap_or(1);
    let offset = if route["kind"] == "mixer" {
        route["multiplier"].as_i64().unwrap_or(1) * tx_hz
    } else {
        0
    };
    (offset, side)
}

fn tune(state: &mut Value) -> Result<()> {
    let video = state["mode"] == "video";
    let settings = &state[if video { "video" } else { "analysis" }];
    let center = int(settings, "center_hz", 70_000_000, 18_000_000_000)?;
    let width = int(settings, "width_hz", 120_000, 56_000_000)?;
    let spectrum_enabled = !video || settings.get("spectrum_k").is_some();
    let mixer = state["route"]["kind"] == "mixer";
    let (offset, side) = mixer_transform(&state["route"], state["tx"]["frequency_hz"].as_i64().unwrap_or(0));
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
    if spectrum_enabled {
        let edges = if video {
            let low = (lo as i64 - 28_000_000).max(70_000_000);
            let high = (lo as i64 + 28_000_000).min(6_000_000_000);
            let a = offset + side * low;
            let b = offset + side * high;
            json!([a.min(b).max(70_000_000), a.max(b)])
        } else {
            json!([center-width/2,center+width.div_ceil(2)])
        };
        state["spectrum"] = json!({"rf_ranges":[edges],
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

fn video_counters(generation: u64, queue: &video_egress::Queue, finished: bool, error: Option<String>) -> Value {
    let c = &queue.counts;
    json!({"generation":generation,"video_bytes":c.bytes,"video_chunks":c.frames,
        "wire_bytes_accepted_by_socket":c.wire_bytes,"source_bytes_read":c.source_bytes,
        "source_reads":c.source_reads,"source_errors":c.source_errors,"iio_block_samples":8192,
        "pipe_read_ns":c.source_read_ns.to_string(),"packed_bytes":c.packed_bytes,
        "packed_frames":c.packed_frames,"pack_ns":c.pack_ns.to_string(),
        "send_calls":c.send_calls,"send_errors":c.send_errors,"send_ns":c.send_ns.to_string(),
        "send_would_block":c.would_block,"last_socket_errno":c.last_socket_errno,
        "queue_frames":queue.frames(),"queue_payload_bytes":queue.payload_bytes(),
        "queue_high_frames":c.high_frames,"queue_high_payload_bytes":c.high_payload_bytes,
        "queue_overflow":c.overflow,"ready_to_send_ns":c.ready_to_send_ns.to_string(),
        "max_ready_to_send_ns":c.max_ready_to_send_ns.to_string(),
        "finished":finished,"error":error,
        "source_bytes_not_packed":c.source_bytes-c.packed_bytes,
        "payload_not_sent_at_finish":finished.then(|| queue.payload_bytes())})
}

fn continuous(
    c: &Config,
    shared: &Shared,
    state: &Value,
    id: &Value,
    regs: &Registers,
    profile: &video_profile::Profile,
    cancel: &AtomicBool,
) -> Result<()> {
    let with_spectrum = state["video"].get("spectrum_k").is_some();
    let spectrum_status = regs.read(0x64);
    let stopping = || shared.switch.load(Ordering::Acquire) || cancel.load(Ordering::Acquire);
    ensure!(
        (if with_spectrum { spectrum_status & 1 == 1 } else { spectrum_status == 16 })
            && regs.read(0x1e0) == 0 && regs.read(0x284) == 0,
        "video needs the requested FFT state and released DDC"
    );
    if stopping() {
        return Ok(());
    }
    let generation = state["generation"].as_u64().unwrap();
    let mut queue = video_egress::Queue::new(profile.code);
    shared.inbox.lock().unwrap().metrics = video_counters(generation, &queue, false, None);
    // Connect before admitting samples. A slow or absent receiver cannot fill
    // IIO during connection setup.
    let socket = TcpStream::connect_timeout(&c.video, Duration::from_millis(100))
        .and_then(|conn| { conn.set_nonblocking(true)?; Ok(conn) });
    let mut conn = match socket {
        Ok(conn) => conn,
        Err(error) => {
            queue.counts.send_errors += 1;
            queue.counts.last_socket_errno = error.raw_os_error();
            shared.inbox.lock().unwrap().metrics = video_counters(generation, &queue, true, Some(error.to_string()));
            return Err(error.into());
        }
    };
    let saved = [regs.read(0x30), regs.read(0x38), regs.read(0x40)];
    if profile.division == 2 {
        video_profile::select_ddc2(regs)?;
    }
    let nco = state["ddc_hz"].as_i64().unwrap();
    let phase = ((nco * (1i64 << 28)) / FS as i64) as u32 & 0x0fff_ffff;
    regs.write(0x30, phase);
    regs.write(0x40, profile.control);
    regs.write(0x38, 1);
    let mut reader = Command::new("iio_readdev");
    reader
        .args(["-b", "8192", "cf-ad9361-lpc", "voltage0", "voltage1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    tx::child_follows_owner(&mut reader);
    let result = match reader.spawn() {
        Ok(mut child) => {
            let result = (|| {
                ensure!(regs.read(0x40)&0x17f == profile.control, "video settings readback mismatch");
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
                            && !stopping(),
                        "IIO buffer enable timeout"
                    );
                    std::thread::sleep(Duration::from_millis(3));
                }
                regs.write(0x284, 1);
                ensure!(regs.read(0x284) == 1, "stream_run readback");
                let mut bytes = [0u8; video_egress::MAX_BLOCK_BYTES];
                let mut have = 0;
                let mut status_due = Instant::now();
                let mut sampled_reads = 0;
                let mut source_progress_at = None;
                while !stopping() {
                    let previous_sent = queue.counts.frames;
                    let sent = queue.pump(&mut conn).map_err(|e| anyhow!("real8 socket send: {e}"))?;
                    if previous_sent == 0 && queue.counts.frames != 0 {
                        receipt(shared, state, id, None);
                    }
                    let read_start = Instant::now();
                    let input = stdout.read(&mut bytes[have..]);
                    queue.counts.source_read_ns += read_start.elapsed().as_nanos();
                    let n = match input {
                        Ok(0) => {
                            queue.counts.source_errors += 1;
                            bail!("IIO real8 ended")
                        }
                        Ok(n) => {
                            queue.counts.source_reads += 1;
                            queue.counts.source_bytes += n as u64;
                            n
                        }
                        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => 0,
                        Err(e) => {
                            queue.counts.source_errors += 1;
                            return Err(e.into())
                        }
                    };
                    have += n;
                    if have >= video_egress::MIN_BLOCK_BYTES {
                        let len = have & !3;
                        queue.push(generation, &bytes[..len], Instant::now())?;
                        bytes.copy_within(len..have, 0);
                        have -= len;
                    }
                    if status_due.elapsed() >= Duration::from_millis(200) {
                        let now = Instant::now();
                        if queue.counts.source_reads != sampled_reads {
                            sampled_reads = queue.counts.source_reads;
                            source_progress_at = Some(now);
                        }
                        let age = now.duration_since(source_progress_at.unwrap_or(start));
                        let mut stats = video_counters(generation, &queue, false, None);
                        stats["iio_progress_sample_age_ms"] = json!(age.as_millis());
                        stats["iio_progress"] = json!(if age >= Duration::from_secs(1) {
                            "no_progress"
                        } else if source_progress_at.is_some() {
                            "reading"
                        } else {
                            "waiting_first_read"
                        });
                        let video_status=regs.read(0x44);
                        stats["video_sync"]=json!({"locked":video_status&8!=0,"polarity":if video_status&4==0{"negative"}else{"positive"},
                            "flushing":video_status&16!=0,"overrun":video_status&64!=0,"marks_per_s":regs.read(0x48)&0x3ffff});
                        shared.inbox.lock().unwrap().metrics = stats;
                        status_due = now;
                    }
                    if !sent && n == 0 {
                        let mut fds = [
                            libc::pollfd {fd, events:libc::POLLIN, revents:0},
                            libc::pollfd {fd:conn.as_raw_fd(), events:libc::POLLOUT, revents:0},
                        ];
                        let count = if queue.frames() == 0 {1} else {2};
                        if unsafe { libc::poll(fds.as_mut_ptr(), count, 4) } < 0 {
                            let e = std::io::Error::last_os_error();
                            if e.kind() != std::io::ErrorKind::Interrupted { return Err(e.into()); }
                        }
                    }
                }
                Ok(())
            })();
            regs.write(0x284, 0);
            let cleanup = stop_video_reader(&mut child);
            (result, cleanup)
        }
        Err(e) => (Err(e.into()), Ok(())),
    };
    regs.write(0x38, 0);
    if profile.division == 2 {
        video_profile::restore_ddc4(regs);
    }
    regs.write(0x30, saved[0]);
    regs.write(0x40, saved[2]);
    regs.write(0x38, saved[1]);
    let (result, cleanup) = result;
    let cleanup = cleanup.and_then(|()| {
        if profile.division == 2 {
            ensure!(regs.read(0x2c)==4, "DDC4 restore readback mismatch");
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        while capture_enabled()? {
            ensure!(Instant::now() < deadline, "IIO buffer remained enabled after real8 cleanup");
            std::thread::sleep(Duration::from_millis(3));
        }
        Ok(())
    });
    let mut inbox = shared.inbox.lock().unwrap();
    let error = result.as_ref().err().or(cleanup.as_ref().err()).map(ToString::to_string);
    inbox.metrics = video_counters(generation, &queue, true, error);
    inbox.metrics["cleanup_error"] = json!(cleanup.as_ref().err().map(ToString::to_string));
    if let Err(error) = &cleanup {
        inbox.worker_error = Some(error.to_string());
    }
    drop(inbox);
    result.and(cleanup)
}

fn continuous_with_spectrum(
    c: &Config,
    shared: &Shared,
    state: &mut Value,
    id: &Value,
    profile: &video_profile::Profile,
) -> Result<()> {
    let generation = state["generation"].as_u64().unwrap();
    let version = state["map_version"].as_u64().unwrap_or(0) as u32;
    let k = state["k"].clone();
    let tx_active = state["tx"]["on"] == true;
    let grid = &state[state["return_mode"].as_str().unwrap_or("scan")];
    let fft = grid["fft_bins"].as_u64().unwrap_or(4096);
    let stream_config = match fft {
        4096 => 0,
        2048 => 2,
        1024 => 4,
        _ => bail!("OBN5 FFT length"),
    };
    let view = grid["view_bins"].as_u64().filter(|bins| *bins != 0).unwrap_or(fft / 2);
    let mode = view_mode(&json!({"fft_bins":fft,"view_bins":view}))?;
    let cfg = serde_json::to_vec(&json!({
        "boot_id":c.boot_id,"output":c.output.join("demod-spectrum"),"command":"rate",
        "k":k,"mode":mode,"peak":grid["spectrum_detector"]=="peak","seconds":1.0,"warmup":0.0,"batch_limit":64,
        "stream_config":stream_config,"tx_active":tx_active,"batch_sync":true,"udp":c.view.to_string(),
        "trace_limit":0,"send_after_release":true,
    }))?;
    let cancel_video = AtomicBool::new(false);
    let video_finished = AtomicBool::new(false);
    let (ready, start_video) = std::sync::mpsc::sync_channel::<Value>(1);
    let state_with_spectrum = RefCell::new(state.clone());
    let result = std::thread::scope(|scope| {
        let cancel = &cancel_video;
        let finished = &video_finished;
        let video = scope.spawn(move || {
            let result = (|| {
                let Ok(actual) = start_video.recv() else { return Ok(()); };
                let regs = Registers::open()?;
                continuous(c, shared, &actual, id, &regs, profile, cancel)
            })();
            finished.store(true, Ordering::Release);
            result
        });
        let started = |header: crate::system_reader::Header, execution: crate::system_reader::Execution| {
            let mut state = state_with_spectrum.borrow_mut();
            let bins = crate::system_reader::view_bins(header);
            state["epoch"] = json!(execution.anchor);
            state["dma_run"] = json!(execution.run);
            state["view_bins"] = json!(bins);
            let spectrum = state["spectrum"].as_object_mut().unwrap();
            spectrum.extend([
                ("generation".into(), json!(generation)), ("version".into(), json!(version)),
                ("map_epoch".into(), json!(execution.anchor)), ("scan".into(), json!(false)),
                ("k".into(), k.clone()), ("fs_hz".into(), json!(FS)),
                ("flags".into(), json!(header.flags)), ("fft_bins".into(), json!(header.bins)),
                ("view_bins".into(), json!(bins)), ("view_power_revision".into(), json!(2)),
                ("TX_OFF".into(), json!(!tx_active)),
            ]);
            let _ = ready.send(state.clone());
        };
        let reader = crate::system_reader::run_controlled(&cfg, &crate::system_reader::RunControl {
            generation, version, stop: &shared.switch, started: &started,
            trace: &|stage| trace_control(stage, || json!({"id":id,"generation":generation})),
            route: &|_, _, _, _, _| Ok(None), route_released: &|_, _, _| {},
            check: &|| {
                if video_finished.load(Ordering::Acquire) && !shared.switch.load(Ordering::Acquire) {
                    return Err("real8 worker stopped".into());
                }
                Ok(())
            },
            progress: &|sample| {
                let mut inbox = shared.inbox.lock().unwrap();
                if progress_matches(&sample, &inbox.actual) {
                    inbox.last_progress = sample.clone();
                    inbox.progress = sample;
                }
            },
            stalled: &|_| {},
            bench_stall: false, bench_stall_after_next: false, sweep: None, next: None, roi: None,
        });
        cancel_video.store(true, Ordering::Release);
        drop(ready);
        let video = video.join().map_err(|_| anyhow!("real8 worker panicked"))?;
        video?;
        reader.map_err(anyhow::Error::msg)
    });
    *state = state_with_spectrum.into_inner();
    result
}

fn stop_video_reader(child: &mut std::process::Child) -> Result<()> {
    // iio_readdev must be allowed to cancel and destroy its libiio buffer.
    // SIGKILL is a bounded fallback, never evidence that the buffer drained.
    for (signal, grace_ms) in [(libc::SIGINT, 350), (libc::SIGKILL, 150)] {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        unsafe { libc::kill(child.id() as libc::pid_t, signal); }
        let deadline = Instant::now() + Duration::from_millis(grace_ms);
        while Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(3));
        }
    }
    bail!("IIO real8 reader did not exit")
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
    let mut calibration = calibration::Cache::default();
    let mut resume_scan: Option<Value> = None;
    let mut restart: Option<Value> = None;
    let mut retried_id = Value::Null;
    let mut bench_stall = std::env::var_os("OBS_CONTROL_TEST_PROGRESS").is_some_and(|v|v=="1");
    let bench_stall_after_next = std::env::var_os("OBS_CONTROL_TEST_NEXT_PROGRESS").is_some_and(|v|v=="1");
    while !shared.shutdown.load(Ordering::Acquire) {
        let cmd = {
            let mut q = shared.inbox.lock().unwrap();
            if q.next.is_none() && restart.is_none() && !shared.shutdown.load(Ordering::Acquire) {
                q = shared
                    .wake
                    .wait_timeout(q, Duration::from_millis(200))
                    .unwrap()
                    .0;
            }
            let queued = q.next.take();
            if queued.is_some() { q.received_ns = q.queued_ns; }
            let retry = restart.take();
            queued.or(retry).map(|cmd| {
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
        if let Some(reason) = tx_conflict(&cmd, &state, manual && c.external_mixer.is_none()) {
            let mut q = shared.inbox.lock().unwrap();
            q.pending = None;
            q.failed_id = id.clone();
            q.command_timing = json!({"id":id,"received_ns":q.received_ns.to_string(),
                "completed_ns":clock_ns().to_string(),"rejected":true,"reason":reason});
            continue;
        }
        let before = state.clone();
        {
            let mut q = shared.inbox.lock().unwrap();
            q.phase = "calculating";
            q.progress = Value::Null;
            q.actual["running"] = json!(false);
            q.actual["spectrum"] = Value::Null;
        }
        let op = cmd["op"].as_str().unwrap();
        let mut pending = state.clone();
        let previous_mode = pending["mode"].clone();
        let mut request = cmd.clone();
        request.as_object_mut().unwrap().remove("id");
        let generation = clock_ns() / 1000;
        pending["generation"] = json!(generation);
        trace_control("worker_start", ||json!({"id":id,"generation":generation,"op":op,
            "received_ns":shared.inbox.lock().unwrap().received_ns}));
        pending["spectrum"] = Value::Null;
        pending["view_bins"] = Value::Null;
        pending["epoch"] = json!(0);
        pending["rx_lo_hz"] = Value::Null;
        pending["if_hz"] = Value::Null;
        pending["ddc_hz"] = Value::Null;
        pending["ddc_fs_hz"] = Value::Null;
        pending["video_format"] = Value::Null;
        pending["waiting"] = Value::Null;
        pending["guard_tx"] = Value::Null;
        pending["bank_apply"] = Value::Null;
        pending["prepared_next_version"] = Value::Null;
        pending["failed_plan"] = Value::Null;
        pending["field_watch"] = Value::Null;
        let (radio_lock, lock_error) = if manual && pending["xtg1_wait_ticks"].is_number()
            && (matches!(op,"antennas"|"scan"|"alternative"|"cancel_alternative")
                || op == "return" && pending["return_mode"] == "scan") {
            match carrier.radio_lock(&shared.switch) {
                Ok(file) => (Some(file), None),
                Err(error) => (None, Some(error)),
            }
        } else {(None,None)};
        let config_result = (|| {
            if let Some(error) = lock_error {return Err(error);}
            if pending["pl_timing_only"] == true {
                ensure!(op == "antennas" || op == "scan" && cmd["planner_version"] == 2,
                        "PL timing fixture accepts v2 scan commands only");
            }
            ensure!(
                previous_mode != "video" || matches!(op, "return" | "tx" | "scan" | "video"),
                "close video before changing mode"
            );
            match op {
                "antennas" => {
                    ensure!(pending["catalog_policy"] == true, "antenna catalog disabled");
                    if carrier.on() { pending["tx"] = carrier.off()?; }
                    pending["mode"] = json!("scan");
                    pending["scan"]["run"] = json!(false);
                    pending["scan"]["antenna_bands"] = request["antenna_bands"].clone();
                    pending["scan"]["mixer"] = pending["catalog_mixer"].clone();
                    pending["scan_bounds"] = json!(scan_bounds(&c,&parse_antenna_bands(&request["antenna_bands"])?)?);
                    let (next, info) = calibration.catalog(&pending,&shared)?;
                    let tmp = c.output.join("antennas.json.tmp");
                    fs::write(&tmp, serde_json::to_vec(&request["antenna_bands"])?)?;
                    fs::rename(tmp,c.output.join("antennas.json"))?;
                    calibration = next;
                    pending["catalog_antennas"] = request["antenna_bands"].clone();
                    pending["catalog"] = info;
                    pending["running"] = json!(false);
                    return Ok(None);
                }
                "alternative" => {
                    ensure!(
                        before["generation"] == cmd["source_generation"],
                        "stale source generation"
                    );
                    let position = cmd["source_position"].as_u64().unwrap() as usize;
                    let p = &before["spectrum"]["positions"][position];
                    resume_scan = Some(before["scan"].clone());
                    pending["alternative"] = json!({"request_id":id,"source_generation":before["generation"],
                        "source_position":position,"rf_hz":[cmd["lo_hz"],cmd["hi_hz"]],
                        "excluded_tx_pll_hz":p["tx_pll_hz"],"excluded_rx_lo_hz":p["rx_lo_hz"]});
                    pending["scan"]["ranges"] = json!([{"lo_hz":cmd["lo_hz"],"hi_hz":cmd["hi_hz"],"enabled":true,
                        "path":if p["mixer"]==true {"mixer"}else{"direct"}}]);
                    pending["scan"]["antenna_bands"] = json!(BTreeMap::from([(
                        p["antenna"].as_u64().unwrap(),
                        [[
                            cmd["lo_hz"].as_u64().unwrap(),
                            cmd["hi_hz"].as_u64().unwrap()
                        ]]
                    )]));
                }
                "cancel_alternative" | "return"
                    if resume_scan.is_some() && previous_mode == "scan" =>
                {
                    pending["scan"] = resume_scan.as_ref().unwrap().clone();
                    pending["alternative"] = Value::Null;
                }
                "scan_ranges" => {
                    ensure!(previous_mode == "analysis", "scan_ranges requires analysis");
                    pending["scan"]["ranges"] = request["ranges"].clone();
                }
                "scan" | "analysis" => {
                    if request.get("spectrum_detector").is_none() {
                        if let Some(saved) = pending[op].get("spectrum_detector") {
                            request["spectrum_detector"] = saved.clone();
                        }
                    }
                    if request.get("view_bins").is_none() {
                        let previous=pending[op]["view_bins"].as_u64().unwrap_or(512);
                        let fft=request["fft_bins"].as_u64().unwrap_or(4096);
                        request["view_bins"] = if [0,fft/8,fft/4,fft/2,fft].contains(&previous) {
                            previous
                        } else {512}.into();
                    }
                    resume_scan = None;
                    pending["alternative"] = Value::Null;
                    if op == "scan" && carrier.on() {
                        pending["tx"] = carrier.off()?;
                    }
                    if previous_mode != op {
                        if op == "scan" {
                            if previous_mode == "analysis" || pending["return_mode"] == "analysis" {
                                pending["analysis_route"] = pending["route"].clone();
                            }
                            pending["route"] = json!({"kind":"direct","antenna":pending["scan_antenna"],"multiplier":1,"side":1});
                        } else {
                            pending["route"] = pending["analysis_route"].clone();
                        }
                    }
                    if op == "scan" {
                        pending["video"] = Value::Null;
                        pending["return_mode"] = Value::Null;
                        if request.get("gain_db").is_some() {
                            pending["scan_rx"] = json!({"agc":"manual","gain_db":request["gain_db"]});
                        }
                        if request["planner_version"] == 2 && request.get("tx_attenuation_mdb").is_none() {
                            if let Some(saved) = pending["scan"].get("tx_attenuation_mdb") {
                                request["tx_attenuation_mdb"] = saved.clone();
                            }
                        }
                        if pending["catalog_policy"] == true {
                            ensure!(request["run"] != true || !pending["catalog"].is_null(),
                                    "antenna catalog not ready");
                            if request.get("antenna_bands").is_none() {
                                request["antenna_bands"] = pending["catalog_antennas"].clone();
                            }
                            if request.get("mixer").is_none() {
                                request["mixer"] = pending["catalog_mixer"].clone();
                            }
                        }
                        if request.get("antenna_bands").is_none()
                            && pending["scan"].get("antenna_bands").is_some()
                        {
                            request["antenna_bands"] = pending["scan"]["antenna_bands"].clone();
                        }
                        pending["scan_bounds"] = json!(scan_bounds(
                            &c,
                            &parse_antenna_bands(&request["antenna_bands"])?
                        )?);
                    }
                    pending["mode"] = json!(op);
                    pending[op] = request;
                }
                "video" => {
                    if previous_mode != "video" {
                        pending["return_mode"] = previous_mode.clone();
                    }
                    pending["mode"] = json!("video");
                    pending["video"] = request;
                }
                "return" => {
                    ensure!(previous_mode == "video", "no active video");
                    pending["mode"] = pending["return_mode"].take();
                    pending["video"] = Value::Null;
                    if pending["mode"] == "scan" && carrier.on() {
                        pending["tx"] = carrier.off()?;
                    }
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
                                if request["kind"] == "mixer" {"manual_mixer"} else {"manual_direct"},
                            )
                            .map_err(|e| anyhow!(e))?
                            .is_some()
                            {
                                break;
                            }
                            ensure!(!shared.shutdown.load(Ordering::Acquire) &&
                                !shared.switch.load(Ordering::Acquire), "route cancelled");
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
                    let q = shared.inbox.lock().unwrap();
                    pending["route_confirmation"] = if q.route_last["generation"] == generation {
                        q.route_last.clone()
                    } else {
                        Value::Null
                    };
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
                }
                _ => bail!("observation already finished"),
            };
            if pending["mode"] == "scan" {
                pending["map_version"] = json!((generation % 0x7fff_fffd) + 1);
            }
            let mode = pending["mode"].as_str().unwrap().to_owned();
            pending["k"] = if mode == "video" {
                pending["video"]["spectrum_k"].clone()
            } else {
                pending[&mode]["k"].clone()
            };
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
                ensure!(mode != "video" || pending["video"].get("spectrum_k").is_some(), "mixer LO off");
                pending["running"] = json!(false);
                pending["waiting"] = json!("mixer LO off");
                return Ok(None);
            }
            if mode == "video" {
                let profile=video_profile::Profile::parse(&pending["video"],regs.read(0x50)==0x354e424f)?;
                pending["ddc_fs_hz"] = json!(profile.rate);
                pending["video_format"] = profile.actual();
            }
            if mode == "scan" {
                if pending["running"] != true {
                    if op == "scan" && cmd.get("gain_db").is_some() {
                        pending["rx_gain_db"] = json!(apply_gain(&pending["scan_rx"])?);
                    }
                    return Ok(None);
                }
                let (bytes, spectrum) = if pending["scan"]["planner_version"] == 2 {
                    calibration.prepare(&pending, &shared)?
                } else {
                    plan(&c, &template, &pending, op == "route")?
                };
                pending["rx_gain_db"] = json!(apply_gain(&pending["scan_rx"])?);
                if pending["running"] == true {
                    pending["spectrum"] = spectrum;
                }
                if word(&bytes, bytes.len() - 160) == 1 && word(&bytes, bytes.len() - 156) == 0 {
                    pending["waiting"] = json!("TX profiles loaded, mixer RF off");
                }
                Ok::<_, anyhow::Error>(Some(bytes))
            } else {
                tune(&mut pending)?;
                Ok(None)
            }
        })();
        trace_control("configured", ||json!({"id":id,"generation":generation,"ok":config_result.is_ok()}));
        let mut guard_taken = false;
        let mut guard_rf = false;
        let mut managed_tx = false;
        let actual = RefCell::new(pending);
        let applied_next = RefCell::new(None);
        let stall_sample = RefCell::new(Value::Null);
        let watch = RefCell::new(None);
        let resident = RefCell::new(None);
        let result = (|| {
            let mut setup = config_result?;
            let mode = actual.borrow()["mode"].as_str().unwrap().to_owned();
            if actual.borrow()["running"] != true {
                receipt(&shared, &actual.borrow(), id, None);
                return Ok(());
            }
            if mode == "video" {
                let _radio = if c.external_mixer.is_none() {
                    // The stock lease daemon uses the same lock for its legacy
                    // TX and DDC writer. Keep it excluded throughout capture.
                    let radio = carrier.radio_lock(&shared.switch)?;
                    if carrier.on() {
                        let confirmed = carrier.take_process_ownership(&radio)?;
                        actual.borrow_mut()["tx"] = confirmed;
                    }
                    Some(radio)
                } else {
                    None
                };
                let mut a=actual.borrow_mut();
                let profile=video_profile::Profile::parse(&a["video"],regs.read(0x50)==0x354e424f)?;
                if a["video"].get("spectrum_k").is_some() {
                    return continuous_with_spectrum(&c, &shared, &mut a, id, &profile);
                }
                return continuous(&c, &shared, &a, id, &regs, &profile, &shared.switch);
            }
            let mut sweep = None;
            let mut next_spectrum = None;
            let mut next_sweep = None;
            let mut field_directory = None;
            let targets = if mode=="scan" {resident_field::targets(&actual.borrow()["scan"]["field_targets"])?}
                else {Vec::new()};
            let roi_only = mode == "scan" && actual.borrow()["scan"].get("roi").is_some();
            if let Some(setup) = setup.as_mut() {
                let mut a = actual.borrow_mut();
                a["spectrum"]["overview_enabled"] = json!(!roi_only);
                if a["spectrum"]["sweep_wire_version"] == 1 {
                    let delivery = a["scan"]["view_delivery"].as_str()
                        .or_else(||c.scan["view_delivery"].as_str()).unwrap_or("stream").to_owned();
                    a["spectrum"]["version"] = json!(word(setup,16));
                    sweep = crate::system_reader::SweepSpec::from_descriptor(&a["spectrum"],&delivery)
                        .map_err(|e|anyhow!(e))?;
                    a["spectrum"]["view_delivery"] = json!(delivery);
                    if let Some(mut second) = a["spectrum"].as_object_mut().unwrap().remove("prepared_next") {
                        ensure!(c.field_preview.is_none(),"NEXT cannot replace an active FIELD watch");
                        next_sweep = crate::system_reader::SweepSpec::from_descriptor(&second,&delivery)
                            .map_err(|e|anyhow!(e))?;
                        second["view_delivery"] = json!(delivery);
                        second["overview_enabled"] = json!(!roi_only);
                        a["prepared_next_version"] = second["version"].clone();
                        next_spectrum = Some(second);
                    }
                }
                if !targets.is_empty() {
                    ensure!(c.resident_field.is_some() && regs.read(0x350)==0x314d4646
                        && a["spectrum"]["sweep_wire_version"]==1,
                        "resident FIELD requires FLD2/FFM1 and the physical A/B map");
                    let directory=c.output.join(format!("field-{generation}"));
                    fs::create_dir(&directory)?;
                    let joint_rows = regs.read(0x50) == 0x354e424f && matches!(regs.read(0xe4), 1 | 2)
                        && regs.read(0xe0) & 0x100 != 0;
                    let stride=(setup.len()-200)/3;
                    let count=word(setup,8) as usize;
                    let start=40+stride;
                    ensure!(count*32<=stride,"FIELD active map bounds");
                    let plan=resident_field::Plan::compile(&mut setup[start..start+count*32],
                        &a["spectrum"],&targets,generation,joint_rows)?;
                    a["field_watch"]=plan.save(&directory)?;
                    if let Some(desc)=next_spectrum.as_ref() {
                        let count=word(setup,12) as usize;
                        let start=40+2*stride;
                        ensure!(count*32<=stride,"FIELD NEXT map bounds");
                        let plan=resident_field::Plan::compile(&mut setup[start..start+count*32],
                            desc,&targets,generation+1,joint_rows)?;
                        a["field_watch"]["next"]=plan.save(&directory)?;
                    }
                    field_directory=Some(directory);
                }
                drop(a);
                managed_tx = word(setup, setup.len() - 4) & 1 != 0;
                let expected_control = if managed_tx {31} else {3 | (word(setup, setup.len() - 160) << 2)};
                guard_rf = word(setup, setup.len() - 156) != 0;
                if actual.borrow()["scan"]["planner_version"] == 2 {
                    let required = c.scan["xtg1_wait_ticks"].is_number()
                        || c.scan["path_ack_version"] == 1 || actual.borrow()["scan"]["path_ack_version"] == 1;
                    let first_mixer = if c.scan["xtg1_wait_ticks"].is_number() {
                        actual.borrow()["spectrum"]["positions"][0]["mixer"] == true
                    } else { guard_rf };
                    let path = if first_mixer {"mixer"} else {"direct"};
                    let mut confirmation = json!({"required_path":path,"confirmed_by_client":false});
                    if required {
                        shared.inbox.lock().unwrap().phase = "waiting_path_ack";
                        let antenna = actual.borrow()["spectrum"]["positions"][0]["antenna"]
                            .as_u64().ok_or_else(||anyhow!("first antenna missing"))? as u32;
                        let version = word(setup, 16);
                        let mut socket = None;
                        loop {
                            ensure!(!shared.switch.load(Ordering::Acquire) && !shared.shutdown.load(Ordering::Acquire),
                                "initial path confirmation cancelled");
                            let ack = acknowledge(&mut socket, &c, &shared, generation, 0, 0, version, 0, antenna,
                                if first_mixer {"startup_mixer"} else {"startup_direct"}).map_err(|e|anyhow!(e))?;
                            if ack.is_some() { break; }
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        confirmation = json!({"required_path":path,"confirmed_by_client":true,
                            "boot_id":c.boot_id,"generation":generation,"version":version,"antenna":antenna,
                            "ack_arm_ns":clock_ns().to_string()});
                    }
                    actual.borrow_mut()["spectrum"]["path_confirmation"] = confirmation;
                }
                let mut guarded_pll = None;
                if guard_rf {
                    let cap = guard_cmd(&guard_fd, 0x4605)?;
                    ensure!(
                        cap[114] == 1 && (!managed_tx || cap[117] == XTG1),
                        "guard with RF lease required"
                    );
                    let pll = actual.borrow()["spectrum"]["first_tx_pll_hz"]
                        .as_u64()
                        .ok_or_else(|| anyhow!("first TX PLL"))?;
                    ensure!(!managed_tx || cap[125] == 1, "managed scan needs cancellable TAKE_RF guard");
                    guarded_pll = Some(pll);
                    shared.inbox.lock().unwrap().operation = json!({"kind":"ps_preparation","command_id":id,
                        "generation":generation,"pll_hz":pll,"started_ns":clock_ns().to_string()});
                    trace_radio(id,generation,|| carrier.prepare_guarded(pll, &shared.switch, |stage| {
                        trace_control(stage, ||json!({"id":id,"generation":generation}));
                        shared.inbox.lock().unwrap().operation["stage"] = json!(stage);
                    })).inspect_err(|e|{
                        shared.inbox.lock().unwrap().operation["primary_error"]=json!(format!("{e:#}"));
                    })?;
                    trace_control("carrier_ready", ||json!({"id":id,"generation":generation}));
                }
                shared.inbox.lock().unwrap().phase = "loading";
                let load_start = Instant::now();
                let load_cmd = if word(setup, 0) == 0x3847534f {
                    0x4608
                } else {
                    0x4601
                };
                let loaded = guard(&guard_fd, load_cmd, setup.as_mut_ptr().cast())?;
                trace_control("map_loaded", ||json!({"id":id,"generation":generation,"version":word(setup,16)}));
                let load_us = load_start.elapsed().as_micros() as u64;
                shared.inbox.lock().unwrap().phase = "applying";
                let take_start = Instant::now();
                if guard_rf {
                    ensure!(carrier.guarded_lease_live() && !shared.switch.load(Ordering::Acquire),
                        "lease expired or preparation cancelled before guard take");
                }
                let s = if let Some(pll) = guarded_pll.filter(|_| managed_tx) {
                    guard_take::take(&guard_fd, &shared, &mut carrier, pll, id, generation, &mut guard_taken)
                } else {
                    guard_taken = true;
                    guard_cmd(&guard_fd, if guard_rf { 0x4607 } else { 0x4602 })
                }
                    .inspect_err(|error| trace_control("take_failed", || {
                        let raw = guard_raw(&guard_fd);
                        json!({"id":id,"generation":generation,"error":error.to_string(),
                            "guard":raw.map(|s| [s[1],s[2],s[9],s[17],s[30],s[112],s[118]])})
                    }))?;
                trace_control("take_done", ||json!({"id":id,"generation":generation,"version":word(setup,16)}));
                ensure!(
                    s[1] == 2
                        && s[9] == expected_control
                        && s[112] == (!guard_rf || managed_tx) as u32
                        && (if managed_tx {s[113] == 89750}
                            else {!guard_rf || s[113] == word(setup, setup.len() - 152)}),
                    "guard take readback"
                );
                actual.borrow_mut()["bank_apply"] = json!({"load_us":load_us,"take_us":take_start.elapsed().as_micros() as u64,
                    "bytes_per_table":loaded[7],"loaded_map_version":word(setup,16)});
                if managed_tx {
                    actual.borrow_mut()["tx"] = json!({"on":null,"powerdown":null,"owner":"obs_fl_guard",
                        "carrier_source":"cyclic_fs_div_64","mode":"per_visit"});
                } else if guard_rf {
                    actual.borrow_mut()["tx"] = json!({"on":true,"powerdown":s[112],"attenuation_mdb":s[113],
                        "owner":"obs_fl_guard","carrier_source":"cyclic_fs_div_64"});
                }
                actual.borrow_mut()["guard_tx"] =
                    json!({"executor":s[9]&4!=0,"powerdown":s[112],"attenuation_mdb":s[113],
                        "readback_scope":"take","per_visit":managed_tx});
            }
            if let Some(directory)=field_directory.as_ref() {
                let mut renewed=Instant::now();
                let session=resident_field::Session::start(c.resident_field.as_ref().unwrap(),
                    directory,c.video,&regs,||{
                        ensure!(!shared.switch.load(Ordering::Acquire),"FFM1 startup cancelled");
                        if guard_rf && renewed.elapsed()>=Duration::from_millis(80) {
                            guard_cmd(&guard_fd,0x4606)?;
                            renewed=Instant::now();
                        }
                        Ok(())
                    })?;
                *resident.borrow_mut()=Some(session);
            }
            let scan = mode == "scan";
            let tx_active = actual.borrow()["tx"]["on"] == true;
            let work = c.output.join("reader");
            let k = actual.borrow()["k"].clone();
            let settings=actual.borrow()[&mode].clone();
            let mode_index = view_mode(&settings)?;
            let obn5=regs.read(0x50)==0x354e424f;
            let view_only = scan && c.bench_obn3_view_only;
            let stream_config=if obn5 {
                Some(match settings["fft_bins"].as_u64().unwrap_or(4096) {4096=>0,2048=>2,1024=>4,_=>bail!("OBN5 FFT length")})
            } else {view_only.then_some(0)};
            let cfg = json!({"boot_id":c.boot_id,"output":work,"command":"rate","k":k,"mode":mode_index,
                "peak":settings["spectrum_detector"]=="peak","previous_mode":if obn5{0}else{mode_index},
                "seconds":1.0,"warmup":0.0,"shift544":c.bench_split544,"bench_obn3_layout":c.bench_obn3_layout,"settling":0,"batch_limit":64,"direct":c.bench_split544,"batch_sync":true,
                "neon":true,"hot":scan,"scan":scan,"tx_active":tx_active,
                "stream_config":stream_config,"hold_k":settings["hold_k"],"bench_obn3_view_only":view_only,
                "udp":(!roi_only).then(||c.view.to_string()),"trace_limit":0,"send_after_release":true,"managed_tx":managed_tx});
            let applied = std::cell::Cell::new(false);
            let initial_version = actual.borrow()["map_version"].as_u64().unwrap_or(0) as u32;
            let started = |h: crate::system_reader::Header, execution: crate::system_reader::Execution| {
                trace_control("first_view_arm", ||json!({"id":id,"generation":generation,
                    "epoch":h.epoch,"fft_tick":h.first_tick,"ordinal":h.ordinal}));
                let mut a = actual.borrow_mut();
                let version = json!(execution.version);
                let epoch = execution.anchor;
                a["epoch"] = json!(epoch);
                a["dma_run"] = json!(execution.run);
                a["view_bins"] = json!(crate::system_reader::view_bins(h));
                if !a["alternative"].is_null() {
                    a["alternative"]["generation"] = json!(generation);
                    a["alternative"]["first_epoch"] = json!(h.epoch);
                    a["alternative"]["first_observed_arm_ns"] = json!(clock_ns());
                }
                let s = a["spectrum"].as_object_mut().unwrap();
                for (k, v) in [
                    ("generation", json!(generation)),
                    ("map_epoch", json!(epoch)),
                    ("version", version),
                    ("k", k.clone()),
                    ("scan", json!(scan)),
                    ("fs_hz", json!(FS)),
                    ("view_power_revision", json!(if obn5 {regs.read(0xe4)} else {1})),
                    ("flags", json!(h.flags)),
                    ("fft_bins", json!(h.bins)),
                    (
                        "view_bins",
                        json!(crate::system_reader::view_bins(h)),
                    ),
                    ("TX_OFF", if managed_tx {Value::Null} else {json!(!tx_active)}),
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
                applied.set(true);
            };
            let stage_sent = std::cell::Cell::new(false);
            let next_epoch = std::cell::Cell::new(None);
            let next_gen = generation + 1;
            let next_version = next_spectrum.as_ref().and_then(|s|s["version"].as_u64()).map(|v|v as u32);
            let map_epoch = || u64::from(regs.read(0x238)) | (u64::from(regs.read(0x23c))<<32);
            let next_anchor = || {
                if !stage_sent.get() { return None; }
                if let Some(epoch) = next_epoch.get() { return Some(epoch); }
                if Some(regs.read(0x22c)) != next_version {return None;}
                let epoch = map_epoch();
                if epoch == 0 || Some(regs.read(0x22c)) != next_version || map_epoch()!=epoch {return None;}
                next_epoch.set(Some(epoch));
                Some(epoch)
            };
            let next_started = |h: crate::system_reader::Header, execution: crate::system_reader::Execution| {
                let request = shared.inbox.lock().unwrap().next_map_request.take();
                let Some((cmd,received)) = request else {return};
                let anchor = execution.anchor;
                *applied_next.borrow_mut() = Some(cmd.clone());
                let mut a = actual.borrow_mut();
                let mut s = next_spectrum.as_ref().unwrap().clone();
                for (key,value) in [("generation",json!(next_gen)),("map_epoch",json!(anchor)),
                    ("view_power_revision",json!(if obn5 {regs.read(0xe4)} else {1})),
                    ("k",k.clone()),("scan",json!(true)),("fs_hz",json!(FS)),("flags",json!(h.flags)),
                    ("fft_bins",json!(h.bins)),("view_bins",json!(crate::system_reader::view_bins(h))),("TX_OFF",Value::Null)] {
                    s[key]=value;
                }
                a["generation"]=json!(next_gen);
                a["view_bins"]=s["view_bins"].clone();
                a["map_version"]=s["version"].clone();
                a["epoch"]=json!(anchor);
                a["dma_run"]=json!(execution.run);
                a["scan"]["ranges"]=s["requested_ranges"].clone();
                a["scan"].as_object_mut().unwrap().remove("next_ranges");
                if let Some(watch)=a["field_watch"].get_mut("next") {
                    a["field_watch"]=watch.take();
                }
                a["spectrum"]=s;
                a["prepared_next_version"]=Value::Null;
                a["tx"]["on"]=Value::Null;
                a["tx"]["visit"]=Value::Null;
                shared.inbox.lock().unwrap().received_ns=received;
                receipt(&shared,&a,&cmd["id"],None);
                trace_control("next_first_view",||json!({"id":cmd["id"],"generation":next_gen,
                    "version":next_version,"anchor":anchor,"epoch":h.epoch,"fft_tick":h.first_tick,"ordinal":h.ordinal}));
            };
            let route = RefCell::new(None);
            let route_fn = |id, epoch, version, position, antenna| {
                let anchor = map_epoch();
                let a = actual.borrow();
                let (desc, context) = if stage_sent.get() && next_version == Some(version) {
                    (next_spectrum.as_ref().unwrap(),next_gen)
                } else { (&a["spectrum"],generation) };
                let count = desc["positions"]
                    .as_array()
                    .map_or(0, Vec::len);
                let marker_position = if count > 0 && epoch >= anchor {
                    ((epoch - anchor) % count as u64) as u32
                } else {
                    position
                };
                let marker = &desc["positions"][marker_position as usize];
                let source = match marker["marker_path"].as_str() {
                    Some("mixer") => "visit_mixer",
                    Some("direct") => "visit_direct",
                    _ if a["scan"]["planner_version"] == 2 => "PLv2",
                    _ => "PL",
                };
                acknowledge(
                    &mut route.borrow_mut(),
                    &c,
                    &shared,
                    context,
                    id,
                    epoch,
                    version,
                    marker_position,
                    antenna,
                    source,
                )
            };
            let last_renew = std::cell::Cell::new(Instant::now());
            let roi_request = || {
                let q = shared.inbox.lock().unwrap();
                if !applied.get() && (roi_only || q.actual["scan"].get("roi").is_some()) { None } else { q.roi }
            };
            let roi=if let Some(peer)=c.roi.or(c.bench_roi).filter(|_| scan && (obn5 || !c.bench_full_compact_tcp || mode_index == 0)) {
                let a=actual.borrow();
                let initial=crate::system_reader::RoiMap::from_descriptor(&a["spectrum"],initial_version,generation)
                    .map_err(anyhow::Error::msg)?;
                let next=next_spectrum.as_ref().zip(next_version).map(|(desc,version)|
                    crate::system_reader::RoiMap::from_descriptor(desc,version,next_gen)).transpose()
                    .map_err(anyhow::Error::msg)?;
                Some(crate::system_reader::RoiControl{peer,initial,next,request:&roi_request,
                    full_scan:c.bench_full_compact_tcp, production:c.roi.is_some()})
            }else{None};
            let q = crate::system_reader::RunControl {
                generation,
                version:initial_version,
                stop: &shared.switch,
                started: &started,
                trace: &|stage| trace_control(stage, ||json!({"id":id,"generation":generation})),
                route: &route_fn,
                route_released: &|marker,epoch,version| {
                    let mut q = shared.inbox.lock().unwrap();
                    if q.route_last["marker_id"] == marker && q.route_last["epoch"] == epoch
                        && q.route_last["version"] == version {
                        q.route_last["pl_continue_arm_ns"] = json!(clock_ns().to_string());
                    }
                },
                progress: &|sample| {
                    let mut q = shared.inbox.lock().unwrap();
                    if progress_matches(&sample,&q.actual) {
                        q.last_progress = sample.clone();
                        q.progress = sample;
                    } else {
                        q.progress = Value::Null;
                    }
                },
                stalled: &|sample| *stall_sample.borrow_mut()=sample,
                bench_stall: scan && std::mem::take(&mut bench_stall),
                bench_stall_after_next,
                roi,
                sweep: sweep.as_ref(),
                next: next_sweep.as_ref().zip(next_version).map(|(s,version)|crate::system_reader::NextView {
                    generation:next_gen,version,sweep:s,anchor:&next_anchor,started:&next_started,
                }),
                check: &|| {
                    if let Some(note)=resident.borrow_mut().as_mut().and_then(|session| session.health_note(&regs)) {
                        actual.borrow_mut()["field_watch"]["error"]=json!(&note);
                        shared.inbox.lock().unwrap().actual["field_watch"]["error"]=json!(note);
                    }
                    if let Some(note) = watch
                        .borrow_mut()
                        .as_mut()
                        .and_then(field::Watch::exit_note)
                    {
                        shared.inbox.lock().unwrap().actual["field_watch"]["error"] = json!(note);
                    }
                    if guard_rf && last_renew.get().elapsed() >= Duration::from_millis(80) {
                        if !carrier.guarded_lease_live() || !carrier.buffer_live() {
                            return Err("guarded TX lease or buffer lost".into());
                        }
                        let s = guard_cmd(&guard_fd, 0x4606).map_err(|e| {
                            if managed_tx && let Some(raw) = guard_raw(&guard_fd) {
                                let mut a = actual.borrow_mut();
                                a["guard_tx"]["failure"] = gate_receipt(&raw,&a["spectrum"]);
                                a["guard_tx"]["failure"]["driver_state"] = json!(raw[1]);
                                a["guard_tx"]["failure"]["fld_errors"] = json!(raw[30]);
                                a["tx"]["on"] = Value::Null;
                                a["tx"]["visit"] = Value::Null;
                            }
                            e.to_string()
                        })?;
                        if managed_tx && applied.get() {
                            let mut a = actual.borrow_mut();
                            let gate = gate_receipt(&s, &a["spectrum"]);
                            a["tx"]["on"] = gate["on"].clone();
                            a["tx"]["visit"] = gate;
                            shared.inbox.lock().unwrap().actual["tx"] = a["tx"].clone();
                        }
                        last_renew.set(Instant::now());
                    } else if !guard_rf && tx_active && c.external_mixer.is_none() {
                        if (!carrier.process_owned() && !tx::lease_is_live())
                            || !carrier.buffer_live()
                            || tx::readback(FS / 64).map_err(|e| e.to_string())?["on"] != true
                        {
                            return Err("manual TX owner or carrier lost".into());
                        }
                    }
                    if next_version.is_some() && !stage_sent.get()
                        && !shared.switch.load(Ordering::Acquire) {
                        let request=shared.inbox.lock().unwrap().next_map_request.clone();
                        if let Some((cmd,_))=request {
                            match guard_cmd(&guard_fd,0x4603) {
                                Ok(_) => {
                                    stage_sent.set(true);
                                    trace_control("next_staged",||json!({"id":cmd["id"],"generation":next_gen,"version":next_version}));
                                }
                                Err(err) => {
                                    let mut inbox=shared.inbox.lock().unwrap();
                                    inbox.next_map_request=None;
                                    inbox.failed_id=cmd["id"].clone();
                                    inbox.error=json!(err.to_string());
                                }
                            }
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
        let applied_next = applied_next.into_inner();
        let execution_cmd = applied_next.as_ref().unwrap_or(&cmd);
        let execution_id = &execution_cmd["id"];
        let execution_generation = actual.borrow()["generation"].clone();
        trace_control("mode_exit", ||json!({"id":execution_id,"generation":execution_generation,"ok":result.is_ok()}));
        let field_stop = watch
            .into_inner()
            .map_or(Ok(()), |field| field.finish(&regs));
        let resident_stop=resident.into_inner().map_or(Ok(()),|field| {
            let mut renewed=Instant::now();
            field.finish(&regs,||{
                if guard_rf && renewed.elapsed()>=Duration::from_millis(80) {
                    guard_cmd(&guard_fd,0x4606)?;
                    renewed=Instant::now();
                }
                Ok(())
            })
        });
        let result = result.and(field_stop).and(resident_stop);
        let stalled = result.as_ref().is_err_and(|e| e.to_string()==crate::system_reader::PROGRESS_LOST);
        if stalled {
            let mut q = shared.inbox.lock().unwrap();
            let sample = stall_sample.into_inner();
            if q.progress_failure["id"] == *execution_id {
                q.progress_failure["repeat_sample"] = sample;
                q.progress_failure["recovery"] = json!("returning");
            } else {
                q.progress_failure = json!({"id":execution_id,"generation":sample["generation"],
                    "primary_error":crate::system_reader::PROGRESS_LOST,"last_sample":sample,"recovery":"returning"});
            }
        }
        let result = if guard_taken {
            let return_start = Instant::now();
            {
                let mut q = shared.inbox.lock().unwrap();
                q.phase = if stalled {"recovering"} else {"returning"};
                q.actual["running"] = json!(false);
                q.actual["spectrum"] = Value::Null;
                q.progress = Value::Null;
            }
            let handback = (|| {
                let s = guard_cmd(&guard_fd, 0x4605)?;
                let s = if guard_rf && s[1] == 3 {
                    s
                } else {
                    guard_cmd(&guard_fd, 0x4604)?
                };
                trace_control("guard_return", ||json!({"id":id,"generation":generation,"state":s[1],"powerdown":s[112]}));
                ensure!(
                    s[1] == 3 && s[9] == 0 && s[112] == 1 && (!guard_rf || s[113] == 89750),
                    "guard return credential"
                );
                if guard_rf {
                    actual.borrow_mut()["tx"] = trace_radio(id,generation,||carrier.after_guard())?;
                    trace_control("carrier_closed", ||json!({"id":id,"generation":generation}));
                }
                Ok::<(), anyhow::Error>(())
            })();
            {
                let mut q = shared.inbox.lock().unwrap();
                let mut handback = json!({"us":return_start.elapsed().as_micros() as u64,
                    "execution_id":execution_id,"generation":execution_generation,
                    "error":handback.as_ref().err().map(|e|format!("{e:#}"))});
                if q.operation["generation"] == generation {
                    if managed_tx {handback["trace"] = guard_take::return_trace(&guard_fd,&q.operation);}
                    q.operation["return"] = handback.clone();
                }
                if stalled { q.progress_failure["handback"] = handback; }
            }
            match handback {
                Ok(()) => result,
                Err(e) => {
                    let error = match &result {
                        Ok(()) => e.to_string(),
                        Err(original) => format!("{original:#}; guard handback: {e:#}"),
                    };
                    let mut stopped = if applied_next.is_some(){actual.borrow().clone()}else{before};
                    stopped["running"] = json!(false);
                    stopped["spectrum"] = Value::Null;
                    stopped["generation"] = execution_generation.clone();
                    receipt(&shared, &stopped, execution_id, Some(error.clone()));
                    return Err(anyhow!(error));
                }
            }
        } else {
            result
        };
        {
            let mut inbox=shared.inbox.lock().unwrap();
            inbox.route_pending=Value::Null;
            if let Some((request,_))=inbox.next_map_request.take() {
                inbox.failed_id=request["id"].clone();
                inbox.error=json!("NEXT stopped before its first observation");
            }
        }
        if stalled {
            let may_restart = guard_taken && op=="scan" && cmd["run"]==true && *id!=retried_id
                && actual.borrow()["generation"]==generation && actual.borrow()["prepared_next_version"].is_null()
                && !shared.switch.load(Ordering::Acquire) && !shared.shutdown.load(Ordering::Acquire)
                && !STOP_RUNTIME.load(Ordering::Acquire) && (!guard_rf || carrier.guarded_lease_live());
            let mut q=shared.inbox.lock().unwrap();
            if may_restart && q.next.is_none() && !shared.switch.load(Ordering::Acquire) {
                q.phase="recovering";
                q.pending=Some(cmd.clone());
                q.progress_failure["recovery"]=json!("repreparing");
                retried_id=id.clone();
                restart=Some(cmd.clone());
                continue;
            }
            q.progress_failure["recovery"]=json!("exhausted_or_cancelled");
        }
        match result {
            Ok(()) => {
                state = actual.into_inner();
                if matches!(op, "return" | "cancel_alternative") && state["alternative"].is_null() {
                    resume_scan = None;
                }
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
                state = if applied_next.is_some(){actual.borrow().clone()}else{before};
                if op == "scan" || op == "alternative" {
                    state["failed_plan"] = generated::failure(&actual.borrow()["scan"], execution_cmd, e.to_string());
                }
                state["running"] = json!(false);
                state["spectrum"] = Value::Null;
                state["generation"] = execution_generation;
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
                receipt(&shared, &state, execution_id, Some(e.to_string()));
            }
        }
        drop(radio_lock);
    }
    Ok(())
}

/// Probe the actual binary table encoder on ARM without installing a table.
pub fn check_scan_tx_bank() -> Result<()> {
    let mut state = json!({"map_version":1,"pl_timing_only":true});
    let mut tiles = Vec::new();
    let mut tx = BTreeMap::new();
    let mut rx = BTreeMap::new();
    for i in 0..9u8 {
        let pll = 2_250_000_000 + u64::from(i) * 1_000_000;
        let lo = 6_100_000_000 + u64::from(i) * 8_000_000;
        state["scan"] = json!({"ranges":[{"enabled":true,"path":"mixer","lo_hz":lo,"hi_hz":lo+1_000_000}],
            "mixer":{"pll_range_hz":[pll,pll]}});
        let tile = generated::core_tiles(&state)?.pop().ok_or_else(|| anyhow!("fixture tile missing"))?;
        rx.insert(tile.rx,(tile.rx,[i;16]));
        tx.insert(pll,(pll,[i;16]));
        tiles.push(tile);
    }
    let last = *tx.keys().last().unwrap();
    let eight = tx.range(..last).map(|(k,v)|(*k,*v)).collect();
    let (bytes,descriptors) = generated::encode(&tiles[..8],&rx,&eight,
        [2_400_000_000,2_450_000_000],&state,128)?;
    let tail = bytes.len()-160;
    ensure!(word(&bytes,tail+8)==89_750 && descriptors["tx_attenuation_mdb"].is_null(), "muted TX bank");
    for (i,position) in descriptors["positions"].as_array().unwrap().iter().enumerate() {
        ensure!(position["tx_slot"]==i && bytes[tail+24+16*i..tail+40+16*i]==[i as u8;16],"TX slot {i}");
        for page in [40+4096,40+8192] {
            ensure!((word(&bytes,page+32*i+4)>>8)&7==i as u32,"TX row slot {i}");
        }
    }
    let error = generated::encode(&tiles,&rx,&tx,[2_400_000_000,2_450_000_000],&state,128)
        .expect_err("nine TX profiles must be rejected").to_string();
    ensure!(error.contains("9 TX") && error.contains("/8"),"unexpected bank error: {error}");
    state["pl_timing_only"] = json!(false);
    state["xtg1_wait_ticks"] = json!(256);
    let mut request = state["scan"].clone();
    request["op"] = json!("scan");
    request["planner_version"] = json!(2);
    request["k"] = json!(3);
    request["run"] = json!(false);
    request["tx_attenuation_mdb"] = json!(-250);
    ensure!(validate(&request).is_err(), "negative scan TX attenuation must fail");
    request["tx_attenuation_mdb"] = json!(0);
    validate(&request)?;
    for override_attenuation in [None, Some(0u32), Some(12_250)] {
        if let Some(attenuation) = override_attenuation {
            state["scan"]["tx_attenuation_mdb"] = json!(attenuation);
        }
        let attenuation = override_attenuation.unwrap_or(30_000);
        let (bytes,desc) = generated::encode(&tiles[..8],&rx,&eight,
            [2_400_000_000,2_450_000_000],&state,128)?;
        ensure!(word(&bytes,bytes.len()-152)==attenuation
                && desc["tx_attenuation_mdb"]==attenuation,"scan TX attenuation {attenuation}");
    }
    let [lo, hi] = generated::scan_rf_bounds()?;
    let (threshold, _) = generated::environment()?;
    let fixed = if lo + 200_000_000 < threshold && threshold + 200_000_000 < hi {
        let mut state = json!({"map_version":1,"generation":1,"catalog_policy":true,"grid_27":true,
            "xtg1_wait_ticks":256,"route":{"antenna":0},"scan":{"k":3,
            "mixer":{"pll_range_hz":[2_250_000_000u64,4_500_000_000u64]}}});
        generated::fixed_profile_request(&mut state)?;
        let core = generated::core_tiles(&state)?;
        let grid = generated::working_grid(&state,&core)?;
        let rx = grid.iter().map(|t| (t.rx,(t.rx,[0;16]))).collect::<calibration::Bank>();
        let tx = grid.iter().filter_map(|t| t.tx.map(|t| t.pll as u64))
            .map(|pll| (pll,(pll,[0;16]))).collect::<calibration::Bank>();
        let mut cases = Vec::new();
        for (upper, id) in [(threshold, "1"), (threshold - 123_456_789, "2")] {
            state["scan"]["antenna_bands"] = json!({"0":[[lo,upper]]});
            state["scan"]["antenna_bands"][id] = json!([[threshold - 200_000_000,hi]]);
            let visits = generated::retarget_fixed_catalog(&state,&grid,&rx,&tx)?;
            let used_rx = visits.iter().map(|t| (t.rx,rx[&t.rx])).collect::<calibration::Bank>();
            let used_tx = visits.iter().filter_map(|t| t.tx.map(|t| t.pll as u64))
                .map(|pll| (pll,tx[&pll])).collect::<calibration::Bank>();
            let (_, desc) = generated::encode(&visits,&used_rx,&used_tx,[2_400_000_000,2_450_000_000],&state,0)?;
            let positions = desc["positions"].as_array().unwrap();
            let antenna = id.parse::<u64>()?;
            ensure!(positions.iter().any(|p| p["marker_route"]==antenna && p["hold_boundary"]==true),
                "missing antenna ACK/hold in calibrated map");
            cases.push(json!({"antenna":antenna,"boundary_hz":upper,"visits":visits.len()}));
        }
        json!({"range_hz":[lo,hi],"rx_profiles":rx.len(),"tx_profiles":tx.len(),"cases":cases})
    } else { Value::Null };
    println!("{}",json!({"eight_slots":[0,1,2,3,4,5,6,7],"nine_error":error,
        "fixed_profiles":fixed,"installed":false}));
    Ok(())
}

/// Run the owner only on the prepared row99 RAM kit.
pub async fn run(path: &Path, listen: SocketAddr) -> Result<()> {
    let mut c: Config = serde_json::from_slice(&fs::read(path)?)?;
    if c.analysis.is_object() && c.analysis.get("k").is_none() {
        c.analysis["k"] = json!(DEFAULT_ANALYSIS_K);
    }
    if c.roi.is_some() && c.scan.is_object() && c.scan.get("fft_bins").is_none() && c.scan.get("view_bins").is_none() {
        c.scan["fft_bins"] = json!(4096);
        c.scan["view_bins"] = json!(2048);
    }
    ensure!(cfg!(feature="scan-stream-bench") ||
        (!c.bench_obn3_layout && !c.bench_obn3_view_only && c.bench_roi.is_none()),
        "RAM layout and ROI require the scan-stream-bench feature");
    ensure!(!c.bench_obn3_view_only || c.bench_roi.is_none(),
        "OBN3 view-only bench configuration excludes compact delivery");
    ensure!(!c.bench_full_compact_tcp || c.bench_roi.is_some(),"full compact TCP requires a bench destination");
    ensure!(c.roi.is_none() || c.bench_roi.is_none(), "configure one ROI destination");
    ensure!(c.roi.is_none_or(|peer| peer.is_ipv4()), "ROI destination must be IPv4");
    let catalog_policy = c.scan["planner_version"] == 2 && c.scan["catalog_on_boot"] != false
        && c.external_mixer.is_none();
    if let Some(preview) = &c.field_preview {
        preview.validate()?;
    }
    if let Some(field)=&c.resident_field {field.validate()?;}
    ensure!(c.resident_field.is_none() || c.field_preview.is_none(),"only one FIELD collector may be configured");
    if c.scan.get("antenna_bands").is_none() && !c.antenna_bands.is_empty() {
        c.scan["antenna_bands"] = json!(c.antenna_bands);
    }
    if catalog_policy {
        let saved = c.output.join("antennas.json");
        if saved.exists() { c.scan["antenna_bands"] = serde_json::from_slice(&fs::read(saved)?)?; }
        if c.scan["antenna_bands"].is_null() {
            c.scan["antenna_bands"] = json!({"0":[[50_000_000u64,7_000_000_000u64]],
                                             "1":[[4_000_000_000u64,13_000_000_000u64]]});
        }
        if c.scan["mixer"].is_null() {
            c.scan["mixer"] = json!({"pll_range_hz":[2_250_000_000u64,4_500_000_000u64]});
        }
    }
    validate(&c.scan)?;
    let initial_antenna_bands = parse_antenna_bands(&c.scan["antenna_bands"])?;
    let bounds = scan_bounds(&c, &initial_antenna_bands)?;
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
    let template = if c.setup.as_os_str().is_empty() && c.scan["planner_version"] == 2 {
        ensure!(
            c.positions.is_empty() && c.scan_tx.is_empty(),
            "generated catalog must start empty"
        );
        let mut blank = vec![0; 12488];
        put(&mut blank, 0, 0x3747534f);
        blank
    } else {
        fs::read(&c.setup)?
    };
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
            && (c.positions.len() <= 128
                && (!c.positions.is_empty() || c.scan["planner_version"] == 2))
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
    let obn5 = regs.read(0x50)==0x354e424f && matches!(regs.read(0xe4), 1 | 2) && regs.read(0xe0)&0x1ff==0x1ff;
    let obn3 = regs.read(0x50)==0x334e424f && regs.read(0xe4)==4 && regs.read(0xe0)&0x60==0x60;
    let stream002 = (obn3 || obn5) && regs.read(0x280)==0x32444c46 && regs.read(0x350)==0x314d4646;
    let legacy = regs.read(0x50) == 0x324e424f && regs.read(0xe4) == 3;
    ensure!(
        (stream002 || legacy) && regs.read(0x224) == 0x31444c46
            && Path::new("/dev/obs_cap2").exists(),
        "row99 CAP2 and compatible spectral ABI required"
    );
    ensure!(!stream002 || !c.bench_split544, "compact ABI requires separate DMA regions");
    ensure!(obn3 || !c.bench_obn3_layout,"OBN3 RAM layout requires OBN3");
    ensure!(obn3 || !c.bench_obn3_view_only,"OBN3 view-only bench configuration requires OBN3");
    check_spectrum_settings(&c.scan,obn5)?;
    check_spectrum_settings(&c.analysis,obn5)?;
    ensure!(!stream002 || c.field_preview.is_none(), "legacy FIELD1 collector requires OBN2");
    ensure!(stream002 || c.resident_field.is_none(), "resident FIELD collector requires OBN3/FLD2/FFM1");
    ensure!(stream002 || c.bench_roi.is_none(),"bench ROI requires OBN3");
    ensure!(obn5 || c.roi.is_none(),"production ROI requires OBN5");
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
    let scan_rx = if let Some(gain) = c.scan.get("gain_db") {
        json!({"agc":"manual","gain_db":gain})
    } else {
        json!({"agc":scan_agc,"gain_db":scan_gain})
    };
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
    let pl_timing_only = c.scan["bench_timing_only"] == true;
    ensure!(!pl_timing_only || (c.external_mixer.is_none() && c.field_preview.is_none() && c.resident_field.is_none()
            && c.scan["planner_version"] == 2), "PL timing fixture requires a v2 scan-only configuration");
    let guard_info = guard_cmd(&guard_fd, 0x4605)?;
    let demod_tx = manual && obn5 && regs.read(0xe4) == 2 && guard_info[126] == 1;
    let xtg1 = if c.scan["xtg1_wait_ticks"].is_null() { None } else {
        ensure!(guard_info[117] == XTG1 && guard_info[114] == 1 && c.scan["planner_version"] == 2
                && c.external_mixer.is_none(), "XTG1 opt-in requires compatible PL, RF guard and v2 scan");
        Some(int(&c.scan, "xtg1_wait_ticks", 0, 65535)?)
    };
    let mut initial = json!({"mode":initial_mode,"generation":clock_ns()/1000,"map_version":1,"epoch":0,"boot":c.boot_id,
        "pl_timing_only":pl_timing_only,
        "catalog_policy":catalog_policy,"catalog_antennas":c.scan["antenna_bands"],"catalog_mixer":c.scan["mixer"],
        "scan":c.scan,"analysis":c.analysis,"video":null,"return_mode":null,"running":false,"k":c.scan["k"],
        "fs_hz":FS,"ddc_fs_hz":null,"spectrum":null,"rx_lo_hz":null,"if_hz":null,"ddc_hz":null,
        "scan_rx":scan_rx,"rx_gain_db":scan_gain,
        "route":route,"analysis_route":route,"scan_antenna":0,"tx":tx_readback,"manual_tx_available":manual,
        "manual_demod_tx_available":demod_tx,
        "scan_bounds":bounds});
    initial["xtg1_wait_ticks"] = json!(xtg1);
    let grid_27 = match c.scan["sweep_geometry"].as_str() {
        Some("27/9") => true,
        Some("legacy") => false,
        None if c.scan["sweep_geometry"].is_null() => xtg1.is_some(),
        _ => bail!("sweep_geometry must be 27/9 or legacy"),
    };
    ensure!(!grid_27 || c.scan["planner_version"] == 2,"27/9 requires planner v2");
    initial["grid_27"] = json!(grid_27);
    let profiles = c
        .scan_tx
        .iter()
        .enumerate()
        .map(|(slot, p)| {
            json!({"slot":slot,"pll_hz":p.pll_hz,"tone_hz":p.tone(),
        "multiplier":p.multiplier,"antenna":p.antenna})
        })
        .collect::<Vec<_>>();
    let (threshold, mult) = generated::environment()?;
    let rf_bounds = generated::scan_rf_bounds()?;
    let rf_guard = guard_info[114] == 1;
    let view_modes: &[u32] = if obn5 { &[0,128,256,512,1024,2048,4096] } else if stream002 { &[0,512,1024,2048] } else { &[512,1024,2048] };
    let capabilities = json!({"tx_on":manual,"scan_gain":true,"view_bins":view_modes,"view_delivery_reads_full":obn3&&!c.bench_obn3_view_only,
        "spectrum_detector":obn5,"analysis_scan_ranges":true,
        "analysis_max_width_hz":if obn5{56_000_000}else{48_000_000},
        "roi":c.roi.is_some(),"roi_exact_bins":c.roi.is_some(),"scan_roi_only":c.roi.is_some(),
        "loop_grid_config":obn5&&c.roi.is_some(),
        "resident_field":c.resident_field.is_some(),"video_profiles":obn5,
        "video_spectrum":obn5 && regs.read(0xe4)==2,
        "video_manual_tx":demod_tx,
        "view_power_revision":if obn5 {regs.read(0xe4)} else {1},
        "spectrum_abi":if obn5 {"OBN5"} else if obn3 {"OBN3"} else {"OBN2"},
        "ring_layout":if obn5{"detected on START"}else if c.bench_obn3_layout{"obn3-ram-displaced"}else if c.bench_split544{"split544"}else{"native"},"ps_tx_process_owner":guard_info[126]==1,"mixer":c.external_mixer.is_none(),"finite_field":c.field_preview.is_some()||c.resident_field.is_some(),"bench_roi":c.bench_roi.is_some(),
        "scan_tx_profiles":profiles,"scan_tx_rf":false,"scan_tx_attenuation":rf_guard && manual,
        "planner_v2":{"direct":true,"guarded_rf":rf_guard && manual,
        "path_ack_version":1,"path_ack_required_by_config":c.scan["path_ack_version"]==1 || xtg1.is_some(),
        "xtg1":guard_info[117]==XTG1,"xtg1_opt_in":xtg1.is_some(),"visit_ack_version":4,
        "mixer_threshold_hz":threshold,"mixer_input_multiplier":mult,"scan_rf_bounds_hz":rf_bounds,
        "tx_model_pll_hz":[2250000000u64,2490000000u64]}});
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
        clock_session: format!("{}:{:x}",std::process::id(),clock_ns()),
        loop_config: (obn5 && c.roi.is_some()).then(|| Mutex::new(loop_grid::Store::load(&c.output))),
        inbox: Mutex::new(Inbox {
            phase: if catalog_policy {"accepted"} else {"stopped"},
            next: catalog_policy.then(||json!({"op":"antennas","id":format!("{:032x}",clock_ns()),
                                                "antenna_bands":c.scan["antenna_bands"]})),
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
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(||
                worker(c, shared.clone(), template, guard_fd, regs)))
                .unwrap_or_else(|_| Err(anyhow!("control worker panic")));
            if let Err(e) = &result {
                let mut q = shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
                q.actual["running"] = json!(false);
                q.actual["spectrum"] = Value::Null;
                q.progress = Value::Null;
                q.phase = "failed";
                q.take_cancel = None;
                q.route_pending = Value::Null;
                q.worker_error = Some(format!("{e:#}"));
                shared.inbox.clear_poison();
            }
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
            bench_split544: false,
            bench_obn3_layout: false,
            bench_obn3_view_only: false,
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
            field_preview: None,
            resident_field: None,
            bench_roi: None,
            roi: None,
            bench_full_compact_tcp: false,
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
