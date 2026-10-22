//! Finding the licensed sites near a point.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Read, Seek};
use std::ops::RangeInclusive;

use anyhow::{Context, Result};
use zip::ZipArchive;

use crate::geo::Point;
use crate::records::{col, degrees, field, for_each_row, number};

/// What to look for: fixed sites within `radius_km` of `home` that are
/// licensed for a channel in `band_hz`.
#[derive(Clone, Debug)]
pub struct Query {
    pub home: Point,
    pub radius_km: f64,
    pub band_hz: RangeInclusive<i64>,
}

/// One licensed site: where it is and the channels it may transmit.
#[derive(Clone, Debug)]
pub struct Site {
    pub call: String,
    pub number: u32,
    pub name: String,
    pub city: String,
    pub county: String,
    pub state: String,
    pub at: Point,
    pub distance_km: f64,
    /// Terrain height at the site, m above sea level.
    pub ground_m: Option<f64>,
    /// Height of the antenna structure above the ground, m.
    pub structure_m: Option<f64>,
    /// The most effective radiated power licensed on any channel, W.
    pub erp_w: Option<f64>,
    /// Licensed channels in the band, Hz.
    pub channels: BTreeSet<i64>,
}

/// Everything one licensee has near the point, nearest site first.
#[derive(Clone, Debug)]
pub struct Licensee {
    pub name: String,
    pub sites: Vec<Site>,
}

type Key = (u32, u32);

fn id(row: &[&str], i: usize) -> Option<u32> {
    field(row, i).parse().ok()
}

#[allow(clippy::cast_possible_truncation)]
fn hz(mhz: f64) -> i64 {
    (mhz * 1e6).round() as i64
}

fn table<'a, R: Read + Seek>(
    zip: &'a mut ZipArchive<R>,
    name: &str,
) -> Result<zip::read::ZipFile<'a, R>> {
    zip.by_name(name)
        .with_context(|| format!("{name} is not in the ULS zip"))
}

/// The active licences' sites near `q.home` that are licensed in `q.band_hz`,
/// by licensee.
pub fn find<R: Read + Seek>(zip: &mut ZipArchive<R>, q: &Query) -> Result<Vec<Licensee>> {
    let mut sites: HashMap<Key, Site> = HashMap::new();
    for_each_row(table(zip, "LO.dat")?, |row| {
        if field(row, col::LO_KIND) != "F" {
            return;
        }
        let (Some(usi), Some(no)) = (id(row, col::USI), id(row, col::LO_SITE)) else {
            return;
        };
        let (Some(lat), Some(lon)) = (degrees(row, col::LO_LAT), degrees(row, col::LO_LON)) else {
            return;
        };
        let at = Point { lat, lon };
        let distance_km = q.home.distance_km(at);
        if distance_km > q.radius_km {
            return;
        }
        sites.insert(
            (usi, no),
            Site {
                call: field(row, col::CALL).to_owned(),
                number: no,
                name: field(row, col::LO_NAME).to_owned(),
                city: field(row, col::LO_CITY).to_owned(),
                county: field(row, col::LO_COUNTY).to_owned(),
                state: field(row, col::LO_STATE).to_owned(),
                at,
                distance_km,
                ground_m: number(row, col::LO_GROUND_M),
                structure_m: number(row, col::LO_STRUCTURE_M),
                erp_w: None,
                channels: BTreeSet::new(),
            },
        );
    })?;

    for_each_row(table(zip, "FR.dat")?, |row| {
        let (Some(usi), Some(site)) = (id(row, col::USI), id(row, col::FR_SITE)) else {
            return;
        };
        let Some(entry) = sites.get_mut(&(usi, site)) else {
            return;
        };
        let Some(mhz) = number(row, col::FR_MHZ) else {
            return;
        };
        let channel = hz(mhz);
        if !q.band_hz.contains(&channel) {
            return;
        }
        entry.channels.insert(channel);
        if let Some(erp) = number(row, col::FR_ERP) {
            entry.erp_w = Some(entry.erp_w.map_or(erp, |e| e.max(erp)));
        }
    })?;
    sites.retain(|_, s| !s.channels.is_empty());

    let wanted: HashSet<u32> = sites.keys().map(|&(usi, _)| usi).collect();
    let mut active = HashSet::new();
    for_each_row(table(zip, "HD.dat")?, |row| {
        if let Some(usi) = id(row, col::USI)
            && wanted.contains(&usi)
            && field(row, col::HD_STATUS) == "A"
        {
            active.insert(usi);
        }
    })?;
    sites.retain(|&(usi, _), _| active.contains(&usi));

    let mut names: HashMap<u32, String> = HashMap::new();
    for_each_row(table(zip, "EN.dat")?, |row| {
        if let Some(usi) = id(row, col::USI)
            && active.contains(&usi)
            && field(row, col::EN_KIND) == "L"
        {
            names.insert(usi, field(row, col::EN_NAME).to_owned());
        }
    })?;

    let mut by_name: BTreeMap<String, Vec<Site>> = BTreeMap::new();
    for ((usi, _), site) in sites {
        let name = names
            .get(&usi)
            .filter(|n| !n.is_empty())
            .cloned()
            .unwrap_or_else(|| site.call.clone());
        by_name.entry(name).or_default().push(site);
    }
    Ok(by_name
        .into_iter()
        .map(|(name, mut sites)| {
            sites.sort_by(|a, b| a.distance_km.total_cmp(&b.distance_km));
            Licensee { name, sites }
        })
        .collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{Cursor, Write};

    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    use super::*;

    const LO_ROLPH: &str = "LO|10|||WQAA001||F|T|1|P||1 HILL RD|WOODSIDE|SAN MATEO|CA||||720.8|37|19|50.7|N|122|12|49.7|W||||||||||||18.3|18.3|TOWER||ROLPH HILL";
    // the same licence's second site, far away
    const LO_FAR: &str = "LO|10|||WQAA001||F|T|2|P||2 FLAT ST|FRESNO|FRESNO|CA||||90.0|36|44|0.0|N|119|47|0.0|W||||||||||||10.0|10.0|TOWER||FRESNO";
    // a mobile location is not a site
    const LO_MOBILE: &str = "LO|10|||WQAA001||M|T|3|P||||SAN MATEO|CA";
    // an expired licence next door
    const LO_EXPIRED: &str = "LO|11|||WQBB002||F|T|1|P||3 PEAK RD|PACIFICA|SAN MATEO|CA||||578.5|37|33|41.0|N|122|28|40.0|W||||||||||||27.0|27.0|TOWER||NORTH PEAK";

    /// A zip of the four tables with the rows above.
    pub fn sample() -> ZipArchive<Cursor<Vec<u8>>> {
        fn add(out: &mut ZipWriter<Cursor<Vec<u8>>>, name: &str, rows: &[&str]) {
            out.start_file(name, SimpleFileOptions::default()).unwrap();
            for row in rows {
                writeln!(out, "{row}").unwrap();
            }
        }
        let mut out = ZipWriter::new(Cursor::new(Vec::new()));
        add(
            &mut out,
            "LO.dat",
            &[LO_ROLPH, LO_FAR, LO_MOBILE, LO_EXPIRED],
        );
        add(
            &mut out,
            "FR.dat",
            &[
                "FR|10|||WQAA001||1|1|FB2||770.03125000|||||60.300|275.400",
                "FR|10|||WQAA001||1|1|FB2||771.28125000|||||60.300|150.000",
                "FR|10|||WQAA001||1|1|FB2||453.50000000|||||60.300|275.400",
                "FR|10|||WQAA001||2|1|FB2||770.03125000|||||60.300|275.400",
                "FR|11|||WQBB002||1|1|FB2||770.03125000|||||60.300|10.000",
            ],
        );
        add(
            &mut out,
            "HD.dat",
            &["HD|10|0001||WQAA001|A|SY", "HD|11|0002||WQBB002|E|SY"],
        );
        add(
            &mut out,
            "EN.dat",
            &[
                "EN|10|||WQAA001|L|L001|SAN MATEO, COUNTY OF",
                "EN|11|||WQBB002|L|L002|SOMEONE ELSE",
            ],
        );
        ZipArchive::new(out.finish().unwrap()).unwrap()
    }

    pub fn query() -> Query {
        Query {
            home: Point {
                lat: 37.46,
                lon: -122.18,
            },
            radius_km: 60.0,
            band_hz: 769_000_000..=776_000_000,
        }
    }

    #[test]
    fn the_sites_are_the_active_fixed_ones_near_home_licensed_in_the_band() {
        let found = find(&mut sample(), &query()).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        let county = &found[0];
        assert_eq!(county.name, "SAN MATEO, COUNTY OF");
        assert_eq!(county.sites.len(), 1);
        let site = &county.sites[0];
        assert_eq!((site.call.as_str(), site.number), ("WQAA001", 1));
        assert_eq!(site.name, "ROLPH HILL");
        assert_eq!(site.county, "SAN MATEO");
        // the 453.5 MHz channel is out of the band
        assert_eq!(
            site.channels.iter().copied().collect::<Vec<_>>(),
            [770_031_250, 771_281_250]
        );
        assert_eq!(site.erp_w, Some(275.4));
        assert_eq!(site.ground_m, Some(720.8));
        assert_eq!(site.structure_m, Some(18.3));
        assert!(
            (site.distance_km - 14.7).abs() < 0.3,
            "{}",
            site.distance_km
        );
    }

    #[test]
    fn a_wider_radius_finds_the_far_site_too() {
        let q = Query {
            radius_km: 400.0,
            ..query()
        };
        let found = find(&mut sample(), &q).unwrap();
        assert_eq!(found[0].sites.len(), 2);
        assert!(found[0].sites[0].distance_km < found[0].sites[1].distance_km);
    }
}
