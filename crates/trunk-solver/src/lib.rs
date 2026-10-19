pub mod config;
pub mod device;
pub mod solver;

/// Half the P25/narrowband FM channel width (12,500 Hz / 2).
///
/// Used as a guard band so channel frequencies aren't at the very edge
/// of an SDR's usable bandwidth, and to keep the SDR center frequency
/// off any active channel.
pub const CHANNEL_HALFWIDTH: i64 = 6_250;

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
}
