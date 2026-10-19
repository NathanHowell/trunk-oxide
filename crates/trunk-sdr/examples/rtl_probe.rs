//! List the RTL-SDR dongles on the bus, or open one and stream it briefly.
//!
//! ```text
//! cargo run -p trunk-sdr --example rtl_probe
//! cargo run -p trunk-sdr --example rtl_probe -- rtl=13360853 488661750 912000 [GAIN_DB] [SECONDS]
//! ```
//!
//! With a device: prints what it is, its gain steps, the gain applied, then
//! streams for `SECONDS` (default 3) and reports samples, drops and the mean
//! power, which should sit well below full scale (|I|,|Q| < 1).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use num_complex::Complex32;
use trunk_sdr::{DeviceSpec, Gain, Rtl, Tuning};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        for d in trunk_rtlsdr_sys::devices() {
            println!(
                "#{} serial={} {} {} ({})",
                d.index, d.serial, d.manufacturer, d.product, d.name
            );
        }
        return Ok(());
    }
    let spec = DeviceSpec::parse(&args[0]);
    let center_hz: f64 = args[1].parse().context("centre Hz")?;
    let rate_hz: f64 = args[2].parse().context("rate Hz")?;
    let gain = args
        .get(3)
        .map(|g| g.parse::<f64>().map(Gain::Manual))
        .transpose()
        .context("gain dB")?
        .unwrap_or(Gain::Auto);
    let seconds: f64 = args
        .get(4)
        .map_or(Ok(3.0), |s| s.parse())
        .context("seconds")?;

    let rtl = Rtl::open(
        &spec,
        &Tuning {
            center_hz,
            rate_hz,
            ppm: 0,
            gain,
        },
    )?;
    println!("{:#?}", rtl.info());
    println!("gain steps dB: {:?}", rtl.gain_steps_db());
    println!("tuner gain now: {} dB", rtl.tuner_gain_db());

    let (stream, _streaming) = rtl.stream(32_768, rate_hz as usize)?;
    let started = Instant::now();
    let mut buf = vec![Complex32::ZERO; 65_536];
    let (mut n, mut power, mut peak) = (0u64, 0.0f64, 0.0f32);
    while started.elapsed() < Duration::from_secs_f64(seconds) {
        let got = stream.read(&mut buf, Duration::from_millis(200));
        for s in &buf[..got] {
            power += f64::from(s.norm_sqr());
            peak = peak.max(s.re.abs()).max(s.im.abs());
        }
        n += got as u64;
    }
    let st = stream.stats();
    println!(
        "{n} samples read in {:.2} s ({:.0} S/s); queued {} dropped {} ({} drops), {} transfers",
        started.elapsed().as_secs_f64(),
        n as f64 / started.elapsed().as_secs_f64(),
        st.samples,
        st.dropped,
        st.drops,
        st.transfers
    );
    println!(
        "mean power {:.1} dBFS, peak |I|/|Q| {peak:.3}",
        10.0 * (power / n.max(1) as f64).log10()
    );
    Ok(())
}
