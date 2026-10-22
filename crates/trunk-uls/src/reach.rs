//! Whether a licensed site can be heard from a point: free-space loss plus
//! the diffraction over the terrain between them.
//!
//! The terrain profile is bent by the earth's curvature (4/3 earth radius,
//! the usual allowance for refraction) and the obstructions on it become
//! knife edges, summed with Deygout's method (the main edge, then the
//! worst edge on each side of it). Trees and buildings are not in the
//! terrain, so this is the best a path can do, not a promise.

// Link budgets read as written, not as fused multiply-adds.
#![allow(clippy::suboptimal_flops)]

use anyhow::Result;

use crate::geo::Point;
use crate::terrain::Terrain;

const EARTH_RADIUS_M: f64 = 6.371e6;
/// The effective earth radius factor.
const K: f64 = 4.0 / 3.0;
/// A path is sampled this often, m.
pub const STEP_M: f64 = 100.0;

/// Free-space path loss, dB.
#[must_use]
pub fn free_space_db(distance_km: f64, freq_mhz: f64) -> f64 {
    32.45 + 20.0 * distance_km.log10() + 20.0 * freq_mhz.log10()
}

/// The loss a knife edge adds, dB, for the Fresnel–Kirchhoff parameter `v`
/// (ITU-R P.526): none while the edge is well below the line of sight.
#[must_use]
pub fn knife_edge_db(v: f64) -> f64 {
    if v <= -0.78 {
        return 0.0;
    }
    let t = v - 0.1;
    6.9 + 20.0 * (t.mul_add(t, 1.0).sqrt() + t).log10()
}

/// Terrain height at `distance_m` along a path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub distance_m: f64,
    pub ground_m: f64,
}

/// What the terrain does to a path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Link {
    pub distance_km: f64,
    pub free_space_db: f64,
    /// Added by obstructions, dB; 0 on a clear path.
    pub diffraction_db: f64,
    /// The worst obstruction's height above the line of sight, m; negative
    /// when the path clears everything by that much.
    pub worst_above_line_m: f64,
}

impl Link {
    #[must_use]
    pub fn loss_db(&self) -> f64 {
        self.free_space_db + self.diffraction_db
    }
}

/// The power at the receiver's antenna terminals, dBm, for a transmitter of
/// `erp_w` watts effective radiated power over a path losing `loss_db`
/// (a receiving antenna of 0 dBi, no cable).
#[must_use]
pub fn received_dbm(erp_w: f64, loss_db: f64) -> f64 {
    10.0 * (erp_w * 1000.0).log10() - loss_db
}

/// The terrain between two points, `STEP_M` apart, ends included.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn profile(terrain: &mut Terrain, from: Point, to: Point) -> Result<Vec<Sample>> {
    let total = from.distance_km(to) * 1000.0;
    let n = ((total / STEP_M).ceil() as usize).max(1);
    (0..=n)
        .map(|i| {
            let f = i as f64 / n as f64;
            let at = Point {
                lat: from.lat + (to.lat - from.lat) * f,
                lon: from.lon + (to.lon - from.lon) * f,
            };
            Ok(Sample {
                distance_m: total * f,
                ground_m: terrain.height(at)?,
            })
        })
        .collect()
}

/// The path loss over `profile` between antennas `tx_m` and `rx_m` above
/// sea level.
#[must_use]
pub fn link(profile: &[Sample], tx_m: f64, rx_m: f64, freq_mhz: f64) -> Link {
    let total = profile.last().map_or(0.0, |s| s.distance_m);
    let lambda = 299.792_458 / freq_mhz;
    // the terrain, raised by the earth's bulge at that point
    let height: Vec<(f64, f64)> = profile
        .iter()
        .map(|s| {
            let bulge = s.distance_m * (total - s.distance_m) / (2.0 * K * EARTH_RADIUS_M);
            (s.distance_m, s.ground_m + bulge)
        })
        .collect();
    let mut ends = height;
    let last = ends.len() - 1;
    ends[0].1 = tx_m;
    ends[last].1 = rx_m;
    let (diffraction_db, worst_above_line_m) = deygout(&ends, 0, ends.len() - 1, lambda, 0);
    Link {
        distance_km: total / 1000.0,
        free_space_db: free_space_db(total / 1000.0, freq_mhz),
        diffraction_db,
        worst_above_line_m,
    }
}

/// The loss over `h[lo..=hi]` and the worst height above the line between
/// its ends.
fn deygout(h: &[(f64, f64)], lo: usize, hi: usize, lambda: f64, depth: u8) -> (f64, f64) {
    let ((d_lo, h_lo), (d_hi, h_hi)) = (h[lo], h[hi]);
    let mut worst: Option<(usize, f64, f64)> = None;
    for (i, &(d, top)) in h.iter().enumerate().take(hi).skip(lo + 1) {
        let line = h_lo + (h_hi - h_lo) * (d - d_lo) / (d_hi - d_lo);
        let above = top - line;
        let (d1, d2) = (d - d_lo, d_hi - d);
        let v = above * (2.0 * (d1 + d2) / (lambda * d1 * d2)).sqrt();
        if worst.is_none_or(|(_, w, _)| v > w) {
            worst = Some((i, v, above));
        }
    }
    let Some((at, v, above)) = worst else {
        return (0.0, f64::NEG_INFINITY);
    };
    if v <= -0.78 {
        return (0.0, above);
    }
    let mut loss = knife_edge_db(v);
    if depth < 1 {
        // A side edge counts only if it really obstructs its sub-path; the
        // samples beside the main edge graze that line at v just under 0 and
        // would each add ~6 dB over flat ground.
        for (a, b) in [(lo, at), (at, hi)] {
            let (side, above) = deygout(h, a, b, lambda, depth + 1);
            if above > 0.0 {
                loss += side;
            }
        }
    }
    (loss, above)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn flat(km: f64, ground_m: f64) -> Vec<Sample> {
        let n = (km * 1000.0 / STEP_M) as usize;
        (0..=n)
            .map(|i| Sample {
                distance_m: i as f64 * STEP_M,
                ground_m,
            })
            .collect()
    }

    #[test]
    fn free_space_loss_is_32_45_at_1_km_1_mhz() {
        assert!((free_space_db(1.0, 1.0) - 32.45).abs() < 1e-9);
        // 10 times as far or as high is 20 dB more
        assert!((free_space_db(10.0, 770.0) - free_space_db(1.0, 770.0) - 20.0).abs() < 1e-9);
    }

    #[test]
    fn a_knife_edge_on_the_line_costs_6_db_and_far_below_it_nothing() {
        assert!(
            (knife_edge_db(0.0) - 6.03).abs() < 0.05,
            "{}",
            knife_edge_db(0.0)
        );
        assert_eq!(knife_edge_db(-1.0), 0.0);
        assert!(knife_edge_db(2.0) > knife_edge_db(1.0));
    }

    #[test]
    fn a_short_path_over_flat_ground_is_free_space() {
        let l = link(&flat(5.0, 10.0), 40.0, 15.0, 770.0);
        // the ground only just clears the line of sight (v ~ -0.78)
        assert!(l.diffraction_db < 0.5, "{}", l.diffraction_db);
        assert!(l.worst_above_line_m < 0.0, "{}", l.worst_above_line_m);
        assert!(l.loss_db() - free_space_db(5.0, 770.0) < 0.5);
    }

    #[test]
    fn a_100_w_transmitter_over_80_db_of_loss_arrives_at_minus_30_dbm() {
        // 100 W = 50 dBm
        assert!((received_dbm(100.0, 80.0) + 30.0).abs() < 1e-9);
    }

    #[test]
    fn a_hill_in_the_way_adds_loss() {
        let mut p = flat(20.0, 10.0);
        let mid = p.len() / 2;
        for s in &mut p[mid - 5..=mid + 5] {
            s.ground_m = 300.0;
        }
        let blocked = link(&p, 40.0, 15.0, 770.0);
        assert!(blocked.worst_above_line_m > 200.0);
        assert!(blocked.diffraction_db > 20.0, "{}", blocked.diffraction_db);
        let clear = link(&flat(20.0, 10.0), 40.0, 15.0, 770.0);
        assert!(blocked.loss_db() > clear.loss_db() + 20.0);
    }

    #[test]
    fn the_earths_curve_hides_a_low_antenna_far_away() {
        // 60 km of flat ground: the bulge is ~85 m at the middle, which a
        // 10 m antenna at each end cannot see over
        let far = link(&flat(60.0, 0.0), 10.0, 10.0, 770.0);
        assert!(far.diffraction_db > 10.0, "{}", far.diffraction_db);
        // raised to 100 m each it clears
        let high = link(&flat(60.0, 0.0), 100.0, 100.0, 770.0);
        assert!(high.diffraction_db < 1.0, "{}", high.diffraction_db);
    }
}
