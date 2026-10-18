//! The rate-1/2 trellis code of TSBKs and PDU headers (TIA-102.BAAA 7.2).
//!
//! 48 data dibits plus one flush dibit drive a 4-state encoder; each step emits
//! a 4-bit constellation point, sent as two dibits. The 98 dibits are then
//! interleaved. op25 decodes this greedily, one step at a time, on hard
//! dibits, and gives up on the first tie; here it is a soft-decision Viterbi
//! decoder over the whole block, terminated in state 0.

use crate::symbols::{LEVELS, dist2, slice};

pub const BLOCK_DIBITS: usize = 98;
/// Soft symbols of one interleaved block.
pub type SoftBlock = [f32; BLOCK_DIBITS];
pub const DATA_BYTES: usize = 12;
const STEPS: usize = 49;

/// Constellation point for (state, input dibit). The state is the previous
/// input dibit.
const NEXT_WORDS: [[u8; 4]; 4] = [
    [0x2, 0xC, 0x1, 0xF],
    [0xE, 0x0, 0xD, 0x3],
    [0x9, 0x7, 0xA, 0x4],
    [0x5, 0xB, 0x6, 0x8],
];

/// Position in the interleaved block of the first dibit of codeword `i`
/// (the second follows it).
pub const fn interleave_pos(i: usize) -> usize {
    const COL: [usize; 4] = [0, 26, 50, 74];
    COL[i % 4] + 2 * (i / 4)
}

const fn data_dibit(data: &[u8; DATA_BYTES], i: usize) -> u8 {
    (data[i / 4] >> (6 - 2 * (i % 4))) & 3
}

/// Encode and interleave 12 bytes into 98 dibits.
pub fn encode(data: &[u8; DATA_BYTES]) -> [u8; BLOCK_DIBITS] {
    let mut out = [0u8; BLOCK_DIBITS];
    let mut state = 0usize;
    for i in 0..STEPS {
        let input = if i < 48 { data_dibit(data, i) } else { 0 };
        let cw = NEXT_WORDS[state][usize::from(input)];
        let p = interleave_pos(i);
        out[p] = cw >> 2;
        out[p + 1] = cw & 3;
        state = usize::from(input);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decoded {
    pub data: [u8; DATA_BYTES],
    /// Received dibits that disagree with the decoded codeword.
    pub corrected: u32,
}

/// Soft-decision Viterbi decode of one interleaved block.
pub fn decode(soft: &SoftBlock) -> Decoded {
    let bm = branch_metrics(soft);

    let mut pm = [0f32, f32::INFINITY, f32::INFINITY, f32::INFINITY];
    let mut back = [[0u8; 4]; STEPS];
    for i in 0..STEPS {
        let mut next = [f32::INFINITY; 4];
        for (prev, &metric) in pm.iter().enumerate() {
            if !metric.is_finite() {
                continue;
            }
            for input in 0..4 {
                let m = metric + bm[i][usize::from(NEXT_WORDS[prev][input])];
                if m < next[input] {
                    next[input] = m;
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        back[i][input] = prev as u8;
                    }
                }
            }
        }
        // The flush dibit is zero, so the last step must land in state 0.
        if i == STEPS - 1 {
            next[1..].fill(f32::INFINITY);
        }
        pm = next;
    }

    let mut inputs = [0u8; STEPS];
    let mut state = 0u8;
    for i in (0..STEPS).rev() {
        inputs[i] = state;
        state = back[i][usize::from(state)];
    }

    let mut data = [0u8; DATA_BYTES];
    for (i, &d) in inputs.iter().take(48).enumerate() {
        data[i / 4] |= d << (6 - 2 * (i % 4));
    }
    let corrected = encode(&data)
        .iter()
        .zip(soft)
        .map(|(&d, &s)| u32::from(slice(s) != d))
        .sum();
    Decoded { data, corrected }
}

fn branch_metrics(soft: &SoftBlock) -> [[f32; 16]; STEPS] {
    let mut bm = [[0f32; 16]; STEPS];
    for (i, row) in bm.iter_mut().enumerate() {
        let p = interleave_pos(i);
        let da: [f32; 4] = std::array::from_fn(|d| {
            #[allow(clippy::cast_possible_truncation)]
            dist2(soft[p], d as u8)
        });
        let db: [f32; 4] = std::array::from_fn(|d| {
            #[allow(clippy::cast_possible_truncation)]
            dist2(soft[p + 1], d as u8)
        });
        for (cw, m) in row.iter_mut().enumerate() {
            *m = da[cw >> 2] + db[cw & 3];
        }
    }
    bm
}

/// One survivor of the list decoder: path metric and where it came from.
#[derive(Clone, Copy)]
struct Entry {
    metric: f32,
    prev_state: u8,
    prev_rank: u8,
}

/// The `list` most likely terminated paths, best first, with their metrics.
///
/// Parallel list Viterbi: each state keeps its `list` best survivors. Used to
/// recover a block whose best path fails its CRC; the CRC picks among the
/// runners-up.
pub fn decode_list(soft: &SoftBlock, list: usize) -> Vec<(Decoded, f32)> {
    let list = list.clamp(1, 64);
    let bm = branch_metrics(soft);
    // table[i][state] = top-`list` entries arriving in `state` after step i.
    let mut table: Vec<[Vec<Entry>; 4]> = Vec::with_capacity(STEPS);
    let mut prev: [Vec<(f32, u8)>; 4] = [vec![(0.0, 0)], Vec::new(), Vec::new(), Vec::new()];
    for (i, row) in bm.iter().enumerate() {
        let mut here: [Vec<Entry>; 4] = Default::default();
        for (input, slot) in here.iter_mut().enumerate() {
            if i == STEPS - 1 && input != 0 {
                continue;
            }
            for (ps, entries) in prev.iter().enumerate() {
                let m = row[usize::from(NEXT_WORDS[ps][input])];
                for (rank, &(metric, _)) in entries.iter().enumerate() {
                    #[allow(clippy::cast_possible_truncation)]
                    slot.push(Entry {
                        metric: metric + m,
                        prev_state: ps as u8,
                        prev_rank: rank as u8,
                    });
                }
            }
            slot.sort_by(|a, b| a.metric.total_cmp(&b.metric));
            slot.truncate(list);
        }
        prev = std::array::from_fn(|s| here[s].iter().map(|e| (e.metric, 0)).collect());
        table.push(here);
    }

    let mut out = Vec::new();
    for (rank, end) in table[STEPS - 1][0].iter().enumerate() {
        let mut inputs = [0u8; STEPS];
        let (mut state, mut r) = (0usize, rank);
        for i in (0..STEPS).rev() {
            #[allow(clippy::cast_possible_truncation)]
            {
                inputs[i] = state as u8;
            }
            let e = table[i][state][r];
            state = usize::from(e.prev_state);
            r = usize::from(e.prev_rank);
        }
        let mut data = [0u8; DATA_BYTES];
        for (i, &d) in inputs.iter().take(48).enumerate() {
            data[i / 4] |= d << (6 - 2 * (i % 4));
        }
        let corrected = encode(&data)
            .iter()
            .zip(soft)
            .map(|(&d, &s)| u32::from(slice(s) != d))
            .sum();
        out.push((Decoded { data, corrected }, end.metric));
    }
    out
}

/// Result of [`decode_checked`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checked {
    pub decoded: Decoded,
    /// 0 when the best path passed its CRC; n when the n-th runner-up did.
    pub rank: usize,
}

/// Decode a CRC-16-terminated block. Tries the best Viterbi path first, then
/// up to `list − 1` runners-up whose metric is within `max_extra` of the
/// best; returns the first that passes the CRC.
pub fn decode_checked(soft: &SoftBlock, list: usize, max_extra: f32) -> Option<Checked> {
    let best = decode(soft);
    if crate::crc::crc16_ok(&best.data) {
        return Some(Checked {
            decoded: best,
            rank: 0,
        });
    }
    if list <= 1 {
        return None;
    }
    let paths = decode_list(soft, list);
    let floor = paths.first()?.1;
    paths
        .iter()
        .enumerate()
        .skip(1)
        .take_while(|(_, (_, m))| *m - floor <= max_extra)
        .find(|(_, (d, _))| crate::crc::crc16_ok(&d.data))
        .map(|(rank, (d, _))| Checked { decoded: *d, rank })
}

/// op25's decoder, kept as the baseline the tests compare against.
///
/// `block_deinterleave` in `p25p1_fdma.cc` walks the trellis on hard dibits,
/// taking the nearest constellation point at each step and failing on a tie.
pub fn decode_greedy(soft: &SoftBlock) -> Option<[u8; DATA_BYTES]> {
    let mut data = [0u8; DATA_BYTES];
    let mut state = 0usize;
    for i in 0..STEPS {
        let p = interleave_pos(i);
        let cw = (slice(soft[p]) << 2) | slice(soft[p + 1]);
        let hd: [u32; 4] = std::array::from_fn(|j| (cw ^ NEXT_WORDS[state][j]).count_ones());
        let min = *hd.iter().min().unwrap_or(&0);
        let mut best = hd.iter().enumerate().filter(|&(_, &h)| h == min);
        let (input, _) = best.next()?;
        if best.next().is_some() {
            return None;
        }
        if i < 48 {
            #[allow(clippy::cast_possible_truncation)]
            {
                data[i / 4] |= (input as u8) << (6 - 2 * (i % 4));
            }
        }
        state = input;
    }
    Some(data)
}

/// Soft symbols for a clean block (ideal levels).
pub fn ideal(dibits: &[u8; BLOCK_DIBITS]) -> SoftBlock {
    dibits.map(|d| LEVELS[usize::from(d)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc;
    use crate::testgen::Rng;

    fn tsbk(rng: &mut Rng) -> [u8; DATA_BYTES] {
        let mut b = [0u8; DATA_BYTES];
        for x in &mut b[..10] {
            #[allow(clippy::cast_possible_truncation)]
            {
                *x = rng.next_u64() as u8;
            }
        }
        let c = crc::crc16(&b[..10]);
        b[10..].copy_from_slice(&c.to_be_bytes());
        b
    }

    #[test]
    fn interleave_covers_every_dibit_once() {
        let mut seen = [false; BLOCK_DIBITS];
        for i in 0..STEPS {
            let p = interleave_pos(i);
            assert!(!seen[p] && !seen[p + 1]);
            seen[p] = true;
            seen[p + 1] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    #[test]
    fn interleave_matches_op25_bit_table() {
        // First entries of each row of op25's deinterleave_tb, in bits.
        let rows = [0usize, 52, 100, 148, 4, 56, 104, 152, 48];
        let cw = [0usize, 1, 2, 3, 4, 5, 6, 7, 48];
        for (r, i) in rows.iter().zip(cw) {
            assert_eq!(interleave_pos(i) * 2, *r);
        }
    }

    #[test]
    fn clean_blocks_round_trip() {
        let mut rng = Rng::new(1);
        for _ in 0..50 {
            let b = tsbk(&mut rng);
            let soft = ideal(&encode(&b));
            assert_eq!(
                decode(&soft),
                Decoded {
                    data: b,
                    corrected: 0
                }
            );
            assert_eq!(decode_greedy(&soft), Some(b));
        }
    }

    #[test]
    fn viterbi_corrects_symbol_errors_greedy_cannot() {
        let mut rng = Rng::new(2);
        let (mut viterbi, mut greedy) = (0, 0);
        let trials = 300;
        for _ in 0..trials {
            let b = tsbk(&mut rng);
            let mut dibits = encode(&b);
            // Four scattered hard symbol errors to an adjacent level.
            for _ in 0..4 {
                let p = rng.below(BLOCK_DIBITS);
                dibits[p] = match dibits[p] {
                    0 => 2,
                    1 => 0,
                    2 => 3,
                    _ => 1,
                };
            }
            let soft = ideal(&dibits);
            viterbi += usize::from(decode(&soft).data == b);
            greedy += usize::from(decode_greedy(&soft) == Some(b));
        }
        assert!(viterbi >= trials * 95 / 100, "viterbi {viterbi}/{trials}");
        assert!(
            viterbi > greedy + trials / 5,
            "viterbi {viterbi} greedy {greedy}"
        );
    }

    #[test]
    fn list_head_is_the_viterbi_path_and_metrics_ascend() {
        let mut rng = Rng::new(4);
        for _ in 0..50 {
            let b = tsbk(&mut rng);
            #[allow(clippy::cast_possible_truncation)]
            let noisy = ideal(&encode(&b)).map(|s| 0.9f32.mul_add(rng.gauss() as f32, s));
            let list = decode_list(&noisy, 8);
            assert_eq!(list.len(), 8);
            assert_eq!(list[0].0.data, decode(&noisy).data);
            assert!(list.windows(2).all(|w| w[0].1 <= w[1].1));
            let mut distinct: Vec<_> = list.iter().map(|(d, _)| d.data).collect();
            distinct.dedup();
            assert_eq!(distinct.len(), 8, "paths must differ");
        }
    }

    #[test]
    fn list_decoding_recovers_crc_failures() {
        let mut rng = Rng::new(5);
        let trials = 600;
        let (mut plain, mut listed, mut wrong) = (0, 0, 0);
        for _ in 0..trials {
            let b = tsbk(&mut rng);
            #[allow(clippy::cast_possible_truncation)]
            let noisy = ideal(&encode(&b)).map(|s| 0.85f32.mul_add(rng.gauss() as f32, s));
            plain += usize::from(decode(&noisy).data == b);
            match decode_checked(&noisy, 16, 12.0) {
                Some(c) if c.decoded.data == b => listed += 1,
                Some(_) => wrong += 1,
                None => {}
            }
        }
        assert!(listed > plain + trials / 50, "list {listed} plain {plain}");
        assert_eq!(wrong, 0, "list decoding accepted a wrong block");
    }

    #[test]
    fn soft_decisions_beat_hard_in_gaussian_noise() {
        let mut rng = Rng::new(3);
        let trials = 400;
        let (mut soft_ok, mut greedy_ok) = (0, 0);
        for _ in 0..trials {
            let b = tsbk(&mut rng);
            #[allow(clippy::cast_possible_truncation)]
            let noisy = ideal(&encode(&b)).map(|s| 0.75f32.mul_add(rng.gauss() as f32, s));
            soft_ok += usize::from(crc::crc16_ok(&decode(&noisy).data));
            greedy_ok += usize::from(decode_greedy(&noisy).is_some_and(|d| crc::crc16_ok(&d)));
        }
        assert!(soft_ok >= trials * 9 / 10, "soft {soft_ok}/{trials}");
        assert!(soft_ok > greedy_ok * 2, "soft {soft_ok} greedy {greedy_ok}");
    }
}
