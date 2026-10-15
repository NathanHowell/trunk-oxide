//! Shared overlap-save FFT channelizer (Borgerding multiband filter bank, as
//! used by ka9q-radio): one forward FFT per input block shared by every
//! channel; each channel selects a bin slice around its centre, applies the
//! channel filter in the frequency domain, and runs a small inverse FFT.
//!
//! wxt.1 spike. See `examples/bench.rs` for measurements.

mod channelizer;
mod design;
mod fft;
mod service;

/// FFTW backend, kept only so the bench can compare against rustfft.
#[cfg(feature = "fftw")]
pub mod fftw;

pub use channelizer::{BlockStats, ChannelId, Channelizer, Config};
pub use design::lowpass_taps;
pub use service::{Counters, SharedChannelizer};
