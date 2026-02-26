use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub oxide: OxideConfig,
    pub whisperer: WhispererConfig,
    pub discord: DiscordConfig,
    #[serde(default)]
    pub audio: AudioConfig,
}

#[derive(Debug, Deserialize)]
pub struct OxideConfig {
    #[serde(default = "default_oxide_socket")]
    pub socket_path: String,
}

fn default_oxide_socket() -> String {
    "/var/run/trunk-oxide/trunk-oxide.sock".to_string()
}

#[derive(Debug, Deserialize)]
pub struct WhispererConfig {
    #[serde(default = "default_whisperer_socket")]
    pub socket_path: String,
}

fn default_whisperer_socket() -> String {
    "/var/run/trunk-whisperer/trunk-whisperer.sock".to_string()
}

#[derive(Debug, Deserialize)]
pub struct DiscordConfig {
    #[serde(default)]
    pub bot_token: String,
    pub channel_id: u64,
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

impl Config {
    pub fn load() -> Result<Self> {
        let content = std::fs::read_to_string("/etc/trunk-discord/config.toml")?;
        let mut config: Self = toml::from_str(&content)?;

        if let Ok(token) = std::env::var("DISCORD_TOKEN") {
            config.discord.bot_token = token;
        }

        Ok(config)
    }
}
