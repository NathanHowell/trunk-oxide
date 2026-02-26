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
}
