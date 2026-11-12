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
    /// Sample rate must be a multiple of this value: the channelizer
    /// decimates to its 24 kHz narrow channels by a whole number.
    pub rate_multiple: i64,
    /// The centre it runs at now, if it is already placed: of two plans that
    /// cost the same, the one that moves the dongles least wins.
    pub near: Option<i64>,
}

impl Device {
    #[must_use]
    pub fn rtlsdr() -> Self {
        Self {
            sample_rates: vec![
                SampleRate::Range(225_001, 300_000),
                SampleRate::Range(900_001, 2_496_000),
            ],
            ppm: 1,
            driver: "rtlsdr".into(),
            device_template: "rtl={}".into(),
            rate_multiple: 24_000,
            near: None,
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "rtlsdr" => Some(Self::rtlsdr()),
            _ => None,
        }
    }

    /// Infer the device type from a source's `device` string (`"rtl=SERIAL"`),
    /// overriding ppm with the value from the existing config.
    #[must_use]
    pub fn from_source_config(device_str: &str, ppm: i64) -> Option<Self> {
        let prefix = device_str.split('=').next()?;
        let mut device = match prefix {
            "rtl" => Self::rtlsdr(),
            _ => return None,
        };
        device.ppm = ppm;
        Some(device)
    }
}
