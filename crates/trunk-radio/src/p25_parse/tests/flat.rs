//! trunk-recorder's flat trunk message, kept only to compare the parser
//! with what `P25Parser` made of the same packets (`reference.rs`).

#![allow(dead_code)] // recorded values use every field

use crate::types::{Patch, TrunkMessage, Trunking, Voice};

/// What a trunking message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TrMessageType {
    Grant = 0,
    Status = 1,
    Update = 2,
    ControlChannel = 3,
    Registration = 4,
    Deregistration = 5,
    Affiliation = 6,
    Sysid = 7,
    Acknowledge = 8,
    Location = 9,
    PatchAdd = 10,
    PatchDelete = 11,
    DataGrant = 12,
    UuAnsReq = 13,
    UuVGrant = 14,
    UuVUpdate = 15,
    InvalidCcMessage = 16,
    Tdulc = 17,
    Unknown = 99,
}

impl TrMessageType {
    pub const fn repr(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrPatchData {
    pub sg: u64,
    pub ga1: u64,
    pub ga2: u64,
    pub ga3: u64,
}

/// A parsed control-channel message in trunk-recorder's flat form, which
/// the reference corpus is recorded in.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct TrTrunkMessage {
    pub message_type: TrMessageType,
    pub meta: String,
    pub freq: f64,
    pub talkgroup: i64,
    pub encrypted: bool,
    pub emergency: bool,
    pub duplex: bool,
    pub mode: bool,
    pub priority: i32,
    pub tdma_slot: i32,
    pub phase2_tdma: bool,
    pub source: i64,
    pub sys_num: i32,
    pub sys_id: u64,
    pub sys_rfss: i32,
    pub sys_site_id: i32,
    pub nac: u64,
    pub wacn: u64,
    pub patch_data: TrPatchData,
    pub opcode: u64,
}

impl From<&TrunkMessage> for TrTrunkMessage {
    fn from(m: &TrunkMessage) -> Self {
        use Trunking as K;
        let mut out = Self {
            message_type: TrMessageType::Unknown,
            meta: String::new(),
            freq: 0.0,
            talkgroup: 0,
            encrypted: false,
            emergency: false,
            duplex: false,
            mode: false,
            priority: 0,
            tdma_slot: 0,
            phase2_tdma: false,
            source: -1,
            sys_num: m.sys_num,
            sys_id: 0,
            sys_rfss: 0,
            sys_site_id: 0,
            nac: u64::from(m.nac),
            wacn: 0,
            patch_data: TrPatchData::default(),
            opcode: m.opcode,
        };
        let voice = |out: &mut Self, t: TrMessageType, v: &Voice| {
            out.message_type = t;
            out.freq = v.channel.freq_hz;
            out.phase2_tdma = v.channel.slot.is_some();
            out.tdma_slot = v.channel.slot.map_or(0, i32::from);
            out.talkgroup = i64::from(v.talkgroup);
            out.source = v.source.map_or(-1, i64::from);
            out.emergency = v.options.emergency;
            out.encrypted = v.options.encrypted;
            out.duplex = v.options.duplex;
            out.mode = v.options.packet;
            out.priority = i32::from(v.options.priority);
        };
        let unit = |out: &mut Self, t: TrMessageType, group: u16, unit: u32| {
            out.message_type = t;
            out.talkgroup = i64::from(group);
            out.source = i64::from(unit);
        };
        let patch = |out: &mut Self, t: TrMessageType, p: &Patch| {
            out.message_type = t;
            out.patch_data = TrPatchData {
                sg: u64::from(p.supergroup),
                ga1: u64::from(p.groups[0]),
                ga2: u64::from(p.groups[1]),
                ga3: u64::from(p.groups[2]),
            };
        };
        match &m.kind {
            K::Grant(v) => voice(&mut out, TrMessageType::Grant, v),
            K::Update(v) => voice(&mut out, TrMessageType::Update, v),
            K::UnitGrant(v) => voice(&mut out, TrMessageType::UuVGrant, v),
            K::UnitUpdate(v) => voice(&mut out, TrMessageType::UuVUpdate, v),
            K::AnswerRequest(v) => voice(&mut out, TrMessageType::UuAnsReq, v),
            K::DataGrant {
                freq_hz,
                unit: u,
                options,
            } => {
                out.message_type = TrMessageType::DataGrant;
                out.freq = *freq_hz;
                out.source = i64::from(*u);
                out.emergency = options.emergency;
                out.encrypted = options.encrypted;
                out.duplex = options.duplex;
                out.mode = options.packet;
            }
            K::Acknowledge { group, unit: u } => {
                unit(&mut out, TrMessageType::Acknowledge, *group, *u);
            }
            K::Affiliation { group, unit: u } => {
                unit(&mut out, TrMessageType::Affiliation, *group, *u);
            }
            K::Location { group, unit: u } => unit(&mut out, TrMessageType::Location, *group, *u),
            K::Registration { unit: u } => unit(&mut out, TrMessageType::Registration, 0, *u),
            K::Deregistration { unit: u } => {
                unit(&mut out, TrMessageType::Deregistration, 0, *u);
            }
            K::ControlChannel { freq_hz } => {
                out.message_type = TrMessageType::ControlChannel;
                out.freq = *freq_hz;
            }
            K::Site { sys_id, rfss, site } => {
                out.message_type = TrMessageType::Sysid;
                out.sys_id = u64::from(*sys_id);
                out.sys_rfss = i32::from(*rfss);
                out.sys_site_id = i32::from(*site);
            }
            K::Network {
                freq_hz,
                wacn,
                sys_id,
            } => {
                out.message_type = TrMessageType::Status;
                out.freq = *freq_hz;
                out.wacn = u64::from(*wacn);
                out.sys_id = u64::from(*sys_id);
            }
            K::PatchAdd(p) => patch(&mut out, TrMessageType::PatchAdd, p),
            K::PatchDelete(p) => patch(&mut out, TrMessageType::PatchDelete, p),
            K::Tdulc { talkgroup } => {
                out.message_type = TrMessageType::Tdulc;
                out.talkgroup = talkgroup.map_or(0, i64::from);
            }
            K::Invalid => out.message_type = TrMessageType::InvalidCcMessage,
            K::Unknown => {}
        }
        out
    }
}
