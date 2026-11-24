use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
};

const GRID_SIZES: [u64; 4] = [512, 1024, 2048, 4096];

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Step {
    max_span: [u64; 2],
    view_bins: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    overview_bins: u64,
    steps: Vec<Step>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            overview_bins: 1024,
            steps: vec![
                Step {
                    max_span: [1, 3],
                    view_bins: 4096,
                },
                Step {
                    max_span: [2, 3],
                    view_bins: 2048,
                },
            ],
        }
    }
}

impl Policy {
    pub(super) fn parse(value: Value) -> Result<Self> {
        let policy: Self = serde_json::from_value(value)?;
        ensure!(
            GRID_SIZES.contains(&policy.overview_bins),
            "unsupported overview_bins"
        );
        ensure!(
            policy.steps.len() < GRID_SIZES.len(),
            "too many loop grid steps"
        );
        let [mut last_num, mut last_den] = [0, 1];
        for step in &policy.steps {
            let [num, den] = step.max_span;
            ensure!(
                num > 0 && num < den && den <= 1_000_000,
                "max_span must be a positive fraction below 1"
            );
            ensure!(
                last_num * den < num * last_den,
                "max_span must strictly increase"
            );
            ensure!(
                GRID_SIZES.contains(&step.view_bins),
                "unsupported loop view_bins"
            );
            [last_num, last_den] = [num, den];
        }
        Ok(policy)
    }

    fn select(&self, viewport: [u64; 2], span: [u64; 2]) -> u64 {
        let width = viewport[1] - viewport[0];
        let full = span[1] - span[0];
        self.steps
            .iter()
            .find(|step| {
                let [num, den] = step.max_span;
                u128::from(width) * u128::from(den) <= u128::from(full) * u128::from(num)
            })
            .map_or(self.overview_bins, |step| step.view_bins)
    }

    pub(super) fn resolve_scan(&self, cmd: &mut Value, rf_bounds: [u64; 2]) -> Result<()> {
        let fft = cmd["fft_bins"].as_u64().unwrap_or(4096);
        let Some(viewport) = cmd.as_object_mut().and_then(|object| object.remove("loop")) else {
            if cmd.get("roi").is_none() && cmd.get("view_bins").is_none() {
                cmd["view_bins"] = json!(self.overview_bins.min(fft));
            }
            return Ok(());
        };
        ensure!(
            cmd["op"] == "scan" && cmd["planner_version"] == 2 && cmd["run"] == true,
            "loop requires a running v2 scan"
        );
        ensure!(cmd.get("roi").is_none(), "specify either loop or roi");
        let object = viewport
            .as_object()
            .ok_or_else(|| anyhow!("loop must be an object"))?;
        ensure!(
            object
                .keys()
                .all(|key| matches!(key.as_str(), "lo_hz" | "hi_hz" | "view_bins")),
            "unknown loop parameter"
        );
        let lo = super::int(&viewport, "lo_hz", rf_bounds[0], rf_bounds[1] - 1)?;
        let hi = super::int(&viewport, "hi_hz", lo + 1, rf_bounds[1])?;
        let explicit = viewport.get("view_bins").or_else(|| cmd.get("view_bins"));
        if let Some(grid) = explicit {
            ensure!(
                cmd.get("view_bins").is_none_or(|outer| outer == grid),
                "scan and loop view_bins must match"
            );
            cmd["view_bins"] = grid.clone();
        } else {
            let mut span = [rf_bounds[1], rf_bounds[0]];
            let mut enabled = false;
            let ranges = cmd["ranges"]
                .as_array()
                .ok_or_else(|| anyhow!("scan ranges required"))?;
            for row in ranges.iter().filter(|row| row["enabled"] == true) {
                let start = super::int(row, "lo_hz", rf_bounds[0], rf_bounds[1] - 1)?;
                let end = super::int(row, "hi_hz", start + 1, rf_bounds[1])?;
                span = [span[0].min(start), span[1].max(end)];
                enabled = true;
            }
            ensure!(enabled, "no enabled range");
            if lo <= span[0] && hi >= span[1] {
                cmd["view_bins"] = json!(self.overview_bins.min(fft));
                return Ok(());
            }
            cmd["view_bins"] = json!(self.select([lo, hi], span).min(fft));
        }
        cmd["roi"] = json!({"lo_hz":lo, "hi_hz":hi, "view_bins":cmd["view_bins"], "source":"view"});
        Ok(())
    }
}

pub(super) struct Store {
    path: PathBuf,
    user: Option<Policy>,
    id: Option<String>,
}

impl Store {
    pub(super) fn load(output: &Path) -> Self {
        let path = output.join("loop-user.json");
        let user = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(Into::into)
                .and_then(Policy::parse),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Self {
                    path,
                    user: None,
                    id: None,
                };
            }
            Err(error) => Err(error.into()),
        };
        let user = user
            .map_err(|error| {
                tracing::warn!(path = %path.display(), %error, "using default loop settings");
            })
            .ok();
        Self {
            path,
            user,
            id: None,
        }
    }

    pub(super) fn effective(&self) -> Policy {
        self.user.clone().unwrap_or_default()
    }

    pub(super) fn status(&self) -> Value {
        json!({"defaults":Policy::default(), "effective":self.effective(),
               "source":if self.user.is_some() {"user"}else{"default"}, "saved_id":self.id})
    }

    pub(super) fn save(&mut self, id: &str, policy: Policy) -> Result<()> {
        if self.id.as_deref() == Some(id) {
            return Ok(());
        }
        let tmp = self.path.with_extension("json.tmp");
        let mut file = File::create(&tmp)?;
        serde_json::to_writer_pretty(&mut file, &policy)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        self.user = Some(policy);
        self.id = Some(id.to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_thresholds_are_inclusive_and_relative_to_the_enabled_map() {
        let policy = Policy::default();
        let scan = json!({"op":"scan","run":true,"planner_version":2,"fft_bins":4096,
                          "ranges":[{"lo_hz":100_000_000u64,"hi_hz":12_000_000_000u64,"enabled":true},
                                    {"lo_hz":70_000_000u64,"hi_hz":15_000_000_000u64,"enabled":false}]});
        let bounds = [70_000_000, 18_000_000_000];
        let full = 11_900_000_000u64;
        for (width, bins, roi) in [
            (full / 3, 4096, true),
            (full / 3 + 1, 2048, true),
            (full * 2 / 3, 2048, true),
            (full * 2 / 3 + 1, 1024, true),
            (full, 1024, false),
        ] {
            let mut request = scan.clone();
            request["loop"] = json!({"lo_hz":100_000_000u64,"hi_hz":100_000_000+width});
            policy.resolve_scan(&mut request, bounds).unwrap();
            assert_eq!(request["view_bins"], bins);
            assert_eq!(request.get("roi").is_some(), roi);
        }
        let mut exact = scan;
        exact["loop"] = json!({"lo_hz":4_000_000_000u64,"hi_hz":8_000_000_000u64,"view_bins":4096});
        policy.resolve_scan(&mut exact, bounds).unwrap();
        assert_eq!(exact["roi"]["view_bins"], 4096);
    }

    #[test]
    fn saves_only_valid_user_settings_and_reads_them_on_restart() {
        let path = std::env::temp_dir().join(format!(
            "maia-loop-grid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        let mut store = Store::load(&path);
        assert_eq!(store.status()["source"], "default");
        assert!(!store.path.exists());
        let custom = json!({"overview_bins":2048,"steps":[{"max_span":[1,2],"view_bins":4096}]});
        store
            .save("first", Policy::parse(custom.clone()).unwrap())
            .unwrap();
        assert_eq!(store.status()["saved_id"], "first");
        assert_eq!(store.effective().select([0, 6], [0, 12]), 4096);
        assert!(
            Policy::parse(
                json!({"overview_bins":1024,"steps":[{"max_span":[2,3],"view_bins":2048},
            {"max_span":[1,3],"view_bins":4096}]})
            )
            .is_err()
        );
        let reopened = Store::load(&path);
        assert_eq!(reopened.status()["source"], "user");
        assert_eq!(reopened.status()["effective"], custom);
        assert_eq!(reopened.status()["defaults"]["overview_bins"], 1024);
        fs::remove_file(reopened.path).unwrap();
        fs::remove_dir(&path).unwrap();
    }
}
