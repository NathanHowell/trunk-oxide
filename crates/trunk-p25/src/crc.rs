//! CRCs used by P25 data blocks.

/// CRC-16 of a TSBK or PDU header: CCITT polynomial, zero initial value,
/// inverted (CRC-16/GSM). Returns the two check bytes to append to `data`.
pub const fn crc16(data: &[u8]) -> u16 {
    let mut reg: u32 = 0;
    let mut i = 0;
    while i < data.len() {
        let mut j = 8;
        while j > 0 {
            j -= 1;
            let bit = ((data[i] >> j) & 1) as u32;
            let top = ((reg >> 15) & 1) ^ bit;
            reg = (reg << 1) & 0xffff;
            if top == 1 {
                reg ^= 0x1021;
            }
        }
        i += 1;
    }
    #[allow(clippy::cast_possible_truncation)]
    let crc = (reg ^ 0xffff) as u16;
    crc
}

/// True when the last two bytes of `block` are its CRC-16.
pub const fn crc16_ok(block: &[u8]) -> bool {
    let (data, check) = block.split_at(block.len() - 2);
    crc16(data) == u16::from_be_bytes([check[0], check[1]])
}

/// CRC-32 over the first `nbits` bits of `buf` (MBT payloads), as op25
/// computes it.
pub const fn crc32(buf: &[u8], nbits: usize) -> u32 {
    const G: u64 = 0x04c1_1db7;
    let mut crc: u64 = 0;
    let mut i = 0;
    while i < nbits {
        crc <<= 1;
        let b = ((buf[i / 8] >> (7 - (i % 8))) & 1) as u64;
        if ((crc >> 32) ^ b) & 1 == 1 {
            crc ^= G;
        }
        i += 1;
    }
    #[allow(clippy::cast_possible_truncation)]
    let crc = ((crc & 0xffff_ffff) ^ 0xffff_ffff) as u32;
    crc
}

/// CRC-12 of a Phase 2 MAC PDU over the first `nbits` bits of `buf`:
/// g(x) = x¹² + x¹¹ + x⁷ + x⁴ + x² + x + 1, zero initial value, inverted.
pub const fn crc12(buf: &[u8], nbits: usize) -> u16 {
    const G: u16 = 0x897;
    let mut reg: u16 = 0;
    let mut i = 0;
    while i < nbits {
        let b = ((buf[i / 8] >> (7 - (i % 8))) & 1) as u16;
        let top = ((reg >> 11) & 1) ^ b;
        reg = (reg << 1) & 0xfff;
        if top == 1 {
            reg ^= G;
        }
        i += 1;
    }
    reg ^ 0xfff
}

/// `count` bits of `buf` from bit `from`, first bit highest.
pub const fn bits_at(buf: &[u8], from: usize, count: usize) -> u32 {
    let mut v = 0;
    let mut i = from;
    while i < from + count {
        v = (v << 1) | ((buf[i / 8] >> (7 - (i % 8))) & 1) as u32;
        i += 1;
    }
    v
}

/// True when bits `nbits..nbits + 12` of `buf` are the CRC-12 of the bits
/// before them.
pub const fn crc12_ok(buf: &[u8], nbits: usize) -> bool {
    bits_at(buf, nbits, 12) == crc12(buf, nbits) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// op25's check: run every bit (data and CRC) through the register,
    /// invert, expect zero.
    fn op25_crc16(buf: &[u8]) -> u16 {
        let poly: u32 = (1 << 12) + (1 << 5) + 1;
        let mut crc: u32 = 0;
        for &bits in buf {
            for j in 0..8 {
                let bit = u32::from((bits >> (7 - j)) & 1);
                crc = ((crc << 1) | bit) & 0x1ffff;
                if crc & 0x10000 != 0 {
                    crc = (crc & 0xffff) ^ poly;
                }
            }
        }
        #[allow(clippy::cast_possible_truncation)]
        let out = ((crc ^ 0xffff) & 0xffff) as u16;
        out
    }

    #[test]
    fn crc16_is_crc16_gsm() {
        const CHECK: u16 = crc16(b"123456789");
        assert_eq!(CHECK, 0xce3c);
    }

    #[test]
    fn crc16_agrees_with_op25_check() {
        // A TSBK from the golden capture (NAC stripped), CRC appended.
        let mut block = vec![0x3b, 0x00, 0x00, 0xbe, 0xe0, 0x03, 0x8d, 0x17, 0x2c, 0x70];
        let c = crc16(&block);
        block.extend_from_slice(&c.to_be_bytes());
        assert!(crc16_ok(&block));
        assert_eq!(op25_crc16(&block), 0);
        block[3] ^= 0x10;
        assert!(!crc16_ok(&block));
        assert_ne!(op25_crc16(&block), 0);
    }

    #[test]
    fn crc32_detects_a_flip() {
        let mut data = [0x12u8, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0];
        let a = crc32(&data, 64);
        data[2] ^= 1;
        assert_ne!(a, crc32(&data, 64));
    }

    /// op25's `crc12` (`p25p2_tdma.cc`), from a harness linked against op25.
    #[test]
    fn crc12_matches_op25() {
        let bits = "010000111000111011101101000110011100111101010111111011100010110001100110011000001010110100001000101001010110100100101101101000010100111001101011010100101010001111000110";
        let mut buf = [0u8; 21];
        for (i, c) in bits.bytes().enumerate() {
            buf[i / 8] |= (c - b'0') << (7 - i % 8);
        }
        assert_eq!(crc12(&buf, 144), 1506);
        assert_eq!(crc12(&buf, 168), 2815);
    }
}
