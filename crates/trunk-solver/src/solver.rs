use anyhow::{Result, bail};
use trunk_channelizer::passband;
use z3::ast::{Bool, Int};

use crate::device::{Device, SampleRate};
use crate::{CHANNEL_HALFWIDTH, Channel, Signal, Source};

/// The guard each side of a band sampled at `rate`: what lies past
/// [`passband::USABLE`] of Nyquist, where the RTL's filter rolls off. A
/// channel's centre must sit `guard + CHANNEL_HALFWIDTH` inside the edge.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn roll_off_guard(rate: i64) -> i64 {
    rate / 2 - (passband::USABLE * rate as f64 / 2.0).floor() as i64
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
    // A channel must sit inside the usable band, clear of the roll-off
    // (trunk_channelizer::passband). The guard depends on the sample rate,
    // a Z3 variable, so every valid rate of each device is enumerated with
    // its own guard.
    for channel in channels {
        let freq = Int::from_i64(channel.frequency);
        let hw = Int::from_i64(CHANNEL_HALFWIDTH);

        let in_range_options: Vec<Bool> = lower_freq
            .iter()
            .zip(upper_freq.iter())
            .zip(ranges.iter())
            .zip(devices.iter())
            .map(|(((l, u), r), device)| {
                // Per-rate disjuncts, each with its rate's guard.
                let rate_disjuncts: Vec<Bool> = valid_rates(device, channel.signal)
                    .map(|rate| {
                        let guard = Int::from_i64(roll_off_guard(rate));
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

        let coverage = Bool::or(&in_range_options);
        if channel.tag == Some(crate::Tag::Alternate) {
            // Alternate control channels are covered if possible but not required.
            opt.assert_soft(&coverage, 1, None);
        } else {
            opt.assert(&coverage);
        }
    }

    Problem {
        ranges,
        lower_freq,
        upper_freq,
    }
}

/// Every sample rate a device supports that suits a `signal` channel: one
/// the channelizer runs cheaply and whose channel needs no resampling. A rate
/// whose decimation is a large prime (2.472 MS/s is 103 x 24 kHz) costs
/// several times a neighbouring rate's FFT, and an analog channel is exactly
/// 96 kS/s only at a multiple of 96 kHz.
fn valid_rates(device: &Device, signal: Signal) -> impl Iterator<Item = i64> + '_ {
    supported_rates(device).filter(move |&rate| channelizer_suits(rate, signal))
}

#[allow(clippy::cast_precision_loss)]
fn channelizer_suits(rate: i64, signal: Signal) -> bool {
    use trunk_channelizer::{NARROW, WIDE};
    trunk_channelizer::Config::standard(rate as f64).is_ok_and(|c| {
        c.is_fast()
            && c.is_exact(match signal {
                Signal::Digital => NARROW,
                Signal::Analog => WIDE,
            })
    })
}

/// Every sample rate a device supports.
fn supported_rates(device: &Device) -> impl Iterator<Item = i64> + '_ {
    device.sample_rates.iter().flat_map(move |sr| {
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
}

/// The `[recorder] max_channels` that lets every channel `sources` cover be
/// open at once: control, voice and conventional channels alike, each
/// frequency once however many sources cover it.
#[must_use]
pub fn max_channels(channels: &[Channel], sources: &[Source]) -> usize {
    channels
        .iter()
        .filter(|c| {
            sources.iter().any(|s| {
                let guard = roll_off_guard(s.rate);
                let lower = s.center - s.rate / 2 + guard;
                let upper = s.center + s.rate / 2 - guard;
                (lower..=upper).contains(&c.frequency)
            })
        })
        .map(|c| c.frequency)
        .collect::<std::collections::BTreeSet<_>>()
        .len()
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

    extract_sources(&model, &problem, devices)
}

fn extract_sources(
    model: &z3::Model,
    problem: &Problem,
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
            sources.push(Source {
                center: i64::midpoint(upper, lower),
                rate,
                ppm: device.ppm,
                driver: device.driver.clone(),
                device: device.device_template.replace("{}", &i.to_string()),
                error: 0,
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

    /// Helper: assert every channel is within the usable range of some source.
    fn assert_all_covered(channels: &[Channel], sources: &[Source]) {
        for ch in channels {
            let covered = sources.iter().any(|s| {
                let guard = roll_off_guard(s.rate);
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
    fn max_channels_counts_every_covered_frequency_once() {
        let source = |center| Source {
            center,
            rate: 912_000,
            ppm: 0,
            driver: "rtlsdr".into(),
            device: String::new(),
            error: 0,
        };
        let channel = |frequency, tag| Channel {
            frequency,
            signal: Signal::Digital,
            tag,
            system: None,
        };
        // two overlapping sources; a control channel, two voice channels
        // (one in the overlap) and one no source covers
        let sources = [source(771_000_000), source(771_400_000)];
        let channels = [
            channel(771_100_000, Some(crate::Tag::Primary)),
            channel(771_200_000, None),
            channel(771_500_000, None),
            channel(790_000_000, None),
        ];
        assert_eq!(max_channels(&channels, &sources), 3);
        assert_eq!(max_channels(&channels, &[]), 0);
    }

    #[test]
    fn the_guard_keeps_channels_out_of_the_roll_off() {
        // 2.4 MS/s: usable to 85% of 1.2 MHz = 1.02 MHz from the centre
        assert_eq!(roll_off_guard(2_400_000), 180_000);
        assert_eq!(roll_off_guard(1_872_000), 140_400);
        // the channel at 97% of source 2's Nyquist on 2026-10-19 is refused
        let (centre, rate) = (773_775_500, 1_872_000);
        let upper = centre + rate / 2 - roll_off_guard(rate) - CHANNEL_HALFWIDTH;
        assert!(774_681_250 > upper);
        assert!(774_406_250 <= upper);
    }

    #[test]
    fn test_two_conventional_channels() {
        // The exact bug case: two analog channels 750 kHz apart.
        // Old solver picked rate=912k which couldn't cover both inside its guard.
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
    fn rates_with_a_slow_channelizer_fft_are_not_offered() {
        // 2.472 MS/s is 103 x 24 kHz: its FFT needs a 103-point stage.
        let rates: Vec<i64> = valid_rates(&Device::rtlsdr(), Signal::Digital).collect();
        assert!(!rates.contains(&2_472_000));
        for fast in [2_496_000, 2_400_000, 1_872_000, 912_000] {
            assert!(rates.contains(&fast), "{fast} S/s");
        }
        assert!(rates.iter().all(|&r| channelizer_suits(r, Signal::Digital)));
    }

    #[test]
    fn an_analog_channel_is_only_offered_rates_that_need_no_resampling() {
        let rates: Vec<i64> = valid_rates(&Device::rtlsdr(), Signal::Analog).collect();
        assert!(rates.contains(&960_000));
        assert!(!rates.contains(&912_000), "9.5 x 96 kHz");
        assert!(rates.iter().all(|r| r % 96_000 == 0), "{rates:?}");
    }

    #[test]
    fn a_source_with_an_analog_channel_is_planned_on_a_96_khz_multiple() {
        // Two conventional channels a little under 0.8 MHz apart: 912 kS/s
        // would reach, but it resamples; the next multiple of 96 kHz is 960.
        let analog = |f: i64| Channel {
            frequency: f,
            signal: Signal::Analog,
            tag: None,
            system: None,
        };
        let channels = vec![analog(488_337_500), analog(489_087_500)];
        let sources = solve(&channels, &[Device::rtlsdr()]).expect("should find a solution");
        assert_all_covered(&channels, &sources);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].rate % 96_000, 0, "{} S/s", sources[0].rate);
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

    #[test]
    fn test_preferred_control_channel() {
        // Two P25 systems with control channels spread across ~771–774 MHz.
        // With preferred channels set, the solver only requires coverage of
        // the preferred one per system; alternates are soft constraints.
        let channels = vec![
            // System 0: preferred = 772_306_250
            Channel {
                frequency: 771_856_250,
                signal: Signal::Digital,
                tag: Some(Tag::Alternate),
                system: Some(0),
            },
            Channel {
                frequency: 772_156_250,
                signal: Signal::Digital,
                tag: Some(Tag::Alternate),
                system: Some(0),
            },
            Channel {
                frequency: 772_306_250,
                signal: Signal::Digital,
                tag: Some(Tag::Primary),
                system: Some(0),
            },
            Channel {
                frequency: 772_456_250,
                signal: Signal::Digital,
                tag: Some(Tag::Alternate),
                system: Some(0),
            },
            // System 1: preferred = 773_481_250
            Channel {
                frequency: 772_031_250,
                signal: Signal::Digital,
                tag: Some(Tag::Alternate),
                system: Some(1),
            },
            Channel {
                frequency: 773_481_250,
                signal: Signal::Digital,
                tag: Some(Tag::Primary),
                system: Some(1),
            },
        ];

        let devices = vec![Device::rtlsdr(), Device::rtlsdr()];
        let sources =
            solve(&channels, &devices).expect("should find a solution with preferred channels");
        assert!(!sources.is_empty());

        // The two Primary channels must be covered
        for ch in &channels {
            if ch.tag == Some(Tag::Primary) {
                let covered = sources.iter().any(|s| {
                    let guard = roll_off_guard(s.rate);
                    let min = s.center - s.rate / 2 + guard;
                    let max = s.center + s.rate / 2 - guard;
                    ch.frequency >= min + CHANNEL_HALFWIDTH
                        && ch.frequency <= max - CHANNEL_HALFWIDTH
                });
                assert!(covered, "preferred channel {} not covered", ch.frequency);
            }
        }
    }
}
