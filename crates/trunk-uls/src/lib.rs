//! The FCC's Universal Licensing System, for the sites and channels licensed
//! around the receiver.
//!
//! The FCC publishes every land-mobile licence as a weekly zip of
//! pipe-delimited tables (`l_LMpriv.zip`). [`select::find`] reads the ones it
//! needs straight out of the zip, finds the fixed sites near a point that are
//! licensed in a band, and groups them by licensee; [`report`] sets what they
//! are licensed for against what the config scans.
//!
//! A licence is not proof of use: it says what a site may transmit, not what
//! it does, and it names no control channel, talkgroup or system ID.

pub mod fetch;
pub mod geo;
pub mod reach;
pub mod records;
pub mod report;
pub mod select;
pub mod terrain;

pub use geo::Point;
pub use select::{Licensee, Query, Site};
