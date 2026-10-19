//! Demodulate a conditioned cf32 tap (96 kS/s) to 16 kHz audio files:
//! `<out>.raw.f32` (before the band-pass) and `<out>.voice.f32`.
//!
//! `cargo run --release -p trunk-fm --example dump -- <in.cf32> <out>`

use num_complex::Complex32;
use trunk_fm::{Audio, FmDemod, TAU_US};

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, out] = args.as_slice() else {
        eprintln!("usage: dump <in.cf32> <out>");
        std::process::exit(2);
    };
    let bytes = std::fs::read(input)?;
    let iq: Vec<Complex32> = bytes
        .chunks_exact(8)
        .map(|c| {
            Complex32::new(
                f32::from_le_bytes(c[..4].try_into().unwrap()),
                f32::from_le_bytes(c[4..].try_into().unwrap()),
            )
        })
        .collect();
    let mut fm = FmDemod::new(96_000.0, TAU_US);
    let mut audio = Audio::default();
    for block in iq.chunks(9_600) {
        fm.process(block, &mut audio);
    }
    let write = |path: String, v: &[f32]| {
        let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
        std::fs::write(path, bytes)
    };
    write(format!("{out}.raw.f32"), &audio.raw)?;
    write(format!("{out}.voice.f32"), &audio.voice)?;
    eprintln!(
        "{} IQ samples -> {} audio samples ({:.1} s)",
        iq.len(),
        audio.raw.len(),
        audio.raw.len() as f64 / 16_000.0
    );
    Ok(())
}
