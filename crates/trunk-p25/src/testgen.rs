//! Synthetic P25 signals for tests and benches: frame encoders, CQPSK and
//! C4FM modulators, and channel impairments (noise, frequency offset, timing).

use std::f64::consts::PI;

use num_complex::Complex32;

use crate::bch;
use crate::crc;
use crate::framer::{
    Duid, HDU_DIBITS, LDU_DIBITS, NID_DIBITS, TDU_DIBITS, TDULC_DIBITS, TSDU_TRIPLE_DIBITS,
    nid_word,
};
use crate::symbols::{LEVELS, STATUS_PERIOD, sync_dibits};
use crate::trellis;
use crate::types::Nac;

/// xorshift64* with Box–Muller Gaussians: deterministic, dependency free.
pub struct Rng(u64);

impl Rng {
    pub const fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    pub const fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn below(&mut self, n: usize) -> usize {
        ((self.uniform() * n as f64) as usize).min(n - 1)
    }

    pub fn gauss(&mut self) -> f64 {
        let u = self.uniform().max(1e-300);
        let v = self.uniform();
        (-2.0 * u.ln()).sqrt() * (2.0 * PI * v).cos()
    }
}

/// A TSBK payload (10 bytes) with its CRC appended.
pub fn tsbk_block(payload: &[u8; 10], last: bool) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[..10].copy_from_slice(payload);
    if last {
        b[0] |= 0x80;
    } else {
        b[0] &= 0x7f;
    }
    let c = crc::crc16(&b[..10]);
    b[10..].copy_from_slice(&c.to_be_bytes());
    b
}

/// Insert status dibits (every 36th, counting from the first sync dibit) into
/// a frame's data dibits, padding with idle status to `total` dibits.
pub fn with_status(data: &[u8], total: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(total);
    let mut it = data.iter();
    while out.len() < total {
        if (out.len() + 1).is_multiple_of(STATUS_PERIOD) {
            out.push(1);
        } else {
            out.push(it.next().copied().unwrap_or(0));
        }
    }
    out
}

fn push_u64_dibits(out: &mut Vec<u8>, word: u64, ndibits: usize) {
    for i in (0..ndibits).rev() {
        #[allow(clippy::cast_possible_truncation)]
        out.push(((word >> (2 * i)) & 3) as u8);
    }
}

/// Sync + NID dibits (data only, no status).
pub fn header(nac: Nac, duid: Duid) -> Vec<u8> {
    let mut d = sync_dibits().to_vec();
    push_u64_dibits(&mut d, nid_word(nac, duid), NID_DIBITS);
    d
}

/// A TSDU carrying `tsbks` (1–3), the last flagged, always padded to the
/// three-TSBK length the framer expects (on air, shorter TSDUs end sooner).
pub fn tsdu(nac: Nac, tsbks: &[[u8; 10]]) -> Vec<u8> {
    let mut d = header(nac, Duid::Tsdu);
    for (i, t) in tsbks.iter().enumerate() {
        d.extend_from_slice(&trellis::encode(&tsbk_block(t, i + 1 == tsbks.len())));
    }
    with_status(&d, TSDU_TRIPLE_DIBITS)
}

fn dibits_of(bits: &[bool]) -> Vec<u8> {
    bits.chunks(2)
        .map(|p| (u8::from(p[0]) << 1) | u8::from(p.get(1).copied().unwrap_or(false)))
        .collect()
}

fn header_bits(nac: Nac, duid: Duid) -> Vec<bool> {
    header(nac, duid)
        .iter()
        .flat_map(|&d| [d & 2 != 0, d & 1 != 0])
        .collect()
}

/// Random IMBE parameters.
pub fn random_imbe(rng: &mut Rng) -> crate::voice::ImbeParams {
    const WIDTHS: [u32; 8] = [12, 12, 12, 12, 11, 11, 11, 7];
    #[allow(clippy::cast_possible_truncation)]
    crate::voice::ImbeParams(std::array::from_fn(|i| {
        (rng.next_u64() & ((1 << WIDTHS[i]) - 1)) as u16
    }))
}

/// An LDU1 or LDU2 (864 dibits): nine IMBE frames and the 240 signalling
/// bits (link control or encryption sync, already encoded).
pub fn ldu(
    nac: Nac,
    duid: Duid,
    frames: &[crate::voice::ImbeParams; 9],
    signalling: &[bool],
) -> Vec<u8> {
    use crate::voice::layout::{IMBE_BASE, IMBE_BITS, LDU_DATA_BITS, imbe_bit, signalling_bit};
    let mut bits = vec![false; LDU_DATA_BITS];
    bits[..112].copy_from_slice(&header_bits(nac, duid));
    for (i, f) in frames.iter().enumerate() {
        let cw = f.encode();
        for j in 0..IMBE_BITS {
            bits[imbe_bit(IMBE_BASE[i], j)] = cw.bit(j);
        }
    }
    for (j, &b) in signalling.iter().enumerate() {
        bits[signalling_bit(j)] = b;
    }
    with_status(&dibits_of(&bits), LDU_DIBITS)
}

/// A header data unit.
pub fn hdu(nac: Nac, mi: &[u8; 9], mfid: u8, algid: u8, keyid: u16, talkgroup: u16) -> Vec<u8> {
    let mut bits = header_bits(nac, Duid::Hdu);
    bits.extend(crate::voice::signalling::encode::header(
        mi, mfid, algid, keyid, talkgroup,
    ));
    with_status(&dibits_of(&bits), HDU_DIBITS)
}

/// A terminator with link control.
pub fn tdulc(nac: Nac, lc: &[u8; 9]) -> Vec<u8> {
    let mut bits = header_bits(nac, Duid::TduLc);
    bits.extend(crate::voice::signalling::encode::link_control_tdulc(lc));
    with_status(&dibits_of(&bits), TDULC_DIBITS)
}

/// A frame with only a header (TDU).
pub fn tdu(nac: Nac) -> Vec<u8> {
    with_status(&header(nac, Duid::Tdu), TDU_DIBITS)
}

/// Random TSBK payloads.
pub fn random_tsbks(rng: &mut Rng, n: usize) -> Vec<[u8; 10]> {
    (0..n)
        .map(|_| {
            let mut p = [0u8; 10];
            for x in &mut p {
                #[allow(clippy::cast_possible_truncation)]
                {
                    *x = rng.next_u64() as u8;
                }
            }
            p[0] &= 0x3f;
            p
        })
        .collect()
}

/// Raised-cosine (`root == false`) or root-raised-cosine pulse, `span`
/// symbols long, unit energy-ish (peak 1 for RC).
#[allow(clippy::cast_precision_loss)]
pub fn pulse(sps: usize, alpha: f64, span: usize, root: bool) -> Vec<f64> {
    let n = sps * span + 1;
    let mid = (n / 2) as f64;
    let taps: Vec<f64> = (0..n)
        .map(|i| {
            let t = (i as f64 - mid) / sps as f64;
            if root { rrc(t, alpha) } else { rc(t, alpha) }
        })
        .collect();
    let norm: f64 = if root {
        taps.iter().map(|x| x * x).sum::<f64>().sqrt() / (sps as f64).sqrt()
    } else {
        1.0
    };
    taps.iter().map(|x| x / norm).collect()
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

fn rc(t: f64, a: f64) -> f64 {
    let d = (2.0 * a * t).mul_add(-(2.0 * a * t), 1.0);
    if d.abs() < 1e-9 {
        PI / 4.0 * sinc(1.0 / (2.0 * a))
    } else {
        sinc(t) * (PI * a * t).cos() / d
    }
}

fn rrc(t: f64, a: f64) -> f64 {
    if t.abs() < 1e-12 {
        return 1.0 - a + 4.0 * a / PI;
    }
    if (t.abs() - 1.0 / (4.0 * a)).abs() < 1e-9 {
        return a / 2f64.sqrt()
            * (1.0 - 2.0 / PI).mul_add(
                (PI / (4.0 * a)).cos(),
                (1.0 + 2.0 / PI) * (PI / (4.0 * a)).sin(),
            );
    }
    let num = (4.0 * a * t).mul_add((PI * t * (1.0 + a)).cos(), (PI * t * (1.0 - a)).sin());
    let den = PI * t * (4.0 * a * t).mul_add(-(4.0 * a * t), 1.0);
    num / den
}

fn convolve(x: &[Complex32], taps: &[f64]) -> Vec<Complex32> {
    let half = taps.len() / 2;
    (0..x.len())
        .map(|n| {
            let mut acc = Complex32::new(0.0, 0.0);
            for (k, &t) in taps.iter().enumerate() {
                if let Some(i) = (n + half).checked_sub(k)
                    && i < x.len()
                {
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        acc += x[i] * t as f32;
                    }
                }
            }
            acc
        })
        .collect()
}

/// CQPSK (π/4-DQPSK) at `sps` samples per symbol: each dibit is a phase step
/// of its level × 45°, shaped with a root-raised-cosine (α = 0.2).
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn modulate_cqpsk(dibits: &[u8], sps: usize) -> Vec<Complex32> {
    let mut x = vec![Complex32::new(0.0, 0.0); dibits.len() * sps];
    let mut phase = 0.0f64;
    for (k, &d) in dibits.iter().enumerate() {
        phase += f64::from(LEVELS[usize::from(d)]) * PI / 4.0;
        x[k * sps] = Complex32::from_polar(1.0, phase as f32) * sps as f32;
    }
    let shaped = convolve(&x, &pulse(sps, 0.2, 8, true));
    normalize(shaped)
}

/// C4FM: deviation level × 600 Hz, raised-cosine shaped frequency pulse.
#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn modulate_c4fm(dibits: &[u8], sps: usize) -> Vec<Complex32> {
    let fs = crate::symbols::SYMBOL_RATE * sps as f64;
    let mut f = vec![Complex32::new(0.0, 0.0); dibits.len() * sps];
    for (k, &d) in dibits.iter().enumerate() {
        f[k * sps] = Complex32::new(LEVELS[usize::from(d)] * 600.0, 0.0);
    }
    let freq = convolve(&f, &c4fm_pulse(sps, 0.2, 16));
    let mut phase = 0.0f64;
    freq.iter()
        .map(|v| {
            phase += 2.0 * PI * f64::from(v.re) / fs;
            Complex32::from_polar(1.0, phase as f32)
        })
        .collect()
}

/// The C4FM frequency pulse (TIA-102.BAAA): raised cosine cascaded with the
/// πfT/sin(πfT) shaping filter, so that integrating the frequency over one
/// symbol period recovers the deviation without inter-symbol interference.
/// Built by frequency sampling.
#[allow(clippy::cast_precision_loss)]
fn c4fm_pulse(sps: usize, alpha: f64, span: usize) -> Vec<f64> {
    let n = sps * span + 1;
    let mid = (n / 2) as f64;
    let len = 4096usize;
    // Spectrum in cycles per symbol, sampled finely; real and even, so the
    // impulse response is a cosine sum.
    let spectrum = |f: f64| -> f64 {
        let f = f.abs();
        let (lo, hi) = ((1.0 - alpha) / 2.0, f64::midpoint(1.0, alpha));
        let rc = if f <= lo {
            1.0
        } else if f <= hi {
            f64::midpoint(1.0, (PI / alpha * (f - lo)).cos())
        } else {
            0.0
        };
        let x = PI * f;
        if x < 1e-9 { rc } else { rc * x / x.sin() }
    };
    let df = sps as f64 / len as f64;
    (0..n)
        .map(|i| {
            let t = (i as f64 - mid) / sps as f64;
            let mut acc = spectrum(0.0);
            for k in 1..len / 2 {
                let f = k as f64 * df;
                acc = f64::mul_add(2.0 * spectrum(f), (2.0 * PI * f * t).cos(), acc);
            }
            acc * df
        })
        .collect()
}

fn normalize(x: Vec<Complex32>) -> Vec<Complex32> {
    #[allow(clippy::cast_precision_loss)]
    let rms = (x.iter().map(Complex32::norm_sqr).sum::<f32>() / x.len().max(1) as f32).sqrt();
    x.into_iter().map(|v| v / rms.max(1e-12)).collect()
}

/// Channel impairments applied to a unit-power signal.
pub struct Channel {
    /// Symbol energy to noise density, dB.
    pub es_n0_db: f64,
    pub freq_offset_hz: f64,
    /// Fractional-sample delay at the start, in samples.
    pub delay: f64,
    /// Sample clock error, parts per million.
    pub clock_ppm: f64,
    pub seed: u64,
}

impl Channel {
    pub const fn clean() -> Self {
        Self {
            es_n0_db: 200.0,
            freq_offset_hz: 0.0,
            delay: 0.0,
            clock_ppm: 0.0,
            seed: 1,
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::many_single_char_names
    )]
    pub fn apply(&self, x: &[Complex32], sps: usize) -> Vec<Complex32> {
        let fs = crate::symbols::SYMBOL_RATE * sps as f64;
        let mut rng = Rng::new(self.seed);
        // Noise per complex sample: Es/N0 with Es = sps × unit sample power.
        let n0 = sps as f64 / 10f64.powf(self.es_n0_db / 10.0);
        let sigma = (n0 / 2.0).sqrt();
        let step = self.clock_ppm.mul_add(1e-6, 1.0);
        let mut out = Vec::with_capacity(x.len());
        let count = ((x.len() as f64 - 2.0 - self.delay) / step).ceil().max(0.0) as usize;
        for n in 0..count {
            let t = (n as f64).mul_add(step, self.delay);
            let i = t.floor() as usize;
            let mu = (t - t.floor()) as f32;
            let y = [x[i.saturating_sub(1)], x[i], x[i + 1], x[i + 2]];
            let v = crate::demod::cubic(&y, mu);
            let rot = 2.0 * PI * self.freq_offset_hz * n as f64 / fs;
            let noise = Complex32::new((sigma * rng.gauss()) as f32, (sigma * rng.gauss()) as f32);
            out.push(v * Complex32::from_polar(1.0, rot as f32) + noise);
        }
        out
    }
}

/// Info word of a NID, for tests that poke at BCH directly.
pub const fn nid_info(nac: Nac, duid: Duid) -> u16 {
    bch::info(nid_word(nac, duid) >> 1)
}

/// Phase 2 test signals.
pub mod tdma {
    use super::Rng;
    use crate::tdma::isch::{Isch, IschInfo};
    use crate::tdma::layout::{
        BURSTS_PER_SUPERFRAME, DUID_AT, ESS_A, ESS_B, PAYLOAD_BITS, VOICE_2V, VOICE_4V,
        is_sync_slot, payload_dibit,
    };
    use crate::tdma::mac::{self, MacChannel};
    use crate::tdma::{AmbeParams, BurstType, Scrambler};
    use crate::voice::EncryptionSync;

    pub fn random_ambe(rng: &mut Rng) -> AmbeParams {
        #[allow(clippy::cast_possible_truncation)]
        AmbeParams(std::array::from_fn(|i| {
            (rng.next_u64() & ((1 << [12, 12, 11, 14][i]) - 1)) as u16
        }))
    }

    /// The ISCH a slot carries (flag and count zero).
    pub const fn isch_for(slot: usize) -> Isch {
        if is_sync_slot(slot) {
            Isch::Sync
        } else {
            #[allow(clippy::cast_possible_truncation)]
            Isch::Info(IschInfo::new(((slot & 3) << 5 | (slot / 4) << 3) as u8))
        }
    }

    /// One 180-dibit burst from its 312 clear payload bits.
    pub fn burst(slot: usize, kind: BurstType, payload: &[bool], scrambler: &Scrambler) -> Vec<u8> {
        let mut d = vec![0u8; 180];
        let cw = isch_for(slot).codeword();
        for (i, x) in d.iter_mut().take(20).enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            {
                *x = ((cw >> (38 - 2 * i)) & 3) as u8;
            }
        }
        let duid = kind.codeword();
        for (i, &at) in DUID_AT.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            {
                d[at] = ((duid >> (6 - 2 * i)) & 3) as u8;
            }
        }
        for p in 0..PAYLOAD_BITS / 2 {
            let clear = u8::from(payload[2 * p]) << 1 | u8::from(payload[2 * p + 1]);
            let m = if kind.scrambled() {
                scrambler.payload(slot, p)
            } else {
                0
            };
            d[payload_dibit(p)] = clear ^ m;
        }
        d
    }

    fn put(payload: &mut [bool], p: usize, bits: &[bool]) {
        payload[2 * p..2 * p + bits.len()].copy_from_slice(bits);
    }

    /// What a test call sent on logical channel 0.
    #[derive(Debug, Default)]
    pub struct Sent {
        pub voice: Vec<AmbeParams>,
        pub pdus: Vec<Vec<u8>>,
    }

    /// `superframes` of a call on logical channel 0 (four 4V and a 2V per
    /// superframe, encryption sync `es`, a `MAC_ACTIVE` SACCH in slot 11),
    /// with idle FACCH on channel 1, starting at superframe slot `first`.
    pub fn call(
        rng: &mut Rng,
        scrambler: &Scrambler,
        es: &EncryptionSync,
        first: usize,
        superframes: usize,
        sent: &mut Sent,
    ) -> Vec<u8> {
        let ess = mac::encode::ess(es);
        let mut d = Vec::new();
        for k in 0..superframes * BURSTS_PER_SUPERFRAME {
            let slot = (first + k) % BURSTS_PER_SUPERFRAME;
            let channel = crate::tdma::layout::CHANNEL_OF_SLOT[slot];
            let mut payload = vec![false; PAYLOAD_BITS];
            let kind = if slot >= 10 {
                let mut pdu = [0u8; 21];
                pdu[0] = if channel == 0 { 0x80 } else { 0x60 }; // MAC_ACTIVE / MAC_IDLE
                #[allow(clippy::cast_possible_truncation)]
                for b in &mut pdu[1..] {
                    *b = rng.next_u64() as u8;
                }
                if channel == 0 {
                    sent.pdus.push(pdu.to_vec());
                }
                payload = mac::encode::acch(MacChannel::Sacch, &pdu);
                BurstType::Sacch { scrambled: true }
            } else if channel == 1 {
                let mut pdu = [0u8; 18];
                pdu[0] = 0x60;
                payload = mac::encode::acch(MacChannel::Facch, &pdu);
                BurstType::Facch { scrambled: true }
            } else {
                let position = slot / 2;
                let voice: &[usize] = if position < 4 { &VOICE_4V } else { &VOICE_2V };
                for &p in voice {
                    let a = random_ambe(rng);
                    sent.voice.push(a);
                    let cw = a.encode();
                    let bits: Vec<bool> = (0..72).map(|j| cw.bit(j)).collect();
                    put(&mut payload, p, &bits);
                }
                if position < 4 {
                    let part = &ess[4 * position..4 * position + 4];
                    put(&mut payload, ESS_B.start, &mac::encode::hexbit_bits(part));
                    BurstType::Voice4
                } else {
                    put(
                        &mut payload,
                        ESS_A.start,
                        &mac::encode::hexbit_bits(&ess[16..]),
                    );
                    BurstType::Voice2
                }
            };
            d.extend(burst(slot, kind, &payload, scrambler));
        }
        d
    }
}
