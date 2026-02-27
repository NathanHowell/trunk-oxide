pub mod discord;
pub mod recorder;
pub mod service;
pub mod source;
pub mod system;

use std::collections::HashMap;

use anyhow::Result;
use serde::Deserialize;

pub use discord::{ChannelGroup, ChannelMapping, DiscordConfig, NonZeroU64, TalkgroupSelector};
pub use recorder::{DebugRecorderConfig, FrequencyFormat, RecorderConfig};
pub use service::{AudioConfig, GrpcConfig, OxideConfig, WhispererConfig};
pub use source::{SourceConfig, SourceDriver};
pub use system::{
    ChannelConfig, Modulation, SystemConfig, SystemType, TalkgroupConfig, TalkgroupDisplayFormat,
    UnitTagEntry, UnitTagsSettings,
};

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub recorder: Option<RecorderConfig>,
    #[serde(default)]
    pub source: Vec<SourceConfig>,
    #[serde(default)]
    pub system: HashMap<String, SystemConfig>,

    // Service configs
    #[serde(default)]
    pub oxide: Option<OxideConfig>,
    #[serde(default)]
    pub whisperer: Option<WhispererConfig>,
    #[serde(default)]
    pub discord: Option<DiscordConfig>,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub grpc: GrpcConfig,
}

impl Config {
    /// Load and parse a TOML config file.
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_discord_config() {
        let toml = r#"
[discord]
channel_id = 123456789
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let discord = config.discord.unwrap();
        assert_eq!(discord.channel_id.get(), 123_456_789);
        assert!(discord.guild_id.is_none());
        assert!(discord.groups.is_empty());
    }

    #[test]
    fn parse_discord_with_groups() {
        let toml = r#"
[discord]
channel_id = 123456789
guild_id = 987654321

[[discord.groups]]
name = "SMC Sheriff"

[[discord.groups.channels]]
name = "smcso-patrol-dispatch"
talkgroups = [
    { system = "smco", ids = [865, 866] },
]

[[discord.groups.channels]]
name = "smcso-patrol-tac"
talkgroups = [
    { system = "smco", ids = [867, 868, 869] },
]

[[discord.groups]]
name = "San Jose Police"

[[discord.groups.channels]]
name = "sjpd-1-north"
talkgroups = [
    { system = "svrcs", ids = [1551] },
]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let discord = config.discord.unwrap();
        assert_eq!(discord.guild_id.unwrap(), 987_654_321);
        assert_eq!(discord.groups.len(), 2);

        let smc = &discord.groups[0];
        assert_eq!(smc.name, "SMC Sheriff");
        assert_eq!(smc.channels.len(), 2);
        assert_eq!(smc.channels[0].name, "smcso-patrol-dispatch");
        assert_eq!(smc.channels[0].talkgroups[0].system, "smco");
        assert_eq!(smc.channels[0].talkgroups[0].ids, vec![865, 866]);
        assert_eq!(smc.channels[1].talkgroups[0].ids, vec![867, 868, 869]);

        let sjpd = &discord.groups[1];
        assert_eq!(sjpd.name, "San Jose Police");
        assert_eq!(sjpd.channels[0].talkgroups[0].system, "svrcs");
        assert_eq!(sjpd.channels[0].talkgroups[0].ids, vec![1551]);
    }

    #[test]
    fn parse_channel_id_zero_fails() {
        let toml = r#"
[discord]
channel_id = 0
"#;
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn parse_recorder_config() {
        let toml = r#"
[recorder]
temp_dir = "/tmp/test"
call_timeout = 5.0
soft_vocoder = true

[recorder.debug]
enabled = true
port = 5678
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let rec = config.recorder.unwrap();
        assert_eq!(rec.temp_dir, "/tmp/test");
        assert!((rec.call_timeout - 5.0).abs() < f64::EPSILON);
        assert!(rec.soft_vocoder);
        assert!(rec.debug.enabled);
        assert_eq!(rec.debug.port, 5678);
    }

    #[test]
    fn parse_source_config() {
        let toml = r#"
[[source]]
driver = "osmosdr"
device = "rtl=0"
center = 771226551.0
rate = 2472000.0
gain = 36.0
digital_recorders = 8
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.source.len(), 1);
        let src = &config.source[0];
        assert_eq!(src.driver, SourceDriver::Osmosdr);
        assert_eq!(src.device, "rtl=0");
        assert!((src.gain - 36.0).abs() < f64::EPSILON);
        assert_eq!(src.digital_recorders, 8);
    }

    #[test]
    fn parse_system_config() {
        let toml = r#"
[system.smco]
type = "p25"
control_channels = [772031250.0, 772406250.0]
modulation = "qpsk"
record_unknown = true

[[system.smco.talkgroup]]
id = 865
mode = "D"
alpha_tag = "SMC SO Pat 1"
description = "Patrol Primary 1"
tag = "Law Dispatch"
group = "County Sheriff"
priority = 1
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let sys = &config.system["smco"];
        assert_eq!(sys.system_type, SystemType::P25);
        assert_eq!(sys.control_channels.len(), 2);
        assert!(sys.record_unknown);

        assert_eq!(sys.talkgroup.len(), 1);
        let tg = &sys.talkgroup[0];
        assert_eq!(tg.id, 865);
        assert_eq!(tg.mode, "D");
        assert_eq!(tg.alpha_tag, "SMC SO Pat 1");
        assert_eq!(tg.priority, 1);
    }

    #[test]
    fn parse_conventional_system() {
        let toml = r#"
[system.conv]
type = "conventional"

[[system.conv.channel]]
id = 1
freq = 488337500.0
tone = 162.2
alpha_tag = "Menlo Pk PD"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let sys = &config.system["conv"];
        assert_eq!(sys.system_type, SystemType::Conventional);
        assert_eq!(sys.channel.len(), 1);
        let ch = &sys.channel[0];
        assert_eq!(ch.id, 1);
        assert!((ch.freq - 488_337_500.0).abs() < f64::EPSILON);
        assert!((ch.tone - 162.2).abs() < f64::EPSILON);
        assert_eq!(ch.alpha_tag, "Menlo Pk PD");
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
[recorder]
temp_dir = "/dev/shm"
call_timeout = 3.0

[[source]]
driver = "osmosdr"
device = "rtl=0"
center = 771226551.0
rate = 2472000.0
gain = 36.0
digital_recorders = 8

[system.smco]
type = "p25"
control_channels = [772031250.0]
modulation = "qpsk"

[[system.smco.talkgroup]]
id = 865
mode = "D"
priority = 1

[oxide]
socket_path = "/run/trunk-oxide/grpc.sock"

[discord]
channel_id = 123456789
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.recorder.is_some());
        assert_eq!(config.source.len(), 1);
        assert_eq!(config.system.len(), 1);
        assert!(config.oxide.is_some());
        assert!(config.discord.is_some());
    }

    #[test]
    fn parse_real_discord_config() {
        let toml =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config-discord.toml"))
                .unwrap();
        let config: Config = toml::from_str(&toml).unwrap();
        let discord = config.discord.expect("missing discord section");
        assert!(!discord.groups.is_empty());

        let total_tgs: usize = discord
            .groups
            .iter()
            .flat_map(|g| &g.channels)
            .flat_map(|c| &c.talkgroups)
            .map(|s| s.ids.len())
            .sum();
        assert!(total_tgs > 100, "expected 100+ mapped talkgroups, got {total_tgs}");
    }
}
