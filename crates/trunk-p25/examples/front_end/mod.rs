//! What a scorer feeds the receiver from a tap: the conditioned IQ the live
//! receiver gets (`.out.cf32`, the default) or, in older baselines, the
//! conditioner's input (`iq=in`), optionally low-passed (`pre=<cutoff>:
//! <transition>`) and then levelled by an RMS AGC like the conditioner's
//! (`agc=<alpha>`), to try filtering ahead of the AGC.

use std::fs;

use num_complex::Complex32;
use trunk_p25::demod::lowpass_taps;

#[derive(Default)]
pub struct FrontEnd {
    input: bool,
    pre: Option<(f64, f64)>,
    agc: Option<f64>,
}

/// The conditioner's `rms_agc` output level.
const AGC_REFERENCE: f32 = 0.85;

impl FrontEnd {
    /// Take `arg` if it is one of ours.
    pub fn parse(&mut self, arg: &str) -> bool {
        if let Some(v) = arg.strip_prefix("iq=") {
            self.input = v == "in";
        } else if let Some(v) = arg.strip_prefix("pre=") {
            let (c, t) = v.split_once(':').expect("pre=<cutoff>:<transition>");
            self.pre = Some((c.parse().unwrap(), t.parse().unwrap()));
        } else if let Some(v) = arg.strip_prefix("agc=") {
            self.agc = Some(v.parse().unwrap());
        } else {
            return false;
        }
        true
    }

    /// The samples of tap `base` at `rate`.
    pub fn load(&self, base: &str, rate: f64) -> Vec<Complex32> {
        let ext = if self.input { "in" } else { "out" };
        let raw = fs::read(format!("{base}.{ext}.cf32")).unwrap();
        let mut iq: Vec<Complex32> = raw
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| {
                Complex32::new(
                    f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                    f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                )
            })
            .collect();
        if let Some((cutoff, transition)) = self.pre {
            let taps = lowpass_taps(rate, cutoff, transition);
            let half = taps.len() / 2;
            iq = (0..iq.len())
                .map(|n| {
                    taps.iter()
                        .enumerate()
                        .filter_map(|(k, &t)| {
                            (n + half)
                                .checked_sub(k)
                                .and_then(|i| iq.get(i))
                                .map(|&x| x * t)
                        })
                        .sum()
                })
                .collect();
        }
        if let Some(a) = self.agc {
            let mut avg = 0.0f64;
            for s in &mut iq {
                avg = (1.0 - a).mul_add(avg, a * f64::from(s.norm_sqr()));
                *s /= (avg.sqrt() as f32) / AGC_REFERENCE + 1e-18;
            }
        }
        iq
    }
}
