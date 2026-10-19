//! What each 100 ms of demodulated audio says about the channel: how much
//! the noise above the voice band has quieted (a carrier is there), and
//! which CTCSS tone, if any, dominates the sub-audible band.
//!
//! Both are read off the decimated audio before the band-pass
//! ([`Audio::raw`](crate::Audio::raw)). The conditioner's AGC normalises the
//! slot's level, so with no carrier the discriminator's noise has one fixed
//! spectral density whatever the dongle, and the energy in a voice-free band
//! above speech (4–6 kHz) is a known constant; a carrier captures the
//! discriminator and that energy drops by the signal-to-noise ratio. That
//! "quieting" is the carrier detector. It is why FM receivers squelch on
//! noise rather than on signal strength: it measures what the audio will
//! sound like.
//!
//! Open-squelch FM noise also has far more energy below 300 Hz than any PL
//! tone (26 dB more than speech, on the capture that sized this), so a
//! tone bank on its own is fooled a tenth of the time. The tone verdict
//! here is only meaningful alongside the quieting.

use crate::AUDIO_RATE;
use crate::design::{Window as Win, band_pass};
use crate::fir::Fir;

/// Standard EIA CTCSS tones, Hz, lowest first.
pub const CTCSS_HZ: [f32; 38] = [
    67.0, 71.9, 74.4, 77.0, 79.7, 82.5, 85.4, 88.5, 91.5, 94.8, 97.4, 100.0, 103.5, 107.2, 110.9,
    114.8, 118.8, 123.0, 127.3, 131.8, 136.5, 141.3, 146.2, 151.4, 156.7, 162.2, 167.9, 173.8,
    179.9, 186.2, 192.8, 203.5, 210.7, 218.1, 225.7, 233.6, 241.8, 250.3,
];

/// Windows are reported every hop; the tone correlators look back this far.
pub const HOP: usize = 1_600;
/// Samples the tone correlators integrate: 200 ms, 5 Hz resolution, which
/// is what separates tones 2.5–8 Hz apart at the bottom of the table.
pub const TONE_WINDOW: usize = 3_200;

/// Energy per sample of the 4–6 kHz band with no carrier, after the
/// conditioner's AGC: what `quieting_db` is measured against.
///
/// Measured on the pre-conditioning taps of both conventional channels of
/// the 2026-10-17 capture (`examples/stats.rs --calibrate`, with
/// [`Decimation::AntiAliased`](crate::Decimation::AntiAliased)): 1.3834e-2
/// and 1.3858e-2, 0.01 dB apart.
pub const NOISE_REFERENCE: f64 = 1.385e-2;

/// A CTCSS tone, by its index into [`CTCSS_HZ`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Tone(pub u8);

impl Tone {
    /// The table entry nearest `hz`, if within 0.5 Hz of one.
    #[must_use]
    pub fn from_hz(hz: f32) -> Option<Self> {
        CTCSS_HZ
            .iter()
            .position(|t| (t - hz).abs() < 0.5)
            .map(|i| Self(i as u8))
    }

    #[must_use]
    pub fn hz(self) -> f32 {
        CTCSS_HZ[usize::from(self.0)]
    }
}

impl std::fmt::Display for Tone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.1} Hz", self.hz())
    }
}

/// One hop's measurements.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measurement {
    /// How far the above-voice noise is below its no-carrier level, dB.
    /// Around 0 with no carrier, 15–25 on a solid signal.
    pub quieting_db: f64,
    /// The strongest tone in the bank and its amplitude (1.0 = full
    /// deviation), whether or not it is believable: see `tone_prominence`.
    pub tone: Tone,
    pub tone_amplitude: f32,
    /// The strongest tone's amplitude over the bank's median: a real PL
    /// tone stands 5–10× above the rest; noise gives 2–3×.
    pub tone_prominence: f32,
    /// RMS of the hop's audio: 0 when the slot was muted upstream.
    pub rms: f32,
}

impl Measurement {
    /// True when the input was all zeros (a squelch ahead of us is closed).
    #[must_use]
    pub fn silent(&self) -> bool {
        self.rms == 0.0
    }
}

/// Goertzel correlator for one frequency over a fixed block.
#[derive(Clone, Copy, Debug)]
struct Goertzel {
    coeff: f32,
    cos: f32,
    sin: f32,
}

impl Goertzel {
    fn new(hz: f32, rate: f32) -> Self {
        let w = 2.0 * std::f32::consts::PI * hz / rate;
        Self {
            coeff: 2.0 * w.cos(),
            cos: w.cos(),
            sin: w.sin(),
        }
    }

    /// Amplitude of a sinusoid at this frequency in `block` (1.0 for a full
    /// scale sine), rectangular window.
    fn amplitude(&self, block: &[f32]) -> f32 {
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &x in block {
            let s0 = x + self.coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let re = s1 - s2 * self.cos;
        let im = s2 * self.sin;
        2.0 * re.hypot(im) / block.len() as f32
    }
}

/// The per-hop measurer: feed it [`Audio::raw`](crate::Audio::raw) and take
/// a [`Measurement`] per [`HOP`] samples.
#[derive(Clone, Debug)]
pub struct Detector {
    bank: Vec<Goertzel>,
    /// The last `TONE_WINDOW` raw samples, oldest first.
    history: Vec<f32>,
    pending: usize,
    noise_band: Fir,
    noise_out: Vec<f32>,
    noise_reference: f64,
}

impl Detector {
    #[must_use]
    pub fn new() -> Self {
        Self::with_reference(NOISE_REFERENCE)
    }

    /// With another no-carrier noise level (for calibration runs).
    #[must_use]
    pub fn with_reference(noise_reference: f64) -> Self {
        Self {
            bank: CTCSS_HZ
                .iter()
                .map(|&hz| Goertzel::new(hz, AUDIO_RATE as f32))
                .collect(),
            history: Vec::with_capacity(TONE_WINDOW + HOP),
            pending: 0,
            noise_band: Fir::new(
                &band_pass(1.0, AUDIO_RATE, 4_000.0, 6_000.0, 500.0, Win::Hann),
                1,
            ),
            noise_out: Vec::with_capacity(HOP),
            noise_reference,
        }
    }

    /// Feed raw audio; `out` gets one measurement per completed hop.
    pub fn process(&mut self, raw: &[f32], out: &mut Vec<Measurement>) {
        self.history.extend_from_slice(raw);
        self.pending += raw.len();
        while self.pending >= HOP {
            let end = self.history.len() - (self.pending - HOP);
            out.push(Self::measure(
                &self.bank,
                &self.history,
                end,
                &mut self.noise_band,
                &mut self.noise_out,
                self.noise_reference,
            ));
            self.pending -= HOP;
        }
        // Keep what the tone window can still reach.
        let keep = TONE_WINDOW + self.pending;
        if self.history.len() > keep + TONE_WINDOW {
            let drop = self.history.len() - keep;
            self.history.drain(..drop);
        }
    }

    fn measure(
        bank: &[Goertzel],
        history: &[f32],
        end: usize,
        noise_band: &mut Fir,
        noise_out: &mut Vec<f32>,
        noise_reference: f64,
    ) -> Measurement {
        let hop = &history[end - HOP..end];
        let rms = (hop.iter().map(|x| x * x).sum::<f32>() / hop.len() as f32).sqrt();

        noise_out.clear();
        noise_band.process(hop, noise_out);
        let noise = noise_out
            .iter()
            .map(|x| f64::from(*x) * f64::from(*x))
            .sum::<f64>()
            / hop.len() as f64;
        let quieting_db = if noise > 0.0 {
            10.0 * (noise_reference / noise).log10()
        } else {
            0.0
        };

        let start = end.saturating_sub(TONE_WINDOW);
        let block = &history[start..end];
        let mut amps: Vec<f32> = bank.iter().map(|g| g.amplitude(block)).collect();
        let (best, &tone_amplitude) = amps
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .expect("bank is not empty");
        let mid = amps.len() / 2;
        let median = *amps.select_nth_unstable_by(mid, f32::total_cmp).1;
        let tone_prominence = if median > 0.0 {
            tone_amplitude / median
        } else {
            0.0
        };
        Measurement {
            quieting_db,
            tone: Tone(best as u8),
            tone_amplitude,
            tone_prominence,
            rms,
        }
    }

    /// Forget the audio seen so far.
    pub fn reset(&mut self) {
        self.history.clear();
        self.pending = 0;
        self.noise_band.reset();
    }
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn tone(hz: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (TAU * hz * i as f32 / AUDIO_RATE as f32).sin())
            .collect()
    }

    #[test]
    fn goertzel_reads_a_sines_amplitude() {
        let g = Goertzel::new(162.2, AUDIO_RATE as f32);
        let a = g.amplitude(&tone(162.2, 0.1, TONE_WINDOW));
        assert!((a - 0.1).abs() < 0.002, "{a}");
        let off = g.amplitude(&tone(173.8, 0.1, TONE_WINDOW));
        // 11.6 Hz away: the rectangular window's first sidelobe.
        assert!(off < 0.02, "neighbour leaks {off}");
    }

    #[test]
    fn finds_the_tone_and_its_prominence() {
        let mut d = Detector::new();
        let mut x = tone(162.2, 0.1, TONE_WINDOW);
        for (i, s) in x.iter_mut().enumerate() {
            *s += 0.3 * (TAU * 1_000.0 * i as f32 / AUDIO_RATE as f32).sin();
        }
        let mut out = Vec::new();
        d.process(&x, &mut out);
        assert_eq!(out.len(), 2);
        let m = out[1];
        assert_eq!(m.tone, Tone::from_hz(162.2).unwrap());
        assert!((m.tone_amplitude - 0.1).abs() < 0.005);
        assert!(m.tone_prominence > 10.0, "{}", m.tone_prominence);
        assert!(!m.silent());
    }

    #[test]
    fn noise_at_the_reference_level_reads_zero_quieting_and_a_tone_quiets() {
        // White noise scaled so the 4–6 kHz band carries the reference.
        let mut s = 12345u64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s >> 11) as f32 / (1u64 << 53) as f32 * 2.0 - 1.0
        };
        let noise: Vec<f32> = (0..HOP * 20).map(|_| next()).collect();
        let mut d = Detector::with_reference(1.0);
        let mut out = Vec::new();
        d.process(&noise, &mut out);
        // Uniform ±1 noise has power 1/3 per sample; the 4–6 kHz band (a
        // quarter of 0–8 kHz) holds about a quarter of it.
        let q: f64 = out[5..].iter().map(|m| m.quieting_db).sum::<f64>() / (out.len() - 5) as f64;
        let expected = 10.0 * (1.0f64 / (1.0 / 3.0 / 4.0)).log10();
        assert!((q - expected).abs() < 1.0, "quieting {q} vs {expected}");

        let mut d = Detector::with_reference(1.0 / 12.0);
        let mut out = Vec::new();
        d.process(&tone(1_000.0, 0.3, HOP * 4), &mut out);
        assert!(out[3].quieting_db > 30.0, "{}", out[3].quieting_db);
    }

    #[test]
    fn zeros_are_silent() {
        let mut d = Detector::new();
        let mut out = Vec::new();
        d.process(&vec![0.0; HOP], &mut out);
        assert!(out[0].silent());
        assert_eq!(out[0].quieting_db, 0.0);
    }

    #[test]
    fn block_boundaries_do_not_change_measurements() {
        let x = tone(123.0, 0.05, HOP * 6);
        let mut a = Vec::new();
        Detector::new().process(&x, &mut a);
        let mut b = Vec::new();
        let mut d = Detector::new();
        for c in x.chunks(777) {
            d.process(c, &mut b);
        }
        assert_eq!(a, b);
    }
}
