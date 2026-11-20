use super::*;
use std::collections::BTreeSet;

type Bank = BTreeMap<u64, (u64, [u8; 16])>;

#[derive(Default)]
pub(super) struct Cache {
    snapshot: Vec<String>,
    rx: Bank,
    tx: Bank,
    core: Vec<generated::Tile>,
    bands: Value,
    mixer: Value,
    keep_rx: BTreeSet<u64>,
    keep_tx: BTreeSet<u64>,
}

fn calibrate(
    keys: &BTreeSet<u64>,
    cache: &Bank,
    prefix: &str,
    old: u64,
    stop: &AtomicBool,
) -> Result<(Bank, usize)> {
    let frequency = format!("{prefix}_frequency");
    let mut result = Bank::new();
    let mut attempted = 0;
    let prepared = (|| -> Result<()> {
        for &requested in keys {
            ensure!(
                !stop.load(Ordering::Acquire) && !STOP_RUNTIME.load(Ordering::Acquire),
                "preparation cancelled"
            );
            let profile = if let Some(&hit) = cache.get(&requested) {
                hit
            } else {
                attempted += 1;
                let hz = set_attr(&frequency, requested)?.parse::<u64>()?;
                ensure!(
                    hz.abs_diff(requested) <= 10,
                    "{prefix} frequency readback {hz} for {requested}"
                );
                std::thread::sleep(Duration::from_millis(100));
                for suffix in ["store", "save"] {
                    fs::write(
                        Path::new(PHY).join(format!("{prefix}_fastlock_{suffix}")),
                        "0",
                    )?;
                }
                let raw = attr(&format!("{prefix}_fastlock_save"))?;
                let mut fields = raw.split_whitespace();
                ensure!(fields.next() == Some("0"), "unexpected fastlock slot");
                let bytes = fields
                    .next()
                    .ok_or_else(|| anyhow!("missing fastlock data"))?
                    .split(',')
                    .map(str::parse::<u8>)
                    .collect::<Result<Vec<_>, _>>()?;
                ensure!(fields.next().is_none(), "invalid fastlock data");
                (hz, bytes.try_into().map_err(|_| anyhow!("fastlock width"))?)
            };
            result.insert(requested, profile);
        }
        Ok(())
    })();
    if attempted > 0 {
        let restored = set_attr(&frequency, old)?.parse::<u64>()?;
        ensure!(restored.abs_diff(old) <= 10, "{prefix} restore mismatch");
    }
    prepared?;
    Ok((result, attempted))
}

impl Cache {
    pub fn catalog(state: &Value, shared: &Shared) -> Result<(Self, Value)> {
        let start = Instant::now();
        let cpu_start = cpu_ns()?;
        let bands = parse_antenna_bands(&state["scan"]["antenna_bands"])?;
        let mut intervals: Vec<_> = bands.values().flatten()
            .map(|r| [r[0].max(70_000_000), r[1]]).filter(|r| r[0]<r[1]).collect();
        intervals.sort_unstable();
        let mut union: Vec<[u64; 2]> = Vec::new();
        for r in intervals {
            if let Some(last) = union.last_mut() && last[1] >= r[0] {
                last[1] = last[1].max(r[1]);
            } else { union.push(r); }
        }
        ensure!(!union.is_empty(), "antenna catalog outside receiver limits");
        let mut full = state.clone();
        full["alternative"] = Value::Null;
        full["scan"]["ranges"] = json!(union.iter().map(|r|json!({"lo_hz":r[0],"hi_hz":r[1],"enabled":true})).collect::<Vec<_>>());
        let core = generated::core_tiles(&full)?;
        let tiles = generated::catalog_profiles(&full,&core)?;
        let mut cache = Self::default();
        let (_, desc) = cache.prepare_tiles(&full,shared,tiles,start,cpu_start,true)?;
        cache.core = core;
        cache.bands = full["scan"]["antenna_bands"].clone();
        cache.mixer = full["scan"]["mixer"].clone();
        cache.keep_rx = cache.rx.keys().copied().collect();
        cache.keep_tx = cache.tx.keys().copied().collect();
        let info = json!({"rf_intervals_hz":union,"primary_tiles":cache.core.len(),
            "preparation":desc["preparation"],"rx_bandwidth_hz":attr("in_voltage_rf_bandwidth")?.parse::<u64>()?,
            "tx_powerdown":attr("out_altvoltage1_TX_LO_powerdown")?.parse::<u32>()?});
        Ok((cache,info))
    }

    pub fn prepare(&mut self, pending: &Value, shared: &Shared) -> Result<(Vec<u8>, Value)> {
        let start = Instant::now();
        let cpu_start = cpu_ns()?;
        let scan = &pending["scan"];
        let ranges = scan["ranges"].as_array().ok_or_else(|| anyhow!("ranges"))?;
        ensure!(ranges.iter().filter(|r| r["enabled"]==true).all(|r| r["tx_slot"].is_null()),
                "v2 selects TX from RF");
        let select = !self.core.is_empty() && pending["alternative"].is_null()
            && self.bands == scan["antenna_bands"] && self.mixer == scan["mixer"]
            && ranges.iter().all(|r| r["path"].is_null() || r["path"]=="auto");
        let tiles = if select {
            let mut subset = Vec::new();
            for (row,r) in ranges.iter().enumerate().filter(|(_,r)|r["enabled"]==true) {
                let lo = r["lo_hz"].as_u64().ok_or_else(||anyhow!("range start"))?;
                let hi = r["hi_hz"].as_u64().ok_or_else(||anyhow!("range end"))?;
                let mut cursor = lo;
                for t in &self.core {
                    let a = lo.max(t.rf[0]);
                    let b = hi.min(t.rf[1]);
                    if a >= b {continue}
                    ensure!(a == cursor,"uncovered RF [{cursor},{a})");
                    let mut clipped = t.clone();
                    clipped.rf = [a,b];
                    clipped.rf_row = row;
                    subset.push(clipped);
                    cursor = b;
                }
                ensure!(cursor == hi,"uncovered RF [{cursor},{hi})");
            }
            generated::executable_tiles(pending,&subset)?
        } else { generated::tiles(pending)? };
        self.prepare_tiles(pending,shared,tiles,start,cpu_start,false)
    }

    fn prepare_tiles(&mut self, pending: &Value, shared: &Shared, tiles: Vec<generated::Tile>,
                     start: Instant, cpu_start: u64, catalog_only: bool) -> Result<(Vec<u8>, Value)> {
        let stop = &shared.switch;
        if !catalog_only && pending["pl_timing_only"] != true && pending["xtg1_wait_ticks"].is_null()
            && tiles.iter().any(|t| t.tx.is_some()) && tiles.iter().any(|t| t.tx.is_none()) {
            let requested = pending["scan"]["ranges"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|r| r["enabled"] == true)
                .map(|r| [r["lo_hz"].clone(), r["hi_hz"].clone()])
                .collect::<Vec<_>>();
            bail!(
                "row99 cannot gate TX per visit within one active map; unapplied RF intervals {requested:?}"
            );
        }
        let rx_keys = tiles.iter().map(|v| v.rx).collect::<BTreeSet<_>>();
        let tx_keys = tiles
            .iter()
            .filter_map(|v| v.tx.map(|t| t.pll as u64))
            .collect::<BTreeSet<_>>();
        ensure!(
            tiles.len() <= 65535 && rx_keys.len() <= 65535 && (catalog_only || tx_keys.len() <= 8),
            "row99 needs {} visits, {} RX and {} TX; capacities 65535/65535/8",
            tiles.len(),
            rx_keys.len(),
            tx_keys.len()
        );
        let math_us = start.elapsed().as_micros() as u64;
        shared.inbox.lock().unwrap().phase = "calibrating";
        let now = Instant::now();
        let snapshot = [
            "in_voltage_sampling_frequency",
            "in_voltage_rf_bandwidth",
            "in_voltage0_rf_port_select",
            "out_voltage_rf_bandwidth",
            "out_voltage_sampling_frequency",
            "out_voltage0_rf_port_select",
        ]
        .iter()
        .map(|s| attr(s))
        .collect::<Result<Vec<_>>>()?;
        if snapshot != self.snapshot || pending["scan"]["recalibrate"] == true {
            self.snapshot = snapshot;
            self.rx.clear();
            self.tx.clear();
        }
        let restore = [
            attr("out_altvoltage0_RX_LO_frequency")?.parse()?,
            attr("out_altvoltage1_TX_LO_frequency")?.parse()?,
        ];
        ensure!(
            attr("out_altvoltage1_TX_LO_powerdown")? == "1",
            "TX active before preparation"
        );
        let (rx, rx_cold) = calibrate(
            &rx_keys,
            &self.rx,
            "out_altvoltage0_RX_LO",
            restore[0],
            stop,
        )?;
        let (tx, tx_cold) = calibrate(
            &tx_keys,
            &self.tx,
            "out_altvoltage1_TX_LO",
            restore[1],
            stop,
        )?;
        let prepare_us = now.elapsed().as_micros() as u64;
        let compile = Instant::now();
        let (bytes, mut desc) = if catalog_only {
            (Vec::new(),json!({}))
        } else {
            generated::encode(&tiles, &rx, &tx, restore, pending)?
        };
        let rx_count = if catalog_only {rx.len() as u32} else {word(&bytes,4)};
        self.rx.retain(|key,_|self.keep_rx.contains(key));
        self.tx.retain(|key,_|self.keep_tx.contains(key));
        self.rx.extend(rx);
        self.tx.extend(tx);
        desc["preparation"] = json!({"planner_version":2,"math_us":math_us+compile.elapsed().as_micros() as u64,
            "prepare_us":prepare_us,"worker_cpu_ns":cpu_ns()?.saturating_sub(cpu_start),
            "rss_kib":fs::read_to_string("/proc/self/status")?.lines().find_map(|l|
                l.strip_prefix("VmRSS:")?.split_whitespace().next()?.parse::<u64>().ok()),
            "rx_calibrated":rx_cold,"tx_calibrated":tx_cold,"rx_catalog_count":rx_count});
        Ok((bytes, desc))
    }
}

fn cpu_ns() -> Result<u64> {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    ensure!(
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) } == 0,
        "worker CPU clock"
    );
    Ok(t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64)
}
