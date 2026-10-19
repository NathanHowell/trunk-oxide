//! Protocol value types.

use std::fmt;

/// Network Access Code: 12 bits identifying a P25 system on a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Nac(u16);

impl Nac {
    /// NAC 0 never identifies a system (op25 drops such frames).
    pub const fn new(v: u16) -> Option<Self> {
        if v == 0 || v > 0xfff {
            None
        } else {
            Some(Self(v))
        }
    }

    pub const fn get(self) -> u16 {
        self.0
    }

    pub const fn to_be_bytes(self) -> [u8; 2] {
        self.0.to_be_bytes()
    }
}

impl fmt::Display for Nac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:03x}", self.0)
    }
}

/// One trunking signalling block: 10 bytes, CRC already checked and removed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tsbk([u8; 10]);

impl Tsbk {
    /// Last Block flag: octet 0, bit 7 (TIA-102.AABB-B §5.1).
    pub const LB: u8 = 0x80;
    /// Protected trunking block flag: octet 0, bit 6 (TIA-102.AABB-B §5.1).
    pub const P: u8 = 0x40;
    /// Opcode: octet 0, bits 5–0 (TIA-102.AABB-B §5.1).
    pub const OPCODE: u8 = 0x3f;

    pub const fn new(bytes: [u8; 10]) -> Self {
        Self(bytes)
    }

    pub const fn bytes(&self) -> &[u8; 10] {
        &self.0
    }

    /// Last block of its TSDU.
    pub const fn is_last(&self) -> bool {
        self.0[0] & Self::LB != 0
    }

    pub const fn is_protected(&self) -> bool {
        self.0[0] & Self::P != 0
    }

    pub const fn opcode(&self) -> u8 {
        self.0[0] & Self::OPCODE
    }

    pub const fn manufacturer(&self) -> u8 {
        self.0[1]
    }
}

impl fmt::Debug for Tsbk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tsbk(op {:02x}:", self.opcode())?;
        for b in &self.0 {
            write!(f, " {b:02x}")?;
        }
        write!(f, ")")
    }
}

/// How hard the decoder had to work for a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Recovery {
    /// Received without a symbol error.
    Clean,
    /// The Viterbi path corrected this many dibits.
    Corrected(u32),
    /// The best path failed its CRC; the list decoder's `rank`-th path passed.
    List { rank: u8, corrected: u32 },
}

impl Recovery {
    pub const fn corrected(self) -> u32 {
        match self {
            Self::Clean => 0,
            Self::Corrected(n) | Self::List { corrected: n, .. } => n,
        }
    }
}
