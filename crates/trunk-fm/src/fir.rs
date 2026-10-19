//! Real FIR filter with integer decimation, evaluated as `fir_filter_fff`
//! does: `y[n] = sum_k taps[k] x[n - k]` at `n = 0, decim, 2 decim, ...`,
//! with zeros before the first sample.

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
            let window = &buf[i + 1 - n..=i];
            let mut acc = 0.0f32;
            for (x, t) in window.iter().zip(&self.taps_rev) {
                acc = t.mul_add(*x, acc);
            }
            out.push(acc);
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
}
