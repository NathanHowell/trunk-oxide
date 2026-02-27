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
    #[serde(default)]
    pub ppm: f64,
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
    #[serde(default)]
    pub auto_tune: bool,
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
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Named gain stages, e.g. `{ "IF" = 20.0, "LNA" = 15.0 }`.
    #[serde(default)]
    pub gain_settings: HashMap<String, f64>,

    // SigMF-specific
    #[serde(default)]
    pub sigmf_meta: String,
    #[serde(default)]
    pub sigmf_data: String,

    // IQ file-specific
    #[serde(default)]
    pub iq_file: String,
    #[serde(default)]
    pub iq_type: String,
    #[serde(default)]
    pub repeat: bool,
}
