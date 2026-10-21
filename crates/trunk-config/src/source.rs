use serde::Deserialize;

use crate::Tunable;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceDriver {
    /// gr-osmosdr inside GNU Radio (any device it supports).
    Osmosdr,
    /// An RTL-SDR dongle opened and streamed by trunk-oxide itself (librtlsdr,
    /// no GNU Radio source block). `device` is the serial, with or without
    /// the `rtl=` prefix.
    Rtlsdr,
    Usrp,
    /// A raw IQ file (`device` is its path, `iq_format` its sample
    /// format) played at `rate` by trunk-oxide.
    Iqfile,
}

/// What a source's channelizer may spend on its forward FFT.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fft {
    /// Refuse to start at a rate whose FFT has a large prime stage (say
    /// 2.472 MS/s, 103 x 24 kHz, where the FFT cost 2.8x a smooth size).
    #[default]
    Fast,
    /// Start at any rate.
    Slow,
}

fn default_iq_format() -> String {
    "cf32".to_string()
}

const fn default_true() -> bool {
    true
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
    /// Fine clock correction in Hz, applied in software as a per-channel tune
    /// offset (`tune = chan_freq + freq_correction_hz`). This is the everyday
    /// knob: set it to the negative of the measured `freq_error` for the source
    /// (so `trunk.oxide.freq_error_hz{source}` centers on 0). Hz-precise, unlike
    /// the integer hardware ppm. Only the physical tune shifts — the displayed
    /// call frequency is unaffected. Within a dongle's narrow band a fixed Hz ≈
    /// a fixed ppm, so per-channel scaling isn't needed.
    #[serde(default)]
    pub freq_correction_hz: f64,
    /// Tuner gain, dB. `"auto"` (the default): an `rtlsdr` source sweeps its
    /// gain steps at startup and takes the lowest gain within a small SNR
    /// penalty of the best that stays clear of overload, re-checking the
    /// last sweep's pick on later starts. A number pins it.
    #[serde(default)]
    pub gain: Tunable<f64>,
    #[serde(default)]
    pub agc: bool,
    #[serde(default)]
    pub signal_detector_threshold: Option<f32>,
    /// `"fast"` (the default) fails startup if `rate` makes the channelizer's
    /// forward FFT slow; `"slow"` accepts it. `trunk-solver` plans fast rates.
    #[serde(default)]
    pub fft: Fft,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// `iqfile` driver only: sample format of the file, `cf32` (interleaved
    /// float32, the default) or `cu8` (interleaved unsigned 8-bit, as written
    /// by `rtl_sdr`).
    #[serde(default = "default_iq_format")]
    pub iq_format: String,
    /// `iqfile` driver only: loop the file forever (default) or play it once.
    #[serde(default = "default_true")]
    pub iq_repeat: bool,
}
