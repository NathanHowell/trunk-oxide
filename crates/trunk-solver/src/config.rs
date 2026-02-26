use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::{Channel, Signal, Tag};

pub fn parse_config(path: &Path) -> Result<Vec<Channel>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;
    let config: serde_json::Value =
        serde_json::from_str(&content).context("Failed to parse config JSON")?;

    let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut channels = Vec::new();

    let systems = config["systems"]
        .as_array()
        .context("Missing 'systems' array in config")?;

    for (i, system) in systems.iter().enumerate() {
        let sys_type = system["type"].as_str().unwrap_or("");
        match sys_type {
            "p25" => {
                if let Some(control_channels) = system["control_channels"].as_array() {
                    for freq_val in control_channels {
                        if let Some(freq) = freq_val.as_i64() {
                            channels.push(Channel {
                                frequency: freq,
                                signal: Signal::Digital,
                                tag: Some(Tag::Primary),
                                system: Some(i),
                            });
                        }
                    }
                }
            }
            "conventional" | "conventionalP25" => {
                if let Some(channel_file) = system["channelFile"].as_str() {
                    let channel_path = Path::new(channel_file);
                    // Try the path as-is, then look for basename next to the config
                    let resolved = if channel_path.exists() {
                        channel_path.to_path_buf()
                    } else if let Some(filename) = channel_path.file_name() {
                        let candidate = config_dir.join(filename);
                        if candidate.exists() {
                            candidate
                        } else {
                            bail!(
                                "Cannot find channel file: {channel_file} (tried {})",
                                candidate.display()
                            );
                        }
                    } else {
                        bail!("Cannot resolve channel file: {channel_file}");
                    };
                    let csv_channels = parse_conventional_csv(&resolved)?;
                    channels.extend(csv_channels);
                }
            }
            _ => {}
        }
    }

    Ok(channels)
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
