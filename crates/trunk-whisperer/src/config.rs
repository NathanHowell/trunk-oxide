use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub grpc: GrpcConfig,
    pub whisper: WhisperConfig,
}

#[derive(Debug, Deserialize)]
pub struct GrpcConfig {
    #[serde(default = "default_listen_path")]
    pub listen_path: String,
}

fn default_listen_path() -> String {
    "/run/service/grpc.sock".to_string()
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

impl Config {
    pub fn load(config_path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(config_path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }
}
