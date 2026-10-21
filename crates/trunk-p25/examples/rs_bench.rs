//! Reed-Solomon `decode_gmd` throughput on a fixed corpus.
//!
//! It also prints a checksum of every result, so a change to the decoder can
//! be shown not to alter one.
//!
//!   `cargo run --release -p trunk-p25 --example rs_bench`
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::hint::black_box;
use std::time::Instant;

use trunk_p25::fec::rs::ReedSolomon;
use trunk_p25::fec::{RS_24_12, RS_24_16, RS_36_20};
use trunk_p25::gf64::Gf64;

struct Rng(u64);
impl Rng {
    const fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    const fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Words with 0..=R+2 symbol errors and noisy reliabilities, so decodes
/// succeed, fail and need erasures in realistic proportion.
fn corpus<const N: usize, const K: usize>(
    rs: &ReedSolomon<N, K>,
    rng: &mut Rng,
    count: usize,
) -> Vec<([Gf64; N], [f32; N])> {
    (0..count)
        .map(|_| {
            let data: [Gf64; K] = std::array::from_fn(|_| Gf64::new(rng.below(64) as u8));
            let mut word = rs.encode(&data);
            let mut rel: [f32; N] = std::array::from_fn(|_| 0.6 + (rng.below(100) as f32) / 250.0);
            for _ in 0..rng.below(rs.parity() + 3) {
                let j = rng.below(N);
                word[j] = Gf64::new(rng.below(64) as u8);
                rel[j] = (rng.below(100) as f32) / 400.0;
            }
            (word, rel)
        })
        .collect()
}

fn run<const N: usize, const K: usize>(name: &str, rs: &ReedSolomon<N, K>, seed: u64) -> u64 {
    let mut rng = Rng(seed);
    let words = corpus(rs, &mut rng, 4_000);
    let mut sum = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |v: u64| sum = (sum ^ v).wrapping_mul(0x0000_0100_0000_01b3);
    let mut ok = 0;
    let t = Instant::now();
    for _ in 0..5 {
        for (word, rel) in &words {
            let mut w = *word;
            let r = black_box(rs.decode_gmd(&mut w, rel));
            if let Some(d) = r {
                ok += 1;
                mix(d.changed as u64);
                mix(d.erased as u64);
                mix(u64::from(d.distance.to_bits()));
            } else {
                mix(0xdead);
            }
            for g in w {
                mix(u64::from(g.get()));
            }
        }
    }
    let per = t.elapsed().as_secs_f64() * 1e6 / (5.0 * words.len() as f64);
    println!(
        "{name}: {per:.2} us/word, {} of {} decoded, checksum {sum:016x}",
        ok / 5,
        words.len()
    );
    sum
}

fn main() {
    let a = run("RS(24,12)", &RS_24_12, 1);
    let b = run("RS(24,16)", &RS_24_16, 2);
    let c = run("RS(36,20)", &RS_36_20, 3);
    println!("all: {:016x}", a ^ b.rotate_left(1) ^ c.rotate_left(2));
}
