//! Unsigned 8-bit IQ, as the RTL2832U delivers it, to complex floats.

use num_complex::Complex32;

/// gr-osmosdr's lookup: `(u - 127.4) / 128`, computed in single precision.
/// The 127.4 is the ADC's measured mid-point; the result spans about
/// -0.995..=0.997.
pub struct Lut([f32; 256]);

impl Default for Lut {
    fn default() -> Self {
        Self::new()
    }
}

impl Lut {
    #[allow(clippy::cast_precision_loss)]
    pub fn new() -> Self {
        let mut t = [0.0f32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            *v = (i as f32 - 127.4f32) / 128.0f32;
        }
        Self(t)
    }

    /// Convert interleaved `[i, q, i, q, ..]` bytes, appending to `out`. An
    /// odd trailing byte is ignored.
    pub fn convert(&self, bytes: &[u8], out: &mut Vec<Complex32>) {
        out.extend(
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&[i, q]| Complex32::new(self.0[usize::from(i)], self.0[usize::from(q)])),
        );
    }
}

/// Samples in interleaved `[i, q, ..]` bytes with either component at the
/// ADC's rail (0 or 255): the converter overloaded.
pub fn clipped(bytes: &[u8]) -> u64 {
    let rail = |b: u8| b == u8::MIN || b == u8::MAX;
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .filter(|&&[i, q]| rail(i) || rail(q))
        .count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_osmosdr_scale() {
        let lut = Lut::new();
        let mut out = Vec::new();
        lut.convert(&[0, 255, 127, 128, 1], &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].re, (0.0f32 - 127.4f32) / 128.0f32);
        assert_eq!(out[0].im, (255.0f32 - 127.4f32) / 128.0f32);
        assert!(out[1].re < 0.0 && out[1].im > 0.0);
        assert!((out[1].re + out[1].im).abs() < 0.01);
    }

    #[test]
    fn a_sample_clips_when_either_component_is_on_a_rail() {
        assert_eq!(clipped(&[0, 128, 128, 255, 1, 254, 128, 128, 255]), 2);
    }
}
