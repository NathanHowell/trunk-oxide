use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub grpc: GrpcConfig,
    pub whistle: WhistleConfig,
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
pub struct WhistleConfig {
    /// Path to the `whistle.cact` weights file.
    pub model_path: String,
    /// Force a language (`en`, `de`, `fr`, `es`, `it`, `nl`, `pl`). Unset
    /// auto-detects per clip, which can misfire on short radio chatter.
    #[serde(default = "default_language")]
    pub language: Option<String>,
    /// Words/phrases to bias decoding toward (e.g. unit names, street names).
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Number of permits on the priority admission gate. The engine holds one
    /// process-global, non-thread-safe model, so decode is serialized anyway;
    /// leave at 1 to keep the priority ordering exact.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
}

#[allow(clippy::unnecessary_wraps)]
fn default_language() -> Option<String> {
    Some("en".to_string())
}

const fn default_max_concurrent() -> usize {
    1
}

impl Config {
    pub fn load(config_path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(config_path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }
}
