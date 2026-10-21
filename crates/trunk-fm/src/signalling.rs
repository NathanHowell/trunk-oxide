//! In-band signalling heard in an analog channel's audio: what the audio-rate
//! decoders (MDC-1200, DTMF, ...) report, and nothing about how they hear it.
//!
//! A decoder says one [`Heard`] per thing it hears: a [`Signal`] and the span
//! of audio it took. Everything downstream is a total function of the
//! `Signal`: who it names ([`Signal::unit`]), whether it asks for attention
//! ([`Signal::alert`]), and how it reads ([`fmt::Display`]). A decoder that
//! joins the enum is a new variant and a new arm in each of those; the
//! compiler finds the rest. What a call shows of the signals it carried
//! (the digits dialled, a sequence of tones) is a fold over its `Heard`s
//! ([`fold`]), not something the decoders assemble. The fold's [`Line`]s are
//! the erased form that leaves this crate: consumers need no knowledge of any
//! scheme.

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
    /// A message of a scheme whose messages are not all named, that is not
    /// one of the ones that are: what a radio sends routinely is known, so
    /// anything else is worth a look.
    Unrecognised,
}

impl Alert {
    /// For metric labels and logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Emergency => "emergency",
            Self::RadioCheck => "radio_check",
            Self::CallAlert => "call_alert",
            Self::Unrecognised => "unrecognised",
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
    /// A `FleetSync` message.
    FleetSync(crate::fleetsync::Message),
    /// A page of two tones, one straight after the other. Frequencies are in
    /// tenths of a hertz, the resolution they are specified to.
    TwoTone {
        first_dhz: u32,
        second_dhz: u32,
        first_ms: u32,
        second_ms: u32,
    },
    /// One tone held for seconds.
    LongTone { dhz: u32, ms: u32 },
    /// Two tones taking turns, fast.
    Warble {
        high_dhz: u32,
        low_dhz: u32,
        ms: u32,
    },
}

impl Signal {
    /// The decoder it came from, for metric labels and trace attributes.
    #[must_use]
    pub const fn scheme(&self) -> &'static str {
        match self {
            Self::Mdc { .. } => "mdc1200",
            Self::Dtmf(_) => "dtmf",
            Self::FleetSync(_) => "fleetsync",
            Self::TwoTone { .. } => "two_tone",
            Self::LongTone { .. } => "long_tone",
            Self::Warble { .. } => "warble",
        }
    }

    /// The radio it names, if the scheme carries one.
    #[must_use]
    pub const fn unit(&self) -> Option<u32> {
        match self {
            Self::Mdc { unit, .. } => Some(*unit),
            Self::FleetSync(message) if message.from.is_set() => Some(message.from.key()),
            Self::Dtmf(_)
            | Self::FleetSync(_)
            | Self::TwoTone { .. }
            | Self::LongTone { .. }
            | Self::Warble { .. } => None,
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
            Self::FleetSync(message) if !message.is_routine() => Some(Alert::Unrecognised),
            Self::Mdc { .. }
            | Self::Dtmf(_)
            | Self::FleetSync(_)
            | Self::TwoTone { .. }
            | Self::LongTone { .. }
            | Self::Warble { .. } => None,
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
                write!(f, "MDC-1200 unit {unit:04X} {op}")?;
                // The argument of a PTT ID tells pre- from post-amble, which
                // is no news worth a line of its own.
                if *op == MdcOp::PttId {
                    Ok(())
                } else {
                    write!(f, " (arg 0x{arg:02x})")
                }
            }
            Self::Dtmf(key) => write!(f, "DTMF {key}"),
            Self::FleetSync(message) => message.fmt(f),
            Self::TwoTone {
                first_dhz,
                second_dhz,
                first_ms,
                second_ms,
            } => write!(
                f,
                "Two-tone {} then {} ({}, {})",
                hz(*first_dhz),
                hz(*second_dhz),
                seconds(*first_ms),
                seconds(*second_ms)
            ),
            Self::LongTone { dhz, ms } => write!(f, "Tone {} for {}", hz(*dhz), seconds(*ms)),
            Self::Warble {
                high_dhz,
                low_dhz,
                ms,
            } => write!(
                f,
                "Warble {}/{} for {}",
                hz(*high_dhz),
                hz(*low_dhz),
                seconds(*ms)
            ),
        }
    }
}

/// A frequency given in tenths of a hertz, as it reads.
fn hz(dhz: u32) -> String {
    format!("{}.{} Hz", dhz / 10, dhz % 10)
}

/// A duration given in milliseconds, as it reads.
fn seconds(ms: u32) -> String {
    format!("{}.{} s", ms / 1_000, ms % 1_000 / 100)
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

/// One line of what a call carried, in a form no consumer need interpret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// As [`Signal::scheme`].
    pub scheme: &'static str,
    /// How it reads.
    pub text: String,
    /// The radio it names, if it does.
    pub unit: Option<u32>,
    pub alert: Option<Alert>,
}

/// What a call shows of the signals it carried, in the order heard:
/// consecutive DTMF keys read as one number, every other signal as itself.
#[must_use]
pub fn fold(heard: &[Heard]) -> Vec<Line> {
    fn flush(keys: &mut String, lines: &mut Vec<Line>) {
        if !keys.is_empty() {
            lines.push(Line {
                scheme: Signal::Dtmf(' ').scheme(),
                text: format!("DTMF {keys}"),
                unit: None,
                alert: None,
            });
            keys.clear();
        }
    }
    let mut lines = Vec::new();
    let mut keys = String::new();
    for h in heard {
        if let Signal::Dtmf(key) = h.signal {
            keys.push(key);
            continue;
        }
        flush(&mut keys, &mut lines);
        lines.push(Line {
            scheme: h.signal.scheme(),
            text: h.signal.to_string(),
            unit: h.signal.unit(),
            alert: h.signal.alert(),
        });
    }
    flush(&mut keys, &mut lines);
    lines
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
        assert_eq!(MDC(MdcOp::PttId).to_string(), "MDC-1200 unit 1234 PTT ID");
        assert_eq!(
            MDC(MdcOp::Other(0x61)).to_string(),
            "MDC-1200 unit 1234 op 0x61 (arg 0x00)"
        );
        assert_eq!(Signal::Dtmf('*').to_string(), "DTMF *");
        assert_eq!(MDC(MdcOp::PttId).scheme(), "mdc1200");
        assert_eq!(Signal::Dtmf('*').scheme(), "dtmf");
    }
    fn heard(signal: Signal) -> Heard {
        Heard { span: 0..1, signal }
    }

    #[test]
    fn dtmf_keys_in_a_row_read_as_one_number() {
        let log = [
            heard(Signal::Dtmf('5')),
            heard(Signal::Dtmf('5')),
            heard(Signal::Dtmf('1')),
            heard(MDC(MdcOp::PttId)),
            heard(Signal::Dtmf('#')),
        ];
        let lines: Vec<_> = fold(&log).into_iter().map(|l| l.text).collect();
        assert_eq!(lines, ["DTMF 551", "MDC-1200 unit 1234 PTT ID", "DTMF #"]);
    }

    #[test]
    fn a_line_carries_what_its_signal_says() {
        let lines = fold(&[heard(MDC(MdcOp::Emergency)), heard(Signal::Dtmf('1'))]);
        assert_eq!(
            lines[0],
            Line {
                scheme: "mdc1200",
                text: "MDC-1200 unit 1234 emergency (arg 0x00)".into(),
                unit: Some(0x1234),
                alert: Some(Alert::Emergency),
            }
        );
        assert_eq!((lines[1].unit, lines[1].alert), (None, None));
        assert_eq!(fold(&[]), Vec::<Line>::new());
    }
    #[test]
    fn a_fleetsync_message_names_its_sender_as_an_alias_key() {
        use crate::fleetsync::{Id, Message};
        let id = |fleet, unit| Id { fleet, unit };
        let message = Message {
            kind: 0x80,
            code: 0xFC,
            from: id(120, 2182),
            to: id(120, 2182),
        };
        let signal = Signal::FleetSync(message);
        assert_eq!(signal.unit(), Some(1_202_182));
        assert_eq!(signal.alert(), None);
        assert_eq!(signal.scheme(), "fleetsync");
        assert_eq!(
            signal.to_string(),
            "FleetSync 120-2182 (type 0x80, code 0xfc)"
        );
        let to_another = Signal::FleetSync(Message {
            to: id(121, 1001),
            ..message
        });
        assert_eq!(
            to_another.to_string(),
            "FleetSync 120-2182 to 121-1001 (type 0x80, code 0xfc)"
        );
        // a packet that names nobody attributes nobody
        let anonymous = Signal::FleetSync(Message {
            from: id(99, 999),
            ..message
        });
        assert_eq!(anonymous.unit(), None);
    }
    #[test]
    fn a_fleetsync_message_that_is_not_routine_is_flagged_unrecognised() {
        use crate::fleetsync::{Id, Message};
        let id = Id {
            fleet: 120,
            unit: 2182,
        };
        let message = |kind, code| {
            Signal::FleetSync(Message {
                kind,
                code,
                from: id,
                to: id,
            })
        };
        assert_eq!(message(0x80, 0xFC).alert(), None);
        for (kind, code) in [(0x80, 0x00), (0x20, 0xFC), (0x42, 0x10)] {
            assert_eq!(message(kind, code).alert(), Some(Alert::Unrecognised));
        }
        let line = &fold(&[
            Heard {
                span: 0..1,
                signal: message(0x20, 0x00),
            },
            Heard {
                span: 2..3,
                signal: message(0x80, 0xFC),
            },
        ]);
        assert_eq!(line[0].alert, Some(Alert::Unrecognised));
        assert_eq!(line[1].alert, None);
        assert_eq!(Alert::Unrecognised.as_str(), "unrecognised");
    }
}
