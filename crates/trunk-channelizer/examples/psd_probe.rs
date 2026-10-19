//! Print the signal detector's periodogram of 1024 samples taken 60 s into a
//! cu8 capture (scale (u - 127.4) / 128), for comparison with the C++ block.
//!
//! `cargo run -p trunk-channelizer --example psd_probe -- FILE.cu8`

use std::io::{Read, Seek, SeekFrom};

use num_complex::Complex32;
use trunk_channelizer::{DetectorConfig, SignalDetector};

fn main() {
    let path = std::env::args().nth(1).expect("FILE.cu8");
    let mut f = std::fs::File::open(path).unwrap();
    f.seek(SeekFrom::Start(2 * 912_000 * 60)).unwrap();
    let mut raw = vec![0u8; 2 * 1024];
    f.read_exact(&mut raw).unwrap();
    let iq: Vec<Complex32> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&[i, q]| {
            Complex32::new(
                (f32::from(i) - 127.4) / 128.0,
                (f32::from(q) - 127.4) / 128.0,
            )
        })
        .collect();
    let mut d = SignalDetector::new(
        912_000.0,
        DetectorConfig {
            interval_s: 0.0,
            average: 1.0,
            ..DetectorConfig::default()
        },
    );
    let mut out = Vec::new();
    d.push(&iq, &mut out);
    let s = d.smoothed_db();
    let (mi, mv) =
        s.iter().enumerate().fold(
            (0, f64::NEG_INFINITY),
            |a, (i, &v)| if v > a.1 { (i, v) } else { a },
        );
    #[allow(clippy::cast_precision_loss)]
    let mean = s.iter().sum::<f64>() / s.len() as f64;
    println!(
        "Rust: max bin {mi} = {mv} dB, mean {mean} dB, pxx[0]={} pxx[1]={}",
        s[0], s[1]
    );
    println!("detections: {out:?}");
}
