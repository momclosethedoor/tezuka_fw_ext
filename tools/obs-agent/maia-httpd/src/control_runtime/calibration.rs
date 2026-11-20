use super::*;
use std::collections::BTreeSet;

type Bank = BTreeMap<u64, (u64, [u8; 16])>;

#[derive(Default)]
pub(super) struct Cache {
    snapshot: Vec<String>,
    rx: Bank,
    tx: Bank,
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
    pub fn prepare(&mut self, pending: &Value, stop: &AtomicBool) -> Result<(Vec<u8>, Value)> {
        let start = Instant::now();
        let cpu_start = cpu_ns()?;
        let tiles = generated::tiles(pending)?;
        let rx_keys = tiles.iter().map(|v| v.rx).collect::<BTreeSet<_>>();
        let tx_keys = tiles
            .iter()
            .filter_map(|v| v.tx.map(|t| t.pll as u64))
            .collect::<BTreeSet<_>>();
        ensure!(
            tiles.len() <= 65535 && rx_keys.len() <= 65535 && tx_keys.len() <= 8,
            "row99 needs {} visits, {} RX and {} TX; capacities 65535/65535/8",
            tiles.len(),
            rx_keys.len(),
            tx_keys.len()
        );
        let math_us = start.elapsed().as_micros() as u64;
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
        if snapshot != self.snapshot {
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
        let (bytes, mut desc) = generated::encode(&tiles, &rx, &tx, restore, pending)?;
        self.rx = rx;
        self.tx = tx;
        desc["preparation"] = json!({"planner_version":2,"math_us":math_us+compile.elapsed().as_micros() as u64,
            "prepare_us":prepare_us,"worker_cpu_ns":cpu_ns()?.saturating_sub(cpu_start),
            "rss_kib":fs::read_to_string("/proc/self/status")?.lines().find_map(|l|
                l.strip_prefix("VmRSS:")?.split_whitespace().next()?.parse::<u64>().ok()),
            "rx_calibrated":rx_cold,"tx_calibrated":tx_cold,"rx_catalog_count":word(&bytes,4)});
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
