use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct OxideConfig {
    #[serde(default = "default_oxide_socket")]
    pub socket_path: String,
    /// The least `cpu.weight` some cgroup the scanner runs in must have, or
    /// it will not start (0: no check). See `deploy/systemd/user.slice.d/`.
    #[serde(default = "default_min_cpu_weight")]
    pub min_cpu_weight: u32,
}

impl Default for OxideConfig {
    fn default() -> Self {
        Self {
            socket_path: default_oxide_socket(),
            min_cpu_weight: default_min_cpu_weight(),
        }
    }
}

fn default_oxide_socket() -> String {
    "/run/trunk-oxide/grpc.sock".to_string()
}

const fn default_min_cpu_weight() -> u32 {
    1000
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
    /// Safety cap on how much audio a single transcription request may carry.
    /// Audio is normally transcribed one request per PTT transmission (flushed
    /// at each squelch/PTT boundary and at call end); this only force-flushes a
    /// sub-segment for a pathologically long transmission, to bound memory and
    /// decode latency. Not the normal segmentation unit.
    #[serde(default = "default_max_segment_ms")]
    pub max_segment_ms: u64,
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
            max_segment_ms: default_max_segment_ms(),
            audio_attachment: default_audio_attachment(),
            coalesce_timeout_ms: default_coalesce_timeout_ms(),
            transcription_timeout_s: default_transcription_timeout_s(),
        }
    }
}

const fn default_max_segment_ms() -> u64 {
    120_000
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
