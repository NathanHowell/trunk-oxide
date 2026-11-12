pub mod discord;
pub mod recorder;
pub mod service;
pub mod source;
pub mod system;
pub mod tunable;

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

pub use discord::{ChannelGroup, ChannelMapping, DiscordConfig, NonZeroU64, TalkgroupSelector};
pub use recorder::{ConditionerConfig, ConditionerReset, DefaultMode, RecorderConfig};
pub use service::{AudioConfig, GrpcConfig, MonitorConfig, OxideConfig, WhispererConfig};
pub use source::{SourceConfig, SourceDriver};
pub use system::{
    ChannelConfig, Modulation, SystemConfig, SystemType, TalkgroupConfig, TalkgroupMode,
};
pub use tunable::Tunable;

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
    pub monitor: Option<MonitorConfig>,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub grpc: GrpcConfig,
}

impl Config {
    /// Load and parse a TOML config file.
    ///
    /// For systems with a `talkgroup_file` or `channel_file`, entries are loaded
    /// from the CSV and then any inline TOML entries are applied as overlays —
    /// only explicitly-set TOML fields override CSV values.
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Self = toml::from_str(&content)?;

        // Check if any system uses CSV files
        let needs_resolve = config
            .system
            .values()
            .any(|sys| sys.talkgroup_file.is_some() || sys.channel_file.is_some());

        if needs_resolve {
            let config_dir = Path::new(path).parent().unwrap_or_else(|| Path::new("."));

            // Re-parse raw TOML to detect which fields were explicitly set
            let raw: toml::Value = toml::from_str(&content)?;
            let raw_systems = raw.get("system").and_then(|v| v.as_table());

            for (name, sys) in &mut config.system {
                if sys.talkgroup_file.is_some() {
                    let raw_talkgroups = raw_systems
                        .and_then(|s| s.get(name))
                        .and_then(|v| v.get("talkgroup"))
                        .and_then(|v| v.as_array())
                        .map(std::vec::Vec::as_slice)
                        .unwrap_or_default();

                    sys.talkgroup = system::resolve_talkgroups(
                        config_dir,
                        sys.talkgroup_file.as_deref(),
                        raw_talkgroups,
                    )?;
                }

                if sys.channel_file.is_some() {
                    let raw_channels = raw_systems
                        .and_then(|s| s.get(name))
                        .and_then(|v| v.get("channel"))
                        .and_then(|v| v.as_array())
                        .map(std::vec::Vec::as_slice)
                        .unwrap_or_default();

                    sys.channel = system::resolve_channels(
                        config_dir,
                        sys.channel_file.as_deref(),
                        raw_channels,
                    )?;
                }
            }
        }

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
    fn parse_monitor() {
        let toml = r#"
[monitor]
channel_id = 555
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let monitor = config.monitor.unwrap();
        assert_eq!(monitor.channel_id.get(), 555);
        // the rest is what the compose network has
        assert_eq!(monitor.victoriametrics_url, "http://victoriametrics:8428");
        assert_eq!(monitor.refresh_secs, 300);
        assert!(monitor.bot_token.is_empty());
    }

    #[test]
    fn a_monitor_needs_its_channel() {
        assert!(toml::from_str::<Config>("[monitor]\nrefresh_secs = 60\n").is_err());
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
max_channels = 12
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let rec = config.recorder.unwrap();
        assert_eq!(rec.temp_dir, "/tmp/test");
        assert!((rec.call_timeout - 5.0).abs() < f64::EPSILON);
        assert_eq!(rec.max_channels, Some(12));
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

"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.source.len(), 1);
        let src = &config.source[0];
        assert_eq!(src.driver, SourceDriver::Osmosdr);
        assert_eq!(src.device, "rtl=0");
        assert_eq!(src.gain, crate::Tunable::Fixed(36.0));
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
        assert_eq!(tg.mode, crate::TalkgroupMode::Digital);
        assert_eq!(tg.alpha_tag, "SMC SO Pat 1");
        assert_eq!(tg.priority, Some(1));
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
    fn parse_csv_talkgroups() {
        use std::path::Path;
        let csv_path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../talkgroups.csv.example"
        ));
        let tgs = crate::system::load_talkgroups_csv(csv_path).unwrap();
        assert_eq!(tgs.len(), 7);

        let tg101 = &tgs[&101];
        assert_eq!(tg101.id, 101);
        assert_eq!(tg101.mode, crate::TalkgroupMode::Digital);
        assert_eq!(tg101.alpha_tag, "PD Dispatch");
        assert_eq!(tg101.tag, "Law Dispatch");
        assert_eq!(tg101.group, "City Police");
        assert_eq!(tg101.priority, Some(1));

        assert_eq!(tgs[&103].mode, crate::TalkgroupMode::TrunkedEncrypted);
        assert_eq!(tgs[&103].priority, None);
    }

    #[test]
    fn csv_with_toml_overlay() {
        // Write a temp CSV
        let dir = std::env::temp_dir().join("trunk-config-test-overlay");
        let _ = std::fs::create_dir_all(&dir);
        let csv_path = dir.join("talkgroups.csv");
        std::fs::write(
            &csv_path,
            "Decimal,Hex,Mode,Alpha Tag,Description,Tag,Category,Priority\n\
             100,64,D,TG 100,Test talkgroup 100,Law Dispatch,Police,5\n\
             200,C8,D,TG 200,Test talkgroup 200,Fire,Fire Dept,3\n",
        )
        .unwrap();

        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"
[system.test]
type = "p25"
talkgroup_file = "{}"

# Override just priority for TG 100
[[system.test.talkgroup]]
id = 100
priority = 0

# Add a new TG not in CSV
[[system.test.talkgroup]]
id = 999
mode = "E"
alpha_tag = "New TG"
"#,
                csv_path.file_name().unwrap().to_str().unwrap()
            ),
        )
        .unwrap();

        let config = Config::load(config_path.to_str().unwrap()).unwrap();
        let sys = &config.system["test"];

        // TG 100: priority overridden, other fields from CSV
        let tg100 = sys.talkgroup.iter().find(|t| t.id == 100).unwrap();
        assert_eq!(tg100.priority, Some(0)); // overridden
        assert_eq!(tg100.alpha_tag, "TG 100"); // from CSV
        assert_eq!(tg100.description, "Test talkgroup 100"); // from CSV
        assert_eq!(tg100.tag, "Law Dispatch"); // from CSV
        assert_eq!(tg100.group, "Police"); // from CSV
        assert_eq!(tg100.mode, crate::TalkgroupMode::Digital); // from CSV

        // TG 200: untouched, all from CSV
        let tg200 = sys.talkgroup.iter().find(|t| t.id == 200).unwrap();
        assert_eq!(tg200.alpha_tag, "TG 200");
        assert_eq!(tg200.priority, Some(3));

        // TG 999: new entry from TOML only
        let tg999 = sys.talkgroup.iter().find(|t| t.id == 999).unwrap();
        assert_eq!(tg999.mode, crate::TalkgroupMode::Encrypted);
        assert_eq!(tg999.alpha_tag, "New TG");
        assert_eq!(tg999.priority, None); // unset

        // Sorted by ID
        let ids: Vec<i64> = sys.talkgroup.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![100, 200, 999]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn toml_only_talkgroups_no_csv() {
        let toml = r#"
[system.test]
type = "p25"

[[system.test.talkgroup]]
id = 42
mode = "D"
alpha_tag = "Direct TG"
priority = 1
"#;
        let config: Config = toml::from_str(toml).unwrap();
        let sys = &config.system["test"];
        assert_eq!(sys.talkgroup.len(), 1);
        assert_eq!(sys.talkgroup[0].id, 42);
        assert_eq!(sys.talkgroup[0].alpha_tag, "Direct TG");
        assert_eq!(sys.talkgroup[0].priority, Some(1));
    }

    #[test]
    fn csv_only_no_toml_talkgroups() {
        let dir = std::env::temp_dir().join("trunk-config-test-csv-only");
        let _ = std::fs::create_dir_all(&dir);
        let csv_path = dir.join("talkgroups.csv");
        std::fs::write(
            &csv_path,
            "Decimal,Mode,Alpha Tag,Description,Tag,Category,Priority\n\
             300,D,TG 300,Test,Law,Police,2\n",
        )
        .unwrap();

        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[system.test]
type = "p25"
talkgroup_file = "talkgroups.csv"
"#,
        )
        .unwrap();

        let config = Config::load(config_path.to_str().unwrap()).unwrap();
        let sys = &config.system["test"];
        assert_eq!(sys.talkgroup.len(), 1);
        assert_eq!(sys.talkgroup[0].id, 300);
        assert_eq!(sys.talkgroup[0].alpha_tag, "TG 300");
        assert_eq!(sys.talkgroup[0].priority, Some(2));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channel_file_csv() {
        let dir = std::env::temp_dir().join("trunk-config-test-channels");
        let _ = std::fs::create_dir_all(&dir);
        let csv_path = dir.join("channels.csv");
        std::fs::write(
            &csv_path,
            "TG Number,Frequency,Tone,Alpha Tag,Description,Tag,Category\n\
             1,488337500,162.2,Menlo Pk PD,Menlo Park PD Dispatch,Law Dispatch,Menlo Park\n\
             2,489087500,162.2,Atherton PD,Atherton PD Dispatch,Law Dispatch,Atherton\n",
        )
        .unwrap();

        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[system.conv]
type = "conventional"
channel_file = "channels.csv"
"#,
        )
        .unwrap();

        let config = Config::load(config_path.to_str().unwrap()).unwrap();
        let sys = &config.system["conv"];
        assert_eq!(sys.channel.len(), 2);

        let ch1 = &sys.channel[0];
        assert_eq!(ch1.id, 1);
        assert!((ch1.freq - 488_337_500.0).abs() < f64::EPSILON);
        assert!((ch1.tone - 162.2).abs() < f64::EPSILON);
        assert_eq!(ch1.alpha_tag, "Menlo Pk PD");
        assert_eq!(ch1.group, "Menlo Park");

        let ch2 = &sys.channel[1];
        assert_eq!(ch2.id, 2);
        assert_eq!(ch2.alpha_tag, "Atherton PD");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn channel_file_with_overlay() {
        let dir = std::env::temp_dir().join("trunk-config-test-channel-overlay");
        let _ = std::fs::create_dir_all(&dir);
        let csv_path = dir.join("channels.csv");
        std::fs::write(
            &csv_path,
            "TG Number,Frequency,Tone,Alpha Tag,Description,Tag,Category\n\
             1,488337500,162.2,Menlo Pk PD,Menlo Park PD Dispatch,Law Dispatch,Menlo Park\n",
        )
        .unwrap();

        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[system.conv]
type = "conventional"
channel_file = "channels.csv"

# Override squelch for channel 1
[[system.conv.channel]]
id = 1
squelch_db = -40.0

# Add a new channel not in CSV
[[system.conv.channel]]
id = 99
freq = 500000000.0
alpha_tag = "Custom Ch"
"#,
        )
        .unwrap();

        let config = Config::load(config_path.to_str().unwrap()).unwrap();
        let sys = &config.system["conv"];
        assert_eq!(sys.channel.len(), 2);

        let ch1 = sys.channel.iter().find(|c| c.id == 1).unwrap();
        assert_eq!(ch1.squelch_db, Some(-40.0)); // overridden
        assert_eq!(ch1.alpha_tag, "Menlo Pk PD"); // from CSV

        let ch99 = sys.channel.iter().find(|c| c.id == 99).unwrap();
        assert!((ch99.freq - 500_000_000.0).abs() < f64::EPSILON);
        assert_eq!(ch99.alpha_tag, "Custom Ch");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_example_configs_parse() {
        let read = |name: &str| {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(name);
            let text = std::fs::read_to_string(&path).unwrap();
            toml::from_str::<Config>(&text).unwrap_or_else(|e| panic!("{name}: {e}"))
        };

        let oxide = read("config.toml.example");
        assert!(oxide.recorder.is_some());
        assert_eq!(oxide.source.len(), 1);
        assert_eq!(oxide.system.len(), 2);

        let discord = read("config-discord.toml.example")
            .discord
            .expect("missing discord section");
        let mapped: usize = discord
            .groups
            .iter()
            .flat_map(|g| &g.channels)
            .flat_map(|c| &c.talkgroups)
            .map(|s| s.ids.len())
            .sum();
        assert_eq!(mapped, 5);

        assert!(read("config-monitor.toml.example").monitor.is_some());
    }
}
