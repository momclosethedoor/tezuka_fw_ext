use super::*;
use std::{io, mem::MaybeUninit, sync::atomic::AtomicU32};

const SUBMIT: libc::c_ulong = 0x4609;
const CANCEL: libc::c_ulong = 0x460a;
const GET: libc::c_ulong = 0x460b;
const TAKE_ATTEMPTS: u8 = 3;
const POLL: Duration = Duration::from_millis(5);
const TPC2: u32 = 0x32435054;
const PENDING: u32 = 1;
const CLEANUP: u32 = 2;
const DONE: u32 = 3;

#[derive(Clone, Copy)]
#[repr(C)]
struct Event {
    step: u32,
    resources: u32,
    ns: u64,
}

#[repr(C)]
struct Report {
    id: u32,
    state: u32,
    entered: u32,
    completed: u32,
    primary: i32,
    recovery: i32,
    resources: u32,
    events: u32,
    fld_control: u32,
    mux_status: u32,
    dma_status: u32,
    gate_status: u32,
    capability: u32,
    tpc2: u32,
    queued_ns: u64,
    deadline_ns: u64,
    fault_ns: u64,
    done_ns: u64,
    trace: [Event; 64],
}

fn stage(n: u32) -> &'static str {
    const STAGES: [&str; 21] = ["unknown", "queued", "worker", "validate", "gate", "flush_work",
        "phy_mutex", "gpio", "mute", "fastlock", "spi_bus_lock", "spi_verify", "mux_request",
        "mux_ack", "enable", "stop_drain", "mux_return", "restore_fastlock", "restore_ccf",
        "confirm_mute", "release"];
    STAGES.get(n as usize).copied().unwrap_or("unknown")
}

fn ioctl(fd: &File, cmd: libc::c_ulong, arg: *mut libc::c_void) -> io::Result<()> {
    if unsafe { libc::ioctl(fd.as_raw_fd(), cmd, arg) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(super) fn cancel(fd: &File, mut cookie: u32) -> io::Result<()> {
    ioctl(fd, CANCEL, (&mut cookie as *mut u32).cast())
}

fn read(fd: &File, cookie: u32) -> Result<Report> {
    let mut report = MaybeUninit::<Report>::uninit();
    ioctl(fd, GET, report.as_mut_ptr().cast())?;
    let report = unsafe { report.assume_init() };
    ensure!(report.id == cookie && matches!(report.state, PENDING | CLEANUP | DONE),
        "TAKE_RF operation lost (expected {cookie}, received {})", report.id);
    Ok(report)
}

impl Report {
    fn snapshot(&self, trace: bool) -> Value {
        json!({"cookie":self.id,"state":match self.state {PENDING=>"pending",CLEANUP=>"cleanup",_=>"completed"},
            "started_stage":stage(self.entered),"completed_stage":stage(self.completed),
            "queued_ns":self.queued_ns.to_string(),"deadline_ns":self.deadline_ns.to_string(),
            "fault_ns":(self.fault_ns != 0).then(||self.fault_ns.to_string()),
            "done_ns":(self.done_ns != 0).then(||self.done_ns.to_string()),
            "primary_errno":(self.primary != 0).then_some(-self.primary),
            "cleanup_errno":(self.state==DONE).then_some(-self.recovery),
            "cleanup_us":(self.state==DONE && self.fault_ns != 0)
                .then(|| self.done_ns.saturating_sub(self.fault_ns)/1000),
            "resources":self.resources,
            "pl":{"fld_control":self.fld_control,"mux_status":self.mux_status,"dma_status":self.dma_status,
                "gate_status":self.gate_status,"diagnostic_id":self.capability,
                "tpc2_compare_07c":(self.capability==TPC2).then_some(self.tpc2)},
            "trace":trace.then(|| self.trace.iter().take(self.events as usize).map(|e|
                json!([stage(e.step & 0x7fff_ffff),e.step >> 31 != 0,e.resources,e.ns.to_string()]))
                .collect::<Vec<_>>())})
    }

    fn retryable(&self) -> bool {
        let errno = -self.primary;
        match self.entered {
            4..=6 | 10 | 12..=14 => errno == libc::ETIMEDOUT,
            8 | 9 | 11 => matches!(errno, libc::ETIMEDOUT | libc::EIO | libc::EAGAIN),
            _ => false,
        }
    }
}

fn cancelled(shared: &Shared, carrier: &tx::Carrier) -> bool {
    shared.switch.load(Ordering::Acquire) || shared.shutdown.load(Ordering::Acquire)
        || STOP_RUNTIME.load(Ordering::Acquire) || !carrier.guarded_lease_live()
}

/// Submit on the sole guard FD. A timeout can cancel activation, but only a
/// completed report and a confirmed safe PS state give permission to retry.
pub(super) fn take(
    fd: &File, shared: &Shared, carrier: &mut tx::Carrier, pll: u64, command: &Value,
    generation: u64, guard_taken: &mut bool,
) -> Result<[u32; 128]> {
    static COOKIE: AtomicU32 = AtomicU32::new(1);
    let mut attempts = Vec::with_capacity(TAKE_ATTEMPTS as usize);
    for attempt in 1..=TAKE_ATTEMPTS {
        let mut cookie = COOKIE.fetch_add(1, Ordering::Relaxed);
        ensure!(cookie != 0, "TAKE_RF operation id exhausted");
        let cancel_fd = fd.try_clone()?;
        {
            let mut q = shared.inbox.lock().unwrap();
            ensure!(!cancelled(shared, carrier), "TAKE_RF cancelled before submission");
            q.phase = "applying";
            ioctl(fd, SUBMIT, (&mut cookie as *mut u32).cast())?;
            q.take_cancel = Some((cancel_fd, cookie));
            q.operation = json!({"kind":"take_rf","command_id":command,"generation":generation,
                "attempt":attempt,"attempt_limit":TAKE_ATTEMPTS,"previous_attempts":attempts});
        }
        *guard_taken = true;
        let mut previous = None;
        let mut wait_expired_ns = None;
        let mut expiry_snapshot = None;
        let mut revoke_sent = false;
        let report = loop {
            let report = read(fd, cookie).inspect_err(|_| {
                let _ = cancel(fd, cookie);
                shared.inbox.lock().unwrap().take_cancel = None;
            })?;
            if report.state == PENDING && clock_ns() >= report.deadline_ns && wait_expired_ns.is_none() {
                wait_expired_ns = Some(clock_ns().to_string());
                expiry_snapshot = Some(report.snapshot(false));
            }
            if report.state == PENDING && !revoke_sent && cancelled(shared, carrier) {
                let _ = cancel(fd, cookie);
                revoke_sent = true;
            }
            let key = (report.state,report.entered,report.completed,wait_expired_ns.is_some());
            if previous != Some(key) {
                let mut q = shared.inbox.lock().unwrap();
                q.operation["current"] = report.snapshot(false);
                q.operation["wait_expired_ns"] = json!(wait_expired_ns);
                if report.state == CLEANUP || wait_expired_ns.is_some() {
                    q.phase = "recovering";
                }
                previous = Some(key);
            }
            if report.state == DONE { break report; }
            std::thread::sleep(POLL);
        };
        let mut snapshot = report.snapshot(true);
        snapshot["attempt"] = json!(attempt);
        snapshot["wait_expired_ns"] = json!(wait_expired_ns);
        snapshot["wait_expiry_snapshot"] = json!(expiry_snapshot);
        trace_control("take_attempt", || snapshot.clone());
        attempts.push(snapshot);
        {
            let mut q = shared.inbox.lock().unwrap();
            q.take_cancel = None;
            q.operation["attempts"] = json!(attempts);
        }
        if report.primary == 0 {
            ensure!(!cancelled(shared,carrier), "TAKE_RF cancelled at activation");
            return guard_cmd(fd, 0x4605);
        }
        let raw = guard_raw(fd).ok_or_else(|| anyhow!("TAKE_RF failed and guard state unavailable"))?;
        let safe = report.recovery == 0 && raw[9] == 0 && raw[17] & 7 == 0
            && raw[8] == 16 && matches!(raw[1], 0 | 3);
        let error = anyhow!("TAKE_RF attempt {attempt} at {}: {}", stage(report.entered),
            io::Error::from_raw_os_error(-report.primary));
        if !safe { return Err(error); }
        *guard_taken = false;
        if attempt == TAKE_ATTEMPTS || !report.retryable() || cancelled(shared,carrier) {
            return Err(error);
        }
        shared.inbox.lock().unwrap().phase = "recovering";
        let start = Instant::now();
        let prepared = trace_radio(command,generation,||carrier.prepare_guarded(pll, &shared.switch, |stage| {
            trace_control(stage, ||json!({"id":command,"generation":generation,"after_attempt":attempt}));
        }));
        let preparation = json!({"after_attempt":attempt,"us":start.elapsed().as_micros() as u64,
            "error":prepared.as_ref().err().map(|e| format!("{e:#}"))});
        attempts.last_mut().unwrap()["ps_reprepare"] = preparation.clone();
        {
            let mut q = shared.inbox.lock().unwrap();
            q.operation["attempts"] = json!(attempts);
            q.operation["ps_reprepare"] = preparation;
        }
        if prepared.is_err() { return Err(error); }
    }
    unreachable!()
}
