//! Real FIR filter with integer decimation, evaluated as `fir_filter_fff`
//! does: `y[n] = sum_k taps[k] x[n - k]` at `n = 0, decim, 2 decim, ...`,
//! with zeros before the first sample.
//!
//! The dot product runs in [`LANES`] independent sums, which is what lets it
//! be vectorised and keeps the multiplies from waiting on one another. (One
//! running sum of fused multiply-adds, as `fir_filter_fff` writes it, is a
//! chain: each step waits for the last, and where the build target has no
//! FMA instruction each is a call to libm. For the 641-tap high-pass that
//! was 1.2 ns a tap, a fiftieth of a core per channel.) The sums differ from
//! one running sum in the last bits of an `f32`, well inside what the
//! reference test allows.

/// Independent partial sums in a dot product.
const LANES: usize = 8;

/// `sum(x[k] * t[k])` over equal-length slices, as [`LANES`] partial sums.
fn dot(x: &[f32], t: &[f32]) -> f32 {
    let mut lanes = [0.0f32; LANES];
    let (x_blocks, x_rest) = x.as_chunks::<LANES>();
    let (t_blocks, t_rest) = t.as_chunks::<LANES>();
    for (x, t) in x_blocks.iter().zip(t_blocks) {
        for ((lane, x), t) in lanes.iter_mut().zip(x).zip(t) {
            *lane += x * t;
        }
    }
    let tail: f32 = x_rest.iter().zip(t_rest).map(|(x, t)| x * t).sum();
    lanes.iter().sum::<f32>() + tail
}

/// A causal FIR filter over `f32` samples.
#[derive(Clone, Debug)]
pub struct Fir {
    /// Taps reversed, so the dot product runs forward over the history.
    taps_rev: Vec<f32>,
    decim: usize,
    /// The last `taps.len() - 1` inputs, oldest first, then room for a block.
    history: Vec<f32>,
    /// Inputs since the last output (for decimation across blocks).
    phase: usize,
}

impl Fir {
    /// # Panics
    /// With no taps or a zero decimation.
    #[must_use]
    pub fn new(taps: &[f32], decim: usize) -> Self {
        assert!(!taps.is_empty() && decim > 0);
        let mut taps_rev = taps.to_vec();
        taps_rev.reverse();
        Self {
            history: vec![0.0; taps.len() - 1],
            taps_rev,
            decim,
            phase: 0,
        }
    }

    pub const fn taps_len(&self) -> usize {
        self.taps_rev.len()
    }

    /// Filter `input`, appending the outputs to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let n = self.taps_rev.len();
        let keep = n - 1;
        // history holds the previous keep samples; extend with the block.
        self.history.extend_from_slice(input);
        let buf = &self.history;
        // i indexes the newest sample of each output's window. `phase` inputs
        // have gone by since the last output, so the next is due
        // `decim - phase` inputs into the block (immediately at phase 0).
        let mut i = keep + ((self.decim - self.phase) % self.decim);
        while i < buf.len() {
            out.push(dot(&buf[i + 1 - n..=i], &self.taps_rev));
            i += self.decim;
        }
        self.phase = (self.phase + input.len()) % self.decim;
        let len = self.history.len();
        self.history.drain(..len - keep);
    }

    /// Forget the signal history.
    pub fn reset(&mut self) {
        self.history.iter_mut().for_each(|x| *x = 0.0);
        self.phase = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_tap_passes_input_through() {
        let mut f = Fir::new(&[1.0], 1);
        let mut out = Vec::new();
        f.process(&[1.0, 2.0, 3.0], &mut out);
        assert_eq!(out, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn delay_tap_and_decimation_pick_every_third_delayed_sample() {
        // taps [0, 1]: y[n] = x[n-1]; decim 3: n = 0, 3, 6, ...
        let mut f = Fir::new(&[0.0, 1.0], 3);
        let mut out = Vec::new();
        f.process(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0], &mut out);
        assert_eq!(out, vec![0.0, 3.0, 6.0]);
    }

    #[test]
    fn block_boundaries_do_not_change_the_output() {
        let taps = [0.25, 0.5, 0.25, 0.1];
        let input: Vec<f32> = (0..50).map(|i| (i as f32 * 0.37).sin()).collect();
        let mut whole = Fir::new(&taps, 2);
        let mut a = Vec::new();
        whole.process(&input, &mut a);
        let mut split = Fir::new(&taps, 2);
        let mut b = Vec::new();
        for chunk in input.chunks(7) {
            split.process(chunk, &mut b);
        }
        assert_eq!(a, b);
    }
    #[test]
    fn the_dot_product_is_the_sum_to_within_rounding_at_every_length() {
        let mut state = 7u64;
        let mut rnd = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / (1u64 << 24) as f32 - 0.5
        };
        for len in [1, 2, 7, 8, 9, 15, 16, 17, 65, 641, 1000] {
            let x: Vec<f32> = (0..len).map(|_| rnd()).collect();
            let t: Vec<f32> = (0..len).map(|_| rnd()).collect();
            let exact: f64 = x
                .iter()
                .zip(&t)
                .map(|(a, b)| f64::from(*a) * f64::from(*b))
                .sum();
            let got = f64::from(dot(&x, &t));
            assert!((got - exact).abs() < 1e-4, "len {len}: {got} vs {exact}");
        }
    }
    #[test]
    fn the_voice_high_pass_matches_a_direct_sum_to_far_inside_what_the_reference_allows() {
        // the recording's 300 Hz high-pass, 641 taps, over 4 s of audio-like
        // input fed in the blocks a channel sees, against the plain sum in f64
        let taps =
            crate::design::high_pass(1.0, 16_000.0, 300.0, 50.0, crate::design::Window::Hann);
        assert_eq!(taps.len(), 641);
        let mut state = 99u64;
        let x: Vec<f32> = (0..64_000)
            .map(|n| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let hiss = (state >> 40) as f32 / (1u64 << 24) as f32 - 0.5;
                0.3 * (n as f32 * 0.21).sin() + 0.1 * hiss
            })
            .collect();
        let mut fir = Fir::new(&taps, 1);
        let mut got = Vec::new();
        for block in x.chunks(143) {
            fir.process(block, &mut got);
        }
        let mut worst = 0.0f64;
        for (n, &g) in got.iter().enumerate() {
            let exact: f64 = taps
                .iter()
                .enumerate()
                .filter(|&(k, _)| k <= n)
                .map(|(k, &t)| f64::from(t) * f64::from(x[n - k]))
                .sum();
            worst = worst.max((f64::from(g) - exact).abs());
        }
        // the reference test allows 2e-5 of the band-passed audio
        assert!(worst < 2e-6, "worst error {worst}");
    }
}
