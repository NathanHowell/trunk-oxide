use serde::Deserialize;

fn default_temp_dir() -> String {
    if std::path::Path::new("/dev/shm").exists() {
        "/dev/shm".to_string()
    } else {
        std::env::current_dir().map_or_else(|_| ".".to_string(), |p| p.display().to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultMode {
    Analog,
    #[default]
    Digital,
}

impl DefaultMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Analog => "analog",
            Self::Digital => "digital",
        }
    }
}

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct RecorderConfig {
    #[serde(default = "default_temp_dir")]
    pub temp_dir: String,
    #[serde(default = "default_call_timeout")]
    pub call_timeout: f64,
    #[serde(default = "default_control_warn_rate")]
    pub control_warn_rate: i32,
    #[serde(default)]
    pub soft_vocoder: bool,
    #[serde(default = "default_true")]
    pub record_uu_v_calls: bool,
    #[serde(default = "default_true")]
    pub new_call_from_update: bool,
    #[serde(default)]
    pub default_mode: DefaultMode,
    #[serde(default)]
    pub debug: DebugRecorderConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct DebugRecorderConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_debug_address")]
    pub address: String,
    #[serde(default = "default_debug_port")]
    pub port: i32,
}

const fn default_call_timeout() -> f64 {
    3.0
}
const fn default_control_warn_rate() -> i32 {
    10
}
const fn default_true() -> bool {
    true
}
fn default_debug_address() -> String {
    "127.0.0.1".to_string()
}
const fn default_debug_port() -> i32 {
    1234
}
