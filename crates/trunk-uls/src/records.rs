//! The ULS tables: pipe-delimited text, Latin-1, one record per line, the
//! record type (`FR`, `LO`, `HD`, `EN`) first.

use std::io::{self, BufRead, BufReader, Read};

/// Calls `each` with the fields of every line.
pub fn for_each_row<R: Read>(reader: R, mut each: impl FnMut(&[&str])) -> io::Result<()> {
    let mut reader = BufReader::with_capacity(1 << 20, reader);
    let mut raw = Vec::new();
    let mut line = String::new();
    loop {
        raw.clear();
        if reader.read_until(b'\n', &mut raw)? == 0 {
            return Ok(());
        }
        line.clear();
        line.extend(raw.iter().map(|&b| char::from(b)));
        let row: Vec<&str> = line.trim_end_matches(['\r', '\n']).split('|').collect();
        each(&row);
    }
}

/// Field `i`, or empty when the line is short.
#[must_use]
pub fn field<'a>(row: &[&'a str], i: usize) -> &'a str {
    row.get(i).copied().unwrap_or("").trim()
}

/// A number in field `i`.
#[must_use]
pub fn number(row: &[&str], i: usize) -> Option<f64> {
    field(row, i).parse().ok()
}

/// Degrees, minutes, seconds and a hemisphere (`N`, `S`, `E`, `W`) as signed
/// degrees.
#[must_use]
pub fn degrees(row: &[&str], first: usize) -> Option<f64> {
    let d = number(row, first)?;
    let m = number(row, first + 1).unwrap_or(0.0);
    let s = number(row, first + 2).unwrap_or(0.0);
    let value = d + m / 60.0 + s / 3600.0;
    match field(row, first + 3) {
        "N" | "E" => Some(value),
        "S" | "W" => Some(-value),
        _ => None,
    }
}

/// Column numbers of the tables we read.
pub mod col {
    /// Every table: the licence's unique system identifier, the call sign.
    pub const USI: usize = 1;
    pub const CALL: usize = 4;

    /// `FR` (frequency): the site it is licensed at, the channel in MHz, the
    /// transmitter's power and the effective radiated power in watts.
    pub const FR_SITE: usize = 6;
    pub const FR_MHZ: usize = 10;
    pub const FR_ERP: usize = 16;

    /// `LO` (location): fixed or mobile, the site number, where it is.
    pub const LO_KIND: usize = 6;
    pub const LO_SITE: usize = 8;
    pub const LO_CITY: usize = 12;
    pub const LO_COUNTY: usize = 13;
    pub const LO_STATE: usize = 14;
    pub const LO_GROUND_M: usize = 18;
    pub const LO_LAT: usize = 19;
    pub const LO_LON: usize = 23;
    pub const LO_STRUCTURE_M: usize = 38;
    pub const LO_NAME: usize = 42;

    /// `HD` (header): `A` for an active licence.
    pub const HD_STATUS: usize = 5;

    /// `EN` (entity): `L` is the licensee, whose name is in `EN_NAME`.
    pub const EN_KIND: usize = 5;
    pub const EN_NAME: usize = 7;
}

#[cfg(test)]
mod tests {
    use super::*;

    const LO: &str = "LO|3218421|||WQMG398||F|T|1|P||20000 SKYLINE BLVD|WOODSIDE|SAN MATEO|CA||||720.8|37|19|50.7|N|122|12|49.7|W||||||||||||18.3|18.3|TOWER||ROLPH HILL||||||||";

    #[test]
    fn a_location_row_reads_where_the_site_is() {
        let row: Vec<&str> = LO.split('|').collect();
        assert_eq!(field(&row, col::CALL), "WQMG398");
        assert_eq!(field(&row, col::LO_NAME), "ROLPH HILL");
        assert_eq!(field(&row, col::LO_COUNTY), "SAN MATEO");
        assert_eq!(number(&row, col::LO_GROUND_M), Some(720.8));
        assert_eq!(number(&row, col::LO_STRUCTURE_M), Some(18.3));
        let lat = degrees(&row, col::LO_LAT).unwrap();
        let lon = degrees(&row, col::LO_LON).unwrap();
        assert!((lat - 37.330_749).abs() < 1e-5, "{lat}");
        assert!((lon + 122.213_806).abs() < 1e-5, "{lon}");
    }

    #[test]
    fn a_short_row_has_empty_fields() {
        let row = ["LO", "1"];
        assert_eq!(field(&row, 9), "");
        assert_eq!(number(&row, 9), None);
        assert_eq!(degrees(&row, 3), None);
    }

    #[test]
    fn rows_are_latin_1_split_on_pipes() {
        let bytes = b"EN|1|||X||L|MU\xd1OZ, CITY\r\nHD|2\n";
        let mut seen = Vec::new();
        for_each_row(&bytes[..], |r| seen.push(r.join("/"))).unwrap();
        assert_eq!(seen, ["EN/1///X//L/MUÑOZ, CITY", "HD/2"]);
    }
}
