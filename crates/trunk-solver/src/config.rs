//! The devices and channels of a trunk-oxide config, read with trunk-config:
//! the parser trunk-oxide itself uses, CSV overlays included, so the solver
//! plans for exactly the channels the scanner will open.

use std::path::Path;

use anyhow::{Context, Result};
use trunk_config::{Config, SystemType};

use crate::device::Device;
use crate::{Channel, Signal, Tag};

pub struct SolverConfig {
    config: Config,
}

#[allow(clippy::cast_possible_truncation)]
const fn hz(f: f64) -> i64 {
    f.round() as i64
}

impl SolverConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let path_str = path.to_str().context("the config path is not UTF-8")?;
        let config = Config::load(path_str)
            .with_context(|| format!("reading the config {}", path.display()))?;
        Ok(Self { config })
    }

    /// [`Device`]s inferred from the config's sources.
    pub fn devices(&self) -> Result<Vec<Device>> {
        self.config
            .source
            .iter()
            .map(|s| {
                Device::from_source_config(&s.device, hz(s.gain), hz(s.ppm))
                    .ok_or_else(|| anyhow::anyhow!("Unknown device type: {}", s.device))
            })
            .collect()
    }

    /// Every channel the systems will open: control and voice channels of
    /// the trunked ones, the conventional ones' channels.
    pub fn channels(&self) -> Vec<Channel> {
        let mut systems: Vec<_> = self.config.system.iter().collect();
        systems.sort_by_key(|(name, _)| name.as_str());
        let mut channels = Vec::new();
        for (i, (_, sys)) in systems.into_iter().enumerate() {
            match sys.system_type {
                SystemType::P25 => {
                    let preferred = sys.preferred_control_channel.map(hz);
                    channels.extend(sys.control_channels.iter().map(|&f| Channel {
                        frequency: hz(f),
                        signal: Signal::Digital,
                        tag: Some(match preferred {
                            Some(p) if p != hz(f) => Tag::Alternate,
                            _ => Tag::Primary,
                        }),
                        system: Some(i),
                    }));
                    channels.extend(sys.voice_channels.iter().map(|&f| Channel {
                        frequency: hz(f),
                        signal: Signal::Digital,
                        tag: None,
                        system: Some(i),
                    }));
                }
                SystemType::Conventional | SystemType::ConventionalP25 => {
                    let signal = if sys.system_type == SystemType::Conventional {
                        Signal::Analog
                    } else {
                        Signal::Digital
                    };
                    channels.extend(sys.channel.iter().map(|c| Channel {
                        frequency: hz(c.freq),
                        signal,
                        tag: None,
                        system: None,
                    }));
                }
                _ => {}
            }
        }
        channels
    }
}
