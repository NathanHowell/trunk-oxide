//! Safe FFT backend (rustfft). Plans are shared across threads; each caller
//! owns its scratch space.

use std::sync::Arc;

use num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

/// In-place 1-D complex FFT of fixed size. Unnormalised.
pub struct Plan {
    fft: Arc<dyn Fft<f32>>,
}

impl Plan {
    pub fn new(n: usize, dir: Direction) -> Self {
        let mut planner = FftPlanner::<f32>::new();
        let fft = match dir {
            Direction::Forward => planner.plan_fft_forward(n),
            Direction::Backward => planner.plan_fft_inverse(n),
        };
        Self { fft }
    }

    pub fn scratch(&self) -> Vec<Complex32> {
        vec![Complex32::ZERO; self.fft.get_inplace_scratch_len()]
    }

    pub fn execute(&self, buf: &mut [Complex32], scratch: &mut [Complex32]) {
        self.fft.process_with_scratch(buf, scratch);
    }
}
