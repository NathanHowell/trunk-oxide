//! AMBE+2 decision statistics on synthetic frames, noise to clean: the
//! calibration behind `AmbeFrame::trusted`.
//!
//! cargo run --release -p trunk-p25 --example `tdma_noise`
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
use trunk_p25::symbols::{LEVELS, bit_llrs};
use trunk_p25::tdma::ambe;
use trunk_p25::testgen::{Rng, tdma::random_ambe};

const RULES: [(f32, u32); 4] = [(3.0, 4), (4.0, 4), (4.0, 3), (6.0, 5)];

fn main() {
    let mut rng = Rng::new(1);
    for sigma in [0.0f64, 0.3, 0.4, 0.5, 0.6, 0.7, 100.0] {
        let mut hist = [0usize; 16];
        let mut wrong_hist = [0usize; 16];
        let mut margins = Vec::new();
        let mut wrong_margins = Vec::new();
        let mut trusted = [0usize; 4];
        let mut trusted_wrong = [0usize; 4];
        for _ in 0..20000 {
            let p = random_ambe(&mut rng);
            let cw = p.encode();
            let llr: Vec<f32> = (0..36)
                .flat_map(|d| {
                    let dibit = u8::from(cw.bit(2 * d)) * 2 + u8::from(cw.bit(2 * d + 1));
                    let s = if sigma > 50.0 {
                        rng.uniform().mul_add(8.0, -4.0) as f32
                    } else {
                        sigma.mul_add(rng.gauss(), f64::from(LEVELS[usize::from(dibit)])) as f32
                    };
                    bit_llrs(s)
                })
                .collect();
            let f = ambe::decode(&llr.try_into().unwrap());
            let e = (f.errors as usize).min(15);
            hist[e] += 1;
            if f.params.0[..2] != p.0[..2] {
                wrong_hist[e] += 1;
                wrong_margins.push(f.reliability);
            }
            margins.push(f.reliability);
            for (k, (m, e)) in RULES.iter().enumerate() {
                if f.reliability >= *m && f.errors <= *e {
                    trusted[k] += 1;
                    trusted_wrong[k] += usize::from(f.params.0[..2] != p.0[..2]);
                }
            }
        }
        margins.sort_by(f32::total_cmp);
        let q = |v: &[f32], p: f64| {
            v.get(((v.len() as f64) * p) as usize)
                .copied()
                .unwrap_or(f32::NAN)
        };
        let above =
            |t: f32| margins.iter().filter(|&&m| m >= t).count() as f64 / margins.len() as f64;
        println!(
            "   p90 {:.2} p99 {:.2} p99.9 {:.2}; share ≥2 {:.3} ≥3 {:.3} ≥4 {:.3}; wrong frames {} with margin ≥3: {}",
            q(&margins, 0.9),
            q(&margins, 0.99),
            q(&margins, 0.999),
            above(2.0),
            above(3.0),
            above(4.0),
            wrong_margins.len(),
            wrong_margins.iter().filter(|&&m| m >= 3.0).count()
        );
        println!("   trusted {trusted:?} of which wrong {trusted_wrong:?} for rules {RULES:?}");
        println!("sigma {sigma}");
        println!(
            ": errors {hist:?}\n   wrong {wrong_hist:?}\n   margin p10 {:.1} p50 {:.1}",
            margins[2000], margins[10000]
        );
    }
}
