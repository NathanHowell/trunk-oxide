//! Plans the sources: the centre and rate each dongle runs so that every
//! channel is covered, as far from the band edges as a margin asks.
//!
//! The channels one dongle covers are contiguous in frequency (one that
//! skipped a channel between two it covers would cover that too), so a plan is
//! the sorted channels cut into groups, one per dongle. That is a small exact
//! search: for each group and dongle the cheapest rate and centre that hold
//! every channel inside the margin, then a dynamic program over the cut
//! points and the dongles in use. It replaced an SMT model that had to be
//! told every disjunct and could only ask for the fewest dongles.

use anyhow::{Result, bail};
use trunk_channelizer::passband;

use crate::device::{Device, SampleRate};
use crate::{CENTER_CLEARANCE, CHANNEL_HALFWIDTH, Channel, Signal, Source, Tag};

/// The guard each side of a band sampled at `rate`: what lies past
/// [`passband::USABLE`] of Nyquist, where the RTL's filter rolls off. A
/// channel's centre must sit `guard + CHANNEL_HALFWIDTH` inside the edge.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn roll_off_guard(rate: i64) -> i64 {
    rate / 2 - (passband::USABLE * rate as f64 / 2.0).floor() as i64
}

/// What a plan is asked for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    /// How deep in a band a channel may sit, as a share of Nyquist
    /// ([`passband::depth`]). Past [`passband::USABLE`] the RTL's filter
    /// rolls off, and a plan that cannot hold every channel to the margin
    /// loosens it toward that before it gives a channel up.
    pub margin: f64,
}

impl Default for Options {
    /// What the live config is planned to: comfortably inside the roll-off
    /// for a few thousandths of a core more than the bare limit.
    fn default() -> Self {
        Self { margin: 0.70 }
    }
}

/// Where a channel ended up.
#[derive(Clone, Debug)]
pub struct Placement {
    pub channel: Channel,
    /// The planned source that carries it most cleanly, and how deep in its
    /// band it sits ([`passband::depth`]); `None` if no source reaches it.
    pub carried: Option<(usize, f64)>,
}

/// The sources a plan chose and what it made of each channel.
#[derive(Clone, Debug)]
pub struct Plan {
    pub sources: Vec<Source>,
    /// The margin the plan holds its covered channels to; looser than asked
    /// when the asked one could not cover them all.
    pub margin: f64,
    /// Every channel, in the order given.
    pub placements: Vec<Placement>,
}

impl Plan {
    /// Channels no source carries inside the roll-off limit: alternate control
    /// channels the plan left out for want of a dongle (one a source happens
    /// to reach cleanly anyway is not among them).
    pub fn uncovered(&self) -> impl Iterator<Item = &Channel> {
        self.placements
            .iter()
            .filter(|p| p.carried.is_none_or(|(_, d)| d > passband::USABLE))
            .map(|p| &p.channel)
    }
}

/// A frequency to cover: one entry per distinct channel frequency.
#[derive(Clone, Copy, Debug)]
struct Site {
    freq: i64,
    analog: bool,
    digital: bool,
    /// Only alternate control channels may be left out.
    skippable: bool,
}

fn sites(channels: &[Channel]) -> Vec<Site> {
    let mut sorted: Vec<&Channel> = channels.iter().collect();
    sorted.sort_by_key(|c| c.frequency);
    let mut out: Vec<Site> = Vec::new();
    for c in sorted {
        let alternate = c.tag == Some(Tag::Alternate);
        match out.last_mut() {
            Some(site) if site.freq == c.frequency => {
                site.analog |= c.signal == Signal::Analog;
                site.digital |= c.signal == Signal::Digital;
                site.skippable &= alternate;
            }
            _ => out.push(Site {
                freq: c.frequency,
                analog: c.signal == Signal::Analog,
                digital: c.signal == Signal::Digital,
                skippable: alternate,
            }),
        }
    }
    out
}

/// A dongle's rates, ascending, with the signals each suits.
struct Rates(Vec<(i64, bool, bool)>);

impl Rates {
    fn of(device: &Device) -> Self {
        let mut v: Vec<(i64, bool, bool)> = supported_rates(device)
            .map(|r| {
                (
                    r,
                    channelizer_suits(r, Signal::Analog),
                    channelizer_suits(r, Signal::Digital),
                )
            })
            .collect();
        v.sort_unstable();
        v.dedup();
        Self(v)
    }
}

/// A group of channels on one dongle.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Group {
    rate: i64,
    center: i64,
    /// The deepest any of its channels sits.
    depth: f64,
}

/// How deep a channel at `freq` sits in the band of `center` and `rate`.
#[allow(clippy::cast_precision_loss)]
fn depth(freq: i64, center: i64, rate: i64) -> f64 {
    ((freq - center).abs() + CHANNEL_HALFWIDTH) as f64 / (rate as f64 / 2.0)
}

/// The cheapest rate, and the centre for it, with which one dongle holds
/// every site of `group` within `cap` of Nyquist and its centre
/// [`CENTER_CLEARANCE`] off them all.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn best_group(rates: &Rates, group: &[Site], cap: f64) -> Option<Group> {
    let (lo, hi) = (group[0].freq, group[group.len() - 1].freq);
    let mid = i64::midpoint(lo, hi);
    let analog = group.iter().any(|s| s.analog);
    let digital = group.iter().any(|s| s.digital);
    for &(rate, ok_analog, ok_digital) in &rates.0 {
        if (analog && !ok_analog) || (digital && !ok_digital) {
            continue;
        }
        // every channel within `reach` of the centre
        let reach = (cap * rate as f64 / 2.0).floor() as i64 - CHANNEL_HALFWIDTH;
        let (first, last) = (hi - reach, lo + reach);
        if first > last {
            continue;
        }
        // the allowed centre nearest the middle: the middle, or the edge of
        // the zone around a channel that holds it
        let allowed = |c: i64| {
            (first..=last).contains(&c)
                && group.iter().all(|s| (s.freq - c).abs() >= CENTER_CLEARANCE)
        };
        let center = std::iter::once(mid)
            .chain(
                group
                    .iter()
                    .flat_map(|s| [s.freq - CENTER_CLEARANCE, s.freq + CENTER_CLEARANCE]),
            )
            .filter(|&c| allowed(c))
            .min_by_key(|&c| ((c - mid).abs(), c));
        if let Some(center) = center {
            let depth = group
                .iter()
                .map(|s| depth(s.freq, center, rate))
                .fold(0.0, f64::max);
            return Some(Group {
                rate,
                center,
                depth,
            });
        }
    }
    None
}

/// What a plan costs, compared in this order: alternates left out, total
/// rate, how deep the channels sit, how far the dongles move from where they
/// are now.
type Cost = (u32, i64, i64, i64);

#[derive(Clone, Copy)]
enum Back {
    Skip,
    Group {
        mask: usize,
        from: usize,
        device: usize,
        group: Group,
    },
}

type Table = Vec<Vec<Option<(Cost, Back)>>>;

/// Keep `cost` at `(mask, i)` if it beats what is there.
fn offer(best: &mut Table, (mask, i): (usize, usize), cost: Cost, back: Back) {
    if best[mask][i].is_none_or(|(c, _)| cost < c) {
        best[mask][i] = Some((cost, back));
    }
}

struct Found {
    skipped: u32,
    groups: Vec<(usize, Group)>,
}

/// The best way to cover `sites` with `devices` holding every channel within
/// `cap`, or `None`.
#[allow(clippy::cast_possible_truncation)]
fn search(sites: &[Site], devices: &[Device], rates: &[Rates], cap: f64) -> Option<Found> {
    let (n, d) = (sites.len(), devices.len());
    // the group of sites[i..j] on each device
    let mut groups: Vec<Option<Group>> = vec![None; d * n * (n + 1)];
    let at = |dev: usize, i: usize, j: usize| (dev * n + i) * (n + 1) + j;
    for (dev, r) in rates.iter().enumerate() {
        for i in 0..n {
            for j in i + 1..=n {
                groups[at(dev, i, j)] = best_group(r, &sites[i..j], cap);
            }
        }
    }

    let masks = 1usize << d;
    let mut best: Table = vec![vec![None; n + 1]; masks];
    best[0][0] = Some(((0, 0, 0, 0), Back::Skip));
    for mask in 0..masks {
        for i in 0..=n {
            let Some((cost, _)) = best[mask][i] else {
                continue;
            };
            if i < n && sites[i].skippable {
                offer(
                    &mut best,
                    (mask, i + 1),
                    (cost.0 + 1, cost.1, cost.2, cost.3),
                    Back::Skip,
                );
            }
            for dev in (0..d).filter(|dev| mask & (1 << dev) == 0) {
                for j in i + 1..=n {
                    let Some(g) = groups[at(dev, i, j)] else {
                        continue;
                    };
                    let away = devices[dev]
                        .near
                        .map_or(0, |near| (g.center - near).abs() / 1000);
                    let next = (
                        cost.0,
                        cost.1 + g.rate,
                        cost.2 + (g.depth * 1000.0).round() as i64,
                        cost.3 + away,
                    );
                    offer(
                        &mut best,
                        (mask | (1 << dev), j),
                        next,
                        Back::Group {
                            mask,
                            from: i,
                            device: dev,
                            group: g,
                        },
                    );
                }
            }
        }
    }

    let (mut mask, (cost, _)) = (0..masks)
        .filter_map(|m| best[m][n].map(|b| (m, b)))
        .min_by_key(|&(_, (c, _))| c)?;
    let mut found = Found {
        skipped: cost.0,
        groups: Vec::new(),
    };
    let mut i = n;
    while i > 0 || mask != 0 {
        match best[mask][i].map(|(_, b)| b)? {
            Back::Skip => i -= 1,
            Back::Group {
                mask: prev,
                from,
                device,
                group,
            } => {
                found.groups.push((device, group));
                (mask, i) = (prev, from);
            }
        }
    }
    found.groups.sort_by_key(|&(device, _)| device);
    Some(found)
}

/// The margins to try: the one asked for, then looser by 0.05 up to the
/// roll-off.
fn margins(asked: f64) -> Vec<f64> {
    let top = passband::USABLE;
    let mut v = vec![asked.min(top)];
    v.extend(
        (1..)
            .map(|step| asked + 0.05 * f64::from(step))
            .take_while(|&m| m < top - 1e-9),
    );
    if v.last().is_none_or(|&last| last < top) {
        v.push(top);
    }
    v
}

/// Plan the sources for `channels` on `devices`.
///
/// The plan leaves out the fewest alternate control channels, at the tightest
/// margin from [`Options::margin`] up to the roll-off that covers the rest,
/// with the least total sample rate, and every dongle that helps in use.
///
/// # Errors
/// If the primary channels cannot all be covered, even at the roll-off limit.
pub fn plan(channels: &[Channel], devices: &[Device], options: &Options) -> Result<Plan> {
    let sites = sites(channels);
    if sites.is_empty() {
        bail!("No channels to cover");
    }
    let rates: Vec<Rates> = devices.iter().map(Rates::of).collect();
    let mut chosen = None;
    for margin in margins(options.margin) {
        let Some(found) = search(&sites, devices, &rates, margin) else {
            continue;
        };
        let done = found.skipped == 0;
        chosen = Some((margin, found));
        if done {
            break;
        }
    }
    let Some((margin, found)) = chosen else {
        bail!("No valid assignment possible, add more devices");
    };

    let sources: Vec<Source> = found
        .groups
        .iter()
        .map(|&(i, g)| Source {
            center: g.center,
            rate: g.rate,
            ppm: devices[i].ppm,
            driver: devices[i].driver.clone(),
            device: devices[i].device_template.replace("{}", &i.to_string()),
            error: 0,
        })
        .collect();
    let placements = channels
        .iter()
        .map(|c| Placement {
            channel: c.clone(),
            carried: sources
                .iter()
                .enumerate()
                .map(|(n, s)| (n, depth(c.frequency, s.center, s.rate)))
                .filter(|&(_, d)| d <= 1.0)
                .min_by(|a, b| a.1.total_cmp(&b.1)),
        })
        .collect();
    Ok(Plan {
        sources,
        margin,
        placements,
    })
}

/// [`plan`] with the default [`Options`], keeping only the sources.
///
/// # Errors
/// As [`plan`].
pub fn solve(channels: &[Channel], devices: &[Device]) -> Result<Vec<Source>> {
    Ok(plan(channels, devices, &Options::default())?.sources)
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

    /// The channels of config.toml on 2026-10-21: 19 digital in two P25
    /// systems and two conventional analog.
    fn live_channels() -> Vec<Channel> {
        let digital = |f: i64, tag| Channel {
            frequency: f,
            signal: Signal::Digital,
            tag,
            system: None,
        };
        let alt = Some(Tag::Alternate);
        let pri = Some(Tag::Primary);
        let mut channels = vec![
            digital(772_031_250, alt),
            digital(772_406_250, alt),
            digital(772_706_250, alt),
            digital(773_481_250, pri),
            digital(771_856_250, alt),
            digital(772_156_250, alt),
            digital(772_306_250, pri),
            digital(772_456_250, alt),
        ];
        channels.extend(
            [
                770_306_250,
                771_281_250,
                771_431_250,
                771_581_250,
                771_881_250,
                772_606_250,
                772_906_250,
                773_181_250,
                773_931_250,
                774_406_250,
                774_681_250,
            ]
            .map(|f| digital(f, None)),
        );
        channels.extend([488_337_500, 489_087_500].map(|f| Channel {
            frequency: f,
            signal: Signal::Analog,
            tag: None,
            system: None,
        }));
        channels
    }

    fn total_rate(sources: &[Source]) -> i64 {
        sources.iter().map(|s| s.rate).sum()
    }

    #[test]
    fn four_dongles_hold_every_live_channel_to_the_margin() {
        let devices = vec![Device::rtlsdr(); 4];
        let channels = live_channels();
        let plan = plan(&channels, &devices, &Options::default()).expect("a plan");
        assert!((plan.margin - 0.70).abs() < 1e-9, "{}", plan.margin);
        assert_eq!(plan.sources.len(), 4);
        for p in &plan.placements {
            let (_, depth) = p.carried.expect("every channel is carried");
            assert!(depth <= 0.70 + 1e-9, "{} at {depth}", p.channel.frequency);
        }
        assert_eq!(plan.uncovered().count(), 0);
        // no more sample rate than the three wide sources it replaces
        assert!(total_rate(&plan.sources) <= 5_952_000, "{:?}", plan.sources);
        // the lone channel 1 MHz below the rest gets the narrowest dongle
        assert!(plan.sources.iter().any(|s| s.rate == 240_000));
    }

    #[test]
    fn no_centre_sits_on_a_channel_or_its_dc_spike() {
        let plan = plan(
            &live_channels(),
            &vec![Device::rtlsdr(); 4],
            &Options::default(),
        )
        .expect("a plan");
        for s in &plan.sources {
            for c in live_channels() {
                assert!(
                    (c.frequency - s.center).abs() >= CENTER_CLEARANCE,
                    "centre {} is {} Hz from {}",
                    s.center,
                    (c.frequency - s.center).abs(),
                    c.frequency
                );
            }
        }
    }

    #[test]
    fn three_dongles_cannot_hold_the_margin_so_the_plan_gives_up_an_alternate_not_a_primary() {
        let devices = vec![Device::rtlsdr(); 3];
        let channels = live_channels();
        let plan = plan(&channels, &devices, &Options::default()).expect("a plan");
        assert_eq!(plan.sources.len(), 3);
        // the margin loosened to the roll-off limit before anything was dropped
        assert!(
            (plan.margin - passband::USABLE).abs() < 1e-9,
            "{}",
            plan.margin
        );
        assert!(plan.uncovered().next().is_some());
        for c in plan.uncovered() {
            assert_eq!(c.tag, Some(Tag::Alternate), "{}", c.frequency);
        }
        for p in &plan.placements {
            if p.channel.tag != Some(Tag::Alternate) {
                let (_, d) = p.carried.expect("carried");
                assert!(
                    d <= passband::USABLE + 1e-9,
                    "{} at {d}",
                    p.channel.frequency
                );
            }
        }
    }

    #[test]
    fn a_looser_margin_costs_less_sample_rate() {
        let devices = vec![Device::rtlsdr(); 4];
        let at = |margin| {
            let plan = plan(&live_channels(), &devices, &Options { margin }).unwrap();
            total_rate(&plan.sources)
        };
        assert!(at(0.85) < at(0.70));
        assert!(at(0.70) < at(0.60));
    }

    #[test]
    fn dongles_stay_where_they_are_when_the_plans_tie() {
        // three identical dongles, one placed near each of three far-apart
        // channels: any assignment costs the same, so each keeps its own
        let channel = |f| Channel {
            frequency: f,
            signal: Signal::Digital,
            tag: None,
            system: None,
        };
        let channels = [770_000_000, 780_000_000, 790_000_000].map(channel);
        let mut devices = vec![Device::rtlsdr(); 3];
        for (device, near) in devices
            .iter_mut()
            .zip([790_000_000, 770_000_000, 780_000_000])
        {
            device.near = Some(near);
        }
        let sources = solve(&channels, &devices).unwrap();
        let near = |s: &Source| (s.center + 5_000_000) / 10_000_000;
        assert_eq!(
            sources.iter().map(near).collect::<Vec<_>>(),
            [79, 77, 78],
            "{sources:?}"
        );
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
