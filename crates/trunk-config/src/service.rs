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
    #[serde(default = "default_coalesce_timeout_ms")]
    pub coalesce_timeout_ms: u64,
    /// Hard ceiling on a single whisper transcription request. A request that
    /// exceeds this is abandoned and treated as empty, so one runaway (e.g.
    /// CPU-only whisper hallucination loop) cannot wedge the call handler.
    #[serde(default = "default_transcription_timeout_s")]
    pub transcription_timeout_s: f64,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            buffer_duration_ms: default_buffer_duration_ms(),
            tone_detection: default_tone_detection(),
            tone_detection_duration_s: default_tone_detection_duration_s(),
            audio_attachment: default_audio_attachment(),
            coalesce_timeout_ms: default_coalesce_timeout_ms(),
            transcription_timeout_s: default_transcription_timeout_s(),
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
const fn default_coalesce_timeout_ms() -> u64 {
    10_000
}
const fn default_transcription_timeout_s() -> f64 {
    30.0
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
