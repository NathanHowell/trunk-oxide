use anyhow::Result;
use serde::Deserialize;
use serenity::model::id::{ChannelId, GuildId};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub oxide: OxideConfig,
    pub whisperer: WhispererConfig,
    pub discord: DiscordConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub grpc: GrpcConfig,
}

#[derive(Debug, Deserialize)]
pub struct OxideConfig {
    #[serde(default = "default_oxide_socket")]
    pub socket_path: String,
}

fn default_oxide_socket() -> String {
    "/run/trunk-oxide/grpc.sock".to_string()
}

#[derive(Debug, Deserialize)]
pub struct WhispererConfig {
    #[serde(default = "default_whisperer_socket")]
    pub socket_path: String,
}

fn default_whisperer_socket() -> String {
    "/run/trunk-whisperer/grpc.sock".to_string()
}

#[derive(Debug, Deserialize)]
pub struct DiscordConfig {
    #[serde(default)]
    pub bot_token: String,
    #[serde(deserialize_with = "deserialize_channel_id")]
    pub channel_id: ChannelId,
    #[serde(default)]
    pub guild_id: Option<GuildId>,
    #[serde(default)]
    pub groups: Vec<ChannelGroup>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelGroup {
    pub name: String,
    #[serde(default)]
    pub id: Option<ChannelId>,
    pub channels: Vec<ChannelMapping>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelMapping {
    pub name: String,
    #[serde(default)]
    pub id: Option<ChannelId>,
    pub talkgroups: Vec<TalkgroupSelector>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TalkgroupSelector {
    pub system: String,
    pub ids: Vec<i64>,
}

fn deserialize_channel_id<'de, D: serde::Deserializer<'de>>(d: D) -> Result<ChannelId, D::Error> {
    let id = u64::deserialize(d)?;
    std::num::NonZeroU64::new(id)
        .map(ChannelId::from)
        .ok_or_else(|| serde::de::Error::custom("channel_id must be non-zero"))
}


#[derive(Debug, Deserialize)]
pub struct AudioConfig {
    #[serde(default = "default_buffer_duration_ms")]
    pub buffer_duration_ms: u64,
    #[serde(default = "default_tone_detection")]
    pub tone_detection: bool,
    #[serde(default = "default_tone_detection_duration_s")]
    pub tone_detection_duration_s: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            buffer_duration_ms: default_buffer_duration_ms(),
            tone_detection: default_tone_detection(),
            tone_detection_duration_s: default_tone_detection_duration_s(),
        }
    }
}

const fn default_buffer_duration_ms() -> u64 {
    10000
}
const fn default_tone_detection() -> bool {
    true
}
const fn default_tone_detection_duration_s() -> f32 {
    5.0
}

#[derive(Debug, Deserialize)]
pub struct GrpcConfig {
    #[serde(default = "default_grpc_listen_path")]
    pub listen_path: String,
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            listen_path: default_grpc_listen_path(),
        }
    }
}

fn default_grpc_listen_path() -> String {
    "/run/service/grpc.sock".to_string()
}

impl Config {
    pub fn load(config_path: &str, token_override: Option<&str>) -> Result<Self> {
        let content = std::fs::read_to_string(config_path)?;
        let mut config: Self = toml::from_str(&content)?;

        if let Some(token) = token_override {
            token.trim().clone_into(&mut config.discord.bot_token);
        } else if let Ok(token) = std::env::var("DISCORD_TOKEN") {
            token.trim().clone_into(&mut config.discord.bot_token);
        }

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let toml = r#"
[oxide]
[whisperer]
[discord]
channel_id = 123456789
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.discord.channel_id.get(), 123456789);
        assert!(config.discord.guild_id.is_none());
        assert!(config.discord.groups.is_empty());
    }

    #[test]
    fn parse_config_with_groups() {
        let toml = r#"
[oxide]
[whisperer]
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
        assert_eq!(config.discord.guild_id.unwrap().get(), 987654321);
        assert_eq!(config.discord.groups.len(), 2);

        let smc = &config.discord.groups[0];
        assert_eq!(smc.name, "SMC Sheriff");
        assert_eq!(smc.channels.len(), 2);
        assert_eq!(smc.channels[0].name, "smcso-patrol-dispatch");
        assert_eq!(smc.channels[0].talkgroups[0].system, "smco");
        assert_eq!(smc.channels[0].talkgroups[0].ids, vec![865, 866]);
        assert_eq!(smc.channels[1].talkgroups[0].ids, vec![867, 868, 869]);

        let sjpd = &config.discord.groups[1];
        assert_eq!(sjpd.name, "San Jose Police");
        assert_eq!(sjpd.channels[0].talkgroups[0].system, "svrcs");
        assert_eq!(sjpd.channels[0].talkgroups[0].ids, vec![1551]);
    }

    #[test]
    fn parse_real_config() {
        let toml =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config-discord.toml"))
                .unwrap();
        let config: Config = toml::from_str(&toml).unwrap();
        assert!(!config.discord.groups.is_empty());

        // Count total talkgroup mappings
        let total_tgs: usize = config
            .discord
            .groups
            .iter()
            .flat_map(|g| &g.channels)
            .flat_map(|c| &c.talkgroups)
            .map(|s| s.ids.len())
            .sum();
        assert!(total_tgs > 100, "expected 100+ mapped talkgroups, got {total_tgs}");
    }

    #[test]
    fn parse_config_channel_id_zero_fails() {
        let toml = r#"
[oxide]
[whisperer]
[discord]
channel_id = 0
"#;
        let result: Result<Config, _> = toml::from_str(toml);
        assert!(result.is_err());
    }
}
