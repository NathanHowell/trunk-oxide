//! Terrain heights from the SRTM 1 arc-second tiles that AWS hosts, cached
//! outside `/tmp`. A tile is 3601 × 3601 big-endian `i16` metres, row 0 on its
//! northern edge; a missing tile (open sea) reads as 0 m.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;

use crate::geo::Point;

const VOID: i16 = i16::MIN;

/// `N37W122`: the tile whose south-west corner is 37°N 122°W.
#[must_use]
pub fn tile_name(south: i32, west: i32) -> String {
    format!(
        "{}{:02}{}{:03}",
        if south >= 0 { 'N' } else { 'S' },
        south.abs(),
        if west >= 0 { 'E' } else { 'W' },
        west.abs()
    )
}

/// One tile of heights.
#[derive(Debug)]
pub struct Tile {
    side: usize,
    south: i32,
    west: i32,
    samples: Vec<i16>,
}

impl Tile {
    /// A tile from its raw big-endian samples (a square).
    pub fn parse(south: i32, west: i32, raw: &[u8]) -> Result<Self> {
        let count = raw.len() / 2;
        let side = (1..=count).find(|s| s * s >= count).unwrap_or(0);
        if !raw.len().is_multiple_of(2) || side * side != count || side < 2 {
            bail!("{} bytes is not a square tile of 16-bit heights", raw.len());
        }
        let samples = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_be_bytes(*b))
            .collect();
        Ok(Self {
            side,
            south,
            west,
            samples,
        })
    }

    fn at(&self, row: usize, col: usize) -> Option<f64> {
        let h = self.samples[row * self.side + col];
        (h != VOID).then_some(f64::from(h))
    }

    /// The height at `p`, bilinear between the four nearest samples, or
    /// `None` outside the tile or over a void.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn height(&self, p: Point) -> Option<f64> {
        let span = (self.side - 1) as f64;
        let x = (p.lon - f64::from(self.west)) * span;
        let y = (f64::from(self.south) + 1.0 - p.lat) * span;
        if !(0.0..=span).contains(&x) || !(0.0..=span).contains(&y) {
            return None;
        }
        let (x0, y0) = (
            (x.floor() as usize).min(self.side - 2),
            (y.floor() as usize).min(self.side - 2),
        );
        let (fx, fy) = (x - x0 as f64, y - y0 as f64);
        let top = self
            .at(y0, x0)?
            .mul_add(1.0 - fx, self.at(y0, x0 + 1)? * fx);
        let bottom = self
            .at(y0 + 1, x0)?
            .mul_add(1.0 - fx, self.at(y0 + 1, x0 + 1)? * fx);
        Some(top.mul_add(1.0 - fy, bottom * fy))
    }
}

/// Tiles loaded on demand.
pub struct Terrain {
    dir: PathBuf,
    tiles: HashMap<(i32, i32), Option<Tile>>,
}

impl Terrain {
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            tiles: HashMap::new(),
        }
    }

    /// Terrain height at `p`, m above sea level; 0 where there is no tile.
    #[allow(clippy::cast_possible_truncation)]
    pub fn height(&mut self, p: Point) -> Result<f64> {
        let key = (p.lat.floor() as i32, p.lon.floor() as i32);
        if !self.tiles.contains_key(&key) {
            let tile = self.load(key.0, key.1)?;
            self.tiles.insert(key, tile);
        }
        Ok(self.tiles[&key]
            .as_ref()
            .and_then(|t| t.height(p))
            .unwrap_or(0.0))
    }

    fn load(&self, south: i32, west: i32) -> Result<Option<Tile>> {
        let name = tile_name(south, west);
        let path = self.dir.join(format!("{name}.hgt.gz"));
        if !path.exists() {
            fs::create_dir_all(&self.dir)?;
            let url = format!(
                "https://s3.amazonaws.com/elevation-tiles-prod/skadi/{}/{name}.hgt.gz",
                &name[..3]
            );
            eprintln!("downloading {url}");
            let response = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()?
                .get(&url)
                .send()?;
            // open sea has no tile; an empty file remembers that
            let body = if response.status() == reqwest::StatusCode::NOT_FOUND {
                Vec::new()
            } else {
                response.error_for_status()?.bytes()?.to_vec()
            };
            fs::write(&path, body)?;
        }
        if fs::metadata(&path)?.len() == 0 {
            return Ok(None);
        }
        let mut raw = Vec::new();
        GzDecoder::new(fs::File::open(&path)?)
            .read_to_end(&mut raw)
            .with_context(|| format!("unpacking {}", path.display()))?;
        Ok(Some(Tile::parse(south, west, &raw)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 3 × 3 tile at 37°N 122°W whose heights are row * 10 + column.
    fn tile() -> Tile {
        let raw: Vec<u8> = (0..3i16)
            .flat_map(|r| (0..3i16).flat_map(move |c| (r * 10 + c).to_be_bytes()))
            .collect();
        Tile::parse(37, -122, &raw).unwrap()
    }

    #[test]
    fn tiles_are_named_by_their_south_west_corner() {
        assert_eq!(tile_name(37, -122), "N37W122");
        assert_eq!(tile_name(-5, 7), "S05E007");
    }

    #[test]
    fn row_zero_is_the_northern_edge() {
        let t = tile();
        // north-west corner = sample (0, 0); south-east = (2, 2)
        assert_eq!(
            t.height(Point {
                lat: 38.0,
                lon: -122.0
            }),
            Some(0.0)
        );
        assert_eq!(
            t.height(Point {
                lat: 37.0,
                lon: -121.0
            }),
            Some(22.0)
        );
        assert_eq!(
            t.height(Point {
                lat: 37.0,
                lon: -122.0
            }),
            Some(20.0)
        );
    }

    #[test]
    fn heights_are_interpolated_between_samples() {
        let t = tile();
        // half way east along the northern row: (0 + 1) / 2
        assert_eq!(
            t.height(Point {
                lat: 38.0,
                lon: -121.75
            }),
            Some(0.5)
        );
        // the middle of the four nearest samples, 0 1 10 11
        assert_eq!(
            t.height(Point {
                lat: 37.75,
                lon: -121.75
            }),
            Some(5.5)
        );
    }

    #[test]
    fn outside_the_tile_or_over_a_void_there_is_no_height() {
        let t = tile();
        assert_eq!(
            t.height(Point {
                lat: 39.0,
                lon: -122.0
            }),
            None
        );
        let mut raw = vec![0u8; 18];
        raw[..2].copy_from_slice(&VOID.to_be_bytes());
        let void = Tile::parse(37, -122, &raw).unwrap();
        assert_eq!(
            void.height(Point {
                lat: 38.0,
                lon: -122.0
            }),
            None
        );
    }

    #[test]
    fn a_tile_must_be_square() {
        assert!(Tile::parse(0, 0, &[0; 10]).is_err());
        assert!(Tile::parse(0, 0, &[0; 7]).is_err());
    }
}
