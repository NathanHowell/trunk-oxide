use super::*;
use crate::framer::ideal;
use crate::testgen::tdma::{Sent, call};
use crate::testgen::{Channel, Rng, modulate_cqpsk};
use crate::types::Nac;

fn scrambler() -> Scrambler {
    Scrambler::new(0xbee00, 0x5e4, Nac::new(0x5e1).unwrap())
}

const ES: EncryptionSync = EncryptionSync {
    mi: [0; 9],
    algid: 0x80,
    keyid: 0,
};

fn voice_of(out: &[Received<TdmaEvent>]) -> Vec<AmbeParams> {
    out.iter()
        .filter(|r| r.message.channel == 0)
        .flat_map(|r| r.message.frames().iter().map(|f| f.params))
        .collect()
}

#[test]
fn a_clean_call_decodes_completely() {
    let mut rng = Rng::new(11);
    let mut sent = Sent::default();
    #[allow(clippy::cast_possible_truncation)]
    let mut d: Vec<u8> = (0..133).map(|_| (rng.next_u64() & 3) as u8).collect();
    d.extend(call(&mut rng, &scrambler(), &ES, 2, 4, &mut sent));
    let mut dec = TdmaDecoder::new(DemodConfig::phase2(24000.0), Some(scrambler()));
    let mut out = Vec::new();
    dec.process_symbols(&ideal(&d), &mut out);
    // Locked at the first S-ISCH (slot 2); everything after is decoded.
    let got = voice_of(&out);
    let skip = sent.voice.len() - got.len();
    assert!(skip <= 4, "skipped {skip}");
    assert_eq!(got[..], sent.voice[skip..]);
    let s = dec.stats();
    assert_eq!(s.ambe_errors, 0);
    assert_eq!(s.mac_failed, 0);
    assert!(s.mac >= 7, "{s:?}");
    assert_eq!(s.encryption_sync_failed, 0, "{s:?}");
    assert_eq!(s.encryption_sync, 4);
    let pdus: Vec<&[u8]> = out
        .iter()
        .filter(|r| r.message.channel == 0)
        .filter_map(|r| match &r.message.content {
            BurstContent::Mac(Some(p)) => Some(p.value.bytes()),
            _ => None,
        })
        .collect();
    assert_eq!(pdus.len(), sent.pdus.len());
    assert!(pdus.iter().zip(&sent.pdus).all(|(a, b)| *a == &b[..]));
}

#[test]
fn noisy_call_over_the_air() {
    let mut rng = Rng::new(12);
    let mut sent = Sent::default();
    let d = call(&mut rng, &scrambler(), &ES, 6, 12, &mut sent);
    let iq = Channel {
        es_n0_db: 13.0,
        freq_offset_hz: 30.0,
        delay: 0.4,
        clock_ppm: 20.0,
        seed: 5,
    }
    .apply(
        &modulate_cqpsk(&d, 4),
        4,
        crate::symbols::PHASE2_SYMBOL_RATE,
    );
    let cfg = DemodConfig::phase2(24000.0);
    let mut dec = TdmaDecoder::new(cfg, Some(scrambler()));
    let mut out = Vec::new();
    for chunk in iq.chunks(960) {
        dec.process(chunk, &mut out);
    }
    let got = voice_of(&out);
    assert!(
        got.len() + 8 >= sent.voice.len(),
        "{} of {}",
        got.len(),
        sent.voice.len()
    );
    let skip = sent.voice.len() - got.len();
    let right = got
        .iter()
        .zip(&sent.voice[skip..])
        .filter(|(a, b)| a.0[..2] == b.0[..2])
        .count();
    assert!(right * 100 >= got.len() * 97, "{right} of {}", got.len());
    let s = dec.stats();
    assert_eq!(s.framer.losses, 0, "{s:?}");
    assert!(s.mac_failed * 20 <= s.mac, "{s:?}");
}
