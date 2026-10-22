pub mod config;
pub mod device;
pub mod solver;

/// Half the P25/narrowband FM channel width (12,500 Hz / 2): how far a
/// channel reaches beyond its centre, for the usable band and to keep the
/// SDR's centre frequency off any channel.
#[allow(clippy::cast_possible_truncation)]
pub const CHANNEL_HALFWIDTH: i64 = trunk_channelizer::passband::CHANNEL_HALF_WIDTH_HZ as i64;

/// How far a source's centre must stay from the middle of any channel: the
/// channel's half width plus the dongle's DC spike, a few kHz either side of
/// the centre that a channel on it would be smeared by.
pub const CENTER_CLEARANCE: i64 = CHANNEL_HALFWIDTH + 5_000;

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
    pub ppm: i64,
    pub driver: String,
    pub device: String,
    pub error: i64,
}
