use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::device::Device;
use crate::{Channel, Signal, Tag};

/// Full trunk-recorder config, preserved as raw JSON.
///
/// Unknown fields (plugins, capture dir, system details, etc.) round-trip
/// untouched. Typed views are deserialized on demand from the relevant
/// sub-trees.
pub struct TrunkConfig {
    raw: serde_json::Value,
}

/// Typed view of a source entry — only the fields the solver needs.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigSource {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    device: String,
    #[serde(default)]
    gain: i64,
    #[serde(default)]
    ppm: i64,
}

/// Typed view of a system entry — only the fields the solver needs.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigSystem {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(rename = "type")]
    system_type: String,
    #[serde(default, alias = "control_channels")]
    control_channels: Vec<i64>,
    channel_file: Option<PathBuf>,
}

const fn default_true() -> bool {
    true
}

impl TrunkConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let raw: serde_json::Value =
            serde_json::from_str(&content).context("Failed to parse config JSON")?;
        Ok(Self { raw })
    }

    /// Infer [`Device`] definitions from the existing `sources` array.
    pub fn devices(&self) -> Result<Vec<Device>> {
        let sources: Vec<ConfigSource> =
            serde_json::from_value(self.raw["sources"].clone()).unwrap_or_default();
        sources
            .iter()
            .filter(|s| s.enabled)
            .map(|s| {
                Device::from_source_config(&s.device, s.gain, s.ppm)
                    .ok_or_else(|| anyhow::anyhow!("Unknown device type: {}", s.device))
            })
            .collect()
    }

    /// Extract all channel frequencies from the `systems` array.
    pub fn channels(&self, config_dir: &Path) -> Result<Vec<Channel>> {
        let systems: Vec<ConfigSystem> =
            serde_json::from_value(self.raw["systems"].clone()).unwrap_or_default();
        let mut channels = Vec::new();

        for (i, system) in systems.iter().enumerate() {
            if !system.enabled {
                continue;
            }
            match system.system_type.as_str() {
                "p25" => {
                    for &freq in &system.control_channels {
                        channels.push(Channel {
                            frequency: freq,
                            signal: Signal::Digital,
                            tag: Some(Tag::Primary),
                            system: Some(i),
                        });
                    }
                }
                "conventional" | "conventionalP25" => {
                    if let Some(channel_file) = &system.channel_file {
                        let resolved = resolve_path(channel_file, config_dir)?;
                        let csv_channels = parse_conventional_csv(&resolved)?;
                        channels.extend(csv_channels);
                    }
                }
                _ => {}
            }
        }

        Ok(channels)
    }

    /// Replace the `sources` array in the config with solver output,
    /// preserving everything else.
    pub fn patch_sources(&mut self, sources: &[impl serde::Serialize]) {
        self.raw["sources"] =
            serde_json::to_value(sources).expect("Source is always serializable");
    }

    /// Serialize the full config back to pretty-printed JSON.
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(&self.raw).expect("Value is always serializable")
    }
}

fn resolve_path(path: &Path, config_dir: &Path) -> Result<PathBuf> {
    if path.exists() {
        return Ok(path.to_path_buf());
    }
    if let Some(filename) = path.file_name() {
        let candidate = config_dir.join(filename);
        if candidate.exists() {
            return Ok(candidate);
        }
        bail!(
            "Cannot find channel file: {} (tried {})",
            path.display(),
            candidate.display()
        );
    }
    bail!("Cannot resolve channel file: {}", path.display())
}

fn parse_conventional_csv(path: &Path) -> Result<Vec<Channel>> {
    let mut channels = Vec::new();
    let mut reader = csv::Reader::from_path(path)
        .with_context(|| format!("Failed to open CSV: {}", path.display()))?;

    for result in reader.records() {
        let record = result?;
        // CSV format: TG Number,Frequency,Tone,Alpha Tag,Description,Tag,Category
        if let Some(freq_str) = record.get(1)
            && let Ok(freq) = freq_str.trim().parse::<i64>()
        {
            channels.push(Channel {
                frequency: freq,
                signal: Signal::Analog,
                tag: None,
                system: None,
            });
        }
    }

    Ok(channels)
}
