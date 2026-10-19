use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::*;

mod flat;
mod reference;

const TSDU: i64 = op25::TSDU;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

// svrcs (NAC 5E1, WACN BEE00, system 5E4) from the 2026-10-17 capture.
const IDEN_TDMA: &str = "05e1330033a5806409157562"; // iden 3, 2 slots, 762.00625 MHz
const IDEN_FDMA: &str = "05e13d00432270640b256e84"; // iden 4
const GRANT: &str = "05e100004436d1070acb6654"; // tg 0x70a on 03-1745
const NET_STATUS: &str = "05e13b0000bee005e4167070";
const RFSS_STATUS: &str = "05e13a000035e40101167070";

use flat::{TrMessageType, TrTrunkMessage};

/// The flat form of each, as the reference corpus is recorded.
fn flat(v: Vec<TrunkMessage>) -> Vec<TrTrunkMessage> {
    v.iter().map(TrTrunkMessage::from).collect()
}

fn parse(p: &mut P25TrunkParser, s: &str) -> Vec<TrTrunkMessage> {
    flat(p.parse(TSDU, &hex(s)))
}

/// A parser for a system no other test uses (plans are shared per system).
fn parser() -> P25TrunkParser {
    use std::sync::atomic::{AtomicI32, Ordering};
    static NEXT: AtomicI32 = AtomicI32::new(1000);
    P25TrunkParser::new(NEXT.fetch_add(1, Ordering::Relaxed), "test")
}

fn grant_freq(slot_number: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let f = (0x0915_7562 * 5 + 12_500 * (slot_number / 2)) as f64;
    f
}

#[test]
fn grants_wait_for_their_plan() {
    let mut p = parser();
    // Held: trunk-recorder would send it with frequency 0.
    let before = parse(&mut p, GRANT);
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].message_type, TrMessageType::Unknown);

    // Sent with the plan, then like any other.
    let with_plan = parse(&mut p, IDEN_TDMA);
    assert_eq!(
        with_plan.iter().map(|m| m.message_type).collect::<Vec<_>>(),
        [TrMessageType::Unknown, TrMessageType::Grant]
    );
    assert_eq!(with_plan[1], parse(&mut p, GRANT)[0]);
    let m = &with_plan[1];
    assert_eq!(m.freq, grant_freq(0x6d1));
    assert_eq!((m.phase2_tdma, m.tdma_slot), (true, 1));
    assert_eq!(
        (m.talkgroup, m.source, m.encrypted),
        (0x70a, 0xcb_6654, true)
    );
    assert_eq!((m.nac, m.opcode), (0x5e1, 0));
}

#[test]
fn held_grants_expire() {
    let mut p = parser();
    parse(&mut p, GRANT);
    for _ in 0..MAX_PENDING_AGE {
        parse(&mut p, NET_STATUS);
    }
    assert_eq!(parse(&mut p, IDEN_TDMA).len(), 1);
    let stats = parse_stats()
        .into_iter()
        .find(|r| r.sys_num == p.sys_num)
        .unwrap()
        .stats;
    assert_eq!((stats.deferred, stats.expired), (1, 1));
}

#[test]
fn both_slots_of_a_carrier_are_updated() {
    let mut p = parser();
    parse(&mut p, IDEN_TDMA);
    let out = parse(&mut p, "05e1020037ec047f37ed0480");
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].freq, out[1].freq);
    assert_eq!(
        out.iter()
            .map(|m| (m.talkgroup, m.tdma_slot))
            .collect::<Vec<_>>(),
        [(0x47f, 0), (0x480, 1)]
    );
    // One group named twice is one update.
    assert_eq!(parse(&mut p, "05e1020037ec047f37ec047f").len(), 1);
}

#[test]
fn plans_are_shared_within_a_system_only() {
    let mut a = parser();
    let mut b = P25TrunkParser::new(a.sys_num, "test");
    let mut other = parser();
    parse(&mut a, RFSS_STATUS);
    parse(&mut a, IDEN_TDMA);
    assert_eq!(parse(&mut b, GRANT)[0].freq, grant_freq(0x6d1));
    assert_eq!(
        parse(&mut other, GRANT)[0].message_type,
        TrMessageType::Unknown
    );
    let stats: BTreeMap<_, _> = parse_stats()
        .into_iter()
        .map(|r| (r.sys_num, r.stats))
        .collect();
    assert_eq!(stats[&a.sys_num].shared_plans, 1);
    assert_eq!(stats[&other.sys_num].deferred, 1);
}

#[test]
fn denied_affiliations_are_not_affiliations() {
    let mut p = parser();
    assert_eq!(
        parse(&mut p, "05e128000009f609f7cb0afe")[0].message_type,
        TrMessageType::Affiliation
    );
    assert_eq!(
        parse(&mut p, "05e128000209f609f7cb0afe")[0].message_type,
        TrMessageType::Unknown
    );
}

#[test]
fn network_status_comes_before_the_plan() {
    let mut p = parser();
    let m = &parse(&mut p, NET_STATUS)[0];
    assert_eq!(m.message_type, TrMessageType::Status);
    assert_eq!(
        (m.wacn, m.sys_id, m.nac, m.freq),
        (0xbee00, 0x5e4, 0x5e1, 0.0)
    );
}

#[test]
fn every_packet_yields_a_message() {
    let mut p = parser();
    // Motorola 0x0b, which trunk-recorder drops.
    let m = parse(&mut p, "05e10b900000000000001670");
    assert_eq!(
        (m.len(), m[0].message_type, m[0].opcode),
        (1, TrMessageType::Unknown, 0x0b)
    );
}

#[test]
fn malformed_and_bare_messages() {
    let mut p = parser();
    let t = |p: &mut P25TrunkParser, ty, payload: &[u8]| {
        flat(p.parse(ty, payload))
            .into_iter()
            .map(|m| m.message_type)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        t(&mut p, TSDU, &hex("05e13b00")),
        [TrMessageType::InvalidCcMessage]
    );
    assert_eq!(
        t(&mut p, op25::PDU, &hex("05e1aa")),
        [TrMessageType::InvalidCcMessage]
    );
    // op25's timeout: a type and nothing else.
    assert_eq!(t(&mut p, 0xffff, &[]), [TrMessageType::InvalidCcMessage]);
    assert_eq!(t(&mut p, op25::TDULC, &hex("05e1")), [TrMessageType::Tdulc]);
    assert_eq!(t(&mut p, TSDU, &hex("ffff00")), [TrMessageType::Unknown]);
}

// ── Against trunk-recorder's parser ──

#[derive(Debug, Default)]
struct Tally {
    same: u64,
    diverged: BTreeMap<String, u64>,
    unexplained: Vec<String>,
}

fn without_meta(mut v: Vec<TrTrunkMessage>) -> Vec<TrTrunkMessage> {
    for m in &mut v {
        m.meta.clear();
    }
    v
}

/// Whether the noted divergences account for how `rust` differs from `cpp`.
fn explains(notes: &[Divergence], rust: &[TrTrunkMessage], cpp: &[TrTrunkMessage]) -> bool {
    let kinds = |v: &[TrTrunkMessage]| v.iter().map(|m| m.message_type).collect::<Vec<_>>();
    notes.iter().any(|n| match n {
        Divergence::SecondSlotUpdate => rust.len() == cpp.len() + 1 && rust[..cpp.len()] == *cpp,
        Divergence::NoChannel => cpp.len() == rust.len() + 1 && cpp[..rust.len()] == *rust,
        Divergence::UnitGrantOptions => {
            rust.len() == 1
                && cpp.len() == 1
                && TrTrunkMessage {
                    emergency: false,
                    encrypted: false,
                    duplex: false,
                    mode: false,
                    priority: 0,
                    ..cpp[0].clone()
                } == rust[0]
        }
        Divergence::NotAccepted | Divergence::NotStandard => {
            kinds(rust) == [TrMessageType::Unknown]
        }
        Divergence::EarlyStatus => {
            kinds(cpp) == [TrMessageType::Unknown] && kinds(rust) == [TrMessageType::Status]
        }
        Divergence::SecondaryControl => rust
            .iter()
            .all(|m| m.message_type == TrMessageType::Unknown || cpp.contains(m)),
        Divergence::Malformed => kinds(rust) == [TrMessageType::InvalidCcMessage],
        Divergence::SharedPlan => true,
        Divergence::Deferred => {
            kinds(rust) == [TrMessageType::Unknown]
                || (rust.len() > cpp.len() && rust[..cpp.len()] == *cpp)
        }
    })
}

fn compare(
    rust: &mut P25TrunkParser,
    cpp: Vec<TrTrunkMessage>,
    msg_type: i64,
    payload: &[u8],
    tally: &mut Tally,
) {
    let mut notes = Vec::new();
    let r = without_meta(flat(
        rust.parse_noting(msg_type, payload, &mut |d| notes.push(d)),
    ));
    let c = cpp;
    if r == c {
        tally.same += 1;
    } else if explains(&notes, &r, &c) {
        for n in notes {
            *tally.diverged.entry(format!("{n:?}")).or_default() += 1;
        }
    } else {
        let h = payload.iter().fold(String::new(), |mut h, b| {
            let _ = write!(h, "{b:02x}");
            h
        });
        tally.unexplained.push(format!(
            "type {msg_type} {h}: notes {notes:?}\n  rust {r:?}\n  cpp  {c:?}"
        ));
    }
}

/// One of every message the capture has, plus edge cases: before the plans
/// are known, then after.
const CORPUS: &[(i64, &str)] = &[
    (TSDU, GRANT),
    (TSDU, NET_STATUS),
    (TSDU, RFSS_STATUS),
    (TSDU, "05e1020036fc070a37c105ab"),
    (TSDU, IDEN_TDMA),
    (TSDU, IDEN_FDMA),
    (TSDU, GRANT),
    (TSDU, NET_STATUS),
    (TSDU, "05e1020037ec047f37ec047f"),
    (TSDU, "05e1020037ec047f37ed0480"),
    (TSDU, "05e1020036fc070a37c105ab"),
    (TSDU, "05e180901290129012901290"),
    (TSDU, "05e182904037ed1290ccc671"),
    (TSDU, "05e1839037ed129037ed1290"),
    (TSDU, "05e1839037ed1290ffff1290"),
    (TSDU, "05e105904080c00000000800"),
    (TSDU, "05e189901180000000000000"),
    (TSDU, "05e10b900000000000001670"),
    (TSDU, "05e114000016a0ffffc7e52f"),
    (TSDU, "05e11500010000ffffcccf96"),
    (TSDU, "05e1160000c016a0ffff0001"),
    (TSDU, "05e12000a300fffffdc9535a"),
    (TSDU, "05e128000009f609f7cb0afe"),
    (TSDU, "05e128000209f609f7cb0afe"),
    (TSDU, "05e12a000000c92062fffffd"),
    (TSDU, "05e12b000012a90101ccd03c"),
    (TSDU, "05e12c0005e4cb0afecb0afe"),
    (TSDU, "05e12f0000bee005e4c95523"),
    (TSDU, "05e1300000042a3551823ca2"),
    (TSDU, "05e139000101165804168804"),
    (TSDU, "05e13c000035e40103161070"),
    (TSDU, "05e1040047ec000123000456"),
    (TSDU, "05e1060047ec000123000456"),
    (TSDU, "05e104900016a0ffffc7e52f"),
    (TSDU, "05e1440000c016a0ffff0001"),
    (0xffff, ""),
    (op25::TDULC, "05e1"),
    (TSDU, "ffff00"),
];

/// The corpus against what trunk-recorder's parser made of it.
#[test]
fn matches_trunk_recorder_except_where_noted() {
    let mut rust = parser();
    let mut tally = Tally::default();
    let cpp = reference::reference(rust.sys_num);
    assert_eq!(cpp.len(), CORPUS.len());
    for (&(ty, s), cpp) in CORPUS.iter().zip(cpp) {
        compare(&mut rust, cpp, ty, &hex(s), &mut tally);
    }
    assert!(
        tally.unexplained.is_empty(),
        "{}",
        tally.unexplained.join("\n")
    );
    assert!(tally.same > 20, "{tally:?}");
    for d in [
        "SecondSlotUpdate",
        "NotAccepted",
        "EarlyStatus",
        "UnitGrantOptions",
        "Deferred",
    ] {
        assert!(tally.diverged.contains_key(d), "{d}: {tally:?}");
    }
}

// ── trunk-p25's messages, typed and in op25's form ──

/// The trunk-p25 message whose op25 form this is, if it has one.
fn typed(ty: i64, payload: &[u8]) -> Option<Message> {
    use trunk_p25::{Nac, Recovery};
    if ty == 0xffff {
        return payload.is_empty().then_some(Message::Timeout);
    }
    let (nac, body) = payload.split_first_chunk::<2>()?;
    let nac = Nac::new(u16::from_be_bytes(*nac))?;
    let m = match ty {
        TSDU => Message::Tsbk {
            nac,
            tsbk: Tsbk::new(body.try_into().ok()?),
            recovery: Recovery::Clean,
        },
        op25::PDU => {
            let (header, data) = body.split_first_chunk::<12>()?;
            Message::Mbt {
                nac,
                header: *header,
                data: data.to_vec(),
            }
        }
        3 | op25::TDULC if body.is_empty() => Message::Terminator {
            nac,
            link_control: ty == op25::TDULC,
        },
        _ => return None,
    };
    assert_eq!((m.op25_type(), m.op25_payload()), (ty, payload.to_vec()));
    Some(m)
}

/// Parses `messages` both ways, each with a parser of its own system, and
/// returns how many there were.
fn typed_matches_op25(messages: impl IntoIterator<Item = Message>) -> usize {
    let (mut wire, mut direct) = (parser(), parser());
    let mut n = 0;
    for m in messages {
        let mut d = without_meta(flat(direct.parse_message(&m)));
        for x in &mut d {
            x.sys_num = wire.sys_num;
        }
        assert_eq!(
            without_meta(flat(wire.parse(m.op25_type(), &m.op25_payload()))),
            d,
            "{m:?}"
        );
        n += 1;
    }
    n
}

#[test]
fn typed_messages_parse_as_their_op25_form() {
    let corpus: Vec<_> = CORPUS
        .iter()
        .filter_map(|&(ty, s)| typed(ty, &hex(s)))
        .collect();
    // All but the TDMA marker, which trunk-p25 never sends.
    assert_eq!(corpus.len(), CORPUS.len() - 1);
    let messages: Vec<_> = corpus
        .into_iter()
        .chain([
            Message::Timeout,
            Message::Voice {
                nac: trunk_p25::Nac::new(0x5e1).unwrap(),
                duid: trunk_p25::Duid::Ldu1,
            },
            // An MBT too short for its data.
            typed(op25::PDU, &hex("05e1b0000000000000000000ffffffff")).unwrap(),
        ])
        .collect();
    assert_eq!(typed_matches_op25(messages), CORPUS.len() + 2);
}

/// Every message trunk-p25 decodes from a capture's control channel taps
/// (`*.out.cf32`): `TRUNK_P25_TAPS=<dir> cargo test -p trunk-recorder-sys
/// --release --lib typed_capture -- --ignored --nocapture`.
#[test]
#[ignore = "needs TRUNK_P25_TAPS"]
fn typed_capture_parses_as_its_op25_form() {
    use num_complex::Complex32;
    use trunk_p25::{ControlChannelDecoder, DecoderConfig, DemodConfig, Modulation};
    let dir = std::env::var("TRUNK_P25_TAPS").expect("TRUNK_P25_TAPS");
    let mut total = 0;
    for e in std::fs::read_dir(&dir).unwrap() {
        let path = e.unwrap().path();
        if !path.to_string_lossy().ends_with(".json") {
            continue;
        }
        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        if meta["kind"] != "control_channel" {
            continue;
        }
        let base = path.with_extension("");
        let Ok(raw) = std::fs::read(format!("{}.out.cf32", base.display())) else {
            continue;
        };
        let iq: Vec<Complex32> = raw
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| {
                Complex32::new(
                    f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                    f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                )
            })
            .collect();
        let modulation = if meta["modulation"] == "qpsk" {
            Modulation::Cqpsk
        } else {
            Modulation::C4fm
        };
        let rate = meta["out_rate"].as_f64().unwrap();
        let mut decoder =
            ControlChannelDecoder::new(DecoderConfig::new(DemodConfig::new(rate, modulation)));
        let mut out = Vec::new();
        decoder.process(&iq, &mut out);
        let n = typed_matches_op25(out.into_iter().map(|r| r.message));
        println!("{}: {n} messages", base.display());
        total += n;
    }
    println!("total {total}");
    assert!(total > 0);
}
