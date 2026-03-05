use anyhow::{Result, bail};
use z3::ast::{Bool, Int};

use crate::device::{Device, SampleRate};
use crate::{CHANNEL_HALFWIDTH, Channel, Signal, Source};

/// Replicate trunk-recorder's `Source::set_min_max()` decimation logic.
///
/// Given a sample rate, returns the per-side guard band (`if1 / 2`) that
/// trunk-recorder subtracts from each edge to get the usable bandwidth.
/// The usable range is `center ± (rate/2 - guard)`.
///
/// **Important:** This only produces a meaningful guard when `rate / if_freq`
/// is even for at least one IF frequency (24000, 25000, 32000). Rates where
/// all quotients are odd fall through to a degenerate ~50 Hz guard. The solver
/// must filter such rates out via `valid_rates()`.
fn decimation_guard(rate: i64) -> i64 {
    let if_freqs: [i64; 3] = [24000, 25000, 32000];
    let mut decim: i64 = 24000; // trunk-recorder default

    for &if_freq in &if_freqs {
        if rate % if_freq != 0 {
            continue;
        }
        let q = rate / if_freq;
        if q & 1 != 0 {
            continue;
        }
        if q >= 40 && (q & 3) == 0 {
            decim = q / 4;
        } else {
            decim = q / 2;
        }
    }

    let if1 = rate / decim;
    if1 / 2
}

struct Problem {
    ranges: Vec<Int>,
    lower_freq: Vec<Int>,
    upper_freq: Vec<Int>,
}

fn build_problem(opt: &z3::Optimize, channels: &[Channel], devices: &[Device]) -> Problem {
    let n = devices.len();

    let lower_freq: Vec<Int> = (0..n)
        .map(|i| Int::new_const(format!("lower_{i}")))
        .collect();
    let upper_freq: Vec<Int> = (0..n)
        .map(|i| Int::new_const(format!("upper_{i}")))
        .collect();

    // Order constraint: lower_0 < lower_1 < ...
    for w in lower_freq.windows(2) {
        opt.assert(&w[0].lt(&w[1]));
    }

    let ranges: Vec<Int> = lower_freq
        .iter()
        .zip(upper_freq.iter())
        .map(|(l, u)| u - l)
        .collect();

    let zero = Int::from_i64(0);

    for (i, (r, device)) in ranges.iter().zip(devices.iter()).enumerate() {
        // Rate constraint: 0 (unused) or within valid sample rate range
        let mut rate_options: Vec<Bool> = vec![r.eq(&zero)];
        for sr in &device.sample_rates {
            match sr {
                SampleRate::Exact(v) => {
                    rate_options.push(r.eq(Int::from_i64(*v)));
                }
                SampleRate::Range(lo, hi) => {
                    let lo_z3 = Int::from_i64(*lo);
                    let hi_z3 = Int::from_i64(*hi);
                    rate_options.push(Bool::and(&[r.ge(&lo_z3), r.le(&hi_z3)]));
                }
            }
        }
        opt.assert(&Bool::or(&rate_options));

        // Rate must be a multiple of the device's required rate multiple
        // (e.g. osmosdr requires multiples of 24,000 Hz)
        let rate_mul = Int::from_i64(device.rate_multiple);
        let mult = Int::new_const(format!("mult_{i}"));
        opt.assert(&r.eq(&(&rate_mul * &mult)));
    }

    // Frequency coverage constraints.
    //
    // trunk-recorder shrinks each SDR's usable bandwidth by a decimation-
    // dependent guard band (see Source::set_min_max in source.cc).  The guard
    // depends on the sample rate, which is a Z3 variable.  We enumerate every
    // valid rate for each device and encode the correct guard for each.
    for channel in channels {
        let freq = Int::from_i64(channel.frequency);
        let hw = Int::from_i64(CHANNEL_HALFWIDTH);

        let in_range_options: Vec<Bool> = lower_freq
            .iter()
            .zip(upper_freq.iter())
            .zip(ranges.iter())
            .zip(devices.iter())
            .map(|(((l, u), r), device)| {
                // Build per-rate disjuncts that apply the correct decimation guard.
                let rate_disjuncts: Vec<Bool> = valid_rates(device)
                    .map(|rate| {
                        let guard = Int::from_i64(decimation_guard(rate));
                        let rate_z3 = Int::from_i64(rate);

                        // rate == this candidate
                        let rate_match = r.eq(&rate_z3);
                        // freq >= lower + guard + CHANNEL_HALFWIDTH
                        let lower_bound = freq.ge(&(l + &guard + &hw));
                        // freq <= upper - guard - CHANNEL_HALFWIDTH
                        let upper_bound = freq.le(&(u - &guard - &hw));

                        // Center avoidance: SDR center frequency doesn't land on a channel.
                        // center = (upper + lower) / 2, avoid integer division:
                        //   2*(freq - CHANNEL_HALFWIDTH) >= upper + lower  OR
                        //   2*(freq + CHANNEL_HALFWIDTH) <= upper + lower
                        let center_times_2 = u + l;
                        let avoid_above =
                            Int::from_i64(2 * (channel.frequency - CHANNEL_HALFWIDTH))
                                .ge(&center_times_2);
                        let avoid_below =
                            Int::from_i64(2 * (channel.frequency + CHANNEL_HALFWIDTH))
                                .le(&center_times_2);

                        Bool::and(&[
                            rate_match,
                            lower_bound,
                            upper_bound,
                            Bool::or(&[avoid_above, avoid_below]),
                        ])
                    })
                    .collect();

                Bool::or(&rate_disjuncts)
            })
            .collect();

        opt.assert(&Bool::or(&in_range_options));
    }

    Problem {
        ranges,
        lower_freq,
        upper_freq,
    }
}

/// Minimum acceptable guard band (per side).  Rates where `decimation_guard`
/// returns less than this are excluded — they would place channels at the
/// extreme edge of the SDR bandwidth where RF rolloff kills the signal.
const MIN_GUARD: i64 = 12_000;

/// Enumerate all valid sample rates for a device, excluding rates that produce
/// degenerate decimation guard bands.
fn valid_rates(device: &Device) -> impl Iterator<Item = i64> + '_ {
    device
        .sample_rates
        .iter()
        .flat_map(move |sr| {
            let mul = device.rate_multiple;
            match *sr {
                SampleRate::Exact(v) => {
                    let lo_mult = v / mul;
                    let hi_mult = lo_mult;
                    (lo_mult..=hi_mult)
                        .map(move |m| m * mul)
                        .filter(move |&r| r == v)
                        .collect::<Vec<_>>()
                        .into_iter()
                }
                SampleRate::Range(lo, hi) => {
                    let lo_mult = (lo + mul - 1) / mul; // ceil division
                    let hi_mult = hi / mul;
                    (lo_mult..=hi_mult)
                        .map(move |m| m * mul)
                        .collect::<Vec<_>>()
                        .into_iter()
                }
            }
        })
        .filter(|&rate| decimation_guard(rate) >= MIN_GUARD)
}

pub fn solve(channels: &[Channel], devices: &[Device]) -> Result<Vec<Source>> {
    let opt = z3::Optimize::new();

    let problem = build_problem(&opt, channels, devices);

    if opt.check(&[]) != z3::SatResult::Sat {
        bail!("No valid assignment possible, add more devices");
    }

    // Iteratively eliminate devices from highest to lowest index.
    // Each successful elimination keeps its constraint (no pop).
    // On failure, pop and stop.
    for r in problem.ranges.iter().rev() {
        opt.push();
        opt.assert(&r.eq(Int::from_i64(0)));
        if opt.check(&[]) != z3::SatResult::Sat {
            opt.pop();
            break;
        }
    }

    // Minimize sum of ranges
    let sum_ranges: Int = problem.ranges.iter().sum();
    opt.minimize(&sum_ranges);

    // Minimize sum of lower frequencies for deterministic results
    let sum_lower: Int = problem.lower_freq.iter().sum();
    opt.minimize(&sum_lower);

    let sat = opt.check(&[]);
    if sat != z3::SatResult::Sat {
        bail!("Optimization failed: {sat:?}");
    }

    let model = opt
        .get_model()
        .ok_or_else(|| anyhow::anyhow!("Failed to get model"))?;

    extract_sources(&model, &problem, channels, devices)
}

fn extract_sources(
    model: &z3::Model,
    problem: &Problem,
    channels: &[Channel],
    devices: &[Device],
) -> Result<Vec<Source>> {
    let mut sources = Vec::new();

    for (i, ((l, u), device)) in problem
        .lower_freq
        .iter()
        .zip(problem.upper_freq.iter())
        .zip(devices.iter())
        .enumerate()
    {
        let lower = model
            .eval(l, true)
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("Failed to evaluate lower_{i}"))?;
        let upper = model
            .eval(u, true)
            .and_then(|v| v.as_i64())
            .ok_or_else(|| anyhow::anyhow!("Failed to evaluate upper_{i}"))?;

        if lower != upper {
            let rate = upper - lower;
            let guard = decimation_guard(rate);

            // Usable range after decimation guard
            let usable_lower = lower + guard;
            let usable_upper = upper - guard;

            let analog_recorders = channels
                .iter()
                .filter(|c| {
                    c.signal == Signal::Analog
                        && c.frequency >= usable_lower
                        && c.frequency <= usable_upper
                })
                .count();

            // Count digital frequencies that are not control channels
            let digital_recorders = channels
                .iter()
                .filter(|c| {
                    c.signal == Signal::Digital
                        && c.tag.is_none()
                        && c.frequency >= usable_lower
                        && c.frequency <= usable_upper
                })
                .count();

            sources.push(Source {
                center: i64::midpoint(upper, lower),
                rate,
                gain: device.gain,
                ppm: device.ppm,
                driver: device.driver.clone(),
                device: device.device_template.replace("{}", &i.to_string()),
                error: 0,
                analog_recorders: i64::try_from(analog_recorders)?,
                digital_recorders: i64::try_from(digital_recorders)?,
            });
        }
    }

    eprintln!("Devices required: {}", sources.len());
    Ok(sources)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::Device;
    use crate::{Channel, Signal, Tag};

    /// Helper: assert every channel is within the usable range of some source,
    /// applying trunk-recorder's actual decimation guard.
    fn assert_all_covered(channels: &[Channel], sources: &[Source]) {
        for ch in channels {
            let covered = sources.iter().any(|s| {
                let guard = decimation_guard(s.rate);
                let min = s.center - s.rate / 2 + guard;
                let max = s.center + s.rate / 2 - guard;
                ch.frequency >= min + CHANNEL_HALFWIDTH && ch.frequency <= max - CHANNEL_HALFWIDTH
            });
            assert!(
                covered,
                "channel {:.6} MHz not covered by any source",
                ch.frequency as f64 / 1e6
            );
        }
    }

    #[test]
    fn test_decimation_guard() {
        // Rate 912,000: 912000/24000=38 (even, <40) → decim=19, if1=48000
        assert_eq!(decimation_guard(912_000), 24_000);

        // Rate 2,448,000: 2448000/24000=102 (even, >=40, %4==2) → decim=51, if1=48000
        assert_eq!(decimation_guard(2_448_000), 24_000);

        // Rate 2,500,000: 2500000/25000=100 (even, >=40, %4==0) → decim=25, if1=100000
        assert_eq!(decimation_guard(2_500_000), 50_000);

        // Rates with odd quotients produce degenerate guards (valid_rates filters these out)
        assert!(decimation_guard(2_088_000) < MIN_GUARD); // q=87, odd
        assert!(decimation_guard(2_472_000) < MIN_GUARD); // q=103, odd
    }

    #[test]
    fn test_no_degenerate_rates() {
        // Every rate produced by valid_rates() must have a meaningful guard band
        for device in [Device::rtlsdr(), Device::airspy()] {
            for rate in valid_rates(&device) {
                let guard = decimation_guard(rate);
                assert!(
                    guard >= MIN_GUARD,
                    "device {:?} rate {rate} has degenerate guard {guard}",
                    device.driver,
                );
            }
        }
    }

    #[test]
    fn test_two_conventional_channels() {
        // The exact bug case: two analog channels 750 kHz apart.
        // Old solver picked rate=912k which couldn't cover both after decimation.
        let channels = vec![
            Channel {
                frequency: 488_337_500,
                signal: Signal::Analog,
                tag: None,
                system: None,
            },
            Channel {
                frequency: 489_087_500,
                signal: Signal::Analog,
                tag: None,
                system: None,
            },
        ];
        let devices = vec![Device::rtlsdr()];

        let sources = solve(&channels, &devices).expect("should find a solution");
        assert_eq!(sources.len(), 1);
        assert_all_covered(&channels, &sources);
    }

    #[test]
    fn test_digital_channels_coverage() {
        // P25 control + voice channels spanning ~770–774 MHz.
        let control = [772_031_250, 772_406_250, 772_706_250, 773_481_250];
        let voice = [
            770_031_250,
            770_306_250,
            771_281_250,
            771_431_250,
            771_581_250,
            771_881_250,
        ];

        let mut channels: Vec<Channel> = control
            .iter()
            .map(|&f| Channel {
                frequency: f,
                signal: Signal::Digital,
                tag: Some(Tag::Primary),
                system: Some(0),
            })
            .collect();
        channels.extend(voice.iter().map(|&f| Channel {
            frequency: f,
            signal: Signal::Digital,
            tag: None,
            system: Some(0),
        }));

        let devices = vec![Device::rtlsdr(), Device::rtlsdr(), Device::rtlsdr()];

        let sources = solve(&channels, &devices).expect("should find a solution");
        assert!(!sources.is_empty());
        assert_all_covered(&channels, &sources);
    }

    #[test]
    fn test_single_channel() {
        let channels = vec![Channel {
            frequency: 460_000_000,
            signal: Signal::Analog,
            tag: None,
            system: None,
        }];
        let devices = vec![Device::rtlsdr()];

        let sources = solve(&channels, &devices).expect("should find a solution");
        assert_eq!(sources.len(), 1);
        assert_all_covered(&channels, &sources);
        // Rate should be minimal — smallest valid RTL-SDR rate (multiple of 24k, in 225001..300000)
        assert!(sources[0].rate <= 300_000);
    }

    #[test]
    fn test_impossible_assignment() {
        // Two channels 10 MHz apart — impossible with a single RTL-SDR (max ~2.56 MHz)
        let channels = vec![
            Channel {
                frequency: 460_000_000,
                signal: Signal::Analog,
                tag: None,
                system: None,
            },
            Channel {
                frequency: 470_000_000,
                signal: Signal::Analog,
                tag: None,
                system: None,
            },
        ];
        let devices = vec![Device::rtlsdr()];

        let result = solve(&channels, &devices);
        assert!(result.is_err());
    }
}
