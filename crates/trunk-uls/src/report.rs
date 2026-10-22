//! The licensed sites set against what the config scans.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use anyhow::Result;
use trunk_config::{Config, SystemType};

use crate::select::{Licensee, Site};

/// One trunked system's channels, Hz.
#[derive(Clone, Debug)]
pub struct SystemChannels {
    pub name: String,
    pub channels: BTreeSet<i64>,
}

/// The P25 systems of a config with their control and voice channels.
#[allow(clippy::cast_possible_truncation)]
#[must_use]
pub fn systems_of(config: &Config) -> Vec<SystemChannels> {
    let mut systems: Vec<_> = config
        .system
        .iter()
        .filter(|(_, s)| s.system_type == SystemType::P25)
        .map(|(name, s)| SystemChannels {
            name: name.clone(),
            channels: s
                .control_channels
                .iter()
                .chain(&s.voice_channels)
                .map(|f| f.round() as i64)
                .collect(),
        })
        .collect();
    systems.sort_by(|a, b| a.name.cmp(&b.name));
    systems
}

/// The span the systems' channels cover, Hz, widened by `margin_hz`.
#[must_use]
pub fn band_of(
    systems: &[SystemChannels],
    margin_hz: i64,
) -> Option<std::ops::RangeInclusive<i64>> {
    let low = systems.iter().filter_map(|s| s.channels.first()).min()?;
    let high = systems.iter().filter_map(|s| s.channels.last()).max()?;
    Some(low - margin_hz..=high + margin_hz)
}

/// A licensee's channels set against the systems.
#[derive(Debug)]
pub struct Match<'a> {
    pub licensee: &'a Licensee,
    /// Every channel any of its sites is licensed for.
    pub channels: BTreeSet<i64>,
    /// The system with the most channels in common, and how many.
    pub system: Option<(&'a SystemChannels, usize)>,
}

/// Each licensee with the config system it overlaps most, those that
/// overlap one first.
#[must_use]
pub fn matches<'a>(licensees: &'a [Licensee], systems: &'a [SystemChannels]) -> Vec<Match<'a>> {
    let mut out: Vec<_> = licensees
        .iter()
        .map(|licensee| {
            let channels: BTreeSet<i64> = licensee
                .sites
                .iter()
                .flat_map(|s| s.channels.iter().copied())
                .collect();
            let system = systems
                .iter()
                .map(|s| (s, s.channels.intersection(&channels).count()))
                .filter(|&(_, n)| n > 0)
                .max_by_key(|&(_, n)| n);
            Match {
                licensee,
                channels,
                system,
            }
        })
        .collect();
    out.sort_by_key(|m| (m.system.is_none(), m.licensee.name.clone()));
    out
}

#[allow(clippy::cast_precision_loss)]
fn mhz(hz: i64) -> String {
    format!("{:.5}", hz as f64 / 1e6)
}

fn mhz_list(channels: impl IntoIterator<Item = i64>) -> String {
    channels.into_iter().map(mhz).collect::<Vec<_>>().join(" ")
}

fn site_line(site: &Site) -> String {
    let mut line = format!(
        "{}/{} {} ({}), {:.1} km, {:.4},{:.4}",
        site.call,
        site.number,
        if site.name.is_empty() {
            "-"
        } else {
            &site.name
        },
        site.city,
        site.distance_km,
        site.at.lat,
        site.at.lon
    );
    if let Some(g) = site.ground_m {
        let _ = write!(line, ", ground {g:.0} m");
    }
    if let Some(h) = site.structure_m {
        let _ = write!(line, ", structure {h:.0} m");
    }
    if let Some(e) = site.erp_w {
        let _ = write!(line, ", ERP {e:.0} W");
    }
    line
}

/// The text report: per licensee, its sites grouped by channel set, each
/// group saying how much of it the config scans.
#[must_use]
pub fn render(licensees: &[Licensee], systems: &[SystemChannels]) -> String {
    let in_config: BTreeSet<i64> = systems
        .iter()
        .flat_map(|s| s.channels.iter().copied())
        .collect();
    let mut out = String::new();
    for m in matches(licensees, systems) {
        let _ = write!(
            out,
            "{}: {} sites, {} channels",
            m.licensee.name,
            m.licensee.sites.len(),
            m.channels.len()
        );
        match m.system {
            Some((s, n)) => {
                let _ = writeln!(out, "; overlaps {} on {n}", s.name);
            }
            None => out.push_str("; none of its channels are in the config\n"),
        }
        let mut groups: BTreeMap<&BTreeSet<i64>, Vec<&Site>> = BTreeMap::new();
        for site in &m.licensee.sites {
            groups.entry(&site.channels).or_default().push(site);
        }
        let mut groups: Vec<_> = groups.into_iter().collect();
        groups.sort_by_key(|(set, sites)| (std::cmp::Reverse(sites.len()), (*set).clone()));
        for (set, sites) in groups {
            let scanned = set.iter().filter(|c| in_config.contains(c)).count();
            let _ = write!(out, "  {} channels, {scanned} in the config", set.len());
            if m.system.is_some() {
                let _ = write!(
                    out,
                    "; not scanned: {}",
                    mhz_list(set.iter().copied().filter(|c| !in_config.contains(c)))
                );
            }
            out.push('\n');
            for site in sites {
                let _ = writeln!(out, "    {}", site_line(site));
            }
        }
        out.push('\n');
    }
    out
}

/// One row per site.
pub fn write_csv<W: std::io::Write>(
    out: W,
    licensees: &[Licensee],
    systems: &[SystemChannels],
) -> Result<()> {
    let in_config: BTreeSet<i64> = systems
        .iter()
        .flat_map(|s| s.channels.iter().copied())
        .collect();
    let mut csv = csv::Writer::from_writer(out);
    csv.write_record([
        "licensee",
        "system",
        "call",
        "site",
        "name",
        "city",
        "county",
        "state",
        "lat",
        "lon",
        "distance_km",
        "ground_m",
        "structure_m",
        "erp_w",
        "channels",
        "scanned",
        "not_scanned",
    ])?;
    for m in matches(licensees, systems) {
        let system = m.system.map_or("", |(s, _)| s.name.as_str());
        for s in &m.licensee.sites {
            let scanned = s.channels.iter().copied().filter(|c| in_config.contains(c));
            let missing = s
                .channels
                .iter()
                .copied()
                .filter(|c| !in_config.contains(c));
            let opt = |v: Option<f64>| v.map_or(String::new(), |v| format!("{v:.1}"));
            csv.write_record([
                m.licensee.name.as_str(),
                system,
                &s.call,
                &s.number.to_string(),
                &s.name,
                &s.city,
                &s.county,
                &s.state,
                &format!("{:.5}", s.at.lat),
                &format!("{:.5}", s.at.lon),
                &format!("{:.2}", s.distance_km),
                &opt(s.ground_m),
                &opt(s.structure_m),
                &opt(s.erp_w),
                &mhz_list(s.channels.iter().copied()),
                &mhz_list(scanned),
                &mhz_list(missing),
            ])?;
        }
    }
    csv.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select::{find, tests as sel};

    fn systems() -> Vec<SystemChannels> {
        vec![
            SystemChannels {
                name: "smco".into(),
                channels: [771_281_250, 772_031_250].into(),
            },
            SystemChannels {
                name: "other".into(),
                channels: [775_000_000].into(),
            },
        ]
    }

    #[test]
    fn a_licensee_is_matched_to_the_system_it_overlaps_most() {
        let licensees = find(&mut sel::sample(), &sel::query()).unwrap();
        let systems = systems();
        let m = matches(&licensees, &systems);
        assert_eq!(m.len(), 1);
        let (system, n) = m[0].system.unwrap();
        assert_eq!((system.name.as_str(), n), ("smco", 1));
    }

    #[test]
    fn the_report_names_the_channels_the_config_does_not_scan() {
        let licensees = find(&mut sel::sample(), &sel::query()).unwrap();
        let text = render(&licensees, &systems());
        assert!(
            text.contains("SAN MATEO, COUNTY OF: 1 sites, 2 channels; overlaps smco on 1"),
            "{text}"
        );
        assert!(
            text.contains("2 channels, 1 in the config; not scanned: 770.03125"),
            "{text}"
        );
        assert!(text.contains("WQAA001/1 ROLPH HILL (WOODSIDE)"), "{text}");
        assert!(text.contains("ERP 275 W"), "{text}");
    }

    #[test]
    fn the_csv_has_a_row_per_site() {
        let licensees = find(&mut sel::sample(), &sel::query()).unwrap();
        let mut out = Vec::new();
        write_csv(&mut out, &licensees, &systems()).unwrap();
        let text = String::from_utf8(out).unwrap();
        let mut lines = text.lines();
        assert!(
            lines
                .next()
                .unwrap()
                .starts_with("licensee,system,call,site")
        );
        let row = lines.next().unwrap();
        assert!(
            row.contains("\"SAN MATEO, COUNTY OF\",smco,WQAA001,1,ROLPH HILL"),
            "{row}"
        );
        assert!(
            row.ends_with("770.03125 771.28125,771.28125,770.03125"),
            "{row}"
        );
        assert!(lines.next().is_none());
    }

    #[test]
    fn the_band_is_the_systems_span_widened() {
        assert_eq!(
            band_of(&systems(), 1_000_000),
            Some(770_281_250..=776_000_000)
        );
        assert_eq!(band_of(&[], 1), None);
    }
}
