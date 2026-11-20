use super::*;

#[derive(Default)]
pub(super) struct Cache {
    // In-memory only. A new owner, boot, or RF configuration invalidates it.
    snapshot: Vec<String>,
    profiles: BTreeMap<u64, (u64, [u8; 16])>,
}

impl Cache {
    pub fn direct(
        &mut self,
        c: &Config,
        pending: &Value,
        marker: bool,
        stop: &AtomicBool,
    ) -> Result<(Vec<u8>, Value)> {
        let start = Instant::now();
        let scan = &pending["scan"];
        let mut ranges = Vec::new();
        for r in scan["ranges"].as_array().ok_or_else(|| anyhow!("ranges"))? {
            if r["enabled"] == true {
                ensure!(r["tx_slot"].is_null(), "planner v2 owns fastlock slots");
                let lo = int(r, "lo_hz", 70_000_000, plan_math::DIRECT_LIMIT)?;
                ranges.push([lo, int(r, "hi_hz", lo + 1, plan_math::DIRECT_LIMIT)?]);
            }
        }
        let bands = parse_antenna_bands(&scan["antenna_bands"])?;
        let intervals = bands.into_values().flatten().collect::<Vec<_>>();
        let centers = plan_math::direct_centers(&ranges, &intervals).map_err(anyhow::Error::msg)?;
        ensure!(
            !centers.is_empty() && centers.len() <= 128,
            "row99 RX catalog needs {} entries, capacity 128",
            centers.len()
        );
        let math_us = start.elapsed().as_micros() as u64;
        let prepare = Instant::now();
        let snapshot = [
            "in_voltage_sampling_frequency",
            "in_voltage_rf_bandwidth",
            "in_voltage0_rf_port_select",
        ]
        .iter()
        .map(|s| attr(s))
        .collect::<Result<Vec<_>>>()?;
        if self.snapshot != snapshot {
            self.profiles.clear();
            self.snapshot = snapshot;
        }
        let old = attr("out_altvoltage0_RX_LO_frequency")?.parse::<u64>()?;
        ensure!(
            attr("out_altvoltage1_TX_LO_powerdown")? == "1",
            "TX active at calibration"
        );
        let mut cold = 0;
        // Calibrate in a temporary bank. Publish the cache after the whole bank succeeds.
        let mut bank = Vec::new();
        let prepared = (|| -> Result<()> {
            for &requested in &centers {
                ensure!(
                    !stop.load(Ordering::Acquire) && !STOP_RUNTIME.load(Ordering::Acquire),
                    "preparation cancelled"
                );
                let value = match self.profiles.get(&requested) {
                    Some(value) => *value,
                    None => {
                        let hz = set_attr("out_altvoltage0_RX_LO_frequency", requested)?
                            .parse::<u64>()?;
                        ensure!(
                            hz.abs_diff(requested) <= 10,
                            "RX PLL readback {hz} for {requested}"
                        );
                        std::thread::sleep(Duration::from_millis(100));
                        for name in [
                            "out_altvoltage0_RX_LO_fastlock_store",
                            "out_altvoltage0_RX_LO_fastlock_save",
                        ] {
                            fs::write(Path::new(PHY).join(name), "0")?;
                        }
                        let data = attr("out_altvoltage0_RX_LO_fastlock_save")?;
                        let bytes = data
                            .strip_prefix("0 ")
                            .ok_or_else(|| anyhow!("RX save slot"))?
                            .split(',')
                            .map(str::parse::<u8>)
                            .collect::<Result<Vec<_>, _>>()?;
                        cold += 1;
                        (
                            hz,
                            bytes
                                .try_into()
                                .map_err(|_| anyhow!("RX fastlock length"))?,
                        )
                    }
                };
                bank.push(value);
            }
            Ok(())
        })();
        if cold != 0 || prepared.is_err() {
            let restored = set_attr("out_altvoltage0_RX_LO_frequency", old)?.parse::<u64>()?;
            ensure!(restored.abs_diff(old) <= 10, "restore RX after calibration");
        }
        prepared?;
        let positions = bank.iter().map(|v| v.0).collect::<Vec<_>>();
        let mut template = vec![0; 12488];
        put(&mut template, 0, 0x3747534f);
        put(&mut template, 4, bank.len().max(8) as u32);
        put(&mut template, 24, 512);
        put_hz(&mut template, 32, old);
        put(&mut template, 12336, 89750);
        for i in 0..bank.len().max(8) {
            let entry = 40 + i * 32;
            put(&mut template, entry, (i + 1) as u32);
            put(&mut template, entry + 4, 1);
            template[entry + 8..entry + 24].copy_from_slice(&bank[i % bank.len()].1);
            put(&mut template, MAP + i * 32, i as u32);
        }
        let calib_us = prepare.elapsed().as_micros() as u64;
        let compile = Instant::now();
        let (bytes, mut spectrum) = plan_positions(c, &positions, &template, pending, marker)?;
        self.profiles = centers.into_iter().zip(bank).collect();
        spectrum["preparation"] = json!({"planner_version":2,"math_us":math_us+compile.elapsed().as_micros() as u64,
            "prepare_us":calib_us,"calibrated":cold,"rx_catalog_count":word(&bytes,4)});
        Ok((bytes, spectrum))
    }
}
