use super::*;
use crate::demod::DemodConfig;
use crate::framer::{Duid, ideal};
use crate::tdma::mac::MacChannel;
use crate::tdma::{Scrambler, TdmaDecoder, op25_view};
use crate::testgen::{self, Rng};
use crate::types::Nac;
use crate::voice::signalling::encode;
use crate::voice::{Checked, EncryptionSync, ImbeFrame, ImbeParams, VoiceDecoder};

const NAC: Nac = Nac::new(0x38d).unwrap();
/// Group voice user: talkgroup 865, unit 0x123456.
const LC: [u8; 9] = [0x00, 0x00, 0x00, 0x00, 0x03, 0x61, 0x12, 0x34, 0x56];

/// A phase 1 call: header (`algid`), `superframes` of voice, TDU.
fn phase1_call(rng: &mut Rng, algid: u8, superframes: usize) -> (Vec<u8>, Vec<ImbeParams>) {
    let mut sent = Vec::new();
    let mut d = testgen::hdu(NAC, &[0; 9], 0, algid, 0x1234, 865);
    for _ in 0..superframes {
        for duid in [Duid::Ldu1, Duid::Ldu2] {
            let params: [ImbeParams; 9] = std::array::from_fn(|_| testgen::random_imbe(rng));
            sent.extend(params);
            let signalling = if duid == Duid::Ldu1 {
                encode::link_control_ldu1(&LC)
            } else {
                encode::encryption_sync(&[0; 9], algid, 0x1234)
            };
            d.extend(testgen::ldu(NAC, duid, &params, &signalling));
        }
    }
    d.extend(testgen::tdu(NAC));
    d.extend(vec![0u8; 200]);
    (d, sent)
}

fn assemble_phase1(dibits: &[u8]) -> Vec<CallEvent> {
    let mut dec = VoiceDecoder::new(DemodConfig::new(24000.0, crate::Modulation::C4fm));
    let mut events = Vec::new();
    dec.process_symbols(&ideal(dibits), &mut events);
    let mut a = Phase1Assembler::new();
    let mut out = Vec::new();
    for e in &events {
        a.push(e, &mut out);
    }
    out
}

fn frames(out: &[CallEvent]) -> Vec<Vocode> {
    out.iter()
        .filter_map(|e| match e {
            CallEvent::Frame(v) => Some(*v),
            _ => None,
        })
        .collect()
}

/// op25's inputs of the phase 1 frames: `u`, e0, et.
fn op25_inputs(out: &[CallEvent]) -> Vec<([u32; 8], u32, u32)> {
    frames(out)
        .iter()
        .filter_map(|v| match v {
            Vocode::Imbe { frame, e0, et } => Some((frame.params.to_op25(), *e0, *et)),
            _ => None,
        })
        .collect()
}

/// The phase 2 frames as op25 receives them: dibits, encrypted.
fn op25_dibits(out: &[CallEvent]) -> Vec<([u8; 36], bool)> {
    frames(out)
        .iter()
        .filter_map(|v| match v {
            Vocode::Ambe {
                dibits, encrypted, ..
            } => Some((*dibits, *encrypted)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_clear_phase1_call_vocodes_every_frame_as_sent() {
    let mut rng = Rng::new(1);
    let (d, sent) = phase1_call(&mut rng, ALGID_CLEAR, 3);
    let out = assemble_phase1(&d);
    let want: Vec<([u32; 8], u32, u32)> = sent.iter().map(|p| (p.to_op25(), 0, 0)).collect();
    assert_eq!(op25_inputs(&out), want);
    assert_eq!(out[0], CallEvent::Group(865));
    assert!(out.contains(&CallEvent::Source(0x12_3456)));
    assert_eq!(out.last(), Some(&CallEvent::Terminator(Ended::Tdu)));
    assert!(
        !out.iter()
            .any(|e| matches!(e, CallEvent::Encryption { .. }))
    );
}

#[test]
fn an_encrypted_phase1_call_is_silent_until_its_terminator() {
    let mut rng = Rng::new(2);
    let (mut d, _) = phase1_call(&mut rng, 0x84, 2);
    // A clear call after it on the same channel.
    let (clear, sent) = phase1_call(&mut rng, ALGID_CLEAR, 1);
    d.extend(clear);
    let out = assemble_phase1(&d);
    let f = frames(&out);
    assert_eq!(f.len(), 36 + 18);
    assert!(f[..36].iter().all(|v| *v == Vocode::Silence));
    assert!(matches!(f[36], Vocode::Imbe { frame, e0: 0, et: 0 } if frame.params == sent[0]));
    let reports = out
        .iter()
        .filter(|e| {
            **e == CallEvent::Encryption {
                encrypted: true,
                algid: 0x84,
                keyid: 0x1234,
            }
        })
        .count();
    assert_eq!(reports, 36, "one per encrypted frame, as op25 sends them");
}

fn ldu1(lc: [u8; 9]) -> Received<VoiceEvent> {
    let f = ImbeFrame {
        params: ImbeParams::default(),
        errors: 0,
        errors_c0: 0,
        reliability: 1.0,
        unprotected: 1.0,
    };
    Received {
        symbol: 4800,
        message: VoiceEvent::Ldu1 {
            nac: NAC,
            frames: [f; 9],
            link_control: Some(Checked {
                value: LinkControl(lc),
                corrected: 0,
                erased: 0,
            }),
        },
    }
}

fn aliases(out: &[CallEvent]) -> Vec<&Alias> {
    out.iter()
        .filter_map(|e| match e {
            CallEvent::Alias(a) => Some(a),
            _ => None,
        })
        .collect()
}

#[test]
fn a_motorola_link_control_alias_comes_out_with_its_last_block() {
    let mut a = Phase1Assembler::new();
    let mut out = Vec::new();
    // Header: two blocks, sequence 3.
    let header = [0x15, 0x90, 0, 0, 0x02, 0, 0, 0x30, 0];
    let block = |n: u8| [0x17, 0x90, n, 0x30, 0xaa, 0xbb, 0xcc, 0xdd, n];
    a.push(&ldu1(header), &mut out);
    a.push(&ldu1(block(1)), &mut out);
    assert_eq!(aliases(&out), Vec::<&Alias>::new());
    // A block of another sequence is not this alias's.
    a.push(&ldu1([0x17, 0x90, 2, 0x40, 0, 0, 0, 0, 0]), &mut out);
    assert_eq!(aliases(&out), Vec::<&Alias>::new());
    a.push(&ldu1(block(2)), &mut out);
    assert_eq!(
        aliases(&out),
        [&Alias::Motorola {
            phase2: false,
            messages: 2,
            blocks: vec![
                (0, header.to_vec()),
                (1, block(1).to_vec()),
                (2, block(2).to_vec())
            ],
        }]
    );
}

#[test]
fn a_harris_link_control_alias_names_the_unit_last_heard() {
    let mut a = Phase1Assembler::new();
    let mut out = Vec::new();
    a.push(&ldu1(LC), &mut out);
    a.push(
        &ldu1([0x32, 0xa4, b'A', b'B', b'C', b'D', b'E', b'F', b'G']),
        &mut out,
    );
    a.push(
        &ldu1([0x33, 0xa4, b'H', b'I', b'J', b'K', b'L', b'M', b'N']),
        &mut out,
    );
    assert_eq!(
        aliases(&out),
        [&Alias::Harris {
            phase2: false,
            data: b"ABCDEFGHIJKLMN".to_vec(),
            source: 0x12_3456,
            group: 865,
            cache_age_ms: 0,
        }]
    );
}

#[test]
fn protected_or_implicit_link_control_names_nobody() {
    let mut a = Phase1Assembler::new();
    let mut out = Vec::new();
    let mut protected = LC;
    protected[0] |= 0x80;
    a.push(&ldu1(protected), &mut out);
    let mut implicit = LC;
    implicit[0] |= 0x40;
    a.push(&ldu1(implicit), &mut out);
    assert!(
        !out.iter()
            .any(|e| matches!(e, CallEvent::Source(_) | CallEvent::Group(_)))
    );
}

// ── Phase 2 ──

fn scrambler() -> Scrambler {
    Scrambler::new(0xbee00, 0x5e4, Nac::new(0x5e1).unwrap())
}

#[test]
fn a_clean_phase2_call_vocodes_its_channels_frames_as_sent() {
    let mut rng = Rng::new(3);
    let mut sent = testgen::tdma::Sent::default();
    let es = EncryptionSync {
        mi: [0; 9],
        algid: ALGID_CLEAR,
        keyid: 0,
    };
    let d = testgen::tdma::call(&mut rng, &scrambler(), &es, 0, 4, &mut sent);
    let mut dec = TdmaDecoder::new(DemodConfig::phase2(24000.0), Some(scrambler()));
    let mut a = Phase2Assembler::new(0);
    let mut out = Vec::new();
    let s = scrambler();
    dec.process_symbols_with(&ideal(&d), &mut |burst, event, symbol| {
        let view = op25_view(burst, Some(&s), &event);
        a.push(
            &Received {
                symbol,
                message: event,
            },
            &view,
            &mut out,
        );
    });
    let got = op25_dibits(&out);
    // Every frame of channel 0, re-encoded (all trusted on a clean stream).
    let want: Vec<([u8; 36], bool)> = sent
        .voice
        .iter()
        .map(|p| {
            let cw = p.encode();
            (
                std::array::from_fn(|k| u8::from(cw.bit(2 * k)) << 1 | u8::from(cw.bit(2 * k + 1))),
                false,
            )
        })
        .collect();
    // The framer acquires on the first burst; the last are still in it.
    let skip = want.iter().position(|w| *w == got[0]).unwrap();
    assert!(skip <= 4, "{skip}");
    assert!(
        got.len() + skip + 18 >= want.len(),
        "{} of {}",
        got.len(),
        want.len()
    );
    assert_eq!(got[..], want[skip..skip + got.len()]);
    assert!(out.contains(&CallEvent::Encryption {
        encrypted: false,
        algid: ALGID_CLEAR,
        keyid: 0
    }));
}

fn pdu(channel: MacChannel, bytes: &[u8]) -> MacPdu {
    MacPdu::new(channel, bytes)
}

#[test]
fn mac_ptt_names_the_unit_and_its_encryption_holds_to_the_hangtime() {
    let mut a = Phase2Assembler::new(0);
    let mut out = Vec::new();
    let mut ptt = [0u8; 21];
    ptt[0] = 0x20; // MAC_PTT
    ptt[10] = 0x84;
    ptt[11..13].copy_from_slice(&0xa100u16.to_be_bytes());
    ptt[13..16].copy_from_slice(&[0x12, 0x34, 0x56]);
    ptt[16..18].copy_from_slice(&0x0abcu16.to_be_bytes());
    a.mac(&pdu(MacChannel::Sacch, &ptt), 0, &mut out);
    assert_eq!(
        out,
        [CallEvent::Source(0x12_3456), CallEvent::Group(0x0abc)]
    );
    out.clear();
    a.voice(&[[0; 36]], &[], &mut out);
    assert_eq!(
        out,
        [CallEvent::Frame(Vocode::Ambe {
            dibits: [0; 36],
            frame: None,
            encrypted: true
        })]
    );
    out.clear();
    let mut hangtime = [0u8; 21];
    hangtime[0] = 0xc0; // MAC_HANGTIME
    a.mac(&pdu(MacChannel::Sacch, &hangtime), 0, &mut out);
    assert_eq!(out, [CallEvent::Terminator(Ended::Hangtime)]);
    assert!(!a.encrypted());
}

/// An encrypted `MAC_PTT` for group 0x0abc, then `MAC_END_PTT` with
/// `color_code`, on a system whose color code is 0x5e1.
fn end_ptt_events(color_code: u16) -> (Vec<CallEvent>, bool) {
    let mut a = Phase2Assembler::new(0);
    a.start_call(0, Nac::new(0x5e1));
    let mut ptt = [0u8; 21];
    ptt[0] = 0x20; // MAC_PTT
    ptt[10] = 0x84;
    ptt[16..18].copy_from_slice(&0x0abcu16.to_be_bytes());
    let mut out = Vec::new();
    a.mac(&pdu(MacChannel::Sacch, &ptt), 0, &mut out);
    out.clear();
    let mut end = [0u8; 21];
    end[0] = 0x40; // MAC_END_PTT
    end[1..3].copy_from_slice(&color_code.to_be_bytes());
    end[16..18].copy_from_slice(&0x0abcu16.to_be_bytes());
    a.mac(&pdu(MacChannel::Sacch, &end), 0, &mut out);
    (out, a.encrypted())
}

#[test]
fn mac_end_ptt_ends_the_transmission_on_a_transmission_trunked_system() {
    let (out, encrypted) = end_ptt_events(0x5e1);
    assert_eq!(
        out,
        [
            CallEvent::Group(0x0abc),
            CallEvent::Terminator(Ended::EndPtt)
        ]
    );
    assert!(!encrypted);
}

#[test]
fn mac_end_ptt_with_another_systems_color_code_is_not_ours() {
    let (out, encrypted) = end_ptt_events(0x123);
    assert_eq!(out, []);
    assert!(encrypted);
}

#[test]
fn mac_messages_name_units_and_carry_harris_aliases() {
    let mut a = Phase2Assembler::new(0);
    let mut out = Vec::new();
    let mut active = [0u8; 21];
    active[0] = 0x80; // MAC_ACTIVE
    // Group voice channel user (abbreviated): group 0x0123, unit 0x00beef.
    active[1..8].copy_from_slice(&[0x01, 0x00, 0x01, 0x23, 0x00, 0xbe, 0xef]);
    // Harris talker alias, 6 bytes.
    active[8..14].copy_from_slice(&[0xa8, 0xa4, 0x06, b'X', b'Y', b'Z']);
    a.mac(&pdu(MacChannel::Sacch, &active), 1000, &mut out);
    assert_eq!(
        out,
        [
            CallEvent::Source(0xbeef),
            CallEvent::Group(0x0123),
            CallEvent::Alias(Alias::Harris {
                phase2: true,
                data: vec![0xa8, 0xa4, 0x06, b'X', b'Y', b'Z'],
                source: 0xbeef,
                group: 0x0123,
                cache_age_ms: 0,
            })
        ]
    );
}

#[test]
fn the_other_channels_bursts_are_ignored() {
    let mut rng = Rng::new(4);
    let mut sent = testgen::tdma::Sent::default();
    let es = EncryptionSync {
        mi: [0; 9],
        algid: ALGID_CLEAR,
        keyid: 0,
    };
    let d = testgen::tdma::call(&mut rng, &scrambler(), &es, 0, 2, &mut sent);
    let mut dec = TdmaDecoder::new(DemodConfig::phase2(24000.0), Some(scrambler()));
    // Channel 1 carries only idle FACCH in the test call.
    let mut a = Phase2Assembler::new(1);
    let mut out = Vec::new();
    let s = scrambler();
    dec.process_symbols_with(&ideal(&d), &mut |burst, event, symbol| {
        let view = op25_view(burst, Some(&s), &event);
        a.push(
            &Received {
                symbol,
                message: event,
            },
            &view,
            &mut out,
        );
    });
    assert_eq!(frames(&out), []);
}
