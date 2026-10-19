//! Compares the chain with reference outputs of the GNU Radio blocks.
//!
//! Set `FM_REFERENCE_DIR` to a directory holding `demod.f32`, `deemph.f32`,
//! `audio16k.f32`, `bandpass.f32`, `decim_taps.f32`, `hp_taps.f32`,
//! `lp_taps.f32`, `atan_table.f32` and `atan_sweep.f32` (as the session's
//! `fm_ref/ref.cc` harness writes them) and `FM_REFERENCE_IQ` to the
//! conditioned cf32 tap they were made from. Without them the tests pass
//! vacuously.

use num_complex::Complex32;
use trunk_fm::design::{Window, high_pass, low_pass};
use trunk_fm::io::{cf32_from_bytes, f32_from_bytes};
use trunk_fm::{Audio, Decimation, FmDemod, TAU_US, fast_atan2};

fn reference() -> Option<(std::path::PathBuf, Vec<Complex32>)> {
    let dir = std::env::var_os("FM_REFERENCE_DIR")?;
    let iq_path = std::env::var_os("FM_REFERENCE_IQ")?;
    let iq = cf32_from_bytes(&std::fs::read(iq_path).ok()?);
    Some((dir.into(), iq))
}

fn read_f32(path: &std::path::Path) -> Vec<f32> {
    f32_from_bytes(&std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

/// Largest absolute difference over the common length, and where.
fn max_err(a: &[f32], b: &[f32]) -> (f32, usize) {
    a.iter()
        .zip(b)
        .enumerate()
        .map(|(i, (x, y))| ((x - y).abs(), i))
        .fold((0.0, 0), |m, e| if e.0 > m.0 { e } else { m })
}

#[test]
fn taps_match_firdes_exactly() {
    let Some((dir, _)) = reference() else { return };
    let decim = read_f32(&dir.join("decim_taps.f32"));
    let mine = low_pass(1.0, 1.0, 0.45, 0.1, Window::Kaiser(5.0));
    assert_eq!(mine.len(), decim.len());
    let (e, i) = max_err(&mine, &decim);
    assert!(e <= 1e-7, "decimation taps differ by {e} at {i}");
    let hp = read_f32(&dir.join("hp_taps.f32"));
    let (e, i) = max_err(&high_pass(1.0, 16_000.0, 300.0, 50.0, Window::Hann), &hp);
    assert!(e <= 1e-7, "high-pass taps differ by {e} at {i}");
    let lp = read_f32(&dir.join("lp_taps.f32"));
    let (e, i) = max_err(&low_pass(1.0, 16_000.0, 3_250.0, 500.0, Window::Hann), &lp);
    assert!(e <= 1e-7, "low-pass taps differ by {e} at {i}");
}

#[test]
fn fast_atan2_matches_gnuradio_bit_for_bit() {
    let Some((dir, _)) = reference() else { return };
    let sweep = read_f32(&dir.join("atan_sweep.f32"));
    let (triples, _) = sweep.as_chunks::<3>();
    for (k, s) in triples.iter().enumerate() {
        let mine = fast_atan2(s[0], s[1]);
        assert!(
            mine.to_bits() == s[2].to_bits(),
            "sample {k}: atan2({}, {}) = {mine} vs {}",
            s[0],
            s[1],
            s[2]
        );
    }
}

#[test]
fn chain_matches_the_gnuradio_blocks() {
    let Some((dir, iq)) = reference() else { return };
    let demod_ref = read_f32(&dir.join("demod.f32"));
    let deemph_ref = read_f32(&dir.join("deemph.f32"));
    let audio_ref = read_f32(&dir.join("audio16k.f32"));
    let bp_ref = read_f32(&dir.join("bandpass.f32"));
    let n = demod_ref.len().min(iq.len());

    // Stage by stage, so a difference is attributed.
    let mut demod = trunk_fm::QuadratureDemod::new(96_000.0, trunk_fm::MAX_DEV_HZ);
    let mut d = Vec::new();
    demod.process(&iq[..n], &mut d);
    let (e, i) = max_err(&d, &demod_ref);
    eprintln!("demod: max err {e} at {i} of {n}");
    assert!(e <= 1e-5, "demod differs by {e} at {i}");

    let mut de = trunk_fm::Deemphasis::new(96_000.0, TAU_US);
    let mut dd = demod_ref[..n].to_vec();
    de.process(&mut dd);
    let (e, i) = max_err(&dd, &deemph_ref);
    eprintln!("deemph: max err {e} at {i}");
    assert!(e <= 1e-5, "de-emphasis differs by {e} at {i}");

    // The whole chain, in blocks of an odd size to exercise the boundaries.
    let mut fm = FmDemod::new(96_000.0, TAU_US, Decimation::Recorder);
    let mut audio = Audio::default();
    for block in iq[..n].chunks(4_091) {
        fm.process(block, &mut audio);
    }
    assert_eq!(audio.raw.len(), n / 6);
    let (e, i) = max_err(&audio.raw, &audio_ref);
    eprintln!("audio16k: max err {e} at {i} of {}", audio.raw.len());
    assert!(e <= 2e-5, "decimated audio differs by {e} at {i}");
    let (e, i) = max_err(&audio.voice, &bp_ref);
    eprintln!("bandpass: max err {e} at {i}");
    assert!(e <= 2e-5, "band-passed audio differs by {e} at {i}");
}
