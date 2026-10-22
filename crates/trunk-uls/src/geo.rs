//! Points on the earth.

use anyhow::{Context, Result, bail};

const EARTH_RADIUS_KM: f64 = 6371.0;

/// A latitude and longitude in degrees (north and east positive).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub lat: f64,
    pub lon: f64,
}

impl Point {
    /// `"37.46,-122.18"`.
    pub fn parse(text: &str) -> Result<Self> {
        let (lat, lon) = text
            .split_once(',')
            .context("a point is `lat,lon` in degrees")?;
        let lat: f64 = lat.trim().parse().context("the latitude")?;
        let lon: f64 = lon.trim().parse().context("the longitude")?;
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            bail!("{lat},{lon} is not on the earth");
        }
        Ok(Self { lat, lon })
    }

    /// Great-circle distance, km.
    #[must_use]
    pub fn distance_km(self, other: Self) -> f64 {
        let (p1, p2) = (self.lat.to_radians(), other.lat.to_radians());
        let dlat = p2 - p1;
        let dlon = (other.lon - self.lon).to_radians();
        let (sin_lat, sin_lon) = ((dlat / 2.0).sin(), (dlon / 2.0).sin());
        let a = (p1.cos() * p2.cos()).mul_add(sin_lon * sin_lon, sin_lat * sin_lat);
        2.0 * EARTH_RADIUS_KM * a.sqrt().asin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_is_lat_comma_lon() {
        let p = Point::parse(" 37.46 , -122.18").unwrap();
        assert_eq!(
            p,
            Point {
                lat: 37.46,
                lon: -122.18
            }
        );
        assert!(Point::parse("37.46").is_err());
        assert!(Point::parse("91,0").is_err());
    }

    #[test]
    fn a_degree_of_latitude_is_111_km() {
        let a = Point {
            lat: 37.0,
            lon: -122.0,
        };
        let b = Point {
            lat: 38.0,
            lon: -122.0,
        };
        assert!((a.distance_km(b) - 111.2).abs() < 0.2);
        assert!(a.distance_km(a).abs() < 1e-9);
    }
}
