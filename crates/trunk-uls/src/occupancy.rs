//! How often each channel carries a signal, from an `rtl_power` sweep.
//!
//! `rtl_power -f 769M:775.2M:3125 -i 2 -e 30m -c 20% -d SERIAL out.csv` writes
//! one row per hop per interval: the date, time, the hop's low and high edge
//! (Hz), the bin width, the sample count, then a power (dB) per bin. A channel
//! is *busy* in a row when its strongest bin is `threshold_db` over that row's
//! median bin (the noise: most of a 6 MHz hop is empty).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use anyhow::{Context, Result};

use crate::report::SystemChannels;

/// How far from a channel's centre a bin still counts as the channel, Hz.
pub const HALF_WINDOW_HZ: f64 = 3_500.0;

/// 700 MHz public-safety channels sit on a 12.5 kHz grid from here, Hz.
pub const GRID_ORIGIN_HZ: i64 = 769_006_250;

/// One hop of one interval.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub low_hz: f64,
    pub high_hz: f64,
    pub step_hz: f64,
    pub db: Vec<f64>,
}

/// The rows of an `rtl_power` CSV.
pub fn read_sweep<R: Read>(reader: R) -> Result<Vec<Row>> {
    let mut csv = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(reader);
    let mut rows = Vec::new();
    for (n, record) in csv.records().enumerate() {
        let record = record?;
        let num = |i: usize| -> Result<f64> {
            record
                .get(i)
                .with_context(|| format!("row {}: no column {i}", n + 1))?
                .parse()
                .with_context(|| format!("row {}: column {i}", n + 1))
        };
        let db = (6..record.len()).map(&num).collect::<Result<Vec<_>>>()?;
        rows.push(Row {
            low_hz: num(2)?,
            high_hz: num(3)?,
            step_hz: num(4)?,
            db,
        });
    }
    Ok(rows)
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted.get(sorted.len() / 2).copied().unwrap_or(f64::NAN)
}

/// The strongest bin within `HALF_WINDOW_HZ` of `centre_hz`, over `noise_db`;
/// `None` when the window is not wholly inside the row.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn excess_db(row: &Row, noise_db: f64, centre_hz: f64) -> Option<f64> {
    if centre_hz - HALF_WINDOW_HZ < row.low_hz || centre_hz + HALF_WINDOW_HZ > row.high_hz {
        return None;
    }
    // bin i is centred on low + (i + 0.5) * step
    let first = ((centre_hz - HALF_WINDOW_HZ - row.low_hz) / row.step_hz - 0.5)
        .ceil()
        .max(0.0) as usize;
    let last = ((centre_hz + HALF_WINDOW_HZ - row.low_hz) / row.step_hz - 0.5).floor() as usize;
    row.db
        .get(first..=last.min(row.db.len().saturating_sub(1)))?
        .iter()
        .copied()
        .max_by(f64::total_cmp)
        .map(|peak| peak - noise_db)
}

/// What a sweep saw on one channel.
#[derive(Clone, Debug, Default)]
pub struct Tally {
    pub rows: usize,
    pub busy: usize,
    /// The excess of every busy row, dB.
    pub busy_excess: Vec<f64>,
    pub max_excess: f64,
}

impl Tally {
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn busy_share(&self) -> f64 {
        if self.rows == 0 {
            0.0
        } else {
            self.busy as f64 / self.rows as f64
        }
    }

    /// The typical excess while busy, dB.
    #[must_use]
    pub fn typical_excess(&self) -> f64 {
        if self.busy_excess.is_empty() {
            0.0
        } else {
            median(&self.busy_excess)
        }
    }
}

/// Every channel's tally over `rows`.
#[must_use]
pub fn measure(rows: &[Row], channels: &BTreeSet<i64>, threshold_db: f64) -> BTreeMap<i64, Tally> {
    let mut out: BTreeMap<i64, Tally> = BTreeMap::new();
    for row in rows {
        let noise = median(&row.db);
        for &channel in channels {
            #[allow(clippy::cast_precision_loss)]
            let Some(excess) = excess_db(row, noise, channel as f64) else {
                continue;
            };
            let tally = out.entry(channel).or_default();
            tally.rows += 1;
            tally.max_excess = tally.max_excess.max(excess);
            if excess >= threshold_db {
                tally.busy += 1;
                tally.busy_excess.push(excess);
            }
        }
    }
    out
}

/// Every channel on the grid from `low_hz` to `high_hz`.
#[must_use]
pub fn grid(low_hz: i64, high_hz: i64) -> BTreeSet<i64> {
    (0..)
        .map(|n| GRID_ORIGIN_HZ + 12_500 * n)
        .skip_while(|&c| c < low_hz)
        .take_while(|&c| c <= high_hz)
        .collect()
}

/// The channels `trunk-uls sites --csv` found licensed, by licensee.
pub fn read_licences<R: Read>(reader: R) -> Result<BTreeMap<i64, BTreeSet<String>>> {
    let mut csv = csv::Reader::from_reader(reader);
    let headers = csv.headers()?.clone();
    let column = |name: &str| {
        headers
            .iter()
            .position(|h| h == name)
            .with_context(|| format!("the sites CSV has no {name} column"))
    };
    let (licensee, channels) = (column("licensee")?, column("channels")?);
    let mut out: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    for record in csv.records() {
        let record = record?;
        for mhz in record.get(channels).unwrap_or("").split_whitespace() {
            let mhz: f64 = mhz.parse().with_context(|| format!("channel {mhz}"))?;
            #[allow(clippy::cast_possible_truncation)]
            out.entry((mhz * 1e6).round() as i64)
                .or_default()
                .insert(record.get(licensee).unwrap_or("").to_owned());
        }
    }
    Ok(out)
}

/// How to report.
pub struct Options {
    pub threshold_db: f64,
    /// Channels busy less than this share of the time are not listed.
    pub min_busy: f64,
    /// At most this many unlicensed channels are listed.
    pub unlicensed_limit: usize,
}

fn mhz(hz: i64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let mhz = hz as f64 / 1e6;
    format!("{mhz:.5}")
}

/// The report: the scanned channels (to show the sweep sees what is known to
/// be busy), then the busy ones the config does not scan, licensed or not.
#[must_use]
pub fn render(
    tallies: &BTreeMap<i64, Tally>,
    systems: &[SystemChannels],
    licences: &BTreeMap<i64, BTreeSet<String>>,
    options: &Options,
) -> String {
    use std::fmt::Write as _;

    let scanned_by = |c: i64| {
        systems
            .iter()
            .find(|s| s.channels.contains(&c))
            .map(|s| s.name.as_str())
    };
    let line = |c: i64, t: &Tally, what: &str| {
        format!(
            "  {} {:5.1}% busy  typically +{:4.1} dB  peak +{:4.1} dB  ({} rows)  {what}\n",
            mhz(c),
            100.0 * t.busy_share(),
            t.typical_excess(),
            t.max_excess,
            t.rows
        )
    };
    let mut out = format!(
        "busy = the strongest bin within {HALF_WINDOW_HZ:.0} Hz of the centre is {} dB over the hop's median bin\n\n",
        options.threshold_db
    );

    out.push_str("scanned by the config:\n");
    for (&c, t) in tallies {
        if let Some(system) = scanned_by(c) {
            out.push_str(&line(c, t, &format!("scanned: {system}")));
        }
    }

    let mut licensed: Vec<_> = tallies
        .iter()
        .filter(|&(&c, t)| {
            scanned_by(c).is_none()
                && licences.contains_key(&c)
                && t.busy_share() * 100.0 >= options.min_busy
        })
        .collect();
    licensed.sort_by(|a, b| b.1.busy_share().total_cmp(&a.1.busy_share()));
    let total = tallies
        .keys()
        .filter(|&&c| scanned_by(c).is_none() && licences.contains_key(&c))
        .count();
    let _ = write!(
        out,
        "\nlicensed, not scanned, busy at least {}% of the time ({} of {total} measured):\n",
        options.min_busy,
        licensed.len()
    );
    for (&c, t) in licensed {
        let names: Vec<_> = licences[&c]
            .iter()
            .map(|n| n.chars().take(30).collect::<String>())
            .collect();
        out.push_str(&line(c, t, &format!("licensed: {}", names.join("; "))));
    }

    let mut unlicensed: Vec<_> = tallies
        .iter()
        .filter(|&(&c, t)| {
            scanned_by(c).is_none()
                && !licences.contains_key(&c)
                && t.busy_share() * 100.0 >= options.min_busy
        })
        .collect();
    unlicensed.sort_by(|a, b| b.1.busy_share().total_cmp(&a.1.busy_share()));
    let _ = write!(
        out,
        "\nnot in the licence file, busy at least {}% of the time ({}):\n",
        options.min_busy,
        unlicensed.len()
    );
    for (&c, t) in unlicensed.into_iter().take(options.unlicensed_limit) {
        out.push_str(&line(c, t, "no licence found"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hop from 770.0 to 770.1 MHz of 40 bins of 2.5 kHz at -45 dB, with
    /// `bumps` (bin, dB) laid over them.
    fn hop(bumps: &[(usize, f64)]) -> Row {
        let mut db = vec![-45.0; 40];
        for &(i, v) in bumps {
            db[i] = v;
        }
        Row {
            low_hz: 770_000_000.0,
            high_hz: 770_100_000.0,
            step_hz: 2_500.0,
            db,
        }
    }

    #[test]
    fn a_channel_is_its_strongest_bin_over_the_noise() {
        // bin 20 is centred on 770.05125 MHz
        let row = hop(&[(20, -30.0)]);
        let noise = median(&row.db);
        assert_eq!(noise, -45.0);
        assert_eq!(excess_db(&row, noise, 770_051_250.0), Some(15.0));
        // a channel 12.5 kHz away does not see it
        assert_eq!(excess_db(&row, noise, 770_063_750.0), Some(0.0));
    }

    #[test]
    fn a_channel_at_the_edge_of_a_hop_is_not_measured() {
        let row = hop(&[]);
        assert_eq!(excess_db(&row, -45.0, 770_002_000.0), None);
        assert_eq!(excess_db(&row, -45.0, 770_099_000.0), None);
        assert!(excess_db(&row, -45.0, 770_005_000.0).is_some());
    }

    #[test]
    fn busy_is_the_share_of_rows_over_the_threshold() {
        let rows = [hop(&[(20, -30.0)]), hop(&[]), hop(&[(21, -38.0)]), hop(&[])];
        let channels: BTreeSet<i64> = [770_051_250].into();
        let t = &measure(&rows, &channels, 6.0)[&770_051_250];
        assert_eq!((t.rows, t.busy), (4, 2));
        assert!((t.busy_share() - 0.5).abs() < 1e-9);
        assert_eq!(t.max_excess, 15.0);
        // while busy: 15 and 7 dB, the median of two is the upper
        assert_eq!(t.typical_excess(), 15.0);
    }

    #[test]
    fn the_grid_is_every_12_5_khz_from_the_origin() {
        let g = grid(770_000_000, 770_040_000);
        assert_eq!(
            g.into_iter().collect::<Vec<_>>(),
            [770_006_250, 770_018_750, 770_031_250]
        );
    }

    #[test]
    fn rtl_power_rows_are_read_with_their_bins() {
        let text = "2026-10-22, 11:23:58, 769000253, 771066413, 2522.79, 96, -44.62, -44.78\n";
        let rows = read_sweep(text.as_bytes()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].low_hz, 769_000_253.0);
        assert_eq!(rows[0].db, [-44.62, -44.78]);
        assert!(read_sweep(b"2026-10-22, 11:23:58, 1, 2".as_slice()).is_err());
    }

    #[test]
    fn licences_are_read_from_the_sites_csv() {
        let text = "licensee,system,call,channels\n\"SAN MATEO, COUNTY OF\",smco,WQ1,770.03125 771.28125\nEBRCSA,,WQ2,771.28125\n";
        let l = read_licences(text.as_bytes()).unwrap();
        assert_eq!(
            l[&770_031_250].iter().collect::<Vec<_>>(),
            ["SAN MATEO, COUNTY OF"]
        );
        assert_eq!(l[&771_281_250].len(), 2);
    }

    #[test]
    fn the_report_separates_scanned_licensed_and_unlicensed_channels() {
        let rows = [
            hop(&[(20, -30.0), (8, -33.0), (32, -34.0)]),
            hop(&[(20, -30.0)]),
        ];
        let channels: BTreeSet<i64> = [770_051_250, 770_020_000, 770_080_000].into();
        let tallies = measure(&rows, &channels, 6.0);
        let systems = [SystemChannels {
            name: "smco".into(),
            channels: [770_051_250].into(),
        }];
        let licences: BTreeMap<i64, BTreeSet<String>> =
            [(770_020_000, ["EBRCSA".to_owned()].into())].into();
        let text = render(
            &tallies,
            &systems,
            &licences,
            &Options {
                threshold_db: 6.0,
                min_busy: 1.0,
                unlicensed_limit: 10,
            },
        );
        assert!(text.contains("770.05125 100.0% busy"), "{text}");
        assert!(text.contains("scanned: smco"), "{text}");
        assert!(text.contains("770.02000  50.0% busy"), "{text}");
        assert!(text.contains("licensed: EBRCSA"), "{text}");
        assert!(text.contains("770.08000  50.0% busy"), "{text}");
        assert!(text.contains("no licence found"), "{text}");
    }
}
