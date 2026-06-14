use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub grpc: GrpcConfig,
    pub parakeet: ParakeetConfig,
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
pub struct ParakeetConfig {
    pub model_dir: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Number of permits on the priority admission gate. Decode itself is
    /// serialized on a single model instance, so this primarily bounds how many
    /// requests may be in flight before backpressure; leave at 1 to keep the
    /// priority ordering exact (the gate, not the model mutex, decides order).
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
}

fn default_provider() -> String {
    "cpu".to_string()
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
