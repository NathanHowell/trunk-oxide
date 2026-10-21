//! Channelizer geometry: one shared forward FFT, several channel profiles.

use std::time::Duration;

use anyhow::{Result, bail, ensure};

use crate::design::tap_count;

/// Index into [`Config::profiles`].
pub type ProfileId = usize;

/// The narrow (digital voice) profile in [`Config::standard`].
pub const NARROW: ProfileId = 0;
/// The wide (analog FM voice) profile in [`Config::standard`].
pub const WIDE: ProfileId = 1;

/// What a channel type needs; [`Config`] turns it into an integer decimation.
#[derive(Clone, Copy, Debug)]
pub struct ProfileSpec {
    /// Output rate the consumer wants (Hz). The engine's rate is at least
    /// this; a consumer that needs exactly this resamples the remainder.
    pub target_rate: f64,
    /// Highest output rate worth considering. The engine may pick a rate in
    /// `target_rate..=max_rate` if that makes the FFT sizes cheaper.
    pub max_rate: f64,
    /// Channel filter 6 dB point (Hz, one-sided).
    pub cutoff_hz: f64,
    pub transition_hz: f64,
    pub atten_db: f64,
}

impl ProfileSpec {
    /// P25/DMR/Smartnet: 12.5 kHz channel, same filter spec as
    /// `xlat_channelizer` (`low_pass_2(bw/2, bw/4, 60 dB)`), as close to
    /// 24 kS/s as an integer decimation allows.
    pub const NARROW: Self = Self {
        target_rate: 24_000.0,
        max_rate: 24_000.0,
        cutoff_hz: 6_250.0,
        transition_hz: 3_125.0,
        atten_db: 60.0,
    };

    /// Analog FM voice: 12 kHz bandwidth for the 96 kS/s demodulator chain
    /// (`low_pass_2(6 kHz, 3 kHz, 60 dB)`). Rates up to 3x are allowed so a
    /// source whose rate is not a multiple of 96 kHz can still share an FFT
    /// size with the narrow profile; the analog recorder's arbitrary resampler
    /// brings the channel to exactly 96 kS/s.
    pub const WIDE: Self = Self {
        target_rate: 96_000.0,
        max_rate: 288_000.0,
        cutoff_hz: 6_000.0,
        transition_hz: 3_000.0,
        atten_db: 60.0,
    };
}

/// A channel type, with its decimation fixed.
#[derive(Clone, Debug)]
pub struct Profile {
    /// Output rate is `sample_rate / decim`.
    pub decim: usize,
    pub cutoff_hz: f64,
    pub transition_hz: f64,
    pub atten_db: f64,
}

/// Channelizer geometry. Everything else is derived.
///
/// Forward FFT size `n` is a multiple of every profile's `decim`; each frame
/// holds `hop = n - overlap_in` new samples plus `overlap_in` saved ones.
/// A channel of profile `p` runs a `k = n / decim` point inverse FFT at
/// `rate / decim`, of which the first `overlap_in / decim` outputs are
/// time-aliased and discarded, so every block yields `hop / decim` samples
/// for that channel. The channel filter's impulse response must fit in
/// `overlap_out + 1` taps (the overlap-save constraint). Profiles share the
/// forward FFT and the block cadence; they differ only in the small inverse
/// FFT, so a mixed analog/digital source pays one forward FFT.
#[derive(Clone, Debug)]
pub struct Config {
    /// Input sample rate (Hz).
    pub sample_rate: f64,
    /// Forward FFT size.
    pub n: usize,
    /// Input samples carried between frames.
    pub overlap_in: usize,
    pub profiles: Vec<Profile>,
    /// Worker threads for the per-channel stage (0 = run on the caller).
    pub threads: usize,
}

impl Config {
    /// Narrow profile only (digital voice).
    pub fn p25(sample_rate: f64) -> Result<Self> {
        Self::with_profiles(sample_rate, &[ProfileSpec::NARROW])
    }

    /// Narrow ([`NARROW`]) and, when the source rate allows, wide ([`WIDE`]).
    pub fn standard(sample_rate: f64) -> Result<Self> {
        Self::with_profiles(sample_rate, &[ProfileSpec::NARROW, ProfileSpec::WIDE])
            .or_else(|_| Self::p25(sample_rate))
    }

    /// Pick the decimations and FFT size for `specs` (profile ids follow the
    /// slice order). The first spec's channel gets `k >= 256`, which is what
    /// the P25 path was validated with.
    pub fn with_profiles(sample_rate: f64, specs: &[ProfileSpec]) -> Result<Self> {
        ensure!(!specs.is_empty(), "no channel profiles");
        let candidates: Vec<Vec<usize>> = specs
            .iter()
            .map(|s| candidate_decims(sample_rate, s))
            .collect();
        for (spec, c) in specs.iter().zip(&candidates) {
            ensure!(
                !c.is_empty(),
                "sample rate {sample_rate} too low for a {} S/s channel",
                spec.target_rate
            );
        }
        let mut best: Option<(f64, Config)> = None;
        let mut pick = vec![0usize; specs.len()];
        loop {
            let profiles: Vec<Profile> = specs
                .iter()
                .zip(&candidates)
                .zip(&pick)
                .map(|((s, c), i)| Profile {
                    decim: c[*i],
                    cutoff_hz: s.cutoff_hz,
                    transition_hz: s.transition_hz,
                    atten_db: s.atten_db,
                })
                .collect();
            if let Some((score, n, overlap_in)) = plan(sample_rate, &profiles)
                && best.as_ref().is_none_or(|(b, _)| score < *b)
            {
                best = Some((
                    score,
                    Config {
                        sample_rate,
                        n,
                        overlap_in,
                        profiles,
                        threads: 0,
                    },
                ));
            }
            // odometer over the candidate lists
            let mut i = 0;
            while i < pick.len() {
                pick[i] += 1;
                if pick[i] < candidates[i].len() {
                    break;
                }
                pick[i] = 0;
                i += 1;
            }
            if i == pick.len() {
                break;
            }
        }
        match best {
            Some((_, cfg)) => Ok(cfg),
            None => bail!("no workable channelizer geometry for {sample_rate} S/s"),
        }
    }

    pub const fn fft_size(&self) -> usize {
        self.n
    }

    /// New input samples consumed per block.
    pub const fn hop(&self) -> usize {
        self.n - self.overlap_in
    }

    pub fn profile(&self, p: ProfileId) -> &Profile {
        &self.profiles[p]
    }

    /// Per-channel inverse FFT size.
    pub fn k(&self, p: ProfileId) -> usize {
        self.n / self.profiles[p].decim
    }

    /// Output samples discarded per frame; sets the max filter length.
    pub fn overlap_out(&self, p: ProfileId) -> usize {
        self.overlap_in / self.profiles[p].decim
    }

    /// Output samples produced per channel per block.
    pub fn out_per_block(&self, p: ProfileId) -> usize {
        self.hop() / self.profiles[p].decim
    }

    pub fn max_out_per_block(&self) -> usize {
        (0..self.profiles.len())
            .map(|p| self.out_per_block(p))
            .max()
            .unwrap_or(0)
    }

    pub fn out_rate(&self, p: ProfileId) -> f64 {
        self.sample_rate / self.profiles[p].decim as f64
    }

    /// Wall-clock duration of one block of input.
    pub fn block_duration(&self) -> Duration {
        Duration::from_secs_f64(self.hop() as f64 / self.sample_rate)
    }

    /// Whether profile `p`'s output rate is exactly its consumer's
    /// ([`ProfileSpec::NARROW`] 24 kS/s, [`ProfileSpec::WIDE`] 96 kS/s), so
    /// its slots have nothing to resample. False if the config has no such
    /// profile.
    #[must_use]
    pub fn is_exact(&self, p: ProfileId) -> bool {
        let target = [ProfileSpec::NARROW, ProfileSpec::WIDE]
            .get(p)
            .map(|s| s.target_rate);
        self.profiles.get(p).is_some() && target.is_some_and(|t| (self.out_rate(p) - t).abs() < 0.5)
    }

    /// Whether the forward FFT, the one every block pays, is made only of
    /// radices rustfft has a butterfly for. A larger prime factor of `n` (a
    /// decimation that is a large prime, say) goes through Rader's algorithm,
    /// which at 103 cost 2.8x a smooth size of the same length. Every
    /// profile's inverse size divides `n`, so `n` alone decides.
    #[must_use]
    pub const fn is_fast(&self) -> bool {
        largest_prime_factor(self.n) <= MAX_BUTTERFLY_PRIME
    }
}

/// The largest prime rustfft has a hard-coded butterfly for.
const MAX_BUTTERFLY_PRIME: usize = 31;

const fn largest_prime_factor(mut n: usize) -> usize {
    let (mut p, mut largest) = (2, 1);
    while n > 1 {
        if p * p > n {
            return n;
        }
        while n.is_multiple_of(p) {
            n /= p;
            largest = p;
        }
        p += 1;
    }
    largest
}

/// Decimations giving `target_rate..=max_rate`, highest decimation first.
fn candidate_decims(sample_rate: f64, s: &ProfileSpec) -> Vec<usize> {
    let hi = (sample_rate / s.target_rate).floor() as usize;
    let lo = ((sample_rate / s.max_rate).ceil() as usize).clamp(2, hi.max(2));
    if hi < 2 {
        return Vec::new();
    }
    (lo..=hi).rev().collect()
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn lcm(a: usize, b: usize) -> usize {
    a / gcd(a, b) * b
}

/// Relative cost of a complex FFT of size `n`: prime stages cost roughly
/// `n * log2(p)` for the small radices and grow with `p` beyond that (Rader /
/// Bluestein). Only used to rank geometries, never as a prediction.
fn fft_cost(n: usize) -> f64 {
    let stage = |p: usize| {
        let l = (p as f64).log2();
        if p <= 5 { l } else { l * p as f64 / 3.0 }
    };
    let (mut rest, mut p, mut per_point) = (n, 2usize, 0.0);
    while rest > 1 {
        if p * p > rest {
            per_point += stage(rest);
            break;
        }
        while rest.is_multiple_of(p) {
            per_point += stage(p);
            rest /= p;
        }
        p += 1;
    }
    n as f64 * per_point
}

/// Forward FFT size and overlap for these decimations, with a cost per input
/// sample (lower is better). `None` if no size satisfies every profile's
/// overlap-save constraint.
fn plan(sample_rate: f64, profiles: &[Profile]) -> Option<(f64, usize, usize)> {
    let l = profiles.iter().fold(1, |acc, p| lcm(acc, p.decim));
    let d0 = profiles[0].decim;
    // Smallest multiple of `l` giving profile 0 at least 256 points.
    let m_target = (256 * d0).div_ceil(l).max(1);
    let mut best: Option<(f64, usize, usize)> = None;
    for m in m_target..=m_target + m_target / 2 + 1 {
        let n = l * m;
        // 25% overlap, or whatever the longest filter needs.
        let mut mo = m.div_ceil(4).max(1);
        for p in profiles {
            let rate = sample_rate / p.decim as f64;
            let taps = tap_count(rate, p.transition_hz, p.atten_db);
            mo = mo.max(((taps - 1) * p.decim).div_ceil(l));
        }
        if 2 * mo > m {
            continue;
        }
        let overlap_in = l * mo;
        let hop = (n - overlap_in) as f64;
        let cost =
            (fft_cost(n) + profiles.iter().map(|p| fft_cost(n / p.decim)).sum::<f64>()) / hop;
        if best.is_none_or(|(b, ..)| cost < b) {
            best = Some((cost, n, overlap_in));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p25_geometry_is_the_validated_one() {
        for fs in [2_496_000.0, 1_872_000.0, 912_000.0] {
            let c = Config::p25(fs).unwrap();
            assert_eq!(c.k(NARROW), 256, "fs={fs}");
            assert_eq!(c.overlap_out(NARROW), 64, "fs={fs}");
            assert_eq!(c.profile(NARROW).decim, (fs / 24_000.0).floor() as usize);
        }
    }

    #[test]
    fn standard_keeps_the_narrow_geometry_and_adds_a_wide_profile() {
        for fs in [
            2_496_000.0,
            1_872_000.0,
            912_000.0,
            2_400_000.0,
            2_048_000.0,
        ] {
            let c = Config::standard(fs).unwrap();
            let p25 = Config::p25(fs).unwrap();
            assert_eq!(c.profiles.len(), 2, "fs={fs}");
            assert_eq!(c.profile(NARROW).decim, p25.profile(NARROW).decim);
            assert!(c.k(NARROW) >= 256, "fs={fs} k={}", c.k(NARROW));
            assert!(c.out_rate(WIDE) >= 96_000.0 && c.out_rate(WIDE) <= 288_000.0);
            for p in [NARROW, WIDE] {
                let pr = c.profile(p);
                assert_eq!(c.n % pr.decim, 0);
                assert_eq!(c.overlap_in % pr.decim, 0);
                assert_eq!(c.hop() % pr.decim, 0);
                let taps = tap_count(c.out_rate(p), pr.transition_hz, pr.atten_db);
                assert!(taps <= c.overlap_out(p) + 1, "fs={fs} profile {p}");
            }
            eprintln!(
                "fs={fs}: n={} hop={} narrow k={} wide k={} @ {:.0} S/s",
                c.n,
                c.hop(),
                c.k(NARROW),
                c.k(WIDE),
                c.out_rate(WIDE)
            );
        }
    }

    #[test]
    fn exact_multiples_pick_the_exact_96k_rate() {
        // 2.496 MS/s = 26 x 96 kS/s = 104 x 24 kS/s: both exact, same FFT as P25-only.
        let c = Config::standard(2_496_000.0).unwrap();
        let p25 = Config::p25(2_496_000.0).unwrap();
        assert_eq!(c.profile(WIDE).decim, 26);
        assert_eq!((c.n, c.overlap_in), (p25.n, p25.overlap_in));
    }

    #[test]
    fn a_prime_decimation_is_a_slow_forward_fft() {
        // 2.472 MS/s = 103 x 24 kS/s: the forward FFT needs a 103-point stage.
        assert!(!Config::standard(2_472_000.0).unwrap().is_fast());
        for fs in [2_496_000.0, 2_400_000.0, 1_872_000.0, 912_000.0] {
            assert!(Config::standard(fs).unwrap().is_fast(), "fs={fs}");
        }
    }

    #[test]
    fn a_rate_is_exact_for_a_profile_when_it_is_a_multiple_of_the_target() {
        let c = Config::standard(960_000.0).unwrap();
        assert!(c.is_exact(NARROW) && c.is_exact(WIDE));
        // 912 kS/s = 9.5 x 96 kHz: the wide profile runs at 101.3 kS/s.
        let c = Config::standard(912_000.0).unwrap();
        assert!(c.is_exact(NARROW) && !c.is_exact(WIDE));
        // No wide profile at all.
        assert!(!Config::standard(150_000.0).unwrap().is_exact(WIDE));
    }

    #[test]
    fn largest_prime_factors() {
        assert_eq!(largest_prime_factor(1), 1);
        assert_eq!(largest_prime_factor(2), 2);
        assert_eq!(largest_prime_factor(26_624), 13);
        assert_eq!(largest_prime_factor(30_900), 103);
        assert_eq!(largest_prime_factor(10_944), 19);
        assert_eq!(largest_prime_factor(97), 97);
    }

    #[test]
    fn low_rates_drop_the_wide_profile() {
        let c = Config::standard(150_000.0).unwrap();
        assert_eq!(c.profiles.len(), 1);
    }
}
