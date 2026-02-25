use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub grpc: GrpcConfig,
    pub discord: DiscordConfig,
    pub whisper: WhisperConfig,
    #[serde(default)]
    pub audio: AudioConfig,
}

#[derive(Debug, Deserialize)]
pub struct GrpcConfig {
    #[serde(default = "default_listen_path")]
    pub listen_path: String,
}

fn default_listen_path() -> String {
    "/var/run/trunk-oxide/trunk-oxide.sock".to_string()
}

#[derive(Debug, Deserialize)]
pub struct DiscordConfig {
    #[serde(default)]
    pub bot_token: String,
    pub channel_id: u64,
}

#[derive(Debug, Deserialize)]
pub struct WhisperConfig {
    pub model_path: String,
    #[serde(default = "default_gpu_count")]
    pub gpu_count: usize,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_flash_attn")]
    pub flash_attn: bool,
    #[serde(default = "default_language")]
    pub language: String,
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

const fn default_gpu_count() -> usize {
    2
}
const fn default_max_concurrent() -> usize {
    4
}
const fn default_flash_attn() -> bool {
    true
}
fn default_language() -> String {
    "en".to_string()
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
        let content = std::fs::read_to_string("/etc/trunk-whisperer/config.toml")?;
        let mut config: Self = toml::from_str(&content)?;

        if let Ok(token) = std::env::var("DISCORD_TOKEN") {
            config.discord.bot_token = token;
        }

        Ok(config)
    }
}
