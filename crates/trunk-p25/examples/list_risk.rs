//! Recovered vs wrongly accepted blocks for list sizes and metric margins.

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
use trunk_p25::{
    crc,
    testgen::{self, Rng},
    trellis,
};
fn main() {
    let mut rng = Rng::new(99);
    let trials = 200_000;
    let sigma: f64 = std::env::args().nth(1).map_or(1.0, |s| s.parse().unwrap());
    let configs = [
        (4, 8.0f32),
        (4, 12.0),
        (8, 8.0),
        (8, 12.0),
        (8, 20.0),
        (16, 12.0),
        (16, 1e9),
        (32, 1e9),
    ];
    let mut plain = 0;
    let mut fail = 0;
    let mut rec = vec![0usize; configs.len()];
    let mut wrong = vec![0usize; configs.len()];
    for _ in 0..trials {
        let t = testgen::random_tsbks(&mut rng, 1)[0];
        let b = testgen::tsbk_block(&t, true);
        let noisy = trellis::ideal(&trellis::encode(&b)).map(|s| s + (sigma * rng.gauss()) as f32);
        let best = trellis::decode(&noisy);
        if crc::crc16_ok(&best.data) {
            if best.data == b {
                plain += 1;
            } else {
                println!("plain false accept");
            }
            continue;
        }
        fail += 1;
        for (i, &(l, m)) in configs.iter().enumerate() {
            if let Some(c) = trellis::decode_checked(&noisy, l, m) {
                if c.decoded.data == b {
                    rec[i] += 1;
                } else {
                    wrong[i] += 1;
                }
            }
        }
    }
    println!("sigma {sigma}: plain ok {plain}, crc failures {fail}");
    for (i, c) in configs.iter().enumerate() {
        println!(
            "list {:2} extra {:>5}: recovered {:6} ({:.1}% of failures) wrong {}",
            c.0,
            c.1,
            rec[i],
            100.0 * rec[i] as f64 / f64::from(fail),
            wrong[i]
        );
    }
}
