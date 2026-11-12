use std::f64::consts::TAU;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};
use num_complex::Complex32;
use rayon::prelude::*;

use crate::config::{Config, ProfileId};
use crate::design::lowpass_taps;
use crate::fft::{Direction, Plan};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChannelId(pub u64);

#[derive(Clone, Copy, Debug, Default)]
pub struct BlockStats {
    /// Frame assembly + shared forward FFT.
    pub forward: Duration,
    /// All channel extractions (wall clock, includes parallel speed-up).
    pub channels: Duration,
}

struct Channel {
    id: ChannelId,
    profile: ProfileId,
    freq_hz: f64,
    /// Centre bin in `0..N`.
    bin0: usize,
    /// `freq_hz - bin0 * df`, applied as a mixer at the output rate.
    resid_hz: f64,
    /// Output-sample index of the next kept sample (for residual phase).
    out_index: u64,
    /// This channel's frequency response (K-point, pre-scaled by `1/N`).
    /// Every channel has its own; channels with the same filter share one.
    h: Arc<[Complex32]>,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    out: Vec<Complex32>,
}

/// Per-profile state shared by all channels of that profile.
struct ProfileShared {
    k: usize,
    overlap_out: usize,
    out_rate: f64,
    atten_db: f64,
    inv: Plan,
    /// K-point forward plan, for turning taps into a frequency response.
    kfwd: Plan,
    /// Response of the profile's default filter.
    default_h: Arc<[Complex32]>,
}

struct Shared {
    n: usize,
    profiles: Vec<ProfileShared>,
}

impl ProfileShared {
    /// Frequency response of the causal `taps` at K points, scaled by `1/N`
    /// so the unnormalised fwd/inv pair has unity passband gain.
    fn response(&self, n: usize, taps: &[f32]) -> Arc<[Complex32]> {
        let mut hb = vec![Complex32::ZERO; self.k];
        for (d, t) in hb.iter_mut().zip(taps) {
            *d = Complex32::new(*t, 0.0);
        }
        let mut hs = self.kfwd.scratch();
        self.kfwd.execute(&mut hb, &mut hs);
        let scale = 1.0 / n as f32;
        hb.iter().map(|c| c * scale).collect()
    }

    /// Check a filter fits the overlap-save constraint and return its taps:
    /// circular convolution of length K is exact for outputs `m >= L-1`, so
    /// `L-1 <= overlap_out`.
    fn design(&self, cutoff_hz: f64, transition_hz: f64) -> Result<Vec<f32>> {
        ensure!(
            cutoff_hz > 0.0 && transition_hz > 0.0 && cutoff_hz < self.out_rate / 2.0,
            "filter cutoff {cutoff_hz} Hz / transition {transition_hz} Hz do not fit a {} S/s channel",
            self.out_rate
        );
        let taps = lowpass_taps(self.out_rate, cutoff_hz, transition_hz, self.atten_db);
        if taps.len() > self.overlap_out + 1 {
            bail!(
                "channel filter has {} taps but overlap_out={} allows at most {}",
                taps.len(),
                self.overlap_out,
                self.overlap_out + 1
            );
        }
        Ok(taps)
    }
}

pub struct Channelizer {
    cfg: Config,
    shared: Shared,
    fwd: Plan,
    frame: Vec<Complex32>,
    fwd_scratch: Vec<Complex32>,
    history: Vec<Complex32>,
    /// Index of the next block, for the bin-shift phase.
    block: u64,
    channels: Vec<Channel>,
    next_id: u64,
    pool: Option<rayon::ThreadPool>,
}

impl Channelizer {
    pub fn new(cfg: Config) -> Result<Self> {
        ensure!(
            !cfg.profiles.is_empty() && cfg.overlap_in < cfg.n,
            "bad geometry"
        );
        let n = cfg.fft_size();
        let mut profiles = Vec::with_capacity(cfg.profiles.len());
        for (i, p) in cfg.profiles.iter().enumerate() {
            ensure!(
                p.decim >= 2
                    && n.is_multiple_of(p.decim)
                    && cfg.overlap_in.is_multiple_of(p.decim)
                    && cfg.k(i) >= 8
                    && cfg.overlap_out(i) < cfg.k(i),
                "bad geometry for profile {i}"
            );
            let k = cfg.k(i);
            let mut ps = ProfileShared {
                k,
                overlap_out: cfg.overlap_out(i),
                out_rate: cfg.out_rate(i),
                atten_db: p.atten_db,
                inv: Plan::new(k, Direction::Backward),
                kfwd: Plan::new(k, Direction::Forward),
                default_h: Arc::from(Vec::new()),
            };
            let taps = ps.design(p.cutoff_hz, p.transition_hz)?;
            ps.default_h = ps.response(n, &taps);
            profiles.push(ps);
        }
        let fwd = Plan::new(n, Direction::Forward);
        let pool = if cfg.threads > 0 {
            Some(
                rayon::ThreadPoolBuilder::new()
                    .num_threads(cfg.threads)
                    .thread_name(|i| format!("chan-{i}"))
                    .build()?,
            )
        } else {
            None
        };
        Ok(Self {
            shared: Shared { n, profiles },
            fwd_scratch: fwd.scratch(),
            fwd,
            frame: vec![Complex32::ZERO; n],
            history: vec![Complex32::ZERO; cfg.overlap_in],
            block: 0,
            channels: Vec::new(),
            next_id: 0,
            pool,
            cfg,
        })
    }

    pub const fn config(&self) -> &Config {
        &self.cfg
    }

    pub const fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Frequency resolution of the forward FFT (Hz per bin).
    pub fn bin_hz(&self) -> f64 {
        self.cfg.sample_rate / self.shared.n as f64
    }

    fn place(&self, profile: ProfileId, freq_hz: f64) -> Result<(usize, f64)> {
        ensure!(
            profile < self.cfg.profiles.len(),
            "unknown profile {profile}"
        );
        let p = self.cfg.profile(profile);
        // The channel's slice wraps past Nyquist, so what matters is that the
        // filter's stop band starts before the edge.
        let half = self.cfg.sample_rate / 2.0;
        let reach = p.cutoff_hz + p.transition_hz / 2.0;
        ensure!(
            freq_hz.abs() + reach <= half,
            "channel at {freq_hz} Hz (+/-{reach} Hz) falls outside the +/-{half} Hz passband"
        );
        let df = self.bin_hz();
        let b = (freq_hz / df).round();
        let bin0 = (b as i64).rem_euclid(self.shared.n as i64) as usize;
        Ok((bin0, b.mul_add(-df, freq_hz)))
    }

    /// Add a channel of `profile` centred `freq_hz` from the source centre,
    /// with the profile's default filter. Takes effect on the next block;
    /// there is no per-channel history, so the first output is immediately
    /// valid.
    pub fn add_channel(&mut self, profile: ProfileId, freq_hz: f64) -> Result<ChannelId> {
        let (bin0, resid_hz) = self.place(profile, freq_hz)?;
        let ps = &self.shared.profiles[profile];
        let id = ChannelId(self.next_id);
        self.next_id += 1;
        self.channels.push(Channel {
            id,
            profile,
            freq_hz,
            bin0,
            resid_hz,
            out_index: 0,
            h: Arc::clone(&ps.default_h),
            buf: vec![Complex32::ZERO; ps.k],
            scratch: ps.inv.scratch(),
            out: vec![Complex32::ZERO; self.cfg.out_per_block(profile)],
        });
        Ok(id)
    }

    pub fn remove_channel(&mut self, id: ChannelId) -> bool {
        let before = self.channels.len();
        self.channels.retain(|c| c.id != id);
        self.channels.len() != before
    }

    /// Retune in place. Costs nothing and cannot glitch other channels.
    pub fn retune(&mut self, id: ChannelId, freq_hz: f64) -> Result<()> {
        let Some(profile) = self.channels.iter().find(|c| c.id == id).map(|c| c.profile) else {
            bail!("unknown channel {id:?}")
        };
        let (bin0, resid_hz) = self.place(profile, freq_hz)?;
        let c = self
            .channels
            .iter_mut()
            .find(|c| c.id == id)
            .expect("checked above");
        c.freq_hz = freq_hz;
        c.bin0 = bin0;
        c.resid_hz = resid_hz;
        Ok(())
    }

    /// Give a channel its own filter (cutoff and transition, Hz). Applies to
    /// the next block. The channel's other state is untouched.
    pub fn set_filter(&mut self, id: ChannelId, cutoff_hz: f64, transition_hz: f64) -> Result<()> {
        let Some(c) = self.channels.iter_mut().find(|c| c.id == id) else {
            bail!("unknown channel {id:?}")
        };
        let ps = &self.shared.profiles[c.profile];
        let taps = ps.design(cutoff_hz, transition_hz)?;
        c.h = ps.response(self.shared.n, &taps);
        Ok(())
    }

    pub fn profile_of(&self, id: ChannelId) -> Option<ProfileId> {
        self.channels.iter().find(|c| c.id == id).map(|c| c.profile)
    }

    /// Baseband output of the last block for one channel.
    pub fn output(&self, id: ChannelId) -> Option<&[Complex32]> {
        self.channels
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.out.as_slice())
    }

    pub fn outputs(&self) -> impl Iterator<Item = (ChannelId, f64, &[Complex32])> {
        self.channels
            .iter()
            .map(|c| (c.id, c.freq_hz, c.out.as_slice()))
    }

    /// Consume exactly [`Config::hop`] new input samples and update every
    /// channel's output buffer.
    pub fn process_block(&mut self, new: &[Complex32]) -> BlockStats {
        let hop = self.cfg.hop();
        assert_eq!(
            new.len(),
            hop,
            "process_block needs exactly hop={hop} samples"
        );
        let overlap = self.history.len();

        let t0 = Instant::now();
        self.frame[..overlap].copy_from_slice(&self.history);
        self.frame[overlap..].copy_from_slice(new);
        self.history.copy_from_slice(&self.frame[hop..]);
        self.fwd.execute(&mut self.frame, &mut self.fwd_scratch);
        let forward = t0.elapsed();

        // Frame start in global input samples is block*hop - overlap.
        let n = self.shared.n as u64;
        let frame_start = (self.block * hop as u64 + n - (overlap as u64 % n)) % n;
        self.block += 1;

        let t1 = Instant::now();
        let spec: &[Complex32] = &self.frame;
        let shared = &self.shared;
        match &self.pool {
            Some(pool) => pool.install(|| {
                self.channels
                    .par_iter_mut()
                    .for_each(|c| extract(shared, spec, frame_start, c));
            }),
            None => self
                .channels
                .iter_mut()
                .for_each(|c| extract(shared, spec, frame_start, c)),
        }
        BlockStats {
            forward,
            channels: t1.elapsed(),
        }
    }
}

/// Per-channel work for one block: bin slice * H -> K-point IFFT -> drop the
/// aliased head -> phase-correct (bin shift + residual offset).
fn extract(s: &Shared, spec: &[Complex32], frame_start: u64, c: &mut Channel) {
    let ps = &s.profiles[c.profile];
    let (n, k, half) = (s.n, ps.k, ps.k / 2);

    // buf[j] = spec[bin0 + jj] * H[j], jj = j (j < K/2) or j - K (j >= K/2).
    let mut src = c.bin0;
    for j in 0..half {
        c.buf[j] = spec[src] * c.h[j];
        src += 1;
        if src == n {
            src = 0;
        }
    }
    let mut src = (c.bin0 + n - half) % n;
    for j in half..k {
        c.buf[j] = spec[src] * c.h[j];
        src += 1;
        if src == n {
            src = 0;
        }
    }
    ps.inv.execute(&mut c.buf, &mut c.scratch);

    // Kept sample i (m = overlap_out + i) maps to global output index g and
    // input index D*g. The bin shift contributes exp(-j2pi*bin0*frame_start/N)
    // (frame-local mixer vs global mixer); the residual offset contributes
    // exp(-j2pi*resid*g/out_rate).
    let bin_phase = (c.bin0 as u64 * frame_start) % n as u64;
    let phase0 =
        -TAU * (bin_phase as f64 / n as f64 + c.resid_hz * c.out_index as f64 / ps.out_rate);
    let (sin0, cos0) = phase0.sin_cos();
    let mut rot = Complex32::new(cos0 as f32, sin0 as f32);
    let step = -TAU * c.resid_hz / ps.out_rate;
    let inc = Complex32::new(step.cos() as f32, step.sin() as f32);
    for (o, v) in c.out.iter_mut().zip(&c.buf[ps.overlap_out..]) {
        *o = v * rot;
        rot *= inc;
    }
    c.out_index += (k - ps.overlap_out) as u64;
}

#[cfg(test)]
mod tests;
