//! The last calibration of each device, kept across restarts.
//!
//! A knee sweep runs on whatever is on air for a few seconds, so two sweeps
//! of one dongle can pick gains a step or two apart (smco moved 8 dB between
//! two deploys on 2026-10-18, and its RF chart stepped with it). Keeping the
//! last pick and re-checking it against the noise floor it was made at
//! holds the gain still unless the floor really moved.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

const FILE: &str = "gain-calibration.json";

/// The selection algorithm's revision.
///
/// An entry picked by an older one is dropped on load, so changing how gains
/// are chosen re-sweeps every device at the next start instead of re-checking
/// a pick the new rules would not have made.
///
/// 1: the knee itself, off one tuner step's slope.
/// 2: the knee confirmed over a span and a second window, plus a margin.
pub const ALGORITHM: u32 = 2;

/// One device's last full sweep.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cached {
    pub gain_db: f64,
    /// The noise floor measured at that gain (the sweep's relative dB scale).
    pub noise_floor_db: f64,
    pub reason: String,
    /// When, Unix seconds.
    pub at: u64,
    /// Which [`ALGORITHM`] picked it (absent in files written before 2).
    #[serde(default)]
    pub algorithm: u32,
    /// The curve the pick was made from.
    #[serde(default)]
    pub sweep: Vec<SweepPoint>,
}

/// One step of a cached sweep.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepPoint {
    pub gain_db: f64,
    pub noise_floor_db: f64,
    pub sat_frac: f64,
}

/// The cache file: device string (as configured, e.g. `rtl=13360853`) to its
/// last sweep. Missing or unreadable, it is empty and says so once.
#[derive(Debug)]
pub struct Cache {
    path: PathBuf,
    entries: BTreeMap<String, Cached>,
}

impl Cache {
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(FILE);
        let mut entries: BTreeMap<String, Cached> = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "gain cache unreadable; starting empty");
                BTreeMap::new()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "gain cache unreadable; starting empty");
                BTreeMap::new()
            }
        };
        entries.retain(|device, c| {
            let current = c.algorithm == ALGORITHM;
            if !current {
                tracing::info!(
                    device = %device,
                    gain_db = c.gain_db,
                    picked_by = c.algorithm,
                    "gain cache: last pick was made by an older selection rule; sweeping again"
                );
            }
            current
        });
        Self { path, entries }
    }

    pub fn get(&self, device: &str) -> Option<&Cached> {
        self.entries.get(device)
    }

    pub fn put(&mut self, device: &str, cached: Cached) {
        self.entries.insert(device.to_owned(), cached);
    }

    /// Writes the file; the directory must exist and be writable (a volume
    /// in the container).
    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(&self.entries)?;
        std::fs::write(&self.path, text).with_context(|| format!("writing {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_starts_empty_when_missing() {
        let dir = std::env::temp_dir().join(format!("gain-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut c = Cache::load(&dir);
        assert!(c.get("rtl=1").is_none());
        c.put(
            "rtl=1",
            Cached {
                gain_db: 43.9,
                noise_floor_db: -61.5,
                reason: "knee".into(),
                at: 1_792_000_000,
                algorithm: ALGORITHM,
                sweep: vec![SweepPoint {
                    gain_db: 43.9,
                    noise_floor_db: -61.5,
                    sat_frac: 0.0,
                }],
            },
        );
        c.save().unwrap();
        let again = Cache::load(&dir);
        assert_eq!(again.get("rtl=1"), c.get("rtl=1"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn drops_picks_made_by_an_older_rule() {
        let dir = std::env::temp_dir().join(format!("gain-cache-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // As written before `algorithm` and `sweep` existed.
        std::fs::write(
            dir.join(FILE),
            r#"{"rtl=1":{"gain_db":20.7,"noise_floor_db":-45.5,"reason":"knee","at":1}}"#,
        )
        .unwrap();
        let c = Cache::load(&dir);
        assert!(c.get("rtl=1").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
