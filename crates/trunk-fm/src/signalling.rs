//! In-band signalling heard in an analog channel's audio: what the audio-rate
//! decoders (MDC-1200, DTMF, ...) report, and nothing about how they hear it.
//!
//! A decoder says one [`Heard`] per thing it hears: a [`Signal`] and the span
//! of audio it took. Everything downstream is a total function of the
//! `Signal`: who it names ([`Signal::unit`]), whether it asks for attention
//! ([`Signal::alert`]), and how it reads ([`fmt::Display`]). A decoder that
//! joins the enum is a new variant and a new arm in each of those; the
//! compiler finds the rest. What a call shows of the signals it carried
//! (the digits dialled, a sequence of tones) is a fold over its `Heard`s,
//! not something the decoders assemble.

use std::fmt;
use std::ops::Range;

/// What an MDC-1200 packet asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MdcOp {
    /// The handset identifying itself at key-up or key-down.
    PttId,
    Emergency,
    RadioCheck,
    CallAlert,
    /// An opcode with no name here.
    Other(u8),
}

/// Why a call is worth a second look.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Alert {
    Emergency,
    RadioCheck,
    CallAlert,
}

impl Alert {
    /// For metric labels and logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Emergency => "emergency",
            Self::RadioCheck => "radio_check",
            Self::CallAlert => "call_alert",
        }
    }
}

/// One thing heard in the audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// An MDC-1200 packet from `unit`.
    Mdc { unit: u32, op: MdcOp, arg: u8 },
    /// A DTMF key.
    Dtmf(char),
}

impl Signal {
    /// The decoder it came from, for metric labels and trace attributes.
    #[must_use]
    pub const fn scheme(&self) -> &'static str {
        match self {
            Self::Mdc { .. } => "mdc1200",
            Self::Dtmf(_) => "dtmf",
        }
    }

    /// The radio it names, if the scheme carries one.
    #[must_use]
    pub const fn unit(&self) -> Option<u32> {
        match self {
            Self::Mdc { unit, .. } => Some(*unit),
            Self::Dtmf(_) => None,
        }
    }

    /// What it asks of whoever is watching, if anything.
    #[must_use]
    pub const fn alert(&self) -> Option<Alert> {
        match self {
            Self::Mdc {
                op: MdcOp::Emergency,
                ..
            } => Some(Alert::Emergency),
            Self::Mdc {
                op: MdcOp::RadioCheck,
                ..
            } => Some(Alert::RadioCheck),
            Self::Mdc {
                op: MdcOp::CallAlert,
                ..
            } => Some(Alert::CallAlert),
            Self::Mdc { .. } | Self::Dtmf(_) => None,
        }
    }
}

impl fmt::Display for MdcOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PttId => f.write_str("PTT ID"),
            Self::Emergency => f.write_str("emergency"),
            Self::RadioCheck => f.write_str("radio check"),
            Self::CallAlert => f.write_str("call alert"),
            Self::Other(op) => write!(f, "op 0x{op:02x}"),
        }
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mdc { unit, op, arg } => {
                write!(f, "MDC-1200 unit {unit:04X} {op} (arg 0x{arg:02x})")
            }
            Self::Dtmf(key) => write!(f, "DTMF {key}"),
        }
    }
}

/// A [`Signal`] and where it was heard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heard {
    /// The samples it took, counted in the audio the decoder was fed since
    /// it was last reset, at [`crate::AUDIO_RATE`].
    pub span: Range<u64>,
    pub signal: Signal,
}

impl Heard {
    /// How far into the decoder's audio it began.
    #[must_use]
    pub fn starts_at(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.span.start as f64 / crate::AUDIO_RATE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MDC: fn(MdcOp) -> Signal = |op| Signal::Mdc {
        unit: 0x1234,
        op,
        arg: 0,
    };

    #[test]
    fn an_mdc_packet_names_its_unit_whatever_it_asks() {
        for op in [MdcOp::PttId, MdcOp::Emergency, MdcOp::Other(0x7f)] {
            assert_eq!(MDC(op).unit(), Some(0x1234));
        }
        assert_eq!(Signal::Dtmf('5').unit(), None);
    }

    #[test]
    fn only_the_ops_that_ask_for_attention_alert() {
        assert_eq!(MDC(MdcOp::PttId).alert(), None);
        assert_eq!(MDC(MdcOp::Other(9)).alert(), None);
        assert_eq!(MDC(MdcOp::Emergency).alert(), Some(Alert::Emergency));
        assert_eq!(MDC(MdcOp::RadioCheck).alert(), Some(Alert::RadioCheck));
        assert_eq!(MDC(MdcOp::CallAlert).alert(), Some(Alert::CallAlert));
        assert_eq!(Signal::Dtmf('#').alert(), None);
    }

    #[test]
    fn signals_read_as_their_scheme_would() {
        assert_eq!(
            MDC(MdcOp::PttId).to_string(),
            "MDC-1200 unit 1234 PTT ID (arg 0x00)"
        );
        assert_eq!(Signal::Dtmf('*').to_string(), "DTMF *");
        assert_eq!(MDC(MdcOp::PttId).scheme(), "mdc1200");
        assert_eq!(Signal::Dtmf('*').scheme(), "dtmf");
    }
}
