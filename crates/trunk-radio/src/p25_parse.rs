//! P25 control channel messages to trunking messages, in place of
//! trunk-recorder's (deleted) `P25Parser`.
//!
//! Takes trunk-p25's decoded messages as they are
//! ([`P25TrunkParser::parse_message`], on the channelizer's GR thread), or
//! in op25's form, a type and the NAC-prefixed payload, as the `.msgs.jsonl`
//! taps hold them ([`P25TrunkParser::parse`]); both give the same result.
//! Every control channel of a system shares one [`SystemPlans`], so a grant
//! resolves as soon as any of them has heard the plan it names.
//!
//! The output matches what `P25Parser` made of the same messages, field for
//! field, except where that was wrong ([`Divergence`]; its output for the
//! test corpus is kept in `tests/reference.rs`):
//! - grant updates naming two groups on two slots of one TDMA carrier
//!   report both (trunk-recorder compares frequencies and drops the second),
//!   and a second channel of 0xffff means none for Motorola's as for the
//!   standard one;
//! - unit-to-unit grants carry no service options (it reads the channel);
//! - denied or refused affiliations and registrations are not reported as
//!   accepted;
//! - `SCCB_EXP` offers only its transmit channel as a control channel, and an
//!   `SCCB` whose second channel is unknown still offers the first;
//! - network status is reported before the channel plan is known (its
//!   WACN and system ID set up the TDMA scrambling; the channel is unused);
//! - protected, vendor and reserved packets are not read as standard ones,
//!   and every decoded packet yields a message (it drops some opcodes
//!   silently, under-counting control channel traffic);
//! - malformed payloads are invalid rather than read past their end;
//! - a channel unknown to this control channel resolves through the
//!   system's other control channels, and a grant on a channel no plan
//!   covers yet waits for the plan (trunk-recorder sends it on with
//!   frequency 0, which records nothing).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use trunk_p25::trunking::{
    ChannelId, ControlChannelPlans, GroupChannel, Osp, Resolution, Response, ServiceOptions,
    SystemPlans,
};
use trunk_p25::{Message, Tsbk};

use crate::types::{Channel, Options, Patch, TrunkMessage, Trunking, Voice};

/// op25 message types (the frame's DUID).
mod op25 {
    pub const TSDU: i64 = 7;
    pub const PDU: i64 = 12;
    pub const TDULC: i64 = 15;
    /// Not a P25 opcode: what trunk-recorder puts in non-TSBK messages.
    pub const NO_OPCODE: u64 = 255;
}

/// How a message differs from what `P25Parser` would have made of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Divergence {
    SecondSlotUpdate,
    /// A Motorola grant update's second channel is 0xffff, "none".
    NoChannel,
    UnitGrantOptions,
    NotAccepted,
    SecondaryControl,
    EarlyStatus,
    NotStandard,
    Malformed,
    SharedPlan,
    /// A grant held until its channel's plan arrived, or sent on then.
    Deferred,
}

/// Per-system counters for metrics.
#[derive(Clone, Debug, Default)]
pub struct ParseStats {
    pub kinds: HashMap<&'static str, u64>,
    /// Channels resolved through this control channel's own plans.
    pub own_plans: u64,
    /// Channels resolved through another control channel of the system.
    pub shared_plans: u64,
    /// Channels no control channel of the system has a plan for.
    pub unknown_channels: u64,
    /// Invalid or empty messages (op25's timeouts are empty).
    pub invalid: u64,
    pub conflicts: u64,
    /// Grants held until their channel's plan arrived.
    pub deferred: u64,
    /// Grants whose plan never came.
    pub expired: u64,
}

/// A system's parser counters.
#[derive(Clone, Debug)]
pub struct SystemReport {
    pub sys_num: i32,
    pub system: String,
    pub stats: ParseStats,
}

#[derive(Debug, Default)]
struct System {
    name: String,
    plans: SystemPlans,
    stats: ParseStats,
}

static SYSTEMS: Mutex<Vec<(i32, Weak<Mutex<System>>)>> = Mutex::new(Vec::new());

fn system(sys_num: i32, name: &str) -> Arc<Mutex<System>> {
    let mut all = SYSTEMS.lock().unwrap_or_else(PoisonError::into_inner);
    all.retain(|(_, w)| w.strong_count() > 0);
    if let Some(s) = all
        .iter()
        .find(|(n, _)| *n == sys_num)
        .and_then(|(_, w)| w.upgrade())
    {
        return s;
    }
    let s = Arc::new(Mutex::new(System {
        name: name.to_owned(),
        ..System::default()
    }));
    all.push((sys_num, Arc::downgrade(&s)));
    s
}

/// Counters of every system with a live parser.
pub fn parse_stats() -> Vec<SystemReport> {
    let all = SYSTEMS.lock().unwrap_or_else(PoisonError::into_inner);
    all.iter()
        .filter_map(|(n, w)| {
            let s = w.upgrade()?;
            let s = s.lock().unwrap_or_else(PoisonError::into_inner);
            let mut stats = s.stats.clone();
            stats.conflicts = s.plans.conflicts;
            Some(SystemReport {
                sys_num: *n,
                system: s.name.clone(),
                stats,
            })
        })
        .collect()
}

/// One control channel decoder's parser.
pub struct P25TrunkParser {
    sys_num: i32,
    plans: ControlChannelPlans,
    system: Arc<Mutex<System>>,
    /// Grants on channels no plan covers yet, waiting for one.
    pending: VecDeque<Pending>,
    /// Packets parsed, the clock pending grants age by.
    parsed: u64,
}

/// A grant held back until its channel's plan is known.
#[derive(Clone, Copy, Debug)]
struct Pending {
    osp: Osp,
    nac: u16,
    opcode: u64,
    at: u64,
}

/// Grants held at most.
const MAX_PENDING: usize = 64;
/// How long a grant may wait for its plan, in packets parsed (a control
/// channel sends about 40 a second and its plans every second or so).
const MAX_PENDING_AGE: u64 = 400;

pub fn p25_trunk_parser_new(sys_num: i32, system: &str) -> Box<P25TrunkParser> {
    Box::new(P25TrunkParser::new(sys_num, system))
}

impl P25TrunkParser {
    /// A parser for one of system `sys_num`'s control channels.
    pub fn new(sys_num: i32, name: &str) -> Self {
        Self {
            sys_num,
            plans: ControlChannelPlans::default(),
            pending: VecDeque::new(),
            parsed: 0,
            system: system(sys_num, name),
        }
    }

    /// A message from trunk-p25's control channel decoder.
    pub fn parse_message(&mut self, message: &Message) -> Vec<TrunkMessage> {
        self.parse_message_noting(message, &mut |_| {})
    }

    /// [`Self::parse_message`], reporting each way the result departs from
    /// trunk-recorder's.
    pub fn parse_message_noting(
        &mut self,
        message: &Message,
        note: &mut dyn FnMut(Divergence),
    ) -> Vec<TrunkMessage> {
        self.parse_packet(Packet::of_message(message), note)
    }

    /// A message in op25's form: its type (the DUID) and NAC-prefixed body.
    pub fn parse(&mut self, msg_type: i64, payload: &[u8]) -> Vec<TrunkMessage> {
        self.parse_noting(msg_type, payload, &mut |_| {})
    }

    /// [`Self::parse`], reporting each way the result departs from
    /// trunk-recorder's.
    pub fn parse_noting(
        &mut self,
        msg_type: i64,
        payload: &[u8],
        note: &mut dyn FnMut(Divergence),
    ) -> Vec<TrunkMessage> {
        self.parse_packet(Packet::of_op25(msg_type, payload), note)
    }

    fn parse_packet(
        &mut self,
        packet: Packet,
        note: &mut dyn FnMut(Divergence),
    ) -> Vec<TrunkMessage> {
        let system = Arc::clone(&self.system);
        let mut system = system.lock().unwrap_or_else(PoisonError::into_inner);
        let out = match packet {
            Packet::Bare(kind) => vec![message(self.sys_num, kind, 0, op25::NO_OPCODE)],
            Packet::Malformed => {
                note(Divergence::Malformed);
                vec![message(self.sys_num, Trunking::Invalid, 0, op25::NO_OPCODE)]
            }
            Packet::Osp { nac, osp, opcode } => self.parse_osp(&mut system, nac, osp, opcode, note),
        };
        if out.iter().any(|m| m.kind == Trunking::Invalid) {
            system.stats.invalid += 1;
        }
        out
    }

    fn parse_osp(
        &mut self,
        system: &mut System,
        nac: u16,
        osp: Osp,
        opcode: u64,
        note: &mut dyn FnMut(Divergence),
    ) -> Vec<TrunkMessage> {
        *system.stats.kinds.entry(osp.kind()).or_default() += 1;
        self.plans.learn(&osp, &mut system.plans);
        self.parsed += 1;
        let mut out = if self.unplaced(&osp, system) && self.pending.len() < MAX_PENDING {
            // trunk-recorder sends it on with frequency 0, which records
            // nothing; it is sent again once the plan arrives.
            note(Divergence::Deferred);
            system.stats.deferred += 1;
            self.pending.push_back(Pending {
                osp,
                nac,
                opcode,
                at: self.parsed,
            });
            vec![message(self.sys_num, Trunking::Unknown, nac, opcode)]
        } else {
            self.messages(system, &osp, nac, opcode, note)
        };
        if matches!(osp, Osp::IdenUpdate { plan: Some(_), .. }) && !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            for p in pending {
                if self.unplaced(&p.osp, system) {
                    self.pending.push_back(p);
                } else {
                    note(Divergence::Deferred);
                    out.extend(self.messages(system, &p.osp, p.nac, p.opcode, note));
                }
            }
        }
        let now = self.parsed;
        let before = self.pending.len();
        self.pending.retain(|p| now - p.at < MAX_PENDING_AGE);
        system.stats.expired += (before - self.pending.len()) as u64;
        out
    }

    /// Whether `osp` puts traffic on a channel no plan covers.
    fn unplaced(&self, osp: &Osp, system: &System) -> bool {
        osp.traffic_channels()
            .into_iter()
            .flatten()
            .any(|c| self.plans.resolve(c, &system.plans).channel().is_none())
    }

    fn messages(
        &self,
        system: &mut System,
        osp: &Osp,
        nac: u16,
        opcode: u64,
        note: &mut dyn FnMut(Divergence),
    ) -> Vec<TrunkMessage> {
        Context {
            parser: self,
            system,
            nac,
            opcode,
            note,
        }
        .messages(osp)
    }
}

/// What a decoder message is to the parser.
#[derive(Clone, Copy)]
enum Packet {
    /// A message that carries no packet: only its kind is sent on.
    Bare(Trunking),
    /// A packet too short for its kind.
    Malformed,
    Osp {
        nac: u16,
        osp: Osp,
        opcode: u64,
    },
}

impl Packet {
    fn of_message(m: &Message) -> Self {
        match m {
            Message::Tsbk { nac, tsbk, .. } => {
                let osp = Osp::from_tsbk(tsbk);
                Self::Osp {
                    nac: nac.get(),
                    opcode: opcode_of(&osp, tsbk.bytes()[0]),
                    osp,
                }
            }
            Message::Mbt { nac, header, data } if data.len() >= 8 => {
                let osp = Osp::from_mbt(header, data);
                Self::Osp {
                    nac: nac.get(),
                    opcode: opcode_of(&osp, header[7]),
                    osp,
                }
            }
            Message::Mbt { .. } => Self::Malformed,
            Message::Terminator {
                link_control: true, ..
            } => Self::Bare(Trunking::Tdulc),
            Message::Terminator { .. } | Message::Voice { .. } => Self::Bare(Trunking::Unknown),
            // op25 sends a timeout without a body, which trunk-recorder
            // counts as invalid.
            Message::Timeout => Self::Bare(Trunking::Invalid),
        }
    }

    fn of_op25(msg_type: i64, payload: &[u8]) -> Self {
        // trunk-recorder: -2 is a GUI command, -1 a timeout, other negative
        // types are invalid, and a payload must at least hold the NAC.
        match msg_type {
            -2 | -1 => return Self::Bare(Trunking::Unknown),
            t if t < 0 => return Self::Bare(Trunking::Invalid),
            _ => {}
        }
        let Some((nac, body)) = payload.split_first_chunk::<2>() else {
            return Self::Bare(Trunking::Invalid);
        };
        let nac = u16::from_be_bytes(*nac);
        if nac == 0xffff {
            // Phase 2 (op25's TDMA marker).
            return Self::Bare(Trunking::Unknown);
        }
        let (osp, raw_opcode) = match msg_type {
            op25::TSDU => {
                let Ok(b) = <[u8; 10]>::try_from(body) else {
                    return Self::Malformed;
                };
                (Osp::from_tsbk(&Tsbk::new(b)), b[0])
            }
            op25::PDU => match body.split_first_chunk::<12>() {
                Some((header, data)) if data.len() >= 8 => (Osp::from_mbt(header, data), header[7]),
                _ => return Self::Malformed,
            },
            op25::TDULC => return Self::Bare(Trunking::Tdulc),
            _ => return Self::Bare(Trunking::Unknown),
        };
        Self::Osp {
            nac,
            opcode: opcode_of(&osp, raw_opcode),
            osp,
        }
    }
}

/// The opcode trunk-recorder reports: the packet's, from its first byte
/// (a TSBK's, an MBT header's eighth) unless the packet names one itself.
fn opcode_of(osp: &Osp, raw: u8) -> u64 {
    match osp {
        Osp::Other { opcode, .. } | Osp::Protected { opcode, .. } => u64::from(*opcode),
        _ => u64::from(raw & 0x3f),
    }
}

/// A message of `kind` from system `sys_num`.
const fn message(sys_num: i32, kind: Trunking, nac: u16, opcode: u64) -> TrunkMessage {
    TrunkMessage {
        sys_num,
        nac,
        opcode,
        kind,
    }
}

struct Context<'a> {
    parser: &'a P25TrunkParser,
    system: &'a mut System,
    nac: u16,
    opcode: u64,
    note: &'a mut dyn FnMut(Divergence),
}

impl Context<'_> {
    const fn msg(&self, kind: Trunking) -> TrunkMessage {
        message(self.parser.sys_num, kind, self.nac, self.opcode)
    }

    fn resolve(&mut self, id: ChannelId) -> Option<trunk_p25::trunking::Channel> {
        match self.parser.plans.resolve(id, &self.system.plans) {
            Resolution::Own(c) => {
                self.system.stats.own_plans += 1;
                Some(c)
            }
            Resolution::Site(c) | Resolution::System(c) => {
                self.system.stats.shared_plans += 1;
                (self.note)(Divergence::SharedPlan);
                Some(c)
            }
            Resolution::Unknown => {
                self.system.stats.unknown_channels += 1;
                None
            }
        }
    }

    /// A voice message of `kind` on the channel `id` names.
    fn voice(
        &mut self,
        kind: fn(Voice) -> Trunking,
        id: Option<ChannelId>,
        talkgroup: u32,
        source: Option<u32>,
        options: Options,
    ) -> TrunkMessage {
        let channel = id.map_or(Channel::NONE, |id| self.channel(id));
        self.msg(kind(Voice {
            channel,
            talkgroup,
            source,
            options,
        }))
    }

    /// The channel `id` names: its frequency (0 if unknown) and TDMA slot.
    #[allow(clippy::cast_precision_loss)]
    fn channel(&mut self, id: ChannelId) -> Channel {
        self.resolve(id).map_or(Channel::NONE, |c| Channel {
            freq_hz: c.freq_hz as f64,
            slot: c.slot,
        })
    }

    #[allow(clippy::too_many_lines)] // one arm per packet kind
    fn messages(&mut self, osp: &Osp) -> Vec<TrunkMessage> {
        use Trunking as K;
        let unknown = vec![self.msg(K::Unknown)];
        match *osp {
            Osp::GroupVoiceGrant {
                options,
                channel,
                group,
                source,
            } => vec![self.voice(
                K::Grant,
                Some(channel),
                u32::from(group),
                Some(source),
                options_of(options),
            )],
            Osp::MotPatchGrant {
                options,
                channel,
                supergroup,
                source,
            } => vec![self.voice(
                K::Grant,
                Some(channel),
                u32::from(supergroup),
                source,
                options_of(options),
            )],
            Osp::GroupVoiceGrantUpdate { first, second }
            | Osp::MotPatchGrantUpdate { first, second } => self.updates(first, second),
            Osp::GroupVoiceGrantUpdateExplicit {
                options, tx, group, ..
            } => vec![self.voice(
                K::Update,
                Some(tx),
                u32::from(group),
                None,
                Options {
                    emergency: options.emergency(),
                    encrypted: options.encrypted(),
                    ..Options::default()
                },
            )],
            Osp::UnitVoiceGrant {
                options,
                channel,
                target,
                source,
            } => {
                if options == ServiceOptions::NONE {
                    (self.note)(Divergence::UnitGrantOptions);
                }
                vec![self.voice(
                    K::UnitGrant,
                    Some(channel),
                    target,
                    Some(source),
                    options_of(options),
                )]
            }
            Osp::UnitAnswerRequest {
                options,
                target,
                source,
            } => vec![self.voice(
                K::AnswerRequest,
                None,
                target,
                Some(source),
                options_of(options),
            )],
            Osp::UnitVoiceGrantUpdate {
                channel,
                target,
                source,
            } => vec![self.voice(
                K::UnitUpdate,
                Some(channel),
                target,
                Some(source),
                Options::default(),
            )],
            Osp::DataGrant {
                options,
                tx,
                target,
                ..
            } => {
                // trunk-recorder reads the frequency but not the slot.
                let freq_hz = self.channel(tx).freq_hz;
                vec![self.msg(K::DataGrant {
                    freq_hz,
                    unit: target,
                    options: Options {
                        priority: 0,
                        ..options_of(options)
                    },
                })]
            }
            Osp::Acknowledge { group, target } => vec![self.msg(K::Acknowledge {
                group,
                unit: target,
            })],
            Osp::GroupAffiliation {
                response,
                group,
                target,
                ..
            } => self.accepted(
                response,
                unknown,
                K::Affiliation {
                    group,
                    unit: target,
                },
            ),
            Osp::LocationRegistration {
                response,
                group,
                target,
            } => self.accepted(
                response,
                unknown,
                K::Location {
                    group,
                    unit: target,
                },
            ),
            Osp::UnitRegistration {
                response,
                source_id,
                ..
            } => self.accepted(response, unknown, K::Registration { unit: source_id }),
            Osp::UnitDeregistration { source } => {
                vec![self.msg(K::Deregistration { unit: source })]
            }
            Osp::SecondaryControlExplicit { tx, .. } => {
                (self.note)(Divergence::SecondaryControl);
                self.control_channels(&[tx]).unwrap_or(unknown)
            }
            Osp::SecondaryControl { channels, .. } => {
                let known = channels.map(|c| self.parser.plans.resolve(c, &self.system.plans));
                if known.iter().any(|r| r.channel().is_none()) {
                    (self.note)(Divergence::SecondaryControl);
                }
                self.control_channels(&channels).unwrap_or(unknown)
            }
            Osp::RfssStatus { site, .. } => vec![self.msg(K::Site {
                sys_id: site.system,
                rfss: site.rfss,
                site: site.site,
            })],
            Osp::NetworkStatus {
                wacn,
                system,
                channel,
            } => {
                // trunk-recorder leaves the slot out of status messages.
                let freq_hz = self.channel(channel).freq_hz;
                if freq_hz == 0.0 {
                    (self.note)(Divergence::EarlyStatus);
                }
                vec![self.msg(K::Network {
                    freq_hz,
                    wacn,
                    sys_id: system,
                })]
            }
            Osp::MotPatchAdd { supergroup, groups } => {
                vec![self.msg(K::PatchAdd(Patch { supergroup, groups }))]
            }
            Osp::HarrisPatch {
                activate,
                supergroup,
                group,
            } => {
                let patch = Patch {
                    supergroup,
                    groups: [group; 3],
                };
                vec![self.msg(if activate {
                    K::PatchAdd(patch)
                } else {
                    K::PatchDelete(patch)
                })]
            }
            Osp::Protected { .. } | Osp::Other { .. } => {
                (self.note)(Divergence::NotStandard);
                unknown
            }
            Osp::AdjacentStatus { .. } | Osp::IdenUpdate { .. } => unknown,
        }
    }

    fn updates(&mut self, first: GroupChannel, second: GroupChannel) -> Vec<TrunkMessage> {
        let mut out = vec![self.update(first)];
        if second.channel.get() == 0xffff {
            (self.note)(Divergence::NoChannel);
        } else if second != first {
            let m = self.update(second);
            // Whole hertz, or both 0.
            #[allow(clippy::float_cmp)]
            if let (Trunking::Update(a), Trunking::Update(b)) = (&m.kind, &out[0].kind)
                && a.channel.freq_hz == b.channel.freq_hz
            {
                (self.note)(Divergence::SecondSlotUpdate);
            }
            out.push(m);
        }
        out
    }

    fn update(&mut self, g: GroupChannel) -> TrunkMessage {
        self.voice(
            Trunking::Update,
            Some(g.channel),
            u32::from(g.group),
            None,
            Options::default(),
        )
    }

    fn accepted(
        &mut self,
        response: Response,
        otherwise: Vec<TrunkMessage>,
        kind: Trunking,
    ) -> Vec<TrunkMessage> {
        if response == Response::Accept {
            vec![self.msg(kind)]
        } else {
            (self.note)(Divergence::NotAccepted);
            otherwise
        }
    }

    /// A control channel message per known channel, or `None` if none is.
    fn control_channels(&mut self, channels: &[ChannelId]) -> Option<Vec<TrunkMessage>> {
        let out: Vec<_> = channels
            .iter()
            .filter_map(|&c| {
                let freq_hz = self.channel(c).freq_hz;
                (freq_hz != 0.0).then(|| self.msg(Trunking::ControlChannel { freq_hz }))
            })
            .collect();
        (!out.is_empty()).then_some(out)
    }
}

const fn options_of(o: ServiceOptions) -> Options {
    Options {
        emergency: o.emergency(),
        encrypted: o.encrypted(),
        duplex: o.duplex(),
        packet: o.packet(),
        priority: o.priority(),
    }
}

#[cfg(test)]
mod tests;
