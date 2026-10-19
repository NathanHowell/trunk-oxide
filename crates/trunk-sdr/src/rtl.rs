//! An RTL-SDR dongle: found by serial, tuned, and read either in one
//! synchronous capture (gain calibration) or as a stream on its own thread.
//!
//! The set-up order and the sample scale follow gr-osmosdr's `rtl_source_c`,
//! so a source moved from the `osmosdr` driver to this one sees the same
//! numbers: manual tuner gain with the RTL2832U's digital AGC off, the first
//! USB transfer discarded (it carries stale samples), 15 transfers of 256 KiB
//! in flight.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use num_complex::Complex32;
use trunk_rtlsdr_sys::{Device, Tuner};

use crate::convert::Lut;
use crate::stream::{Producer, Running, Stream};

/// USB transfers in flight and their size, as gr-osmosdr sizes them.
const BUF_NUM: u32 = 15;
const BUF_LEN: u32 = 16 * 32 * 512;
/// Transfers discarded at the start of a stream (stale samples).
const BUF_SKIP: u64 = 1;

/// Which dongle. `rtl=SERIAL`, `serial=SERIAL`, a bare serial, or a bare
/// small integer (a bus index, when no dongle has that serial).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceSpec {
    Serial(String),
    Index(u32),
}

impl DeviceSpec {
    pub fn parse(spec: &str) -> Self {
        let s = spec.trim();
        let s = ["rtl=", "serial=", "rtlsdr="]
            .iter()
            .find_map(|p| s.strip_prefix(p))
            .unwrap_or(s);
        if s.is_empty() {
            return Self::Index(0);
        }
        Self::Serial(s.to_owned())
    }

    /// The bus index of this dongle, now.
    fn resolve(&self) -> Result<u32> {
        match self {
            Self::Index(i) => Ok(*i),
            Self::Serial(serial) => {
                if let Some(i) = trunk_rtlsdr_sys::index_by_serial(serial) {
                    return Ok(i);
                }
                // A short number with no such serial is a bus index.
                if serial.len() <= 2
                    && let Ok(i) = serial.parse::<u32>()
                {
                    return Ok(i);
                }
                let seen: Vec<String> = trunk_rtlsdr_sys::devices()
                    .into_iter()
                    .map(|d| d.serial)
                    .collect();
                bail!("no RTL-SDR with serial {serial:?} (present: {seen:?})")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gain {
    /// The tuner's and the RTL2832U's own AGC.
    Auto,
    /// Manual tuner gain in dB, snapped to the nearest step.
    Manual(f64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    /// Where to tune, in Hz (any software `error` already added).
    pub center_hz: f64,
    pub rate_hz: f64,
    /// The hardware's whole-ppm clock correction.
    pub ppm: i32,
    pub gain: Gain,
}

/// What an opened dongle turned out to be.
#[derive(Clone, Debug, PartialEq)]
pub struct Info {
    pub index: u32,
    pub serial: String,
    pub product: String,
    pub manufacturer: String,
    pub tuner: Tuner,
    /// The rate the RTL2832U settled on (its divider is coarse).
    pub rate_hz: f64,
}

/// An open, tuned dongle.
pub struct Rtl {
    dev: Arc<Device>,
    info: Info,
    /// Tuner gain steps in tenths of a dB.
    steps: Vec<i32>,
}

impl std::fmt::Debug for Rtl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rtl")
            .field("info", &self.info)
            .field("steps", &self.steps)
            .finish_non_exhaustive()
    }
}

impl Rtl {
    /// Open `spec` and apply `tuning`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn open(spec: &DeviceSpec, tuning: &Tuning) -> Result<Self> {
        ensure!(
            tuning.rate_hz > 0.0 && tuning.center_hz > 0.0,
            "tuning needs a positive rate and centre"
        );
        let index = spec.resolve()?;
        let dev = Device::open(index).with_context(|| format!("opening RTL-SDR #{index}"))?;
        let (manufacturer, product, serial) = dev.usb_strings().context("reading USB strings")?;
        let tuner = dev.tuner();
        dev.set_sample_rate(tuning.rate_hz.round() as u32)
            .with_context(|| format!("setting sample rate {} S/s", tuning.rate_hz))?;
        let rate_hz = f64::from(dev.sample_rate());
        dev.set_center_freq(tuning.center_hz.round() as u32)
            .with_context(|| format!("tuning to {} Hz", tuning.center_hz))?;
        if tuning.ppm != 0 {
            dev.set_freq_correction(tuning.ppm)
                .with_context(|| format!("setting {} ppm", tuning.ppm))?;
        }
        let steps = dev.tuner_gains().context("reading tuner gain steps")?;
        let rtl = Self {
            dev: Arc::new(dev),
            info: Info {
                index,
                serial,
                product,
                manufacturer,
                tuner,
                rate_hz,
            },
            steps,
        };
        rtl.set_gain(tuning.gain)?;
        rtl.dev.reset_buffer().context("resetting the USB buffer")?;
        Ok(rtl)
    }

    pub const fn info(&self) -> &Info {
        &self.info
    }

    /// Tuner gain steps in dB, ascending.
    pub fn gain_steps_db(&self) -> Vec<f64> {
        let mut g: Vec<f64> = self.steps.iter().map(|&t| f64::from(t) / 10.0).collect();
        g.sort_by(f64::total_cmp);
        g
    }

    /// Apply `gain`; returns the tuner gain in dB that resulted (the nearest
    /// step for `Manual`, the tuner's current reading for `Auto`).
    pub fn set_gain(&self, gain: Gain) -> Result<f64> {
        match gain {
            Gain::Auto => {
                self.dev.set_tuner_gain_mode(false)?;
                self.dev.set_agc_mode(true)?;
            }
            Gain::Manual(db) => {
                ensure!(db.is_finite(), "gain {db} dB is not a number");
                ensure!(!self.steps.is_empty(), "the tuner reports no gain steps");
                #[allow(clippy::cast_possible_truncation)]
                let want = (db * 10.0).round() as i32;
                let step = *self
                    .steps
                    .iter()
                    .min_by_key(|&&s| (s - want).abs())
                    .expect("non-empty");
                self.dev.set_tuner_gain_mode(true)?;
                self.dev.set_agc_mode(false)?;
                self.dev
                    .set_tuner_gain(step)
                    .with_context(|| format!("setting tuner gain {} dB", f64::from(step) / 10.0))?;
            }
        }
        Ok(f64::from(self.dev.tuner_gain()) / 10.0)
    }

    /// The tuner's current gain in dB.
    pub fn tuner_gain_db(&self) -> f64 {
        f64::from(self.dev.tuner_gain()) / 10.0
    }

    pub fn set_center_freq(&self, hz: f64) -> Result<()> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        self.dev
            .set_center_freq(hz.round() as u32)
            .with_context(|| format!("tuning to {hz} Hz"))
    }

    /// Read `settle + n` samples synchronously and return the last `n`:
    /// the first `settle` cover the front end settling after a gain change
    /// and whatever the USB pipe still held.
    pub fn capture(&self, settle: usize, n: usize) -> Result<Vec<Complex32>> {
        self.dev
            .reset_buffer()
            .context("resetting the USB buffer")?;
        let total_bytes = (settle + n) * 2;
        let mut raw = Vec::with_capacity(total_bytes);
        let mut chunk = vec![0u8; BUF_LEN as usize];
        while raw.len() < total_bytes {
            let got = self.dev.read_sync(&mut chunk).context("synchronous read")?;
            ensure!(got > 0, "the dongle returned no samples");
            raw.extend_from_slice(&chunk[..got]);
        }
        let lut = Lut::new();
        let mut out = Vec::with_capacity(n);
        lut.convert(&raw[settle * 2..total_bytes], &mut out);
        Ok(out)
    }

    /// Start streaming on a new thread into a queue of `capacity` samples,
    /// in blocks of `block` samples. The [`Stream`] is the consumer's end;
    /// the [`Streaming`] keeps the device and stops the stream when dropped.
    pub fn stream(self, block: usize, capacity: usize) -> Result<(Stream, Streaming)> {
        ensure!(block > 0 && capacity >= block, "bad stream geometry");
        let (producer, stream) = Stream::new(self.info.serial.clone(), capacity);
        let stopping = Arc::new(AtomicBool::new(false));
        let dev = Arc::clone(&self.dev);
        let serial = self.info.serial.clone();
        let stop = Arc::clone(&stopping);
        let thread = std::thread::Builder::new()
            .name(format!("rtl-{serial}"))
            .spawn(move || reader(&dev, &producer, block, &stop, &serial))
            .context("spawning the reader thread")?;
        Ok((
            stream,
            Streaming {
                rtl: self,
                stopping,
                thread: Some(thread),
            },
        ))
    }
}

fn reader(dev: &Device, producer: &Producer, block: usize, stop: &AtomicBool, serial: &str) {
    let lut = Lut::new();
    let mut transfers = 0u64;
    let mut cur = producer.take_block();
    let result = dev.read_async(BUF_NUM, BUF_LEN, |buf| {
        producer.count_transfer();
        if stop.load(Ordering::Relaxed) {
            let _ = dev.cancel_async();
            return;
        }
        transfers += 1;
        if transfers <= BUF_SKIP {
            return;
        }
        let mut off = 0;
        while off < buf.len() {
            let need = (block - cur.len()) * 2;
            let take = need.min(buf.len() - off);
            lut.convert(&buf[off..off + take], &mut cur);
            off += take;
            if cur.len() == block {
                producer.push(std::mem::replace(&mut cur, producer.take_block()));
            }
        }
    });
    match result {
        Ok(()) if stop.load(Ordering::Relaxed) => {
            tracing::info!(serial, "RTL-SDR stream stopped");
        }
        Ok(()) => tracing::error!(serial, "RTL-SDR stream ended on its own"),
        Err(e) => tracing::error!(serial, error = %e, "RTL-SDR stream failed"),
    }
    producer.end();
}

/// The device half of a stream: keeps it going and takes gain changes
/// (through [`Self::rtl`]). Stops the stream on drop.
pub struct Streaming {
    rtl: Rtl,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Streaming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Streaming")
            .field("rtl", &self.rtl)
            .finish_non_exhaustive()
    }
}

impl Streaming {
    pub const fn rtl(&self) -> &Rtl {
        &self.rtl
    }
}

impl Running for Streaming {
    fn stop(&mut self) {
        stop(&self.rtl.dev, &self.stopping, &mut self.thread);
    }
}

impl Drop for Streaming {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Ask the reader to stop and wait for it. librtlsdr loses a cancel issued
/// before its read loop starts, so keep cancelling until the thread is gone.
fn stop(dev: &Device, stopping: &AtomicBool, thread: &mut Option<JoinHandle<()>>) {
    let Some(t) = thread.take() else { return };
    stopping.store(true, Ordering::SeqCst);
    while !t.is_finished() {
        let _ = dev.cancel_async();
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = t.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_specs_parse_like_osmosdr_arguments() {
        assert_eq!(
            DeviceSpec::parse("rtl=13360853"),
            DeviceSpec::Serial("13360853".into())
        );
        assert_eq!(
            DeviceSpec::parse(" serial=ABC "),
            DeviceSpec::Serial("ABC".into())
        );
        assert_eq!(DeviceSpec::parse("0"), DeviceSpec::Serial("0".into()));
        assert_eq!(DeviceSpec::parse(""), DeviceSpec::Index(0));
    }

    #[test]
    fn a_missing_serial_that_looks_like_an_index_resolves_to_it() {
        // No dongle has serial "1" (serials are 8 digits), so it is bus index 1.
        assert_eq!(DeviceSpec::Serial("1".into()).resolve().unwrap(), 1);
        assert!(
            DeviceSpec::Serial("no-such-dongle".into())
                .resolve()
                .is_err()
        );
    }
}
