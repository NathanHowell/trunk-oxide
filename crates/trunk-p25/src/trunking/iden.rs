//! Channel plans: how a 16-bit channel number becomes a frequency.
//!
//! A channel number is a 4-bit identifier and a 12-bit channel. Each site
//! broadcasts what its identifiers mean (`IDEN_UP`, `IDEN_UP_VU`, `IDEN_UP_TDMA`):
//! a base frequency, a channel spacing, a transmit offset and, for TDMA, how
//! many slots share a carrier.

use std::fmt;

/// A channel as grants and status broadcasts name it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChannelId(u16);

impl ChannelId {
    /// Channel Number: the low 12 bits; the Channel Identifier is the top 4
    /// (TIA-102.AABC-B §2.3.9, Figure 2.3.9-1).
    pub const NUMBER_BITS: u32 = 12;

    pub const fn new(v: u16) -> Self {
        Self(v)
    }

    pub const fn get(self) -> u16 {
        self.0
    }

    pub const fn iden(self) -> Iden {
        Iden((self.0 >> Self::NUMBER_BITS) as u8)
    }

    pub const fn number(self) -> u16 {
        self.0 & ((1 << Self::NUMBER_BITS) - 1)
    }
}

impl fmt::Debug for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}-{:04}", self.iden().0, self.number())
    }
}

/// A channel identifier (0–15).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Iden(u8);

impl Iden {
    pub const fn new(v: u8) -> Self {
        Self(v & 15)
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    Fdma,
    /// TDMA with this many slots per carrier.
    Tdma {
        slots: u8,
    },
}

/// What one identifier means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChannelPlan {
    pub base_hz: u64,
    pub spacing_hz: u32,
    /// Mobile transmit offset.
    pub tx_offset_hz: i64,
    pub bandwidth_hz: u32,
    pub access: Access,
}

/// A channel number resolved against a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Channel {
    pub id: ChannelId,
    pub freq_hz: u64,
    /// The TDMA slot, on a TDMA identifier.
    pub slot: Option<u8>,
}

impl ChannelPlan {
    /// SU receive frequency = Base Frequency + Channel Number × Channel
    /// Spacing (TIA-102.AABC-B §2.3.9.2); a TDMA carrier holds `slots`
    /// consecutive channel numbers.
    pub const fn resolve(&self, id: ChannelId) -> Channel {
        let n = id.number() as u64;
        match self.access {
            Access::Fdma => Channel {
                id,
                freq_hz: self.base_hz + self.spacing_hz as u64 * n,
                slot: None,
            },
            #[allow(clippy::cast_possible_truncation)] // below `slots`
            Access::Tdma { slots } => Channel {
                id,
                freq_hz: self.base_hz + self.spacing_hz as u64 * (n / slots as u64),
                slot: Some((n % slots as u64) as u8),
            },
        }
    }

    /// `IDEN_UP_TDMA`'s channel types: access (slots per carrier as op25 and
    /// trunk-recorder have them, 1 1 1 2 4 2) and bandwidth (informational
    /// only). Types 6–15 are reserved; trunk-recorder indexes its six-entry
    /// table with them anyway.
    pub const fn tdma_channel_type(t: u8) -> Option<(Access, u32)> {
        Some(match t {
            0 | 2 => (Access::Fdma, 6_250),
            1 => (Access::Fdma, 12_500),
            3 | 5 => (Access::Tdma { slots: 2 }, 12_500),
            4 => (Access::Tdma { slots: 4 }, 25_000),
            _ => return None,
        })
    }
}

/// The sixteen identifiers as one site has announced them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdenTable([Option<ChannelPlan>; 16]);

impl IdenTable {
    pub const fn get(&self, iden: Iden) -> Option<&ChannelPlan> {
        self.0[iden.0 as usize].as_ref()
    }

    /// Records a plan; returns the one it replaced if that differed.
    pub fn set(&mut self, iden: Iden, plan: ChannelPlan) -> Option<ChannelPlan> {
        self.0[iden.0 as usize]
            .replace(plan)
            .filter(|old| *old != plan)
    }

    pub fn resolve(&self, id: ChannelId) -> Option<Channel> {
        self.get(id.iden()).map(|p| p.resolve(id))
    }

    pub fn len(&self) -> usize {
        self.0.iter().flatten().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FDMA: ChannelPlan = ChannelPlan {
        base_hz: 851_006_250,
        spacing_hz: 6_250,
        tx_offset_hz: -45_000_000,
        bandwidth_hz: 12_500,
        access: Access::Fdma,
    };

    #[test]
    fn fdma_channels_step_by_the_spacing() {
        let c = FDMA.resolve(ChannelId::new(0x1010));
        assert_eq!((c.freq_hz, c.slot), (851_006_250 + 16 * 6_250, None));
    }

    #[test]
    fn tdma_channels_share_a_carrier() {
        let plan = |slots| ChannelPlan {
            access: Access::Tdma { slots },
            ..FDMA
        };
        let two: Vec<_> = (0..4).map(|n| plan(2).resolve(ChannelId::new(n))).collect();
        assert_eq!(two[0].freq_hz, two[1].freq_hz);
        assert_eq!(two[2].freq_hz, FDMA.base_hz + 6_250);
        assert_eq!(
            two.iter().map(|c| c.slot).collect::<Vec<_>>(),
            [Some(0), Some(1), Some(0), Some(1)]
        );
        // trunk-recorder takes the slot as `channel & 1` whatever the slot
        // count; four-slot carriers need the remainder.
        assert_eq!(plan(4).resolve(ChannelId::new(6)).slot, Some(2));
        assert_eq!(
            plan(4).resolve(ChannelId::new(6)).freq_hz,
            FDMA.base_hz + 6_250
        );
    }

    #[test]
    fn reserved_channel_types_have_no_plan() {
        assert!(ChannelPlan::tdma_channel_type(5).is_some());
        assert!((6..16).all(|t| ChannelPlan::tdma_channel_type(t).is_none()));
    }

    #[test]
    fn set_reports_changes_only() {
        let mut t = IdenTable::default();
        assert_eq!(t.set(Iden::new(1), FDMA), None);
        assert_eq!(t.set(Iden::new(1), FDMA), None);
        let moved = ChannelPlan { base_hz: 1, ..FDMA };
        assert_eq!(t.set(Iden::new(1), moved), Some(FDMA));
        assert_eq!(t.len(), 1);
    }
}
