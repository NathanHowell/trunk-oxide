//! A recorded IQ file played as a source, paced at its sample rate, for
//! replay and hardware-free tests.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use num_complex::Complex32;

use crate::convert::Lut;
use crate::stream::{Producer, Running, Stream};

/// Sample format of the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Interleaved unsigned 8-bit, as `rtl_sdr` writes (scaled like a dongle).
    Cu8,
    /// Interleaved little-endian float32.
    Cf32,
}

impl Format {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "cu8" => Ok(Self::Cu8),
            "cf32" | "" => Ok(Self::Cf32),
            other => anyhow::bail!("unsupported IQ format {other:?} (expected cu8 or cf32)"),
        }
    }

    const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Cu8 => 2,
            Self::Cf32 => 8,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileSpec {
    pub path: PathBuf,
    pub format: Format,
    pub rate_hz: f64,
    /// Loop forever (else end when the file does).
    pub repeat: bool,
}

/// Start playing `spec` into a stream of `capacity` samples, `block` at a
/// time. The player waits for room rather than dropping: a replay must not
/// lose samples when the consumer is slow.
pub fn play(spec: &FileSpec, block: usize, capacity: usize) -> Result<(Stream, Box<dyn Running>)> {
    ensure!(block > 0 && capacity >= block, "bad stream geometry");
    ensure!(spec.rate_hz > 0.0, "a file needs a positive rate");
    let file = File::open(&spec.path)
        .with_context(|| format!("opening IQ file {}", spec.path.display()))?;
    let label = spec.path.file_name().map_or_else(
        || spec.path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let (producer, stream) = Stream::new(label, capacity);
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&stopping);
    let spec = spec.clone();
    let thread = std::thread::Builder::new()
        .name(format!("iqfile-{}", short(&spec.path)))
        .spawn(move || player(file, &spec, &producer, block, &stop))
        .context("spawning the file player thread")?;
    Ok((
        stream,
        Box::new(Playing {
            stopping,
            thread: Some(thread),
        }),
    ))
}

fn short(p: &Path) -> String {
    p.file_stem().map_or_else(String::new, |s| {
        s.to_string_lossy().chars().take(12).collect()
    })
}

#[allow(clippy::cast_precision_loss)]
fn player(mut file: File, spec: &FileSpec, producer: &Producer, block: usize, stop: &AtomicBool) {
    let lut = Lut::new();
    let bps = spec.format.bytes_per_sample();
    let mut raw = vec![0u8; block * bps];
    let started = Instant::now();
    let mut sent = 0u64;
    'play: while !stop.load(Ordering::Relaxed) {
        // Fill a whole block, wrapping at EOF when repeating.
        let mut filled = 0;
        while filled < raw.len() {
            match file.read(&mut raw[filled..]) {
                Ok(0) => {
                    if !spec.repeat || file.seek(SeekFrom::Start(0)).is_err() {
                        break;
                    }
                }
                Ok(n) => filled += n,
                Err(e) => {
                    tracing::error!(path = %spec.path.display(), error = %e, "reading IQ file");
                    break 'play;
                }
            }
        }
        let whole = filled - filled % bps;
        if whole == 0 {
            break;
        }
        let mut out = producer.take_block();
        match spec.format {
            Format::Cu8 => lut.convert(&raw[..whole], &mut out),
            Format::Cf32 => out.extend(raw[..whole].as_chunks::<8>().0.iter().map(|c| {
                Complex32::new(
                    f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                    f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                )
            })),
        }
        // Pace: this block's last sample is due at sent/rate after start.
        sent += out.len() as u64;
        let due = started + Duration::from_secs_f64(sent as f64 / spec.rate_hz);
        while Instant::now() < due {
            if stop.load(Ordering::Relaxed) {
                break 'play;
            }
            std::thread::sleep((due - Instant::now()).min(Duration::from_millis(20)));
        }
        if !producer.push_wait(out, stop) {
            break;
        }
        if whole < raw.len() {
            break; // EOF without repeat
        }
    }
    producer.end();
}

#[derive(Debug)]
struct Playing {
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Running for Playing {
    fn stop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Playing {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, bytes: &[u8]) -> PathBuf {
        let p = std::env::temp_dir().join(format!("trunk-sdr-{}-{name}", std::process::id()));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn plays_a_cu8_file_once_at_rate_and_ends() {
        // 4000 samples at 40 kS/s = 100 ms
        let bytes: Vec<u8> = (0..8000u32)
            .map(|i| u8::try_from(i % 256).unwrap())
            .collect();
        let path = temp("once.cu8", &bytes);
        let spec = FileSpec {
            path: path.clone(),
            format: Format::Cu8,
            rate_hz: 40_000.0,
            repeat: false,
        };
        let t0 = Instant::now();
        let (stream, mut playing) = play(&spec, 1000, 2000).unwrap();
        let mut out = vec![Complex32::ZERO; 512];
        let mut total = 0;
        let mut first = None;
        while !stream.is_ended() {
            let n = stream.read(&mut out, Duration::from_millis(500));
            if n > 0 && first.is_none() {
                first = Some(out[0]);
            }
            total += n;
        }
        assert_eq!(total, 4000);
        assert_eq!(
            first.unwrap(),
            Complex32::new((0.0 - 127.4) / 128.0, (1.0 - 127.4) / 128.0)
        );
        assert!(
            t0.elapsed() >= Duration::from_millis(90),
            "{:?}",
            t0.elapsed()
        );
        playing.stop();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn repeats_until_stopped_and_cf32_round_trips() {
        let samples = [Complex32::new(0.25, -0.5), Complex32::new(1.0, 2.0)];
        let mut bytes = Vec::new();
        for s in samples {
            bytes.extend_from_slice(&s.re.to_le_bytes());
            bytes.extend_from_slice(&s.im.to_le_bytes());
        }
        let path = temp("loop.cf32", &bytes);
        let spec = FileSpec {
            path: path.clone(),
            format: Format::Cf32,
            rate_hz: 1_000_000.0,
            repeat: true,
        };
        let (stream, mut playing) = play(&spec, 4, 64).unwrap();
        let mut out = [Complex32::ZERO; 10];
        let mut got = 0;
        while got < 10 {
            got += stream.read(&mut out[got..], Duration::from_millis(500));
        }
        assert_eq!(&out[..4], &[samples[0], samples[1], samples[0], samples[1]]);
        playing.stop();
        assert!(stream.is_ended() || stream.stats().samples >= 10);
        let _ = std::fs::remove_file(path);
    }
}
