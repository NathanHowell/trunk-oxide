use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::device::Device;
use crate::{Channel, Signal, Tag};

/// Solver config loaded from the TOML config file.
pub struct SolverConfig {
    sources: Vec<ConfigSource>,
    systems: BTreeMap<String, ConfigSystem>,
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    source: Vec<ConfigSource>,
    #[serde(default)]
    system: BTreeMap<String, ConfigSystem>,
}

/// Typed view of a source entry — only the fields the solver needs.
#[derive(Debug, Deserialize)]
struct ConfigSource {
    #[serde(default)]
    device: String,
    #[serde(default)]
    gain: i64,
    #[serde(default)]
    ppm: i64,
}

/// Typed view of a system entry — only the fields the solver needs.
#[derive(Debug, Deserialize)]
struct ConfigSystem {
    #[serde(rename = "type")]
    system_type: String,
    #[serde(default)]
    control_channels: Vec<i64>,
    #[serde(default)]
    preferred_control_channel: Option<i64>,
    #[serde(default)]
    voice_channels: Vec<i64>,
    #[serde(default)]
    channel_file: Option<PathBuf>,
}

impl SolverConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        let file: ConfigFile = toml::from_str(&content).context("Failed to parse TOML config")?;
        Ok(Self {
            sources: file.source,
            systems: file.system,
        })
    }

    /// Infer [`Device`] definitions from the existing `sources` array.
    pub fn devices(&self) -> Result<Vec<Device>> {
        self.sources
            .iter()
            .map(|s| {
                Device::from_source_config(&s.device, s.gain, s.ppm)
                    .ok_or_else(|| anyhow::anyhow!("Unknown device type: {}", s.device))
            })
            .collect()
    }

    /// Extract all channel frequencies from the systems.
    pub fn channels(&self, config_dir: &Path) -> Result<Vec<Channel>> {
        let mut channels = Vec::new();

        for (i, system) in self.systems.values().enumerate() {
            match system.system_type.as_str() {
                "p25" => {
                    for &freq in &system.control_channels {
                        let tag = match system.preferred_control_channel {
                            Some(pref) if pref == freq => Tag::Primary,
                            Some(_) => Tag::Alternate,
                            None => Tag::Primary,
                        };
                        channels.push(Channel {
                            frequency: freq,
                            signal: Signal::Digital,
                            tag: Some(tag),
                            system: Some(i),
                        });
                    }
                    for &freq in &system.voice_channels {
                        channels.push(Channel {
                            frequency: freq,
                            signal: Signal::Digital,
                            tag: None,
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
