//! Score Phase 2 voice decoding on a tapped replay's TDMA `v_*` taps.
//!
//! Each call is decoded three ways:
//! - op25, emulated on its own sliced symbols (`.symbols.f32`): its framer
//!   (S-ISCH within 4 bit errors, ten bursts of grace), its ISCH table and
//!   sync check, the DUID table, hard Golay decoding and its Reed–Solomon
//!   decoders with the shortened symbols erased;
//! - the Rust decoders on the same op25 symbols;
//! - the whole Rust receiver on the conditioned IQ (`.out.cf32`).
//!
//! Only the call's own logical channel is counted, as op25 decodes only it.
//!
//! cargo run --release -p trunk-p25 --example `tdma_golden` -- <tap dir>
//!     [wacn=0xbee00 sysid=0x5e4 nac=0x5e1] [lp=<cutoff>:<transition>|none]
//!     [iq=in] [pre=<cutoff>:<transition>] [agc=<alpha>] [quiet]

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::similar_names
)]

mod front_end;
use front_end::FrontEnd;

use std::collections::BTreeMap;
use std::fs;

use trunk_p25::DemodConfig;
use trunk_p25::control::Received;
use trunk_p25::gf64::Gf64;
use trunk_p25::symbols::slice;
use trunk_p25::tdma::isch::{self, Isch, S_ISCH};
use trunk_p25::tdma::layout::{CHANNEL_OF_SLOT, ESS_A, ESS_B, VOICE_2V, VOICE_4V, payload_dibit};
use trunk_p25::tdma::mac::{self, MacChannel};
use trunk_p25::tdma::{
    AmbeParams, BurstContent, BurstType, MacOpcode, Scrambler, TdmaDecoder, TdmaEvent,
    TdmaRegenerator, ambe, duid,
};
use trunk_p25::types::Nac;

/// What one decoder made of the call's channel.
#[derive(Default, Debug, Clone)]
struct Score {
    bursts: usize,
    voice_bursts: usize,
    frames: usize,
    errors: u64,
    mac_ok: usize,
    mac_failed: usize,
    ptt: usize,
    /// `MAC_PTT` or group voice user messages naming the call's talkgroup.
    tg_match: usize,
    tg_other: usize,
    /// Voice frames needing at most 3 corrections.
    clean_frames: usize,
    ess_ok: usize,
    ess_failed: usize,
    /// Frames op25's vocoder would mute for its running error rate.
    muted: usize,
}

impl Score {
    const fn add(&mut self, o: &Self) {
        self.bursts += o.bursts;
        self.voice_bursts += o.voice_bursts;
        self.frames += o.frames;
        self.errors += o.errors;
        self.mac_ok += o.mac_ok;
        self.mac_failed += o.mac_failed;
        self.ptt += o.ptt;
        self.tg_match += o.tg_match;
        self.tg_other += o.tg_other;
        self.clean_frames += o.clean_frames;
        self.ess_ok += o.ess_ok;
        self.ess_failed += o.ess_failed;
        self.muted += o.muted;
    }
}

/// op25's running error rate (`errs_mp->ER`) and its mute threshold.
#[derive(Default)]
struct ErrorRate(f64);

impl ErrorRate {
    fn frame(&mut self, errors: u32) -> bool {
        self.0 = 0.95f64.mul_add(self.0, 0.001_064 * f64::from(errors));
        self.0 > 0.096
    }
}

/// op25's `p25p2_framer` + `p25p2_sync` + `p25p2_tdma::handle_packet`.
struct Op25 {
    talkgroup: u16,
    scrambler: Scrambler,
    channel: u8,
    accum: u64,
    body: Vec<u8>,
    in_sync: u32,
    frame_start: u64,
    n: u64,
    // p25p2_sync
    confidence: bool,
    slot: usize,
    // p25p2_tdma
    burst_id: i32,
    first_4v: i32,
    ess_b: [Gf64; 16],
    rate: ErrorRate,
    score: Score,
    /// Voice frames by burst start.
    voice: BTreeMap<u64, Vec<AmbeParams>>,
}

impl Op25 {
    fn new(scrambler: Scrambler, channel: u8, talkgroup: u16) -> Self {
        Self {
            talkgroup,
            scrambler,
            channel,
            accum: 0,
            body: Vec::new(),
            in_sync: 0,
            frame_start: 0,
            n: 0,
            confidence: false,
            slot: 0,
            burst_id: -1,
            first_4v: -1,
            ess_b: [Gf64::ZERO; 16],
            rate: ErrorRate::default(),
            score: Score::default(),
            voice: BTreeMap::new(),
        }
    }

    fn push(&mut self, dibit: u8) {
        let i = self.n;
        self.n += 1;
        self.accum = (self.accum << 2) | u64::from(dibit);
        if ((self.accum & ((1 << 40) - 1)) ^ S_ISCH).count_ones() <= 4 {
            self.body = (0..20)
                .map(|k| ((S_ISCH >> (38 - 2 * k)) & 3) as u8)
                .collect();
            self.in_sync = 10;
            self.frame_start = i + 1 - 20;
            return;
        }
        if self.in_sync > 0 {
            if self.body.is_empty() {
                self.frame_start = i;
            }
            self.body.push(dibit);
            if self.body.len() == 180 {
                let frame = std::mem::take(&mut self.body);
                self.in_sync -= 1;
                self.packet(&frame);
            }
        }
    }

    fn packet(&mut self, d: &[u8]) {
        const EXPECTED: [i32; 12] = [0, 1, -2, -2, 4, 5, -2, -2, 8, 9, -2, -2];
        self.slot = (self.slot + 1) % 12;
        let word = d[..20].iter().fold(0u64, |w, &x| (w << 2) | u64::from(x));
        let rc = isch::decode_hard(word);
        let (checkval, info) = match rc {
            Some(Isch::Info(info)) => (
                i32::from(info.location()) * 4 + i32::from(info.channel()),
                true,
            ),
            Some(Isch::Sync) => (-2, false),
            None => (-1, false),
        };
        let last_rc_ok = rc.is_some();
        if EXPECTED[self.slot] != checkval && checkval != -1 {
            self.confidence = false;
        }
        if info {
            self.confidence = true;
            self.slot = usize::try_from(checkval).unwrap_or(0) % 12;
        }
        if !self.confidence {
            return;
        }
        let dw = [20, 57, 142, 179].iter().fold(0u8, |w, &k| (w << 2) | d[k]);
        let Some(kind) = duid::decode_hard(dw) else {
            return;
        };
        if CHANNEL_OF_SLOT[self.slot] != self.channel {
            return;
        }
        self.score.bursts += 1;
        let clear = |p: usize, scrambled: bool| -> u8 {
            let m = if scrambled {
                self.scrambler.payload(self.slot, p)
            } else {
                0
            };
            d[payload_dibit(p)] ^ m
        };
        let llr = |scrambled: bool| -> Vec<f32> {
            (0..156)
                .flat_map(|p| {
                    let x = clear(p, scrambled);
                    [
                        if x & 2 != 0 { -1.0 } else { 1.0 },
                        if x & 1 != 0 { -1.0 } else { 1.0 },
                    ]
                })
                .collect()
        };
        match kind {
            BurstType::Voice4 | BurstType::Voice2 => {
                self.score.voice_bursts += 1;
                let v4 = kind == BurstType::Voice4;
                let mut current = (self.slot >> 1) as i32;
                // track_vb
                self.burst_id = if v4 { (self.burst_id + 1) % 5 } else { 4 };
                if !v4 && last_rc_ok {
                    self.first_4v = (current + 1) % 5;
                }
                if self.first_4v >= 0 && last_rc_ok {
                    if current < self.first_4v {
                        current += 5;
                    }
                    current -= self.first_4v;
                    if current > self.burst_id {
                        self.burst_id = current;
                    }
                }
                let bits = llr(true);
                let starts: &[usize] = if v4 { &VOICE_4V } else { &VOICE_2V };
                let mut frames = Vec::new();
                for &p in starts {
                    let cw: [f32; 72] = std::array::from_fn(|j| bits[2 * p + j]);
                    let (params, errs) = ambe::decode_hard(&cw);
                    self.score.frames += 1;
                    self.score.errors += u64::from(errs);
                    self.score.clean_frames += usize::from(errs <= 3);
                    self.score.muted += usize::from(self.rate.frame(errs));
                    frames.push(params);
                }
                self.voice.insert(self.frame_start, frames);
                let hb = |at: usize| mac::hexbit(&bits[at..at + 6]).0;
                if self.burst_id < 4 {
                    for h in 0..4 {
                        self.ess_b[4 * self.burst_id as usize + h] = hb(2 * ESS_B.start + 6 * h);
                    }
                } else {
                    let a: [Gf64; 28] = std::array::from_fn(|h| hb(2 * ESS_A.start + 6 * h));
                    if mac::op25::decode_ess(&self.ess_b, &a).is_some() {
                        self.score.ess_ok += 1;
                    } else {
                        self.score.ess_failed += 1;
                    }
                }
            }
            BurstType::Facch { scrambled } | BurstType::Sacch { scrambled } => {
                let channel = if matches!(kind, BurstType::Facch { .. }) {
                    MacChannel::Facch
                } else {
                    MacChannel::Sacch
                };
                match mac::op25::decode_acch(channel, &llr(scrambled)) {
                    Some(pdu) => {
                        self.score.mac_ok += 1;
                        if let Some(g) = group_of(&pdu) {
                            self.score.tg_match += usize::from(g == self.talkgroup);
                            self.score.tg_other += usize::from(g != self.talkgroup);
                        }
                        match pdu.opcode() {
                            MacOpcode::Ptt => {
                                self.score.ptt += 1;
                                self.burst_id = -1;
                                self.first_4v =
                                    ((self.slot as i32 >> 1) + i32::from(pdu.offset()) + 1) % 5;
                            }
                            MacOpcode::Active => {
                                // %101 is the sixth burst (BBAC-1 Table
                                // 8-7), a whole voice cycle on, so the
                                // first's place; %110 and %111 are reserved.
                                let o = i32::from(pdu.offset());
                                self.first_4v = if o > 5 { 0 } else { o % 5 };
                            }
                            MacOpcode::Hangtime => self.burst_id = -1,
                            _ => {}
                        }
                    }
                    None => self.score.mac_failed += 1,
                }
            }
            _ => {}
        }
    }
}

/// Score a Rust decoder's output for one channel.
fn score_rust(
    events: &[Received<TdmaEvent>],
    channel: u8,
    talkgroup: u16,
) -> (Score, BTreeMap<u64, Vec<AmbeParams>>) {
    let mut s = Score::default();
    let mut rate = ErrorRate::default();
    let mut voice = BTreeMap::new();
    for e in events
        .iter()
        .map(|r| &r.message)
        .filter(|e| e.channel == channel)
    {
        if matches!(e.content, BurstContent::Unread) {
            continue;
        }
        s.bursts += 1;
        match &e.content {
            BurstContent::Voice4 { .. } | BurstContent::Voice2 { .. } => {
                s.voice_bursts += 1;
                // Regenerated frames reach op25 clean: it mutes none for errors.
                for f in e.frames() {
                    s.frames += 1;
                    s.clean_frames += usize::from(f.errors <= 3);
                    s.errors += u64::from(f.errors);
                    let _ = rate.frame(0);
                }
                voice.insert(e.start, e.frames().iter().map(|f| f.params).collect());
                if let BurstContent::Voice2 { encryption, .. } = &e.content {
                    if encryption.is_some() {
                        s.ess_ok += 1;
                    } else {
                        s.ess_failed += 1;
                    }
                }
            }
            BurstContent::Mac(Some(pdu)) => {
                s.mac_ok += 1;
                s.ptt += usize::from(pdu.value.opcode() == MacOpcode::Ptt);
                if let Some(g) = group_of(&pdu.value) {
                    s.tg_match += usize::from(g == talkgroup);
                    s.tg_other += usize::from(g != talkgroup);
                }
            }
            BurstContent::Mac(None) => s.mac_failed += 1,
            BurstContent::Unread => {}
        }
    }
    (s, voice)
}

/// The talkgroup a MAC PDU names: `MAC_PTT`'s group, or the first group voice
/// channel user message (abbreviated, opcode 0x01) of an `ACTIVE`/`IDLE`/`HANGTIME`.
fn group_of(pdu: &trunk_p25::tdma::MacPdu) -> Option<u16> {
    let b = pdu.bytes();
    match pdu.opcode() {
        MacOpcode::Ptt => pdu.ptt().map(|p| p.group),
        MacOpcode::Active | MacOpcode::Idle | MacOpcode::Hangtime if b[1] == 0x01 => {
            Some(u16::from_be_bytes([b[3], b[4]]))
        }
        _ => None,
    }
}

/// AMBE+2 pitch index b0 (op25 `unpack_b`); 120 and above are not voice.
const fn b0(p: AmbeParams) -> i32 {
    (((p.0[0] >> 5) & 0x78) | ((p.0[3] >> 9) & 7)) as i32
}

#[derive(Default)]
struct Agreement {
    frames: usize,
    differ: usize,
    pitch_rust: usize,
    pitch_op25: usize,
    invalid_rust: usize,
    invalid_op25: usize,
}

/// Where both decoded the same burst, compare their u0/u1 and, for the
/// disagreements, whose pitch is nearer the agreeing neighbours'.
fn agree(
    a: &mut Agreement,
    rust: &BTreeMap<u64, Vec<AmbeParams>>,
    op25: &BTreeMap<u64, Vec<AmbeParams>>,
) {
    let mut pairs = Vec::new();
    for (start, r) in rust {
        // op25's symbol tap is a few symbols late; match within ±8.
        let o = op25.range(start.saturating_sub(8)..start + 8).next();
        if let Some((_, o)) = o
            && o.len() == r.len()
        {
            pairs.extend(r.iter().copied().zip(o.iter().copied()));
        }
    }
    let same = |i: usize| pairs[i].0.0[..2] == pairs[i].1.0[..2];
    for (k, (r, o)) in pairs.iter().enumerate() {
        a.frames += 1;
        if same(k) {
            continue;
        }
        a.differ += 1;
        a.invalid_rust += usize::from(b0(*r) >= 120);
        a.invalid_op25 += usize::from(b0(*o) >= 120);
        let prev = (0..k).rev().find(|&i| same(i) && b0(pairs[i].0) < 120);
        let next = (k + 1..pairs.len()).find(|&i| same(i) && b0(pairs[i].0) < 120);
        if let (Some(p), Some(n)) = (prev, next)
            && n - p <= 6
            && b0(*r) != b0(*o)
        {
            let target = i32::midpoint(b0(pairs[p].0), b0(pairs[n].0));
            let (dr, d_o) = ((b0(*r) - target).abs(), (b0(*o) - target).abs());
            a.pitch_rust += usize::from(dr < d_o);
            a.pitch_op25 += usize::from(d_o < dr);
        }
    }
}

fn read_f32(path: &str) -> Vec<f32> {
    fs::read(path)
        .unwrap()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

fn parse(v: &str) -> u32 {
    v.strip_prefix("0x").map_or_else(
        || v.parse().unwrap(),
        |h| u32::from_str_radix(h, 16).unwrap(),
    )
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("tap dir");
    let (mut wacn, mut sysid, mut nac, mut quiet) = (0xbee00, 0x5e4, 0x5e1, false);
    let mut bw = None;
    let mut lp = None;
    let mut front = FrontEnd::default();
    for a in args {
        if front.parse(&a) {
        } else if let Some(v) = a.strip_prefix("wacn=") {
            wacn = parse(v);
        } else if let Some(v) = a.strip_prefix("sysid=") {
            sysid = parse(v);
        } else if let Some(v) = a.strip_prefix("nac=") {
            nac = parse(v);
        } else if let Some(v) = a.strip_prefix("bw=") {
            bw = Some(v.parse().unwrap());
        } else if let Some(v) = a.strip_prefix("lp=") {
            lp = Some(
                v.split_once(':')
                    .map(|(c, t)| (c.parse().unwrap(), t.parse().unwrap())),
            );
        } else if a == "quiet" {
            quiet = true;
        }
    }
    let scrambler = Scrambler::new(wacn, sysid as u16, Nac::new(nac as u16).expect("nac"));
    let mut names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    names.sort();
    let (mut op25_total, mut sym_total, mut iq_total) =
        (Score::default(), Score::default(), Score::default());
    let mut regen_total = Score::default();
    let (mut agree_sym, mut agree_iq) = (Agreement::default(), Agreement::default());
    let mut calls = 0;
    // The Rust receiver's MAC PDUs on both logical channels, by opcode.
    let (mut by_opcode, mut foreign) = ([0u64; 8], 0);
    for json in names.iter().filter(|p| {
        let n = p.file_name().unwrap().to_string_lossy();
        n.starts_with("v_") && p.extension().is_some_and(|e| e == "json")
    }) {
        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(json).unwrap()).unwrap();
        if meta["tdma"] != true {
            continue;
        }
        calls += 1;
        let channel = meta["slot"].as_u64().unwrap() as u8;
        let tg = meta["talkgroup"].as_u64().unwrap() as u16;
        let base = json.with_extension("");
        let base = base.to_string_lossy();
        let symbols = read_f32(&format!("{base}.symbols.f32"));

        let mut op25 = Op25::new(scrambler.clone(), channel, tg);
        for &s in &symbols {
            op25.push(slice(s));
        }

        let mut dec = TdmaDecoder::new(DemodConfig::phase2(24000.0), Some(scrambler.clone()));
        let mut ev = Vec::new();
        dec.process_symbols(&symbols, &mut ev);
        let (sym, sym_voice) = score_rust(&ev, channel, tg);

        let iq = front.load(&base, meta["out_rate"].as_f64().unwrap());
        let mut cfg = DemodConfig::phase2(meta["out_rate"].as_f64().unwrap());
        if let Some(bw) = bw {
            cfg.timing_bandwidth = bw;
        }
        if let Some(lp) = lp {
            cfg.lowpass = lp;
        }
        let mut dec = TdmaDecoder::new(cfg, Some(scrambler.clone()));
        let mut ev = Vec::new();
        for c in iq.chunks(4096) {
            dec.process(c, &mut ev);
        }
        let (iqs, iq_voice) = score_rust(&ev, channel, tg);
        let st = dec.stats();
        for (t, n) in by_opcode.iter_mut().zip(st.mac_by_opcode) {
            *t += n;
        }
        foreign += st.end_ptt_foreign;

        // op25 itself, on the Rust-regenerated stream.
        let mut regen = TdmaRegenerator::new(cfg, Some(scrambler.clone()));
        let (mut levels, mut rev) = (Vec::new(), Vec::new());
        for c in iq.chunks(4096) {
            regen.process(c, &mut levels, &mut rev);
        }
        regen.flush(&mut levels);
        let mut after = Op25::new(scrambler.clone(), channel, tg);
        for &s in &levels {
            after.push(slice(s));
        }
        regen_total.add(&after.score);

        agree(&mut agree_sym, &sym_voice, &op25.voice);
        agree(&mut agree_iq, &iq_voice, &op25.voice);
        if !quiet {
            println!(
                "{:26} ch {channel}  voice bursts op25 {:3} sym {:3} iq {:3}  mac ok {:2}/{:2}/{:2}  ess {:2}/{:2}/{:2}  errs/frame {:.2}/{:.2}/{:.2}  muted {:3}  snr {:.1} dB  locks {} slips {}",
                json.file_stem().unwrap().to_string_lossy(),
                op25.score.voice_bursts,
                sym.voice_bursts,
                iqs.voice_bursts,
                op25.score.mac_ok,
                sym.mac_ok,
                iqs.mac_ok,
                op25.score.ess_ok,
                sym.ess_ok,
                iqs.ess_ok,
                op25.score.errors as f64 / op25.score.frames.max(1) as f64,
                sym.errors as f64 / sym.frames.max(1) as f64,
                iqs.errors as f64 / iqs.frames.max(1) as f64,
                op25.score.muted,
                st.demod.symbol_snr_db(),
                st.framer.acquisitions,
                st.framer.slips,
            );
        }
        op25_total.add(&op25.score);
        sym_total.add(&sym);
        iq_total.add(&iqs);
    }
    println!();
    println!("TDMA calls {calls}");
    for (name, s) in [
        ("op25", &op25_total),
        ("rust on op25 symbols", &sym_total),
        ("rust on IQ", &iq_total),
        ("op25 after regeneration", &regen_total),
    ] {
        println!(
            "{name:24} bursts {:5}  voice bursts {:5}  frames {:6} (≤3 corrected {:6})  bits corrected {:6} ({:.2}/frame)  MAC ok {:4} failed {:4} (ptt {:3}, talkgroup right {:4} wrong {:2})  ESS ok {:3} failed {:3}  muted by op25 {}",
            s.bursts,
            s.voice_bursts,
            s.frames,
            s.clean_frames,
            s.errors,
            s.errors as f64 / s.frames.max(1) as f64,
            s.mac_ok,
            s.mac_failed,
            s.ptt,
            s.tg_match,
            s.tg_other,
            s.ess_ok,
            s.ess_failed,
            s.muted,
        );
    }
    let opcodes: Vec<String> = (0..8u8)
        .filter(|&b| by_opcode[usize::from(b)] > 0)
        .map(|b| {
            format!(
                "{} {}",
                MacOpcode::from_bits(b).name(),
                by_opcode[usize::from(b)]
            )
        })
        .collect();
    println!(
        "rust on IQ, MAC PDUs (both channels): {}; END_PTT with another system's color code {foreign}",
        opcodes.join(", ")
    );
    for (name, a) in [("op25 symbols", &agree_sym), ("IQ", &agree_iq)] {
        println!(
            "rust ({name}) vs op25 on bursts both decoded: frames {}, u0/u1 differ {} ({:.2}%); pitch nearer neighbours rust {} op25 {}; non-voice pitch rust {} op25 {}",
            a.frames,
            a.differ,
            100.0 * a.differ as f64 / a.frames.max(1) as f64,
            a.pitch_rust,
            a.pitch_op25,
            a.invalid_rust,
            a.invalid_op25
        );
    }
}
