//! Per-hop measurements of a dumped `.raw.f32` (16 kHz audio before the
//! band-pass, from `examples/dump.rs`): a line per 100 ms with quieting,
//! the dominant tone, its amplitude and prominence, and the RMS. With
//! `--calibrate`, prints the median 4–6 kHz band energy of the hops with
//! quieting within 1 dB of the bulk (the no-carrier level), for
//! `detect::NOISE_REFERENCE`.
//!
//! With `--gate [TONE_HZ]`, runs the gate over the measurements and prints
//! its events instead.
//!
//! `cargo run --release -p trunk-fm --example stats -- <raw.f32> [--calibrate | --gate [TONE_HZ]]`

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use trunk_fm::detect::{Detector, Measurement, Tone};
use trunk_fm::gate::{Config, Event, Gate};
use trunk_fm::io::f32_from_bytes;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let calibrate = args.iter().any(|a| a == "--calibrate");
    let gate = args.iter().position(|a| a == "--gate");
    let tone = gate
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<f32>().ok())
        .and_then(Tone::from_hz);
    let Some(path) = args.first().filter(|a| !a.starts_with("--")) else {
        eprintln!("usage: stats <raw.f32> [--calibrate | --gate [TONE_HZ]]");
        std::process::exit(2);
    };
    let raw = f32_from_bytes(&std::fs::read(path)?);
    let mut d = if calibrate {
        Detector::with_reference(1.0)
    } else {
        Detector::new()
    };
    let mut out: Vec<Measurement> = Vec::new();
    for block in raw.chunks(16_000) {
        d.process(block, &mut out);
    }
    if calibrate {
        // quieting_db = 10 log10(1 / noise): recover each hop's noise energy.
        let mut noise: Vec<f64> = out
            .iter()
            .filter(|m| !m.silent())
            .map(|m| 10f64.powf(-m.quieting_db / 10.0))
            .collect();
        noise.sort_by(f64::total_cmp);
        let median = noise[noise.len() / 2];
        let p90 = noise[noise.len() * 9 / 10];
        println!(
            "hops {} live {}; 4-6 kHz energy/sample median {median:.6e} ({:.2} dB), p90 {p90:.6e}",
            out.len(),
            noise.len(),
            10.0 * median.log10()
        );
        return Ok(());
    }
    if gate.is_some() {
        let mut g = Gate::new(Config::for_tone(tone));
        let (mut audio, mut events) = (Vec::new(), Vec::new());
        for m in &out {
            g.push(m, &[], &mut audio, &mut events);
        }
        println!("gate for tone {tone:?}: {} events", events.len());
        for e in &events {
            match e {
                Event::Open {
                    hop,
                    quieting_db,
                    tone,
                } => println!(
                    "{:.1} open quieting {quieting_db:.1} dB tone {tone:?}",
                    *hop as f64 / 10.0
                ),
                Event::Close { hop, reason } => {
                    println!("{:.1} close {reason:?}", *hop as f64 / 10.0);
                }
            }
        }
        return Ok(());
    }
    println!("t_s quieting_db tone_hz amp prominence rms");
    for (i, m) in out.iter().enumerate() {
        println!(
            "{:.1} {:.1} {:.1} {:.4} {:.1} {:.4}",
            i as f64 / 10.0,
            m.quieting_db,
            m.tone.hz(),
            m.tone_amplitude,
            m.tone_prominence,
            m.rms
        );
    }
    Ok(())
}
