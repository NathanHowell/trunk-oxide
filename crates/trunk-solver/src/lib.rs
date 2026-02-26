pub mod config;
pub mod device;
pub mod solver;

pub const BANDWIDTH: i64 = 12_500;
pub const HALFWIDTH: i64 = BANDWIDTH / 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Analog,
    Digital,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Primary,
    Alternate,
}

#[derive(Debug, Clone)]
pub struct Channel {
    pub frequency: i64,
    pub signal: Signal,
    pub tag: Option<Tag>,
    pub system: Option<usize>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    pub center: i64,
    pub rate: i64,
    pub gain: i64,
    pub ppm: i64,
    pub driver: String,
    pub device: String,
    pub error: i64,
    pub analog_recorders: i64,
    pub digital_recorders: i64,
}
