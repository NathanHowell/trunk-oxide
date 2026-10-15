use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceDriver {
    Osmosdr,
    Usrp,
    Sigmf,
    Iqfile,
}

#[derive(Debug, Deserialize)]
pub struct SourceConfig {
    pub driver: SourceDriver,
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub center: f64,
    #[serde(default)]
    pub rate: f64,
    #[serde(default)]
    pub error: f64,
    /// Coarse clock correction in parts-per-million, sent to the rtl-sdr hardware
    /// knob (`set_freq_corr`, rounded to integer — the hardware only does whole
    /// ppm). Use this for large/wideband offsets (≥1 ppm). For the sub-ppm
    /// per-dongle bias we actually see, use `freq_correction_hz` instead — it's
    /// Hz-precise and is what the FLL measures.
    #[serde(default)]
    pub ppm: f64,
    /// Fine clock correction in Hz, applied in software as a per-recorder tune
    /// offset (`tune = chan_freq + freq_correction_hz`). This is the everyday
    /// knob: set it to the negative of the measured `freq_error` for the source
    /// (so `trunk.oxide.freq_error_hz{source}` centers on 0). Hz-precise, unlike
    /// the integer hardware ppm. Only the physical tune shifts — the displayed
    /// call frequency is unaffected. Within a dongle's narrow band a fixed Hz ≈
    /// a fixed ppm, so per-channel scaling isn't needed.
    #[serde(default)]
    pub freq_correction_hz: f64,
    #[serde(default)]
    pub gain: f64,
    #[serde(default)]
    pub if_gain: f64,
    #[serde(default)]
    pub bb_gain: f64,
    #[serde(default)]
    pub mix_gain: f64,
    #[serde(default)]
    pub lna_gain: f64,
    #[serde(default)]
    pub tia_gain: f64,
    #[serde(default)]
    pub pga_gain: f64,
    #[serde(default)]
    pub amp_gain: f64,
    #[serde(default)]
    pub vga_gain: f64,
    #[serde(default)]
    pub vga1_gain: f64,
    #[serde(default)]
    pub vga2_gain: f64,
    #[serde(default)]
    pub agc: bool,
    /// When true, sweep the device's gain steps at startup and pick the gain
    /// with the best noise figure that stays clear of overload, ignoring the
    /// configured `gain` (which becomes a fallback if calibration fails).
    /// Only supported for the `osmosdr` driver.
    #[serde(default)]
    pub auto_gain: bool,
    #[serde(default)]
    pub antenna: String,
    #[serde(default)]
    pub silence_frames: i32,
    #[serde(default)]
    pub signal_detector_threshold: Option<f32>,
    #[serde(default)]
    pub digital_recorders: i32,
    #[serde(default)]
    pub analog_recorders: i32,
    #[serde(default)]
    pub sigmf_recorders: i32,
    /// Route this source's P25 voice recorders through one shared
    /// fast-convolution channelizer (a single forward FFT per block for all
    /// recorders) instead of a private frequency-translating filter per
    /// recorder. Much cheaper with several recorders. Analog, DMR, SigMF and
    /// control-channel decoders are unaffected.
    #[serde(default)]
    pub shared_channelizer: bool,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Named gain stages, e.g. `{ "IF" = 20.0, "LNA" = 15.0 }`.
    #[serde(default)]
    pub gain_settings: HashMap<String, f64>,
}
