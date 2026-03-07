use serde::Deserialize;

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
pub struct AudioConfig {
    #[serde(default = "default_buffer_duration_ms")]
    pub buffer_duration_ms: u64,
    #[serde(default = "default_tone_detection")]
    pub tone_detection: bool,
    #[serde(default = "default_tone_detection_duration_s")]
    pub tone_detection_duration_s: f32,
    #[serde(default = "default_audio_attachment")]
    pub audio_attachment: bool,
    #[serde(default)]
    pub coalesce_timeout_ms: u64,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            buffer_duration_ms: default_buffer_duration_ms(),
            tone_detection: default_tone_detection(),
            tone_detection_duration_s: default_tone_detection_duration_s(),
            audio_attachment: default_audio_attachment(),
            coalesce_timeout_ms: 0,
        }
    }
}

const fn default_buffer_duration_ms() -> u64 {
    5000
}
const fn default_tone_detection() -> bool {
    true
}
const fn default_tone_detection_duration_s() -> f32 {
    5.0
}
const fn default_audio_attachment() -> bool {
    true
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
