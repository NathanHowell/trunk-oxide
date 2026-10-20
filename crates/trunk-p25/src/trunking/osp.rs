//! Outbound signalling packets: what a TSBK or an MBT says.
//!
//! Field offsets are written the way op25 and trunk-recorder write them: a
//! TSBK is a 96-bit number (its 16 CRC bits zeroed) and a field is `width`
//! bits starting `shift` bits up from the end. That keeps every offset here
//! checkable against theirs; where they disagree with the layout their own
//! offsets imply, the type says which reading is used and why.

use super::iden::{Access, ChannelId, ChannelPlan, Iden};
use crate::types::Tsbk;

/// The extended `UU_V_CH_GRANT` MBT's first data block (TIA-102.AABC-B
/// §4.2.5.2 Fig. 4.2.5-2): source WACN and system (0–3), source ID (4–6),
/// then these.
const UU_MBT_TARGET: std::ops::Range<usize> = 7..10;
/// An MBT header's manufacturer ID (TIA-102.AABB-B §6.1 Fig. 6.1-2).
const MBT_MFID: usize = 2;
const UU_MBT_CHANNEL_T: std::ops::Range<usize> = 10..12;

/// `width` bits, `shift` bits up from the end of `bytes` followed by `pad`
/// zero bytes.
const fn field(bytes: &[u8], pad: usize, shift: usize, width: usize) -> u64 {
    let mut v = 0;
    let mut i = width;
    while i > 0 {
        i -= 1;
        v <<= 1;
        let bit = shift + i;
        if bit >= pad * 8 {
            let k = bit - pad * 8;
            if k / 8 < bytes.len() {
                v |= ((bytes[bytes.len() - 1 - k / 8] >> (k % 8)) & 1) as u64;
            }
        }
    }
    v
}

/// A TSBK's fields (op25 offsets: the 80 sent bits end 16 bits up).
struct Fields<'a>(&'a [u8]);

impl Fields<'_> {
    #[allow(clippy::cast_possible_truncation)]
    const fn u8(&self, shift: usize, width: usize) -> u8 {
        field(self.0, 2, shift, width) as u8
    }

    #[allow(clippy::cast_possible_truncation)]
    const fn u16(&self, shift: usize, width: usize) -> u16 {
        field(self.0, 2, shift, width) as u16
    }

    #[allow(clippy::cast_possible_truncation)]
    const fn u32(&self, shift: usize, width: usize) -> u32 {
        field(self.0, 2, shift, width) as u32
    }

    const fn channel(&self, shift: usize) -> ChannelId {
        ChannelId::new(self.u16(shift, 16))
    }
}

/// Service Options bits (TIA-102.AABC-B §2.3.24, Figure 2.3.24-1). Data
/// Service Options (§2.3.12) share bits 7–4; their bits 3–0 are the NSAPI.
pub mod service_options {
    /// Bit 7: special (emergency) processing.
    pub const EMERGENCY: u8 = 0x80;
    /// Bit 6: the resources are presented protected (encrypted).
    pub const PROTECTED: u8 = 0x40;
    /// Bit 5: full duplex.
    pub const DUPLEX: u8 = 0x20;
    /// Bit 4: packet (rather than circuit) mode.
    pub const MODE: u8 = 0x10;
    /// Bits 2–0: the priority level.
    pub const PRIORITY_LEVEL: u8 = 0x07;
}

/// Service options of a voice or data grant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ServiceOptions(u8);

impl ServiceOptions {
    pub const NONE: Self = Self(0);

    pub const fn new(v: u8) -> Self {
        Self(v)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn emergency(self) -> bool {
        self.0 & service_options::EMERGENCY != 0
    }

    pub const fn encrypted(self) -> bool {
        self.0 & service_options::PROTECTED != 0
    }

    pub const fn duplex(self) -> bool {
        self.0 & service_options::DUPLEX != 0
    }

    /// Packet (rather than circuit) mode.
    pub const fn packet(self) -> bool {
        self.0 & service_options::MODE != 0
    }

    pub const fn priority(self) -> u8 {
        self.0 & service_options::PRIORITY_LEVEL
    }
}

/// The answer in an affiliation or registration response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Response {
    Accept,
    Fail,
    Deny,
    Refused,
}

impl Response {
    pub const fn from_bits(v: u8) -> Self {
        match v & 3 {
            0 => Self::Accept,
            1 => Self::Fail,
            2 => Self::Deny,
            _ => Self::Refused,
        }
    }
}

/// What an `ACK_RSP_FNE`'s Additional Information (octets 3–6) carries
/// (TIA-102.AABC-B §6.2.1, Figs. 6.2.1-2 and -3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AckInfo {
    /// AIV = 0: nothing.
    None,
    /// AIV = 1, EX = 0: the source address.
    Source(u32),
    /// AIV = 1, EX = 1: the target's WACN and system (its address is then
    /// a Target ID).
    System { wacn: u32, system: u16 },
}

/// An adjacent site's status bits, octet 3 bits 7–4 of `ADJ_STS_BCST`
/// (TIA-102.AABC-B §6.2.2.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[allow(clippy::struct_excessive_bools)] // four independent bits of the message
pub struct SiteFlags {
    /// C: the broadcast advertises a conventional channel.
    pub conventional: bool,
    /// F: the site is in a failure condition.
    pub failed: bool,
    /// V: the information is known to be current; clear, it is the last
    /// known.
    pub valid: bool,
    /// A: the site has an active network connection to the RFSS
    /// controller.
    pub networked: bool,
}

/// A group and the channel it is on, as grant updates pair them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GroupChannel {
    pub channel: ChannelId,
    pub group: u16,
}

/// A site within a system.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Site {
    pub system: u16,
    pub rfss: u8,
    pub site: u8,
}

/// Manufacturer IDs.
pub mod mfid {
    /// The standard MFID (TIA-102.BAAC-A §2.3).
    pub const STANDARD: u8 = 0x00;
    /// Also standard, since 2001 (TIA-102.BAAC-A §2.3; TIA-102.AABB-B §5.1).
    pub const STANDARD_ALT: u8 = 0x01;
    pub const MOTOROLA: u8 = 0x90;
    pub const HARRIS: u8 = 0xa4;
}

/// Standard OSP opcodes, by their TIA-102.AABC-B aliases (Annex D; section
/// given per opcode).
pub mod opcode {
    /// §4.2.1.
    pub const GRP_V_CH_GRANT: u8 = 0x00;
    /// §4.2.2.
    pub const GRP_V_CH_GRANT_UPDT: u8 = 0x02;
    /// §4.2.3.
    pub const GRP_V_CH_GRANT_UPDT_EXP: u8 = 0x03;
    /// §4.2.5.
    pub const UU_V_CH_GRANT: u8 = 0x04;
    /// §4.2.4.
    pub const UU_ANS_REQ: u8 = 0x05;
    /// §4.2.8.
    pub const UU_V_CH_GRANT_UPDT: u8 = 0x06;
    /// §5.2.5 (`SN-DATA_CHN_GNT`).
    pub const SN_DATA_CHN_GNT: u8 = 0x14;
    /// §6.2.1.
    pub const ACK_RSP_FNE: u8 = 0x20;
    /// §6.2.8.
    pub const GRP_AFF_RSP: u8 = 0x28;
    /// §6.2.28.
    pub const SCCB_EXP: u8 = 0x29;
    /// §6.2.23.
    pub const LOC_REG_RSP: u8 = 0x2b;
    /// §6.2.21.
    pub const U_REG_RSP: u8 = 0x2c;
    /// §6.2.22.
    pub const U_DE_REG_ACK: u8 = 0x2f;
    /// TIA-102.BBAC-1 Table B-1 (§8.3.1.36).
    pub const IDEN_UP_TDMA: u8 = 0x33;
    /// §6.2.29.
    pub const IDEN_UP_VU: u8 = 0x34;
    /// §6.2.16.
    pub const SCCB: u8 = 0x39;
    /// §6.2.15.
    pub const RFSS_STS_BCST: u8 = 0x3a;
    /// §6.2.11.
    pub const NET_STS_BCST: u8 = 0x3b;
    /// §6.2.2.
    pub const ADJ_STS_BCST: u8 = 0x3c;
    /// §6.2.9.
    pub const IDEN_UP: u8 = 0x3d;
}

/// Identifier update field units and widths (TIA-102.AABC-B).
pub mod iden_up {
    /// Base Frequency: 0.000005 MHz steps (§6.2.29; §2.3.5).
    pub const BASE_FREQUENCY_HZ: u64 = 5;
    /// Channel Spacing: 0.125 kHz steps (§2.3.10).
    pub const CHANNEL_SPACING_HZ: u32 = 125;
    /// BW: 0.125 kHz steps (§2.3.6).
    pub const BW_HZ: u32 = 125;
    /// Transmit Offset: 0.250 MHz steps (§2.3.35).
    pub const TRANSMIT_OFFSET_HZ: i64 = 250_000;
    /// Transmit Offset: a High/Low flag (b8) and an 8-bit value (§2.3.35).
    pub const TRANSMIT_OFFSET_BITS: u32 = 9;
    /// Transmit Offset VU: a High/Low flag (b13) and a 13-bit value, in
    /// units of the channel spacing (§2.3.36).
    pub const TRANSMIT_OFFSET_VU_BITS: u32 = 14;
    /// Transmit Offset TDMA: 14 bits (TIA-102.BBAC field table), read like
    /// Transmit Offset VU.
    pub const TRANSMIT_OFFSET_TDMA_BITS: u32 = 14;
    /// BW VU: 6.25 kHz receiver bandwidth (§2.3.7, Table 2.3.7-1).
    pub const BW_VU_6_25_KHZ: u8 = 0b0100;
    /// BW VU: 12.5 kHz receiver bandwidth (§2.3.7, Table 2.3.7-1).
    pub const BW_VU_12_5_KHZ: u8 = 0b0101;
    /// Transmit Offset $80: no standard transmit offset; grants name the
    /// transmit channel explicitly (§2.3.35).
    pub const NO_STANDARD_OFFSET: u64 = 0x80;
    /// Base Frequency fields in the VHF (136–172 MHz) and UHF (380–512 MHz)
    /// bands, where an `IDEN_UP` takes the layout of Fig. 6.2.9-2 under the
    /// same opcode (§6.2.9).
    const VHF: (u32, u32) = (0x019f_0a00, 0x020c_e700);
    const UHF: (u32, u32) = (0x0487_abd0, 0x061a_8000);

    pub const fn is_vhf_uhf(base: u32) -> bool {
        (VHF.0 <= base && base <= VHF.1) || (UHF.0 <= base && base <= UHF.1)
    }
}

/// An `IDEN_UP` plan outside the VHF and UHF bands (AABC-B Fig. 6.2.9-1).
#[allow(clippy::cast_possible_wrap)] // the offset is at most 8 bits × 250 kHz
const fn iden_up_plan(t: &Fields<'_>) -> Option<ChannelPlan> {
    let spacing = t.u32(48, 10) * iden_up::CHANNEL_SPACING_HZ;
    let bandwidth = t.u32(67, 9) * iden_up::BW_HZ;
    if spacing == 0 || bandwidth == 0 {
        return None;
    }
    let toff = field(t.0, 2, 58, iden_up::TRANSMIT_OFFSET_BITS as usize);
    Some(ChannelPlan {
        base_hz: t.u32(16, 32) as u64 * iden_up::BASE_FREQUENCY_HZ,
        spacing_hz: spacing,
        tx_offset_hz: if toff == iden_up::NO_STANDARD_OFFSET {
            None
        } else {
            Some(iden_offset(toff, iden_up::TRANSMIT_OFFSET_BITS) * iden_up::TRANSMIT_OFFSET_HZ)
        },
        bandwidth_hz: bandwidth,
        access: Access::Fdma,
    })
}

/// A plan in the VHF/UHF layout: `IDEN_UP_VU`, or `IDEN_UP` in those bands
/// (AABC-B Fig. 6.2.9-2, §6.2.29). A reserved BW VU gives a bandwidth of 0:
/// it is informational, and the frequencies still stand.
#[allow(clippy::cast_possible_wrap)] // the spacing is at most 10 bits × 125 Hz
const fn vu_plan(t: &Fields<'_>) -> Option<ChannelPlan> {
    let spacing = t.u32(48, 10) * iden_up::CHANNEL_SPACING_HZ;
    if spacing == 0 {
        return None;
    }
    let toff = field(t.0, 2, 58, iden_up::TRANSMIT_OFFSET_VU_BITS as usize);
    Some(ChannelPlan {
        base_hz: t.u32(16, 32) as u64 * iden_up::BASE_FREQUENCY_HZ,
        spacing_hz: spacing,
        tx_offset_hz: Some(iden_offset(toff, iden_up::TRANSMIT_OFFSET_VU_BITS) * spacing as i64),
        bandwidth_hz: match t.u8(72, 4) {
            iden_up::BW_VU_6_25_KHZ => 6_250,
            iden_up::BW_VU_12_5_KHZ => 12_500,
            _ => 0,
        },
        access: Access::Fdma,
    })
}

/// An SCCB channel, unless its System Service Class is $00 (AABC-B §6.2.16).
const fn secondary(channel: ChannelId, class: u8) -> Option<ChannelId> {
    if class == 0 { None } else { Some(channel) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Osp {
    /// `GRP_V_CH_GRANT` (0x00), or its MBT form.
    GroupVoiceGrant {
        options: ServiceOptions,
        channel: ChannelId,
        group: u16,
        source: u32,
    },
    /// `GRP_V_CH_GRANT_UPDT` (0x02): one or two groups already on a channel.
    GroupVoiceGrantUpdate {
        first: GroupChannel,
        second: GroupChannel,
    },
    /// `GRP_V_CH_GRANT_UPDT_EXP` (0x03).
    GroupVoiceGrantUpdateExplicit {
        options: ServiceOptions,
        tx: ChannelId,
        rx: ChannelId,
        group: u16,
    },
    /// `UU_V_CH_GRANT` (0x04). The TSBK has no service options: its channel,
    /// target and source fill all eight octets. trunk-recorder reads options
    /// from octet 2 anyway, which is the channel's identifier and top bits,
    /// so its unit-to-unit grants carry made-up emergency and encryption
    /// flags. The MBT form carries them in its header and the channel in its
    /// data (AABC-B Fig. 4.2.5-2), where trunk-recorder read the header's
    /// reserved octet and the CRC's padding.
    UnitVoiceGrant {
        options: ServiceOptions,
        channel: ChannelId,
        target: u32,
        source: u32,
    },
    /// `UU_ANS_REQ` (0x05).
    UnitAnswerRequest {
        options: ServiceOptions,
        target: u32,
        source: u32,
    },
    /// `UU_V_CH_GRANT_UPDT` (0x06).
    UnitVoiceGrantUpdate {
        channel: ChannelId,
        target: u32,
        source: u32,
    },
    /// `SNDCP_DATA_CH_GNT` (0x14).
    DataGrant {
        options: ServiceOptions,
        tx: ChannelId,
        rx: ChannelId,
        target: u32,
    },
    /// `ACK_RSP_FNE` (0x20): `info` is what octets 3–6 carry, by the AIV
    /// and EX flags (AABC-B §6.2.1). trunk-recorder took octets 5–6 as a
    /// group; no ACK carries one.
    Acknowledge {
        service: u8,
        info: AckInfo,
        target: u32,
    },
    /// `GRP_AFF_RSP` (0x28).
    GroupAffiliation {
        response: Response,
        announcement: u16,
        group: u16,
        target: u32,
    },
    /// `LOC_REG_RSP` (0x2b).
    LocationRegistration {
        response: Response,
        group: u16,
        target: u32,
    },
    /// `U_REG_RSP` (0x2c).
    UnitRegistration {
        response: Response,
        source_id: u32,
        source_address: u32,
    },
    /// `U_DE_REG_ACK` (0x2f).
    UnitDeregistration { source: u32 },
    /// `SCCB_EXP` (0x29): one secondary control channel, as a transmit and a
    /// receive channel. Only the transmit channel is one to listen to;
    /// trunk-recorder also offers the receive channel as a control channel.
    SecondaryControlExplicit {
        rfss: u8,
        site: u8,
        tx: ChannelId,
        rx: ChannelId,
    },
    /// SCCB (0x39): up to two secondary control channels. One whose System
    /// Service Class is $00 is padding, not a valid secondary control
    /// channel (AABC-B §6.2.16), and is `None`.
    SecondaryControl {
        rfss: u8,
        site: u8,
        channels: [Option<ChannelId>; 2],
    },
    /// `RFSS_STS_BCST` (0x3a): the site this control channel belongs to.
    RfssStatus { site: Site, channel: ChannelId },
    /// `NET_STS_BCST` (0x3b).
    NetworkStatus {
        wacn: u32,
        system: u16,
        channel: ChannelId,
    },
    /// `ADJ_STS_BCST` (0x3c): a neighbouring site's control channel.
    AdjacentStatus {
        site: Site,
        channel: ChannelId,
        flags: SiteFlags,
    },
    /// `IDEN_UP` (0x3d), `IDEN_UP_VU` (0x34), `IDEN_UP_TDMA` (0x33). No plan for
    /// a reserved TDMA channel type, nor for a reserved (zero) channel
    /// spacing or BW (AABC-B §2.3.6, §2.3.10).
    IdenUpdate {
        iden: Iden,
        plan: Option<ChannelPlan>,
    },
    /// Motorola patch: a supergroup and up to three member groups.
    MotPatchAdd { supergroup: u16, groups: [u16; 3] },
    /// Motorola patch group grant (or its MBT form, which has no source).
    MotPatchGrant {
        options: ServiceOptions,
        channel: ChannelId,
        supergroup: u16,
        source: Option<u32>,
    },
    /// Motorola patch group grant update.
    MotPatchGrantUpdate {
        first: GroupChannel,
        second: GroupChannel,
    },
    /// Harris (M/A-COM) patch command.
    HarrisPatch {
        activate: bool,
        supergroup: u16,
        group: u16,
    },
    /// Its payload is encrypted.
    Protected { opcode: u8, mfid: u8 },
    /// Nothing the trunking logic uses.
    Other { opcode: u8, mfid: u8 },
}

/// A transmit offset field: its top bit the High/Low flag, the rest the
/// magnitude (TIA-102.AABC-B §2.3.35, §2.3.36).
#[allow(clippy::cast_possible_wrap)] // at most 13 bits
const fn iden_offset(sign_and_magnitude: u64, bits: u32) -> i64 {
    let magnitude = (sign_and_magnitude & ((1 << (bits - 1)) - 1)) as i64;
    // The High/Low flag set means the mobile transmits above the receive
    // frequency.
    if (sign_and_magnitude >> (bits - 1)) & 1 == 1 {
        magnitude
    } else {
        -magnitude
    }
}

impl Osp {
    pub const fn from_tsbk(tsbk: &Tsbk) -> Self {
        let t = Fields(tsbk.bytes());
        let opcode = tsbk.opcode();
        let mfid = tsbk.manufacturer();
        if tsbk.is_protected() {
            return Self::Protected { opcode, mfid };
        }
        match (mfid, opcode) {
            (mfid::STANDARD | mfid::STANDARD_ALT, op) => Self::standard(&t, op, mfid),
            (mfid::MOTOROLA, 0x00) => Self::MotPatchAdd {
                supergroup: t.u16(64, 16),
                groups: [t.u16(48, 16), t.u16(32, 16), t.u16(16, 16)],
            },
            (mfid::MOTOROLA, 0x02) => Self::MotPatchGrant {
                options: ServiceOptions(t.u8(72, 8)),
                channel: t.channel(56),
                supergroup: t.u16(40, 16),
                source: Some(t.u32(16, 24)),
            },
            (mfid::MOTOROLA, 0x03) => Self::MotPatchGrantUpdate {
                first: GroupChannel {
                    channel: t.channel(64),
                    group: t.u16(48, 16),
                },
                second: GroupChannel {
                    channel: t.channel(32),
                    group: t.u16(16, 16),
                },
            },
            (mfid::HARRIS, 0x30) => {
                let rta = t.u32(16, 24);
                #[allow(clippy::cast_possible_truncation)]
                Self::HarrisPatch {
                    activate: t.u8(77, 1) == 1,
                    supergroup: t.u16(56, 16),
                    group: rta as u16,
                }
            }
            _ => Self::Other { opcode, mfid },
        }
    }

    #[allow(clippy::too_many_lines)] // the opcode table
    const fn standard(t: &Fields<'_>, opcode: u8, mfid: u8) -> Self {
        match opcode {
            opcode::GRP_V_CH_GRANT => Self::GroupVoiceGrant {
                options: ServiceOptions(t.u8(72, 8)),
                channel: t.channel(56),
                group: t.u16(40, 16),
                source: t.u32(16, 24),
            },
            opcode::GRP_V_CH_GRANT_UPDT => Self::GroupVoiceGrantUpdate {
                first: GroupChannel {
                    channel: t.channel(64),
                    group: t.u16(48, 16),
                },
                second: GroupChannel {
                    channel: t.channel(32),
                    group: t.u16(16, 16),
                },
            },
            opcode::GRP_V_CH_GRANT_UPDT_EXP => Self::GroupVoiceGrantUpdateExplicit {
                options: ServiceOptions(t.u8(72, 8)),
                tx: t.channel(48),
                rx: t.channel(32),
                group: t.u16(16, 16),
            },
            opcode::UU_V_CH_GRANT => Self::UnitVoiceGrant {
                options: ServiceOptions::NONE,
                channel: t.channel(64),
                target: t.u32(40, 24),
                source: t.u32(16, 24),
            },
            opcode::UU_ANS_REQ => Self::UnitAnswerRequest {
                options: ServiceOptions(t.u8(72, 8)),
                target: t.u32(40, 24),
                source: t.u32(16, 24),
            },
            opcode::UU_V_CH_GRANT_UPDT => Self::UnitVoiceGrantUpdate {
                channel: t.channel(64),
                target: t.u32(40, 24),
                source: t.u32(16, 24),
            },
            opcode::SN_DATA_CHN_GNT => Self::DataGrant {
                options: ServiceOptions(t.u8(72, 8)),
                tx: t.channel(56),
                rx: t.channel(40),
                target: t.u32(16, 24),
            },
            opcode::ACK_RSP_FNE => Self::Acknowledge {
                service: t.u8(72, 6),
                info: match (t.u8(79, 1), t.u8(78, 1)) {
                    (0, _) => AckInfo::None,
                    (_, 0) => AckInfo::Source(t.u32(40, 24)),
                    _ => AckInfo::System {
                        wacn: t.u32(52, 20),
                        system: t.u16(40, 12),
                    },
                },
                target: t.u32(16, 24),
            },
            opcode::GRP_AFF_RSP => Self::GroupAffiliation {
                response: Response::from_bits(t.u8(72, 2)),
                announcement: t.u16(56, 16),
                group: t.u16(40, 16),
                target: t.u32(16, 24),
            },
            opcode::SCCB_EXP => Self::SecondaryControlExplicit {
                rfss: t.u8(72, 8),
                site: t.u8(64, 8),
                tx: t.channel(48),
                rx: t.channel(24),
            },
            opcode::LOC_REG_RSP => Self::LocationRegistration {
                response: Response::from_bits(t.u8(72, 2)),
                group: t.u16(56, 16),
                target: t.u32(16, 24),
            },
            opcode::U_REG_RSP => Self::UnitRegistration {
                response: Response::from_bits(t.u8(76, 2)),
                source_id: t.u32(40, 24),
                source_address: t.u32(16, 24),
            },
            opcode::U_DE_REG_ACK => Self::UnitDeregistration {
                source: t.u32(16, 24),
            },
            opcode::IDEN_UP_TDMA => {
                let toff = field(t.0, 2, 58, iden_up::TRANSMIT_OFFSET_TDMA_BITS as usize);
                let spacing = t.u32(48, 10) * iden_up::CHANNEL_SPACING_HZ;
                let plan = match ChannelPlan::tdma_channel_type(t.u8(72, 4)) {
                    Some((access, bandwidth_hz)) if spacing != 0 => Some(ChannelPlan {
                        base_hz: t.u32(16, 32) as u64 * iden_up::BASE_FREQUENCY_HZ,
                        spacing_hz: spacing,
                        tx_offset_hz: Some(
                            iden_offset(toff, iden_up::TRANSMIT_OFFSET_TDMA_BITS) * spacing as i64,
                        ),
                        bandwidth_hz,
                        access,
                    }),
                    _ => None,
                };
                Self::IdenUpdate {
                    iden: Iden::new(t.u8(76, 4)),
                    plan,
                }
            }
            opcode::IDEN_UP_VU => Self::IdenUpdate {
                iden: Iden::new(t.u8(76, 4)),
                plan: vu_plan(t),
            },
            opcode::SCCB => Self::SecondaryControl {
                rfss: t.u8(72, 8),
                site: t.u8(64, 8),
                channels: [
                    secondary(t.channel(48), t.u8(40, 8)),
                    secondary(t.channel(24), t.u8(16, 8)),
                ],
            },
            opcode::RFSS_STS_BCST => Self::RfssStatus {
                site: Site {
                    system: t.u16(56, 12),
                    rfss: t.u8(48, 8),
                    site: t.u8(40, 8),
                },
                channel: t.channel(24),
            },
            opcode::NET_STS_BCST => Self::NetworkStatus {
                wacn: t.u32(52, 20),
                system: t.u16(40, 12),
                channel: t.channel(24),
            },
            opcode::ADJ_STS_BCST => Self::AdjacentStatus {
                site: Site {
                    system: t.u16(56, 12),
                    rfss: t.u8(48, 8),
                    site: t.u8(40, 8),
                },
                channel: t.channel(24),
                flags: SiteFlags {
                    conventional: t.u8(71, 1) == 1,
                    failed: t.u8(70, 1) == 1,
                    valid: t.u8(69, 1) == 1,
                    networked: t.u8(68, 1) == 1,
                },
            },
            opcode::IDEN_UP => Self::IdenUpdate {
                iden: Iden::new(t.u8(76, 4)),
                plan: if iden_up::is_vhf_uhf(t.u32(16, 32)) {
                    vu_plan(t)
                } else {
                    iden_up_plan(t)
                },
            },
            _ => Self::Other { opcode, mfid },
        }
    }

    /// An MBT: its 12-byte header and the data blocks without the final
    /// CRC-32, as op25 sends them. trunk-recorder takes 10 header bytes and
    /// reads data fields from the end of everything after them, so they are
    /// read the same way here.
    pub fn from_mbt(header: &[u8; 12], data: &[u8]) -> Self {
        let h = Fields(&header[..10]);
        let mut rest = header[10..].to_vec();
        rest.extend_from_slice(data);
        #[allow(clippy::cast_possible_truncation)]
        let d = |shift, width| field(&rest, 4, shift, width) as u32;
        #[allow(clippy::cast_possible_truncation)]
        let dch = |shift| ChannelId::new(d(shift, 16) as u16);
        let opcode = h.u8(32, 6);
        let mfid = header[MBT_MFID];
        // The standard opcodes mean what AABC-B says only under a standard
        // MFID, as for a TSBK.
        let standard = matches!(mfid, mfid::STANDARD | mfid::STANDARD_ALT);
        #[allow(clippy::cast_possible_truncation)]
        match opcode {
            opcode::GRP_V_CH_GRANT if standard => Self::GroupVoiceGrant {
                options: ServiceOptions(h.u8(24, 8)),
                channel: dch(64),
                group: d(32, 16) as u16,
                source: h.u32(48, 24),
            },
            0x02 if d(168, 8) == u32::from(mfid::MOTOROLA) => Self::MotPatchGrant {
                options: ServiceOptions::NONE,
                channel: dch(80),
                supergroup: d(48, 16) as u16,
                source: None,
            },
            opcode::UU_V_CH_GRANT if standard && data.len() >= UU_MBT_CHANNEL_T.end => {
                let (t, c) = (UU_MBT_TARGET.start, UU_MBT_CHANNEL_T.start);
                Self::UnitVoiceGrant {
                    options: ServiceOptions(h.u8(24, 8)),
                    channel: ChannelId::new(u16::from_be_bytes([data[c], data[c + 1]])),
                    target: u32::from_be_bytes([0, data[t], data[t + 1], data[t + 2]]),
                    source: h.u32(48, 24),
                }
            }
            opcode::RFSS_STS_BCST if standard => Self::RfssStatus {
                site: Site {
                    system: h.u16(48, 12),
                    rfss: d(88, 8) as u8,
                    site: d(80, 8) as u8,
                },
                channel: dch(64),
            },
            opcode::NET_STS_BCST if standard => Self::NetworkStatus {
                wacn: d(76, 20),
                system: h.u16(48, 12),
                channel: dch(56),
            },
            _ => Self::Other { opcode, mfid },
        }
    }

    /// The channels a grant or grant update puts traffic on (0xffff, "no
    /// channel", left out). Empty for everything else.
    pub const fn traffic_channels(&self) -> [Option<ChannelId>; 2] {
        const fn some(c: ChannelId) -> Option<ChannelId> {
            if c.get() == 0xffff { None } else { Some(c) }
        }
        match *self {
            Self::GroupVoiceGrant { channel, .. }
            | Self::MotPatchGrant { channel, .. }
            | Self::UnitVoiceGrant { channel, .. }
            | Self::UnitVoiceGrantUpdate { channel, .. }
            | Self::GroupVoiceGrantUpdateExplicit { tx: channel, .. }
            | Self::DataGrant { tx: channel, .. } => [some(channel), None],
            Self::GroupVoiceGrantUpdate { first, second }
            | Self::MotPatchGrantUpdate { first, second } => {
                [some(first.channel), some(second.channel)]
            }
            _ => [None, None],
        }
    }

    /// A name for metrics.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::GroupVoiceGrant { .. } => "group_grant",
            Self::GroupVoiceGrantUpdate { .. } | Self::GroupVoiceGrantUpdateExplicit { .. } => {
                "group_grant_update"
            }
            Self::UnitVoiceGrant { .. } => "unit_grant",
            Self::UnitAnswerRequest { .. } => "unit_answer_request",
            Self::UnitVoiceGrantUpdate { .. } => "unit_grant_update",
            Self::DataGrant { .. } => "data_grant",
            Self::Acknowledge { .. } => "acknowledge",
            Self::GroupAffiliation { .. } => "affiliation",
            Self::LocationRegistration { .. } => "location",
            Self::UnitRegistration { .. } => "registration",
            Self::UnitDeregistration { .. } => "deregistration",
            Self::SecondaryControlExplicit { .. } | Self::SecondaryControl { .. } => {
                "secondary_control"
            }
            Self::RfssStatus { .. } => "rfss_status",
            Self::NetworkStatus { .. } => "network_status",
            Self::AdjacentStatus { .. } => "adjacent_status",
            Self::IdenUpdate { .. } => "iden_update",
            Self::MotPatchAdd { .. } | Self::HarrisPatch { .. } => "patch",
            Self::MotPatchGrant { .. } => "patch_grant",
            Self::MotPatchGrantUpdate { .. } => "patch_grant_update",
            Self::Protected { .. } => "protected",
            Self::Other { .. } => "other",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vendor_mbt_is_not_read_as_a_standard_one() {
        let mut header = [0u8; 12];
        header[7] = opcode::UU_V_CH_GRANT;
        let data = [0u8; 20];
        assert!(matches!(
            Osp::from_mbt(&header, &data),
            Osp::UnitVoiceGrant { .. }
        ));
        header[MBT_MFID] = mfid::HARRIS;
        assert_eq!(
            Osp::from_mbt(&header, &data),
            Osp::Other {
                opcode: opcode::UU_V_CH_GRANT,
                mfid: mfid::HARRIS
            }
        );
    }

    #[test]
    fn an_extended_unit_to_unit_grant_reads_fig_4_2_5_2() {
        // Header: source 0x123456 (octets 3-5), opcode 0x04, options: emergency.
        let mut header = [0u8; 12];
        header[3..6].copy_from_slice(&[0x12, 0x34, 0x56]);
        header[7] = opcode::UU_V_CH_GRANT;
        header[8] = service_options::EMERGENCY;
        // Two data blocks less the CRC-32: source WACN/system/ID, target
        // 0xabcdef, Channel (T) 0x1234; block 2 opens with Channel (R).
        let mut data = [0u8; 20];
        data[0..7].copy_from_slice(&[0xbe, 0xe0, 0x05, 0xe4, 0x12, 0x34, 0x56]);
        data[7..10].copy_from_slice(&[0xab, 0xcd, 0xef]);
        data[10..12].copy_from_slice(&0x1234u16.to_be_bytes());
        data[12..14].copy_from_slice(&0x1234u16.to_be_bytes());
        assert_eq!(
            Osp::from_mbt(&header, &data),
            Osp::UnitVoiceGrant {
                options: ServiceOptions(service_options::EMERGENCY),
                channel: ChannelId::new(0x1234),
                target: 0xab_cdef,
                source: 0x12_3456,
            }
        );
    }

    fn tsbk(hex: &str) -> Tsbk {
        let b: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        // Corpus lines carry the NAC first, as op25 sends them.
        Tsbk::new(b[2..].try_into().unwrap())
    }

    #[test]
    fn fields_count_from_the_end_like_op25() {
        let b = [0x12, 0x34, 0x56];
        assert_eq!(field(&b, 2, 16, 8), 0x56);
        assert_eq!(field(&b, 2, 20, 8), 0x45);
        assert_eq!(field(&b, 2, 32, 16), 0x12);
        assert_eq!(field(&b, 2, 0, 16), 0);
    }

    // Lines from the svrcs capture (`p25-rust` run, WACN BEE00, system 5E4).

    #[test]
    fn network_status() {
        assert_eq!(
            Osp::from_tsbk(&tsbk("05e13b0000bee005e4167070")),
            Osp::NetworkStatus {
                wacn: 0xbee00,
                system: 0x5e4,
                channel: ChannelId::new(0x1670),
            }
        );
    }

    #[test]
    fn rfss_status_names_the_site() {
        assert_eq!(
            Osp::from_tsbk(&tsbk("05e13a000035e40101167070")),
            Osp::RfssStatus {
                site: Site {
                    system: 0x5e4,
                    rfss: 1,
                    site: 1,
                },
                channel: ChannelId::new(0x1670),
            }
        );
    }

    #[test]
    fn tdma_iden_update() {
        let Osp::IdenUpdate {
            iden,
            plan: Some(p),
        } = Osp::from_tsbk(&tsbk("05e1330033a5806409157562"))
        else {
            panic!()
        };
        assert_eq!(iden, Iden::new(3));
        assert_eq!(p.access, Access::Tdma { slots: 2 });
        assert_eq!(p.base_hz, 0x0915_7562 * 5);
        assert_eq!(p.spacing_hz, 100 * 125);
        // 700 MHz: mobiles transmit 30 MHz up.
        assert_eq!(p.tx_offset_hz, Some(0x960 * 12_500));
        assert_eq!(p.tx_offset_hz, Some(30_000_000));
    }

    #[test]
    fn fdma_iden_update() {
        let Osp::IdenUpdate {
            iden,
            plan: Some(p),
        } = Osp::from_tsbk(&tsbk("05e13d00432270640b256e84"))
        else {
            panic!()
        };
        assert_eq!(iden, Iden::new(4));
        assert_eq!(p.access, Access::Fdma);
        assert_eq!(p.base_hz, 0x0b25_6e84 * 5);
        assert_eq!(p.spacing_hz, 12_500);
        assert_eq!(p.bandwidth_hz, 12_500);
        // 900 MHz: mobiles transmit 39 MHz down.
        assert_eq!(p.tx_offset_hz, Some(-39_000_000));
    }

    /// In the VHF and UHF bands an `IDEN_UP` takes Fig. 6.2.9-2's layout:
    /// a 4-bit BW VU and a 14-bit offset in channel spacings.
    #[test]
    fn a_uhf_iden_update_is_read_in_the_vhf_uhf_layout() {
        // Identifier 2, 12.5 kHz, +400 spacings, 12.5 kHz spacing, 450 MHz.
        let Osp::IdenUpdate {
            iden,
            plan: Some(p),
        } = Osp::from_tsbk(&tsbk("05e13d0025864064055d4a80"))
        else {
            panic!()
        };
        assert_eq!(iden, Iden::new(2));
        assert_eq!(p.base_hz, 450_000_000);
        assert_eq!(p.spacing_hz, 12_500);
        assert_eq!(p.bandwidth_hz, 12_500);
        assert_eq!(p.tx_offset_hz, Some(5_000_000));
    }

    /// `fdma_iden_update`'s with a reserved field: a zero spacing or BW is
    /// no plan, and a $80 offset is none at all (AABC-B §2.3.6, §2.3.10,
    /// §2.3.35).
    #[test]
    fn reserved_iden_update_fields() {
        let plan = |hex| match Osp::from_tsbk(&tsbk(hex)) {
            Osp::IdenUpdate { plan, .. } => plan,
            other => panic!("{other:?}"),
        };
        assert_eq!(plan("05e13d00432270000b256e84"), None);
        assert_eq!(plan("05e13d00400270640b256e84"), None);
        let p = plan("05e13d00432200640b256e84").unwrap();
        assert_eq!((p.spacing_hz, p.tx_offset_hz), (12_500, None));
    }

    /// An acknowledgement's octets 3–6 are by its AIV and EX flags a
    /// source address or the target's WACN and system, never a group.
    #[test]
    fn an_acknowledgement_carries_its_additional_information() {
        assert_eq!(
            Osp::from_tsbk(&tsbk("05e12000a300fffffdc9535a")),
            Osp::Acknowledge {
                service: 0x23,
                info: AckInfo::Source(0xff_fffd),
                target: 0xc9_535a,
            }
        );
        assert_eq!(
            Osp::from_tsbk(&tsbk("05e12000e3bee005e4c9535a")),
            Osp::Acknowledge {
                service: 0x23,
                info: AckInfo::System {
                    wacn: 0xbee00,
                    system: 0x5e4
                },
                target: 0xc9_535a,
            }
        );
        assert!(matches!(
            Osp::from_tsbk(&tsbk("05e120002300fffffdc9535a")),
            Osp::Acknowledge {
                info: AckInfo::None,
                ..
            }
        ));
    }

    /// An SCCB channel of System Service Class $00 is padding.
    #[test]
    fn a_secondary_control_channel_of_class_zero_is_none() {
        assert_eq!(
            Osp::from_tsbk(&tsbk("05e139000101165804168800")),
            Osp::SecondaryControl {
                rfss: 1,
                site: 1,
                channels: [Some(ChannelId::new(0x1658)), None],
            }
        );
    }

    #[test]
    fn adjacent_status_carries_the_sites_flags() {
        let Osp::AdjacentStatus { site, flags, .. } =
            Osp::from_tsbk(&tsbk("05e13c000035e40103161070"))
        else {
            panic!()
        };
        assert_eq!(site.system, 0x5e4);
        assert_eq!(
            flags,
            SiteFlags {
                conventional: false,
                failed: false,
                valid: true,
                networked: true,
            }
        );
    }

    #[test]
    fn grant_update_pairs_slots() {
        assert_eq!(
            Osp::from_tsbk(&tsbk("05e1020036fc070a37c105ab")),
            Osp::GroupVoiceGrantUpdate {
                first: GroupChannel {
                    channel: ChannelId::new(0x36fc),
                    group: 0x070a,
                },
                second: GroupChannel {
                    channel: ChannelId::new(0x37c1),
                    group: 0x05ab,
                },
            }
        );
    }

    #[test]
    fn group_grant() {
        assert_eq!(
            Osp::from_tsbk(&tsbk("05e100004436d1070acb6654")),
            Osp::GroupVoiceGrant {
                options: ServiceOptions::new(0x44),
                channel: ChannelId::new(0x36d1),
                group: 0x070a,
                source: 0xcb_6654,
            }
        );
    }

    #[test]
    fn denied_affiliations_are_marked() {
        let Osp::GroupAffiliation { response, .. } =
            Osp::from_tsbk(&tsbk("05e128000209f609f7cb0afe"))
        else {
            panic!()
        };
        assert_eq!(response, Response::Deny);
    }

    #[test]
    fn unit_grants_have_no_service_options() {
        // Octet 2 is the channel's top byte; 0x47 would read as encrypted,
        // and 0xc7 as an emergency too, in trunk-recorder.
        let Osp::UnitVoiceGrant {
            options, channel, ..
        } = Osp::from_tsbk(&tsbk("05e10400c7ec000123000456"))
        else {
            panic!()
        };
        assert_eq!(
            (options, channel),
            (ServiceOptions::NONE, ChannelId::new(0xc7ec))
        );
    }

    #[test]
    fn vendor_opcodes_are_not_read_as_standard() {
        assert!(matches!(
            Osp::from_tsbk(&tsbk("05e104900016a0ffffc7e52f")),
            Osp::Other {
                opcode: 4,
                mfid: 0x90
            }
        ));
        assert!(matches!(
            Osp::from_tsbk(&tsbk("05e1440000c016a0ffff0001")),
            Osp::Protected { opcode: 4, .. }
        ));
    }

    #[test]
    fn motorola_patch() {
        assert_eq!(
            Osp::from_tsbk(&tsbk("05e180901290129012901290")),
            Osp::MotPatchAdd {
                supergroup: 0x1290,
                groups: [0x1290; 3],
            }
        );
    }
}
