use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::device::Device;
use crate::{Channel, Signal, Tag};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrunkConfig {
    #[serde(default)]
    pub sources: Vec<ConfigSource>,
    #[serde(default)]
    pub systems: Vec<ConfigSystem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSource {
    pub device: String,
    #[serde(default = "default_gain")]
    pub gain: i64,
    #[serde(default)]
    pub ppm: i64,
}

const fn default_gain() -> i64 {
    24
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSystem {
    #[serde(rename = "type")]
    pub system_type: String,
    #[serde(default, alias = "control_channels")]
    pub control_channels: Vec<i64>,
    pub channel_file: Option<PathBuf>,
}

pub fn load_config(path: &Path) -> Result<TrunkConfig> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;
    serde_json::from_str(&content).context("Failed to parse config JSON")
}

impl TrunkConfig {
    pub fn devices(&self) -> Result<Vec<Device>> {
        self.sources
            .iter()
            .map(|s| {
                Device::from_source_config(&s.device, s.gain, s.ppm)
                    .ok_or_else(|| anyhow::anyhow!("Unknown device type: {}", s.device))
            })
            .collect()
    }

    pub fn channels(&self, config_dir: &Path) -> Result<Vec<Channel>> {
        let mut channels = Vec::new();

        for (i, system) in self.systems.iter().enumerate() {
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
