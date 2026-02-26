#[derive(Debug, Clone)]
pub enum SampleRate {
    Exact(i64),
    Range(i64, i64),
}

#[derive(Debug, Clone)]
pub struct Device {
    pub sample_rates: Vec<SampleRate>,
    pub ppm: i64,
    pub driver: String,
    pub device_template: String,
    pub gain: i64,
    /// Sample rate must be a multiple of this value (driver requirement).
    /// For osmosdr sources, trunk-recorder requires a multiple of 24000.
    pub rate_multiple: i64,
}

impl Device {
    #[must_use]
    pub fn rtlsdr() -> Self {
        Self {
            sample_rates: vec![
                SampleRate::Range(225_001, 300_000),
                SampleRate::Range(900_001, 2_560_000),
            ],
            ppm: 1,
            driver: "osmosdr".into(),
            device_template: "rtl={}".into(),
            gain: 24,
            rate_multiple: 24_000,
        }
    }

    #[must_use]
    pub fn airspy() -> Self {
        Self {
            sample_rates: vec![
                SampleRate::Exact(2_500_000),
                SampleRate::Exact(10_000_000),
            ],
            ppm: 1,
            driver: "osmosdr".into(),
            device_template: "airspy={}".into(),
            gain: 24,
            rate_multiple: 24_000,
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "rtlsdr" => Some(Self::rtlsdr()),
            "airspy" => Some(Self::airspy()),
            _ => None,
        }
    }

    /// Infer device type from a trunk-recorder source `device` string
    /// (e.g. `"rtl=0"`, `"airspy=1"`), overriding gain and ppm with
    /// the values from the existing config.
    #[must_use]
    pub fn from_source_config(device_str: &str, gain: i64, ppm: i64) -> Option<Self> {
        let prefix = device_str.split('=').next()?;
        let mut device = match prefix {
            "rtl" => Self::rtlsdr(),
            "airspy" => Self::airspy(),
            _ => return None,
        };
        device.gain = gain;
        device.ppm = ppm;
        Some(device)
    }
}
