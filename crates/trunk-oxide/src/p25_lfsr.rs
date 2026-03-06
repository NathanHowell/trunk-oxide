/// P25 Phase 2 LFSR XOR mask computation.
///
/// Ported from trunk-recorder's `lib/lfsr/lfsr.cxx` (originally translated from
/// OP25's `lfsr.py`). Computes the 2160-dibit XOR mask used for TDMA scrambling
/// from the system's NAC, System ID, and WACN.
const MSIZE: usize = 44;
const SSIZE: usize = 4320;

/// Compute the P25 Phase 2 XOR mask from system identity parameters.
///
/// Returns a `Vec<u8>` of 2160 dibits (values 0–3).
/// Returns an empty vec if all inputs are zero.
pub fn compute_xor_mask(nac: u64, sys_id: u64, wacn: u64) -> Vec<u8> {
    if nac == 0 && sys_id == 0 && wacn == 0 {
        return Vec::new();
    }

    // Build the 44×44 matrix M (identity + feedback taps at offsets 4, 9, 15, 20, 34)
    let mut m = [[0u8; MSIZE]; MSIZE];
    for i in 0..MSIZE {
        m[i][i] = 1;
        if i + 4 < MSIZE {
            m[i][i + 4] = 1;
        }
        if i + 9 < MSIZE {
            m[i][i + 9] = 1;
        }
        if i + 15 < MSIZE {
            m[i][i + 15] = 1;
        }
        if i + 20 < MSIZE {
            m[i][i + 20] = 1;
        }
        if i + 34 < MSIZE {
            m[i][i + 34] = 1;
        }
    }

    // Encode (wacn, sys_id, nac) as a 44-bit integer, then to a bit vector
    let n: u64 = 16_777_216 * wacn + 4096 * sys_id + nac;
    let reg = mk_array(n, MSIZE);

    // Vector-matrix multiply (GF(2): only LSB matters)
    let mut product = [0u8; MSIZE];
    for j in 0..MSIZE {
        let mut sum = 0u32;
        for i in 0..MSIZE {
            sum += u32::from(reg[i]) * u32::from(m[i][j]);
        }
        product[j] = (sum & 1) as u8;
    }

    // Convert product bit vector back to integer
    let mut sreg: u64 = mk_int(&product);

    // Generate LFSR bit stream
    let mut s = [0u8; SSIZE];
    for bit in &mut s {
        *bit = ((sreg >> 43) & 1) as u8;
        sreg = cyc_reg(sreg);
    }

    // Pair bits into dibits
    let mut xor_mask = Vec::with_capacity(SSIZE / 2);
    for i in 0..SSIZE / 2 {
        let dibit = (s[i * 2] << 1) | s[i * 2 + 1];
        xor_mask.push(dibit);
    }

    xor_mask
}

fn mk_array(mut n: u64, len: usize) -> Vec<u8> {
    let mut a = vec![0u8; len];
    for i in (0..len).rev() {
        a[i] = (n & 1) as u8;
        n >>= 1;
    }
    a
}

fn mk_int(a: &[u8]) -> u64 {
    a.iter().fold(0u64, |res, &bit| res * 2 + u64::from(bit & 1))
}

const fn disasm_reg(r: u64) -> [u64; 6] {
    [
        (r >> 40) & 0xf,
        (r >> 35) & 0x1f,
        (r >> 29) & 0x3f,
        (r >> 24) & 0x1f,
        (r >> 10) & 0x3fff,
        r & 0x3ff,
    ]
}

const fn asm_reg(s: &[u64; 6]) -> u64 {
    let s0 = s[0] & 0xf;
    let s1 = s[1] & 0x1f;
    let s2 = s[2] & 0x3f;
    let s3 = s[3] & 0x1f;
    let s4 = s[4] & 0x3fff;
    let s5 = s[5] & 0x3ff;
    (s0 << 40) + (s1 << 35) + (s2 << 29) + (s3 << 24) + (s4 << 10) + s5
}

const fn cyc_reg(reg: u64) -> u64 {
    let mut s = disasm_reg(reg);
    let cy1 = (s[0] >> 3) & 1;
    let cy2 = (s[1] >> 4) & 1;
    let cy3 = (s[2] >> 5) & 1;
    let cy4 = (s[3] >> 4) & 1;
    let cy5 = (s[4] >> 13) & 1;
    let cy6 = (s[5] >> 9) & 1;
    let x1 = cy1 ^ cy2;
    let x2 = cy1 ^ cy3;
    let x3 = cy1 ^ cy4;
    let x4 = cy1 ^ cy5;
    let x5 = cy1 ^ cy6;
    s[0] = ((s[0] << 1) & 0xf) | (x1 & 1);
    s[1] = ((s[1] << 1) & 0x1f) | (x2 & 1);
    s[2] = ((s[2] << 1) & 0x3f) | (x3 & 1);
    s[3] = ((s[3] << 1) & 0x1f) | (x4 & 1);
    s[4] = ((s[4] << 1) & 0x3fff) | (x5 & 1);
    s[5] = ((s[5] << 1) & 0x3ff) | (cy1 & 1);
    asm_reg(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_inputs_returns_empty() {
        assert!(compute_xor_mask(0, 0, 0).is_empty());
    }

    #[test]
    fn output_length() {
        let mask = compute_xor_mask(0x293, 0x001, 0xBEE00);
        assert_eq!(mask.len(), 2160);
    }

    #[test]
    fn all_dibits_in_range() {
        let mask = compute_xor_mask(0x293, 0x001, 0xBEE00);
        assert!(mask.iter().all(|&b| b <= 3));
    }

    #[test]
    fn deterministic() {
        let a = compute_xor_mask(0x293, 0x001, 0xBEE00);
        let b = compute_xor_mask(0x293, 0x001, 0xBEE00);
        assert_eq!(a, b);
    }

    #[test]
    fn different_inputs_different_output() {
        let a = compute_xor_mask(0x293, 0x001, 0xBEE00);
        let b = compute_xor_mask(0x294, 0x001, 0xBEE00);
        assert_ne!(a, b);
    }

    #[test]
    fn matches_cpp_reference() {
        // Reference output from trunk-recorder's p25p2_lfsr(0x293, 0x001, 0xBEE00)
        #[rustfmt::skip]
        let expected: &[u8] = &[
            2,3,3,2,3,2,0,0,0,0,0,0,0,0,0,1,0,2,2,1,0,3,1,2,1,1,0,0,3,1,2,2,
            1,0,1,3,2,3,3,2,0,1,0,2,1,3,2,2,3,0,2,0,1,3,2,2,1,1,2,3,2,1,2,0,
            3,0,1,3,0,2,2,3,0,3,1,1,2,2,1,3,3,3,3,0,1,2,2,3,2,3,1,2,1,3,1,3,
            0,3,2,2,3,1,3,0,2,0,2,3,3,0,2,3,0,2,0,1,1,2,2,0,1,0,3,3,0,2,2,1,
        ];
        let mask = compute_xor_mask(0x293, 0x001, 0xBEE00);
        assert_eq!(&mask[..128], expected, "first 128 dibits must match C++ reference");
    }
}
