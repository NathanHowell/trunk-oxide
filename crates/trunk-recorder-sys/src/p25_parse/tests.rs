use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::*;
use crate::ffi;

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

fn parse(p: &mut P25TrunkParser, s: &str) -> Vec<TrTrunkMessage> {
    p.parse(TSDU, &hex(s))
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
        p.parse(ty, payload)
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
    cpp: std::pin::Pin<&mut ffi::P25ReferenceParser>,
    msg_type: i64,
    payload: &[u8],
    tally: &mut Tally,
) {
    let mut notes = Vec::new();
    let r = without_meta(rust.parse_noting(msg_type, payload, &mut |d| notes.push(d)));
    let c = without_meta(ffi::tr_p25_reference_parse(cpp, msg_type, payload));
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

/// One of every message the capture has, plus edge cases, against
/// trunk-recorder's parser.
#[test]
fn matches_trunk_recorder_except_where_noted() {
    let mut rust = parser();
    let mut cpp = ffi::tr_p25_reference_parser_new(rust.sys_num);
    let mut tally = Tally::default();
    let mut run = |ty: i64, payload: Vec<u8>, tally: &mut Tally| {
        compare(&mut rust, cpp.pin_mut(), ty, &payload, tally);
    };
    // Before the plans are known, then after.
    let corpus = [
        GRANT,
        NET_STATUS,
        RFSS_STATUS,
        "05e1020036fc070a37c105ab",
        IDEN_TDMA,
        IDEN_FDMA,
        GRANT,
        NET_STATUS,
        "05e1020037ec047f37ec047f",
        "05e1020037ec047f37ed0480",
        "05e1020036fc070a37c105ab",
        "05e180901290129012901290",
        "05e182904037ed1290ccc671",
        "05e1839037ed129037ed1290",
        "05e1839037ed1290ffff1290",
        "05e105904080c00000000800",
        "05e189901180000000000000",
        "05e10b900000000000001670",
        "05e114000016a0ffffc7e52f",
        "05e11500010000ffffcccf96",
        "05e1160000c016a0ffff0001",
        "05e12000a300fffffdc9535a",
        "05e128000009f609f7cb0afe",
        "05e128000209f609f7cb0afe",
        "05e12a000000c92062fffffd",
        "05e12b000012a90101ccd03c",
        "05e12c0005e4cb0afecb0afe",
        "05e12f0000bee005e4c95523",
        "05e1300000042a3551823ca2",
        "05e139000101165804168804",
        "05e13c000035e40103161070",
        "05e1040047ec000123000456",
        "05e1060047ec000123000456",
        "05e104900016a0ffffc7e52f",
        "05e1440000c016a0ffff0001",
    ];
    for s in corpus {
        run(TSDU, hex(s), &mut tally);
    }
    run(0xffff, Vec::new(), &mut tally);
    run(op25::TDULC, hex("05e1"), &mut tally);
    run(TSDU, hex("ffff00"), &mut tally);
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

/// The whole of a capture's control channel taps (`*.msgs.jsonl`):
/// `TRUNK_P25_MSGS=<dir> cargo test -p trunk-recorder-sys -- --ignored`.
#[test]
#[ignore = "needs TRUNK_P25_MSGS"]
fn capture_matches_trunk_recorder_except_where_noted() {
    let dir = std::env::var("TRUNK_P25_MSGS").expect("TRUNK_P25_MSGS");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".msgs.jsonl"))
        .collect();
    files.sort();
    let mut total = Tally::default();
    for path in files {
        let mut rust = parser();
        let mut cpp = ffi::tr_p25_reference_parser_new(rust.sys_num);
        let mut tally = Tally::default();
        for line in std::fs::read_to_string(&path).unwrap().lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            let ty = v["type"].as_i64().unwrap();
            let payload = hex(v["hex"].as_str().unwrap());
            compare(&mut rust, cpp.pin_mut(), ty, &payload, &mut tally);
        }
        println!(
            "{}: same {} diverged {:?} unexplained {}",
            path.file_name().unwrap().to_string_lossy(),
            tally.same,
            tally.diverged,
            tally.unexplained.len()
        );
        total.same += tally.same;
        for (k, v) in tally.diverged {
            *total.diverged.entry(k).or_default() += v;
        }
        total.unexplained.extend(tally.unexplained);
    }
    println!("total: same {} diverged {:?}", total.same, total.diverged);
    let shown: Vec<_> = total.unexplained.iter().take(20).cloned().collect();
    assert!(
        total.unexplained.is_empty(),
        "{} unexplained:\n{}",
        total.unexplained.len(),
        shown.join("\n")
    );
}
