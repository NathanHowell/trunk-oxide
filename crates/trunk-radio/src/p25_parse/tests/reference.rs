//! What trunk-recorder's `P25Parser` made of each of [`super::CORPUS`], in
//! order, from a new parser: recorded from it before it was deleted.

#![allow(clippy::unreadable_literal, clippy::struct_excessive_bools)] // recorded values

use crate::types::{TrMessageType as T, TrPatchData, TrTrunkMessage};

struct M {
    freq: f64,
    talkgroup: i64,
    encrypted: bool,
    emergency: bool,
    duplex: bool,
    mode: bool,
    priority: i32,
    tdma_slot: i32,
    phase2_tdma: bool,
    source: i64,
    sys_id: u64,
    sys_rfss: i32,
    sys_site_id: i32,
    nac: u64,
    wacn: u64,
    opcode: u64,
    patch_data: TrPatchData,
}

const D: M = M {
    freq: 0.0,
    talkgroup: 0,
    encrypted: false,
    emergency: false,
    duplex: false,
    mode: false,
    priority: 0,
    tdma_slot: 0,
    phase2_tdma: false,
    source: 0,
    sys_id: 0,
    sys_rfss: 0,
    sys_site_id: 0,
    nac: 0,
    wacn: 0,
    opcode: 0,
    patch_data: TrPatchData {
        sg: 0,
        ga1: 0,
        ga2: 0,
        ga3: 0,
    },
};

/// trunk-recorder's output for each corpus message, for parser `sys_num`.
#[allow(clippy::too_many_lines)]
pub(super) fn reference(sys_num: i32) -> Vec<Vec<TrTrunkMessage>> {
    let m = |message_type, f: M| TrTrunkMessage {
        message_type,
        meta: String::new(),
        freq: f.freq,
        talkgroup: f.talkgroup,
        encrypted: f.encrypted,
        emergency: f.emergency,
        duplex: f.duplex,
        mode: f.mode,
        priority: f.priority,
        tdma_slot: f.tdma_slot,
        phase2_tdma: f.phase2_tdma,
        source: f.source,
        sys_num,
        sys_id: f.sys_id,
        sys_rfss: f.sys_rfss,
        sys_site_id: f.sys_site_id,
        nac: f.nac,
        wacn: f.wacn,
        patch_data: f.patch_data,
        opcode: f.opcode,
    };
    vec![
        vec![m(
            T::Grant,
            M {
                talkgroup: 1802,
                encrypted: true,
                priority: 4,
                source: 13330004,
                nac: 0x5e1,
                ..D
            },
        )],
        vec![m(
            T::Unknown,
            M {
                source: -1,
                nac: 0x5e1,
                opcode: 0x3b,
                ..D
            },
        )],
        vec![m(
            T::Sysid,
            M {
                source: -1,
                sys_id: 0x5e4,
                sys_rfss: 1,
                sys_site_id: 1,
                nac: 0x5e1,
                opcode: 0x3a,
                ..D
            },
        )],
        vec![m(
            T::Update,
            M {
                talkgroup: 1802,
                source: -1,
                nac: 0x5e1,
                opcode: 0x2,
                ..D
            },
        )],
        vec![m(
            T::Unknown,
            M {
                source: -1,
                nac: 0x5e1,
                opcode: 0x33,
                ..D
            },
        )],
        vec![m(
            T::Unknown,
            M {
                source: -1,
                nac: 0x5e1,
                opcode: 0x3d,
                ..D
            },
        )],
        vec![m(
            T::Grant,
            M {
                freq: 772906250.0,
                talkgroup: 1802,
                encrypted: true,
                priority: 4,
                tdma_slot: 1,
                phase2_tdma: true,
                source: 13330004,
                nac: 0x5e1,
                ..D
            },
        )],
        vec![m(
            T::Unknown,
            M {
                source: -1,
                nac: 0x5e1,
                opcode: 0x3b,
                ..D
            },
        )],
        vec![m(
            T::Update,
            M {
                freq: 774681250.0,
                talkgroup: 1151,
                phase2_tdma: true,
                source: -1,
                nac: 0x5e1,
                opcode: 0x2,
                ..D
            },
        )],
        vec![m(
            T::Update,
            M {
                freq: 774681250.0,
                talkgroup: 1151,
                phase2_tdma: true,
                source: -1,
                nac: 0x5e1,
                opcode: 0x2,
                ..D
            },
        )],
        vec![
            m(
                T::Update,
                M {
                    freq: 773181250.0,
                    talkgroup: 1802,
                    phase2_tdma: true,
                    source: -1,
                    nac: 0x5e1,
                    opcode: 0x2,
                    ..D
                },
            ),
            m(
                T::Update,
                M {
                    freq: 774406250.0,
                    talkgroup: 1451,
                    tdma_slot: 1,
                    phase2_tdma: true,
                    source: -1,
                    nac: 0x5e1,
                    opcode: 0x2,
                    ..D
                },
            ),
        ],
        vec![m(
            T::PatchAdd,
            M {
                source: -1,
                nac: 0x5e1,
                patch_data: TrPatchData {
                    sg: 4752,
                    ga1: 4752,
                    ga2: 4752,
                    ga3: 4752,
                },
                ..D
            },
        )],
        vec![m(
            T::Grant,
            M {
                freq: 774681250.0,
                talkgroup: 4752,
                encrypted: true,
                tdma_slot: 1,
                phase2_tdma: true,
                source: 13420145,
                nac: 0x5e1,
                opcode: 0x2,
                ..D
            },
        )],
        vec![m(
            T::Update,
            M {
                freq: 774681250.0,
                talkgroup: 4752,
                tdma_slot: 1,
                phase2_tdma: true,
                source: -1,
                nac: 0x5e1,
                opcode: 0x3,
                ..D
            },
        )],
        vec![
            m(
                T::Update,
                M {
                    freq: 774681250.0,
                    talkgroup: 4752,
                    tdma_slot: 1,
                    phase2_tdma: true,
                    source: -1,
                    nac: 0x5e1,
                    opcode: 0x3,
                    ..D
                },
            ),
            m(
                T::Update,
                M {
                    talkgroup: 4752,
                    source: -1,
                    nac: 0x5e1,
                    opcode: 0x3,
                    ..D
                },
            ),
        ],
        vec![m(
            T::Unknown,
            M {
                source: -1,
                nac: 0x5e1,
                opcode: 0x5,
                ..D
            },
        )],
        vec![m(
            T::Unknown,
            M {
                source: -1,
                nac: 0x5e1,
                opcode: 0x9,
                ..D
            },
        )],
        vec![],
        vec![m(
            T::DataGrant,
            M {
                source: 13100335,
                nac: 0x5e1,
                opcode: 0x14,
                ..D
            },
        )],
        vec![m(
            T::Unknown,
            M {
                source: -1,
                nac: 0x5e1,
                opcode: 0x15,
                ..D
            },
        )],
        vec![m(
            T::Unknown,
            M {
                source: -1,
                nac: 0x5e1,
                opcode: 0x16,
                ..D
            },
        )],
        vec![m(
            T::Acknowledge,
            M {
                talkgroup: 65533,
                source: 13194074,
                nac: 0x5e1,
                opcode: 0x20,
                ..D
            },
        )],
        vec![m(
            T::Affiliation,
            M {
                talkgroup: 2551,
                source: 13306622,
                nac: 0x5e1,
                opcode: 0x28,
                ..D
            },
        )],
        vec![m(
            T::Affiliation,
            M {
                talkgroup: 2551,
                source: 13306622,
                nac: 0x5e1,
                opcode: 0x28,
                ..D
            },
        )],
        vec![m(
            T::Unknown,
            M {
                source: -1,
                nac: 0x5e1,
                opcode: 0x2a,
                ..D
            },
        )],
        vec![m(
            T::Location,
            M {
                talkgroup: 4777,
                source: 13422652,
                nac: 0x5e1,
                opcode: 0x2b,
                ..D
            },
        )],
        vec![m(
            T::Registration,
            M {
                source: 13306622,
                nac: 0x5e1,
                opcode: 0x2c,
                ..D
            },
        )],
        vec![m(
            T::Deregistration,
            M {
                source: 13194531,
                nac: 0x5e1,
                opcode: 0x2f,
                ..D
            },
        )],
        vec![m(
            T::Unknown,
            M {
                source: -1,
                nac: 0x5e1,
                opcode: 0x30,
                ..D
            },
        )],
        vec![m(
            T::Unknown,
            M {
                source: -1,
                nac: 0x5e1,
                opcode: 0x39,
                ..D
            },
        )],
        vec![m(
            T::Unknown,
            M {
                source: -1,
                nac: 0x5e1,
                opcode: 0x3c,
                ..D
            },
        )],
        vec![m(
            T::UuVGrant,
            M {
                freq: 960362500.0,
                talkgroup: 291,
                encrypted: true,
                priority: 7,
                source: 1110,
                nac: 0x5e1,
                opcode: 0x4,
                ..D
            },
        )],
        vec![m(
            T::UuVUpdate,
            M {
                freq: 960362500.0,
                talkgroup: 291,
                source: 1110,
                nac: 0x5e1,
                opcode: 0x6,
                ..D
            },
        )],
        vec![m(
            T::UuVGrant,
            M {
                talkgroup: 10551295,
                source: 13100335,
                nac: 0x5e1,
                opcode: 0x4,
                ..D
            },
        )],
        vec![m(
            T::UuVGrant,
            M {
                talkgroup: 1483007,
                source: 16711681,
                nac: 0x5e1,
                opcode: 0x4,
                ..D
            },
        )],
        vec![m(
            T::InvalidCcMessage,
            M {
                source: -1,
                opcode: 0xff,
                ..D
            },
        )],
        vec![m(
            T::Tdulc,
            M {
                source: -1,
                opcode: 0xff,
                ..D
            },
        )],
        vec![m(
            T::Unknown,
            M {
                source: -1,
                opcode: 0xff,
                ..D
            },
        )],
    ]
}
