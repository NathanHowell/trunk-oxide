use std::f64::consts::TAU;
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};
use num_complex::Complex32;
use rayon::prelude::*;

use crate::design::lowpass_taps;
use crate::fft::{Direction, Plan};

/// Channelizer geometry. Everything else is derived.
///
/// Forward FFT size `N = decim * k`; each frame holds `hop` new samples plus
/// `overlap = decim * overlap_out` saved ones. A channel's inverse FFT has
/// `k` points at `rate / decim`, of which the first `overlap_out` outputs are
/// time-aliased and discarded, so each block yields `k - overlap_out` samples
/// per channel. The channel filter's impulse response must fit in
/// `overlap_out + 1` taps (the overlap-save constraint).
#[derive(Clone, Debug)]
pub struct Config {
    /// Input sample rate (Hz).
    pub sample_rate: f64,
    /// Decimation: output rate is `sample_rate / decim`.
    pub decim: usize,
    /// Per-channel inverse FFT size (output-rate samples per frame).
    pub k: usize,
    /// Output samples discarded per frame; sets the max filter length.
    pub overlap_out: usize,
    /// Channel filter 6 dB point (Hz, one-sided): `channel_bw / 2`.
    pub cutoff_hz: f64,
    /// Channel filter transition width (Hz).
    pub transition_hz: f64,
    /// Channel filter stop-band attenuation (dB).
    pub atten_db: f64,
    /// Worker threads for the per-channel stage (0 = run on the caller).
    pub threads: usize,
}

impl Config {
    /// P25/DMR defaults: 12.5 kHz channel, same filter spec as
    /// `xlat_channelizer` (`low_pass_2(bw/2, bw/4, 60 dB)`), output rate as
    /// close to 24 kS/s as an integer decimation allows (exactly 24 kS/s when
    /// `sample_rate` is a multiple of 24 kHz; otherwise slightly above, and
    /// the host resamples the remainder).
    pub fn p25(sample_rate: f64) -> Result<Self> {
        let decim = (sample_rate / 24_000.0).floor() as usize;
        ensure!(decim >= 2, "sample rate {sample_rate} too low for a 24 kS/s channel");
        Ok(Self { sample_rate, decim, k: 256, overlap_out: 64, cutoff_hz: 6_250.0, transition_hz: 3_125.0, atten_db: 60.0, threads: 0 })
    }

    pub const fn fft_size(&self) -> usize {
        self.decim * self.k
    }

    /// New input samples consumed per block.
    pub const fn hop(&self) -> usize {
        self.decim * (self.k - self.overlap_out)
    }

    /// Output samples produced per channel per block.
    pub const fn out_per_block(&self) -> usize {
        self.k - self.overlap_out
    }

    pub fn out_rate(&self) -> f64 {
        self.sample_rate / self.decim as f64
    }

    /// Wall-clock duration of one block of input.
    pub fn block_duration(&self) -> Duration {
        Duration::from_secs_f64(self.hop() as f64 / self.sample_rate)
    }
}

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
    freq_hz: f64,
    /// Centre bin in `0..N`.
    bin0: usize,
    /// `freq_hz - bin0 * df`, applied as a mixer at the output rate.
    resid_hz: f64,
    /// Output-sample index of the next kept sample (for residual phase).
    out_index: u64,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    out: Vec<Complex32>,
}

struct Shared {
    n: usize,
    k: usize,
    overlap_out: usize,
    decim: usize,
    out_rate: f64,
    inv: Plan,
    /// Channel frequency response (K-point) pre-scaled by `1/N`.
    h: Vec<Complex32>,
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
        ensure!(cfg.decim >= 2 && cfg.k >= 8 && cfg.overlap_out < cfg.k, "bad geometry");
        let n = cfg.fft_size();
        let taps = lowpass_taps(cfg.out_rate(), cfg.cutoff_hz, cfg.transition_hz, cfg.atten_db);
        // Overlap-save constraint: circular convolution of length K is exact
        // for outputs m >= L-1, so L-1 <= overlap_out.
        if taps.len() > cfg.overlap_out + 1 {
            bail!("channel filter has {} taps but overlap_out={} allows at most {}", taps.len(), cfg.overlap_out, cfg.overlap_out + 1);
        }

        let fwd = Plan::new(n, Direction::Forward);
        let inv = Plan::new(cfg.k, Direction::Backward);

        // Frequency response of the (causal) taps at K points, scaled by 1/N
        // so the unnormalised fwd/inv pair has unity passband gain.
        let kplan = Plan::new(cfg.k, Direction::Forward);
        let mut hb = vec![Complex32::ZERO; cfg.k];
        for (d, t) in hb.iter_mut().zip(&taps) {
            *d = Complex32::new(*t, 0.0);
        }
        let mut hs = kplan.scratch();
        kplan.execute(&mut hb, &mut hs);
        let scale = 1.0 / n as f32;
        let h = hb.iter().map(|c| c * scale).collect();

        let pool = if cfg.threads > 0 {
            Some(rayon::ThreadPoolBuilder::new().num_threads(cfg.threads).thread_name(|i| format!("chan-{i}")).build()?)
        } else {
            None
        };
        let overlap = cfg.decim * cfg.overlap_out;
        Ok(Self {
            shared: Shared { n, k: cfg.k, overlap_out: cfg.overlap_out, decim: cfg.decim, out_rate: cfg.out_rate(), inv, h },
            fwd_scratch: fwd.scratch(),
            fwd,
            frame: vec![Complex32::ZERO; n],
            history: vec![Complex32::ZERO; overlap],
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

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Frequency resolution of the forward FFT (Hz per bin).
    pub fn bin_hz(&self) -> f64 {
        self.cfg.sample_rate / self.shared.n as f64
    }

    fn place(&self, freq_hz: f64) -> Result<(usize, f64)> {
        let half = self.cfg.sample_rate / 2.0;
        ensure!(freq_hz.abs() + self.cfg.out_rate() / 2.0 <= half, "channel at {freq_hz} Hz (+/-{} Hz) falls outside the +/-{half} Hz passband", self.cfg.out_rate() / 2.0);
        let df = self.bin_hz();
        let b = (freq_hz / df).round();
        let bin0 = (b as i64).rem_euclid(self.shared.n as i64) as usize;
        Ok((bin0, freq_hz - b * df))
    }

    /// Add a channel centred `freq_hz` from the source centre. Takes effect
    /// on the next block; there is no per-channel history, so the first
    /// output is immediately valid.
    pub fn add_channel(&mut self, freq_hz: f64) -> Result<ChannelId> {
        let (bin0, resid_hz) = self.place(freq_hz)?;
        let id = ChannelId(self.next_id);
        self.next_id += 1;
        self.channels.push(Channel {
            id,
            freq_hz,
            bin0,
            resid_hz,
            out_index: 0,
            buf: vec![Complex32::ZERO; self.cfg.k],
            scratch: self.shared.inv.scratch(),
            out: vec![Complex32::ZERO; self.cfg.out_per_block()],
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
        let (bin0, resid_hz) = self.place(freq_hz)?;
        let Some(c) = self.channels.iter_mut().find(|c| c.id == id) else { bail!("unknown channel {id:?}") };
        c.freq_hz = freq_hz;
        c.bin0 = bin0;
        c.resid_hz = resid_hz;
        Ok(())
    }

    /// Baseband output of the last block for one channel.
    pub fn output(&self, id: ChannelId) -> Option<&[Complex32]> {
        self.channels.iter().find(|c| c.id == id).map(|c| c.out.as_slice())
    }

    pub fn outputs(&self) -> impl Iterator<Item = (ChannelId, f64, &[Complex32])> {
        self.channels.iter().map(|c| (c.id, c.freq_hz, c.out.as_slice()))
    }

    /// Consume exactly [`Config::hop`] new input samples and update every
    /// channel's output buffer.
    pub fn process_block(&mut self, new: &[Complex32]) -> BlockStats {
        let hop = self.cfg.hop();
        assert_eq!(new.len(), hop, "process_block needs exactly hop={hop} samples");
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
            Some(pool) => pool.install(|| self.channels.par_iter_mut().for_each(|c| extract(shared, spec, frame_start, c))),
            None => self.channels.iter_mut().for_each(|c| extract(shared, spec, frame_start, c)),
        }
        BlockStats { forward, channels: t1.elapsed() }
    }
}

/// Per-channel work for one block: bin slice * H -> K-point IFFT -> drop the
/// aliased head -> phase-correct (bin shift + residual offset).
fn extract(s: &Shared, spec: &[Complex32], frame_start: u64, c: &mut Channel) {
    let (n, k, half) = (s.n, s.k, s.k / 2);

    // buf[j] = spec[bin0 + jj] * H[j], jj = j (j < K/2) or j - K (j >= K/2).
    let mut src = c.bin0;
    for j in 0..half {
        c.buf[j] = spec[src] * s.h[j];
        src += 1;
        if src == n {
            src = 0;
        }
    }
    let mut src = (c.bin0 + n - half) % n;
    for j in half..k {
        c.buf[j] = spec[src] * s.h[j];
        src += 1;
        if src == n {
            src = 0;
        }
    }
    s.inv.execute(&mut c.buf, &mut c.scratch);

    // Kept sample i (m = overlap_out + i) maps to global output index g and
    // input index D*g. The bin shift contributes exp(-j2pi*bin0*frame_start/N)
    // (frame-local mixer vs global mixer); the residual offset contributes
    // exp(-j2pi*resid*g/out_rate).
    let bin_phase = (c.bin0 as u64 * frame_start) % n as u64;
    let phase0 = -TAU * (bin_phase as f64 / n as f64 + c.resid_hz * c.out_index as f64 / s.out_rate);
    let (sin0, cos0) = phase0.sin_cos();
    let mut rot = Complex32::new(cos0 as f32, sin0 as f32);
    let step = -TAU * c.resid_hz / s.out_rate;
    let inc = Complex32::new(step.cos() as f32, step.sin() as f32);
    for (o, v) in c.out.iter_mut().zip(&c.buf[s.overlap_out..]) {
        *o = v * rot;
        rot *= inc;
    }
    c.out_index += (k - s.overlap_out) as u64;
    let _ = s.decim;
}

#[cfg(test)]
mod tests;
