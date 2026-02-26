use anyhow::{Result, bail};
use z3::ast::{Bool, Int};

use crate::device::{Device, SampleRate};
use crate::{Channel, BANDWIDTH, HALFWIDTH, Signal, Source};

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

    let bw = Int::from_i64(BANDWIDTH);
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

        // Bandwidth alignment: range is a multiple of channel bandwidth
        let mult = Int::new_const(format!("mult_{i}"));
        opt.assert(&r.eq(&(&bw * &mult)));
    }

    // Frequency coverage constraints
    for channel in channels {
        let freq = Int::from_i64(channel.frequency);
        let hw = Int::from_i64(HALFWIDTH);

        let in_range_options: Vec<Bool> = lower_freq
            .iter()
            .zip(upper_freq.iter())
            .map(|(l, u)| {
                // freq >= lower + HALFWIDTH
                let lower_bound = freq.ge(&(l + &hw));
                // freq <= upper - HALFWIDTH
                let upper_bound = freq.le(&(u - &hw));
                // Center avoidance: center frequency doesn't land on a channel frequency
                // Use multiply-by-2 trick to avoid integer division:
                //   freq - HALFWIDTH >= (upper + lower) / 2
                //   <=> 2*(freq - HALFWIDTH) >= upper + lower
                let center_times_2 = u + l;
                let avoid_above =
                    Int::from_i64(2 * (channel.frequency - HALFWIDTH)).ge(&center_times_2);
                let avoid_below =
                    Int::from_i64(2 * (channel.frequency + HALFWIDTH)).le(&center_times_2);
                Bool::and(&[
                    lower_bound,
                    upper_bound,
                    Bool::or(&[avoid_above, avoid_below]),
                ])
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

    let model = opt.get_model().ok_or_else(|| anyhow::anyhow!("Failed to get model"))?;

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
            let analog_recorders = channels
                .iter()
                .filter(|c| {
                    c.signal == Signal::Analog && c.frequency >= lower && c.frequency <= upper
                })
                .count();

            // Count digital frequencies that are not control channels
            let digital_recorders = channels
                .iter()
                .filter(|c| {
                    c.signal == Signal::Digital
                        && c.tag.is_none()
                        && c.frequency >= lower
                        && c.frequency <= upper
                })
                .count();

            sources.push(Source {
                center: i64::midpoint(upper, lower),
                rate: upper - lower,
                gain: device.gain,
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
