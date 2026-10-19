//! SDR sources in Rust.
//!
//! [`rtl::Rtl`] opens an RTL-SDR dongle and tunes it; [`stream::Stream`]
//! carries its samples from the USB thread to whoever runs the channelizer,
//! counting what is dropped when that consumer falls behind. The 8-bit
//! samples become floats exactly as gr-osmosdr made them, so every threshold
//! calibrated against the GNU Radio source still holds.

pub mod convert;
pub mod file;
pub mod rtl;
pub mod stream;

pub use file::{FileSpec, Format};
pub use rtl::{DeviceSpec, Gain, Info, Rtl, Streaming, Tuning};
pub use stream::{Running, Stream, StreamStats, streams};
