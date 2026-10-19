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

/// Who decodes P25 voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum P25Decoder {
    /// trunk-p25 all the way to audio (op25's vocoders only).
    Rust,
    /// trunk-p25's decoding, regenerated for op25's frame assembler.
    #[default]
    Regen,
    /// op25's GNU Radio chain, kept for A/B replays.
    Op25,
}

/// Shadow mode: run another P25 implementation too, for comparison.
///
/// Beside `rust` or `op25` (`p25_voice_decoder` / `p25_tdma_decoder`) the
/// regenerator runs (`regen`: against `rust`, the same decoding through
/// op25's frame assembler); beside `regen`, op25. Its output is only
/// counted and compared. Costs about twice the decode CPU of whatever it
/// covers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct P25Shadow {
    /// Phase 1 voice.
    #[serde(default)]
    pub voice: bool,
    /// Phase 2 (TDMA) voice.
    #[serde(default)]
    pub tdma: bool,
}

/// When the Rust conditioner restarts a channel's AGC and FLL.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConditionerReset {
    /// Never (state carries over, as the GNU Radio blocks did).
    #[default]
    Never,
    /// When a disabled channel is enabled.
    Enable,
    /// On enable and on every retune.
    Retune,
}

/// Per-channel conditioning (resampler, squelch, AGC, FLL), done in the Rust
/// channelizer.
#[derive(Debug, Deserialize)]
pub struct ConditionerConfig {
    #[serde(default = "default_agc_alpha")]
    pub agc_alpha: f64,
    #[serde(default = "default_agc_reference")]
    pub agc_reference: f64,
    /// FLL loop bandwidth relative to GNU Radio's `2*pi/sps/250`.
    #[serde(default = "default_fll_bw_scale")]
    pub fll_bw_scale: f64,
    #[serde(default)]
    pub reset: ConditionerReset,
}

const fn default_agc_alpha() -> f64 {
    0.45
}
const fn default_agc_reference() -> f64 {
    0.85
}
const fn default_fll_bw_scale() -> f64 {
    1.0
}

impl Default for ConditionerConfig {
    fn default() -> Self {
        Self {
            agc_alpha: default_agc_alpha(),
            agc_reference: default_agc_reference(),
            fll_bw_scale: default_fll_bw_scale(),
            reset: ConditionerReset::Never,
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
    #[serde(default)]
    pub soft_vocoder: bool,
    /// Debug/baseline capture: when set, every channel conditioner and control
    /// decoder writes raw taps (conditioner in/out IQ, demodulated symbols or
    /// dibits, raw decoder messages) into this directory. Used by
    /// `trunk-replay --tap`; leave unset in production.
    #[serde(default)]
    pub tap_dir: Option<String>,
    #[serde(default)]
    pub conditioner: ConditionerConfig,
    /// Who decodes P25 phase 1 voice.
    #[serde(default)]
    pub p25_voice_decoder: P25Decoder,
    /// Who decodes P25 phase 2 (TDMA) voice.
    #[serde(default)]
    pub p25_tdma_decoder: P25Decoder,
    /// Run the other P25 implementation beside each primary, on the same IQ,
    /// to compare them live (`trunk.oxide.shadow.*` metrics).
    #[serde(default)]
    pub p25_shadow: P25Shadow,
    #[serde(default = "default_true")]
    pub record_uu_v_calls: bool,
    #[serde(default = "default_true")]
    pub new_call_from_update: bool,
    #[serde(default)]
    pub default_mode: DefaultMode,
    /// Auto-track each source's clock drift: nudge the per-source software tune
    /// offset (the Hz `freq_correction_hz`) toward whatever zeroes the measured
    /// FLL `freq_error`, so the correction follows TCXO temp/aging drift without
    /// manual re-trimming. `freq_correction_hz` is the starting point. Off by
    /// default; the loop is slow and disruption-free (software offset only).
    #[serde(default)]
    pub adaptive_freq_correction: bool,
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
const fn default_true() -> bool {
    true
}
fn default_debug_address() -> String {
    "127.0.0.1".to_string()
}
const fn default_debug_port() -> i32 {
    1234
}
