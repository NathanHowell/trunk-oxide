//! Turns a capture directory + overrides into a replayable config.
//!
//! The capture directory holds `capture.json` (what was recorded), the
//! `config.toml` that was live at capture time and the talkgroup/channel CSVs.
//! Replay keeps everything from that config (systems, recorders, squelch) and
//! swaps each hardware source for its recorded IQ file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Capture {
    pub start_utc: String,
    pub duration_s: f64,
    pub sources: Vec<CaptureSource>,
}

#[derive(Debug, Deserialize)]
pub struct CaptureSource {
    pub serial: String,
    pub center_hz: f64,
    pub rate_sps: f64,
    pub gain_db: f64,
    pub file: String,
}

impl Capture {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("capture.json");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

/// A `key=value` override applied to the config TOML before it is parsed.
#[derive(Debug, Clone)]
pub struct Override {
    pub key: String,
    pub value: toml::Value,
}

impl std::str::FromStr for Override {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (key, raw) = s
            .split_once('=')
            .ok_or_else(|| anyhow!("expected key=value, got '{s}'"))?;
        // Values are TOML scalars; anything that does not parse is a string.
        let value = format!("v = {raw}")
            .parse::<toml::Table>()
            .ok()
            .and_then(|mut t| t.remove("v"))
            .unwrap_or_else(|| toml::Value::String(raw.to_string()));
        Ok(Self {
            key: key.trim().to_string(),
            value,
        })
    }
}

impl std::fmt::Display for Override {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}={}", self.key, self.value)
    }
}

/// Set `path` (dotted; numeric segments index arrays) to `value`.
fn set_path(root: &mut toml::Value, path: &str, value: &toml::Value) -> Result<()> {
    let mut cur = root;
    let segments: Vec<&str> = path.split('.').collect();
    for (i, seg) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        cur = match cur {
            toml::Value::Table(t) => {
                if last {
                    t.insert((*seg).to_string(), value.clone());
                    return Ok(());
                }
                // Optional sub-tables (e.g. `recorder.conditioner`) may be absent.
                t.entry((*seg).to_string())
                    .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
            }
            toml::Value::Array(a) => {
                let idx: usize = seg
                    .parse()
                    .map_err(|_| anyhow!("'{seg}' is not an array index in '{path}'"))?;
                let len = a.len();
                let item = a
                    .get_mut(idx)
                    .ok_or_else(|| anyhow!("index {idx} out of range ({len}) in '{path}'"))?;
                if last {
                    *item = value.clone();
                    return Ok(());
                }
                item
            }
            _ => bail!("'{path}': cannot descend into a scalar at '{seg}'"),
        };
    }
    bail!("empty override path")
}

/// Expand the convenience aliases into concrete key paths.
///
/// - `squelch` → `system.<name>.squelch` for every conventional system.
/// - `freq_correction_hz` → `source.<i>.freq_correction_hz` for every source.
fn expand(root: &toml::Value, ov: &Override) -> Vec<Override> {
    let with = |key: String| Override {
        key,
        value: ov.value.clone(),
    };
    match ov.key.as_str() {
        "squelch" => root
            .get("system")
            .and_then(toml::Value::as_table)
            .into_iter()
            .flatten()
            .filter(|(_, sys)| {
                sys.get("type").and_then(toml::Value::as_str) == Some("conventional")
            })
            .map(|(name, _)| with(format!("system.{name}.squelch")))
            .collect(),
        "freq_correction_hz" => (0..root
            .get("source")
            .and_then(toml::Value::as_array)
            .map_or(0, Vec::len))
            .map(|i| with(format!("source.{i}.freq_correction_hz")))
            .collect(),
        _ => vec![ov.clone()],
    }
}

pub struct Plan {
    pub config_path: PathBuf,
    /// Wall-clock length of the longest source file, in seconds.
    pub duration_s: f64,
    /// Sources dropped because the capture has no file for them.
    pub skipped_sources: Vec<String>,
}

/// Write `<run_dir>/config.toml` for replaying `capture_dir`.
pub fn build(
    capture_dir: &Path,
    run_dir: &Path,
    overrides: &[Override],
    tap: bool,
) -> Result<Plan> {
    let capture = Capture::load(capture_dir)?;
    let base = capture_dir.join("config.toml");
    let text =
        std::fs::read_to_string(&base).with_context(|| format!("reading {}", base.display()))?;
    let mut root: toml::Value = text
        .parse()
        .with_context(|| format!("parsing {}", base.display()))?;
    let capture_dir = capture_dir.canonicalize()?;

    let mut duration_s = 0.0_f64;
    let mut skipped = Vec::new();
    if let Some(sources) = root.get_mut("source").and_then(toml::Value::as_array_mut) {
        for src in sources {
            let Some(tbl) = src.as_table_mut() else {
                continue;
            };
            let device = tbl
                .get("device")
                .and_then(toml::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let Some(cap) = capture
                .sources
                .iter()
                .find(|c| device.ends_with(&format!("={}", c.serial)) || device == c.serial)
            else {
                tbl.insert("enabled".into(), false.into());
                skipped.push(device);
                continue;
            };
            let file = capture_dir.join(&cap.file);
            let bytes = std::fs::metadata(&file)
                .with_context(|| format!("stat {}", file.display()))?
                .len();
            #[allow(clippy::cast_precision_loss)]
            let secs = bytes as f64 / 2.0 / cap.rate_sps;
            duration_s = duration_s.max(secs);

            tbl.insert("driver".into(), "iqfile".into());
            tbl.insert("device".into(), file.display().to_string().into());
            tbl.insert("center".into(), cap.center_hz.into());
            tbl.insert("rate".into(), cap.rate_sps.into());
            tbl.insert("iq_format".into(), "cu8".into());
            tbl.insert("iq_repeat".into(), false.into());
            // Hardware knobs mean nothing for a file.
            tbl.insert("auto_gain".into(), false.into());
            tbl.remove("gain");
            tbl.remove("ppm");
        }
    }

    // CSV paths are relative to the config file; point them at the capture.
    if let Some(systems) = root.get_mut("system").and_then(toml::Value::as_table_mut) {
        for (_, sys) in systems.iter_mut() {
            let Some(sys) = sys.as_table_mut() else {
                continue;
            };
            for key in ["talkgroup_file", "channel_file"] {
                if let Some(v) = sys.get_mut(key)
                    && let Some(name) = v.as_str()
                {
                    *v = capture_dir.join(name).display().to_string().into();
                }
            }
        }
    }
    let temp_dir = run_dir.join("tmp");
    std::fs::create_dir_all(&temp_dir)?;
    set_path(
        &mut root,
        "recorder.temp_dir",
        &toml::Value::String(temp_dir.display().to_string()),
    )
    .or_else(|_| {
        root.as_table_mut()
            .ok_or_else(|| anyhow!("config root is not a table"))
            .map(|t| {
                let mut r = toml::Table::new();
                r.insert("temp_dir".into(), temp_dir.display().to_string().into());
                t.insert("recorder".into(), r.into());
            })
    })?;
    if tap {
        let tap_dir = run_dir.join("taps");
        std::fs::create_dir_all(&tap_dir)?;
        set_path(
            &mut root,
            "recorder.tap_dir",
            &toml::Value::String(tap_dir.display().to_string()),
        )?;
    }
    // Adaptive ppm correction mutates state from live FLL errors; keep replays
    // reproducible unless explicitly asked.
    let _ = set_path(
        &mut root,
        "recorder.adaptive_freq_correction",
        &false.into(),
    );

    for ov in overrides {
        for concrete in expand(&root, ov) {
            set_path(&mut root, &concrete.key, &concrete.value)
                .with_context(|| format!("applying override {concrete}"))?;
        }
    }

    let config_path = run_dir.join("config.toml");
    std::fs::write(&config_path, toml::to_string_pretty(&root)?)?;
    Ok(Plan {
        config_path,
        duration_s,
        skipped_sources: skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> toml::Value {
        s.parse().unwrap()
    }

    #[test]
    fn override_values_are_typed() {
        let o: Override = "system.conv.squelch=-48".parse().unwrap();
        assert_eq!(o.value, toml::Value::Integer(-48));
        let o: Override = "source.0.freq_correction_hz=185.5".parse().unwrap();
        assert_eq!(o.value, toml::Value::Float(185.5));
        let o: Override = "system.conv.type=conventional".parse().unwrap();
        assert_eq!(o.value, toml::Value::String("conventional".into()));
    }

    #[test]
    fn set_path_descends_tables_and_arrays() {
        let mut root =
            parse("[[source]]\ndevice='a'\n[[source]]\ndevice='b'\n[system.conv]\nsquelch=-50.0\n");
        set_path(
            &mut root,
            "source.1.freq_correction_hz",
            &toml::Value::Float(7.0),
        )
        .unwrap();
        set_path(&mut root, "system.conv.squelch", &toml::Value::Float(-45.0)).unwrap();
        assert_eq!(
            root["source"][1]["freq_correction_hz"],
            toml::Value::Float(7.0)
        );
        assert_eq!(root["system"]["conv"]["squelch"], toml::Value::Float(-45.0));
        assert!(set_path(&mut root, "source.9.x", &toml::Value::Float(1.0)).is_err());
        assert!(set_path(&mut root, "system.conv.squelch.x", &toml::Value::Float(1.0)).is_err());
    }

    #[test]
    fn aliases_fan_out() {
        let root = parse(
            "[[source]]\ndevice='a'\n[[source]]\ndevice='b'\n\
             [system.conv]\ntype='conventional'\n[system.smco]\ntype='p25'\n",
        );
        let sq: Vec<String> = expand(&root, &"squelch=-48".parse().unwrap())
            .into_iter()
            .map(|o| o.key)
            .collect();
        assert_eq!(sq, ["system.conv.squelch"]);
        let fc: Vec<String> = expand(&root, &"freq_correction_hz=0".parse().unwrap())
            .into_iter()
            .map(|o| o.key)
            .collect();
        assert_eq!(
            fc,
            ["source.0.freq_correction_hz", "source.1.freq_correction_hz"]
        );
    }
}
