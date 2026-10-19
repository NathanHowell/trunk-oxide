//! Over-the-air unit aliases.
//!
//! Motorola's obfuscated link-control / MAC messages and Harris's plain
//! ones, as trunk-recorder's `unit_tags_ota` decoded them (Motorola
//! algorithm: Ilya Smirnov / @ilyacodes, 2024).

use trunk_p25::assemble::Alias;

use crate::types::UnitAlias;

/// Decode an alias the voice decoder assembled.
///
/// Harris aliases name no radio themselves: they are taken for
/// `current_source` (the unit talking on the call), and only when the
/// decoder's cached ids agree and are fresh.
pub fn decode(alias: &Alias, current_source: Option<u32>) -> Option<UnitAlias> {
    match alias {
        Alias::Motorola {
            phase2,
            messages,
            blocks,
        } => motorola(*phase2, *messages, blocks),
        Alias::Harris {
            phase2,
            data,
            source,
            group,
            cache_age_ms,
        } => harris(
            *phase2,
            data,
            *source,
            *group,
            *cache_age_ms,
            current_source,
        ),
    }
}

/// How old the decoder's cached unit/group may be for a Harris alias.
const HARRIS_CACHE_MAX_MS: u64 = 300;

fn harris(
    phase2: bool,
    data: &[u8],
    source: i64,
    group: i64,
    cache_age_ms: u64,
    current_source: Option<u32>,
) -> Option<UnitAlias> {
    if cache_age_ms > HARRIS_CACHE_MAX_MS
        || current_source.map(i64::from) != Some(source)
        || group <= 0
    {
        return None;
    }
    // Phase 1: exactly 14 characters (part A + part B). Phase 2: one message,
    // the text from byte 3 (after opcode, MFID, length).
    let text: &[u8] = if phase2 {
        if data.len() < 4 {
            return None;
        }
        &data[3..]
    } else {
        if data.len() != 14 {
            return None;
        }
        data
    };
    let alias = printable(text);
    (!alias.is_empty()).then(|| UnitAlias {
        radio_id: source,
        alias,
        source: if phase2 {
            "HarrisP25_TDMA"
        } else {
            "HarrisP25_FDMA"
        }
        .into(),
        talkgroup_id: group,
    })
}

/// Printable ASCII up to the first NUL, trailing whitespace trimmed.
fn printable(bytes: &[u8]) -> String {
    let s: String = bytes
        .iter()
        .take_while(|&&c| c != 0)
        .filter(|&&c| (0x20..=0x7e).contains(&c))
        .map(|&c| char::from(c))
        .collect();
    s.trim_end().to_owned()
}

fn block(blocks: &[(u8, Vec<u8>)], index: u8) -> Option<&[u8]> {
    blocks
        .iter()
        .find(|(i, _)| *i == index)
        .map(|(_, b)| b.as_slice())
}

/// The payload as a hex string, the way the C++ assembled it: the header
/// block's bytes, then each data block's bytes less their first nibble (the
/// sequence id).
fn assemble(phase2: bool, messages: u8, blocks: &[(u8, Vec<u8>)]) -> Option<String> {
    let (header_range, data_range, min_len) = if phase2 {
        (3..17, 4..17, 17)
    } else {
        (2..9, 3..9, 9)
    };
    let header = block(blocks, 0)?;
    if header.len() < min_len {
        return None;
    }
    let mut hex = to_hex(&header[header_range]);
    for i in 1..=messages {
        let b = block(blocks, i)?;
        if b.len() < min_len {
            return None;
        }
        hex.push_str(&to_hex(&b[data_range.clone()])[1..]);
    }
    Some(hex)
}

fn motorola(phase2: bool, messages: u8, blocks: &[(u8, Vec<u8>)]) -> Option<UnitAlias> {
    if messages == 0 || messages >= 10 {
        return None;
    }
    let payload = assemble(phase2, messages, blocks)?;
    // Phase 2 has one fewer "unknown" nibble group before the WACN.
    let (wacn_at, min_len) = if phase2 { (12, 30) } else { (14, 32) };
    if payload.len() < min_len {
        return None;
    }
    let tg = &payload[0..4];
    let wacn = &payload[wacn_at..wacn_at + 5];
    let sys = &payload[wacn_at + 5..wacn_at + 8];
    let radio = &payload[wacn_at + 8..wacn_at + 14];
    let alias_at = wacn_at + 14;
    let alias_len = match &payload[alias_at..alias_at + 2] {
        "94" => 1,
        "32" => 2,
        "95" => 3,
        "9d" => 4,
        "1b" => 5,
        "77" => 6,
        "b5" => 7,
        "6e" => 8,
        "24" => 9,
        "61" => 10,
        "2d" => 11,
        "7d" => 12,
        "83" => 13,
        "29" => 14,
        _ => return None,
    };
    let end = alias_at + alias_len * 4;
    if payload.len() < end + 4 {
        return None;
    }
    let alias_code = &payload[alias_at..end];
    let checksum = u16::from_str_radix(&payload[end..end + 4], 16).ok()?;
    let crc_payload = from_hex(&format!("{wacn}{sys}{radio}{alias_code}"))?;
    if crc16_gsm(&crc_payload) != checksum {
        return None;
    }
    if crc_payload.len() < 8 {
        return None;
    }
    let alias = deobfuscate(&crc_payload[7..]);
    let alias = alias.trim_end().to_owned();
    if alias.is_empty() {
        return None;
    }
    Some(UnitAlias {
        radio_id: i64::from_str_radix(radio, 16).ok()?,
        alias,
        source: if phase2 {
            "MotoP25_TDMA"
        } else {
            "MotoP25_FDMA"
        }
        .into(),
        talkgroup_id: i64::from_str_radix(tg, 16).ok()?,
    })
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    s.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// CRC-16/GSM: poly 0x1021, init 0, final complement.
fn crc16_gsm(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in bytes {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    !crc
}

const SUBSTITUTION: [u8; 256] = [
    0xd2, 0xf6, 0xd4, 0x2b, 0x63, 0x49, 0x94, 0x5e, 0xa7, 0x5c, 0x70, 0x69, 0xf7, 0x08, 0xb1, 0x7d,
    0x38, 0xcf, 0xcc, 0xd8, 0x51, 0x8f, 0xd5, 0x93, 0x6a, 0xf3, 0xef, 0x7e, 0xfb, 0x64, 0xf4, 0x35,
    0x27, 0x07, 0x31, 0x14, 0x87, 0x98, 0x76, 0x34, 0xca, 0x92, 0x33, 0x1b, 0x4f, 0x8c, 0x09, 0x40,
    0x32, 0x36, 0x77, 0x12, 0xd3, 0xc3, 0x01, 0xab, 0x72, 0x81, 0x95, 0xc9, 0xc0, 0xe9, 0x65, 0x52,
    0x24, 0x30, 0x1c, 0xdb, 0x88, 0xe8, 0x97, 0x9d, 0x58, 0x26, 0x04, 0x39, 0xac, 0x2a, 0x9e, 0xaa,
    0x25, 0xd7, 0xce, 0xeb, 0x96, 0xf5, 0x0e, 0x8d, 0xdc, 0xa9, 0x2f, 0xdd, 0x1f, 0xea, 0x91, 0xb7,
    0xd6, 0x89, 0x8b, 0xd1, 0xb0, 0x99, 0x13, 0x7a, 0xe7, 0x9a, 0xb5, 0x86, 0xff, 0x46, 0x85, 0xb2,
    0x73, 0xda, 0xbf, 0xd0, 0x71, 0xcb, 0x4d, 0x80, 0x15, 0x67, 0x16, 0x1a, 0x20, 0x8e, 0x45, 0x3e,
    0xf2, 0x2e, 0x66, 0x90, 0x74, 0x8a, 0x6f, 0x78, 0xbb, 0x53, 0x03, 0x11, 0x68, 0xcd, 0x44, 0x17,
    0x28, 0x5f, 0x1e, 0x84, 0x75, 0x79, 0x6e, 0x9b, 0x2c, 0xbe, 0x62, 0x2d, 0xf1, 0x7c, 0xb8, 0x83,
    0xd9, 0x4e, 0x6d, 0x02, 0x61, 0x3d, 0xa8, 0x06, 0xb9, 0xf8, 0x9c, 0x37, 0x3a, 0x23, 0xc1, 0x50,
    0xed, 0x9f, 0xaf, 0x3b, 0xbd, 0x82, 0xba, 0xa0, 0xdf, 0xc2, 0x47, 0x22, 0xf0, 0xee, 0xa1, 0xfe,
    0xa2, 0x10, 0x5b, 0x48, 0x57, 0xa3, 0x05, 0x60, 0x7b, 0x0d, 0xf9, 0x6c, 0xb3, 0x56, 0x4c, 0xbc,
    0x29, 0xa4, 0x0f, 0xec, 0xb6, 0xa5, 0xa6, 0x3c, 0x7f, 0x6b, 0xb4, 0x21, 0xad, 0xae, 0xc4, 0xc8,
    0xc5, 0x5d, 0xde, 0xe0, 0x1d, 0x19, 0x4b, 0xc6, 0x0c, 0x3f, 0x5a, 0xc7, 0xe1, 0x59, 0x55, 0x54,
    0x4a, 0x43, 0x42, 0xe2, 0xe3, 0xfa, 0x00, 0xe4, 0xe5, 0x18, 0x41, 0x0b, 0x0a, 0xe6, 0xfc, 0xfd,
];

const MODULAR_INVERSE_ODD: [u8; 128] = [
    0x01, 0xab, 0xcd, 0xb7, 0x39, 0xa3, 0xc5, 0xef, 0xf1, 0x1b, 0x3d, 0xa7, 0x29, 0x13, 0x35, 0xdf,
    0xe1, 0x8b, 0xad, 0x97, 0x19, 0x83, 0xa5, 0xcf, 0xd1, 0xfb, 0x1d, 0x87, 0x09, 0xf3, 0x15, 0xbf,
    0xc1, 0x6b, 0x8d, 0x77, 0xf9, 0x63, 0x85, 0xaf, 0xb1, 0xdb, 0xfd, 0x67, 0xe9, 0xd3, 0xf5, 0x9f,
    0xa1, 0x4b, 0x6d, 0x57, 0xd9, 0x43, 0x65, 0x8f, 0x91, 0xbb, 0xdd, 0x47, 0xc9, 0xb3, 0xd5, 0x7f,
    0x81, 0x2b, 0x4d, 0x37, 0xb9, 0x23, 0x45, 0x6f, 0x71, 0x9b, 0xbd, 0x27, 0xa9, 0x93, 0xb5, 0x5f,
    0x61, 0x0b, 0x2d, 0x17, 0x99, 0x03, 0x25, 0x4f, 0x51, 0x7b, 0x9d, 0x07, 0x89, 0x73, 0x95, 0x3f,
    0x41, 0xeb, 0x0d, 0xf7, 0x79, 0xe3, 0x05, 0x2f, 0x31, 0x5b, 0x7d, 0xe7, 0x69, 0x53, 0x75, 0x1f,
    0x21, 0xcb, 0xed, 0xd7, 0x59, 0xc3, 0xe5, 0x0f, 0x11, 0x3b, 0x5d, 0xc7, 0x49, 0x33, 0x55, 0xff,
];

/// Motorola's obfuscation undone: a substitution, an LCG-keyed subtraction
/// and a modular multiply per byte, then big-endian 16-bit characters.
#[allow(clippy::cast_possible_truncation)]
fn deobfuscate(encoded: &[u8]) -> String {
    let mut decoded = vec![0u8; encoded.len()];
    let mut accumulator = encoded.len() as u16;
    for (d, &e) in decoded.iter_mut().zip(encoded) {
        let lcg = accumulator.wrapping_mul(293).wrapping_add(0x72E9);
        let substituted = SUBSTITUTION[usize::from(e)];
        let intermediate = substituted.wrapping_sub((lcg >> 8) as u8);
        let modulus = (lcg as u8) | 1;
        let inverse = MODULAR_INVERSE_ODD[usize::from(modulus >> 1)];
        *d = intermediate.wrapping_mul(inverse);
        accumulator = accumulator.wrapping_add(u16::from(e)).wrapping_add(1);
    }
    decoded
        .as_chunks::<2>()
        .0
        .iter()
        .filter_map(|&[hi, lo]| {
            let cp = u16::from_be_bytes([hi, lo]);
            (cp > 31 && cp < 128).then(|| char::from(cp as u8))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_gsm_matches_the_reference() {
        // CRC-16/GSM check value for "123456789" is 0xCE3C.
        assert_eq!(crc16_gsm(b"123456789"), 0xCE3C);
    }

    #[test]
    fn harris_aliases_take_the_current_unit_when_fresh_and_agreeing() {
        let a = Alias::Harris {
            phase2: false,
            data: b"ENGINE 7      ".to_vec(),
            source: 1234,
            group: 100,
            cache_age_ms: 50,
        };
        let u = decode(&a, Some(1234)).unwrap();
        assert_eq!(
            (u.radio_id, u.alias.as_str(), u.talkgroup_id),
            (1234, "ENGINE 7", 100)
        );
        assert_eq!(u.source, "HarrisP25_FDMA");
        assert!(decode(&a, Some(999)).is_none(), "another unit is talking");
        let stale = Alias::Harris {
            phase2: false,
            data: b"ENGINE 7      ".to_vec(),
            source: 1234,
            group: 100,
            cache_age_ms: 400,
        };
        assert!(decode(&stale, Some(1234)).is_none());
        let p2 = Alias::Harris {
            phase2: true,
            data: vec![0x81, 0xa4, 0x05, b'M', b'E', b'D', b'1', 0x00, 0xff],
            source: 7,
            group: 9,
            cache_age_ms: 0,
        };
        let u = decode(&p2, Some(7)).unwrap();
        assert_eq!(
            (u.alias.as_str(), u.source.as_str()),
            ("MED1", "HarrisP25_TDMA")
        );
    }

    /// A Motorola alias built the way a radio would send it: the
    /// obfuscation is a bijection per position, so encode by searching.
    fn obfuscate(plain: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        let mut accumulator = plain.len() as u16;
        for &p in plain {
            let lcg = accumulator.wrapping_mul(293).wrapping_add(0x72E9);
            let modulus = (lcg as u8) | 1;
            let inverse = MODULAR_INVERSE_ODD[usize::from(modulus >> 1)];
            let e = (0..=255u8)
                .find(|&e| {
                    SUBSTITUTION[usize::from(e)]
                        .wrapping_sub((lcg >> 8) as u8)
                        .wrapping_mul(inverse)
                        == p
                })
                .expect("bijection");
            encoded.push(e);
            accumulator = accumulator.wrapping_add(u16::from(e)).wrapping_add(1);
        }
        encoded
    }

    /// Phase 1 blocks carrying `payload_hex`, as [`assemble`] takes them
    /// apart: 7 bytes after a 2-byte header prefix, then 11 hex digits per
    /// data block after a 3-byte prefix and a dropped nibble.
    fn pack_phase1(payload_hex: &str) -> (u8, Vec<(u8, Vec<u8>)>) {
        let bytes = from_hex(payload_hex).unwrap();
        let mut header = vec![0u8, 0u8];
        header.extend_from_slice(&bytes[..7]);
        let mut blocks = vec![(0u8, header)];
        let rest = &payload_hex[14..];
        let mut i = 1u8;
        let mut pos = 0;
        while pos < rest.len() {
            let chunk: String = rest.chars().skip(pos).take(11).collect();
            let mut b = vec![0u8, 0u8, 0u8];
            b.extend(from_hex(&format!("0{chunk:0<11}")).unwrap());
            blocks.push((i, b));
            i += 1;
            pos += 11;
        }
        (i - 1, blocks)
    }

    #[test]
    fn motorola_phase1_alias_round_trips() {
        // "CAR 12": six big-endian 16-bit characters. Obfuscated, the first
        // byte comes out as 0x77, the length code for 6.
        let plain: Vec<u8> = "CAR 12".bytes().flat_map(|c| [0, c]).collect();
        let encoded = obfuscate(&plain);
        assert_eq!(encoded[0], 0x77);
        assert_eq!(deobfuscate(&encoded), "CAR 12");
        let (wacn, sys, radio) = ("bee00", "3a1", "0004d2");
        let alias_hex = to_hex(&encoded);
        let crc = crc16_gsm(&from_hex(&format!("{wacn}{sys}{radio}{alias_hex}")).unwrap());
        // tg, block count, unknown, sequence, unknown, wacn, sys, radio, alias, crc
        let payload = format!("00640400000000{wacn}{sys}{radio}{alias_hex}{crc:04x}");
        let (messages, blocks) = pack_phase1(&payload);
        let a = Alias::Motorola {
            phase2: false,
            messages,
            blocks,
        };
        let u = decode(&a, None).unwrap();
        assert_eq!(
            (
                u.radio_id,
                u.alias.as_str(),
                u.talkgroup_id,
                u.source.as_str()
            ),
            (1234, "CAR 12", 100, "MotoP25_FDMA")
        );
        // A corrupted checksum is rejected.
        let bad = payload[..payload.len() - 1].to_owned() + "0";
        let (messages, blocks) = pack_phase1(&bad);
        assert!(
            decode(
                &Alias::Motorola {
                    phase2: false,
                    messages,
                    blocks
                },
                None
            )
            .is_none()
        );
    }
}
