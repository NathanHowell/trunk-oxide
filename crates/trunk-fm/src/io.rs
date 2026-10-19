//! Raw little-endian sample files, as the replay taps and the reference
//! harness write them.

use num_complex::Complex32;

/// Interleaved `f32` I/Q pairs.
#[must_use]
pub fn cf32_from_bytes(bytes: &[u8]) -> Vec<Complex32> {
    let (pairs, _) = bytes.as_chunks::<8>();
    pairs
        .iter()
        .map(|c| {
            Complex32::new(
                f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            )
        })
        .collect()
}

#[must_use]
pub fn f32_from_bytes(bytes: &[u8]) -> Vec<f32> {
    let (words, _) = bytes.as_chunks::<4>();
    words.iter().map(|w| f32::from_le_bytes(*w)).collect()
}

#[must_use]
pub fn f32_to_bytes(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|x| x.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let v = [1.5f32, -2.0, 0.25];
        assert_eq!(f32_from_bytes(&f32_to_bytes(&v)), v);
        let iq = cf32_from_bytes(&f32_to_bytes(&[1.0, 2.0, 3.0, 4.0, 5.0]));
        assert_eq!(iq, vec![Complex32::new(1.0, 2.0), Complex32::new(3.0, 4.0)]);
    }
}
