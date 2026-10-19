//! Demodulate a cf32 tap to 16 kHz audio files: `<out>.raw.f32` (before the
//! band-pass) and `<out>.voice.f32`.
//!
//! ```text
//! dump <in.cf32> <out> [--rate HZ] [--recorder-decimation]
//! ```
//!
//! The input is a conditioned slot output at 96 kS/s unless `--rate` gives
//! another rate, in which case it is a pre-conditioning tap (`.in.cf32`) and
//! is resampled, AGC'd and FLL'd like the channelizer does, without a
//! squelch, so the audio shows what the carrier squelch would have muted.

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use num_complex::Complex32;
use trunk_channelizer::{Conditioner, ConditionerParams};
use trunk_fm::io::{cf32_from_bytes, f32_to_bytes};
use trunk_fm::{Audio, Decimation, FmDemod, TAU_US};

const RATE: f64 = 96_000.0;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut in_rate = RATE;
    let mut decimation = Decimation::AntiAliased;
    let mut positional = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rate" => in_rate = it.next().and_then(|v| v.parse().ok()).unwrap_or(RATE),
            "--recorder-decimation" => decimation = Decimation::Recorder,
            _ => positional.push(a.clone()),
        }
    }
    let [input, out] = positional.as_slice() else {
        eprintln!("usage: dump <in.cf32> <out> [--rate HZ] [--recorder-decimation]");
        std::process::exit(2);
    };
    let mut iq = cf32_from_bytes(&std::fs::read(input)?);
    if in_rate != RATE {
        let mut cond = Conditioner::new(2, 0.2, ConditionerParams::default())
            .with_resampler((RATE / in_rate) as f32);
        let mut conditioned = Vec::with_capacity(iq.len());
        let mut buf = vec![Complex32::ZERO; 1 << 16];
        for block in iq.chunks(1 << 16) {
            let n = cond.process_into(block, &mut buf);
            conditioned.extend_from_slice(&buf[..n]);
        }
        iq = conditioned;
    }
    let mut fm = FmDemod::new(RATE, TAU_US, decimation);
    let mut audio = Audio::default();
    for block in iq.chunks(9_600) {
        fm.process(block, &mut audio);
    }
    std::fs::write(format!("{out}.raw.f32"), f32_to_bytes(&audio.raw))?;
    std::fs::write(format!("{out}.voice.f32"), f32_to_bytes(&audio.voice))?;
    eprintln!(
        "{} IQ samples -> {} audio samples ({:.1} s)",
        iq.len(),
        audio.raw.len(),
        audio.raw.len() as f64 / 16_000.0
    );
    Ok(())
}
