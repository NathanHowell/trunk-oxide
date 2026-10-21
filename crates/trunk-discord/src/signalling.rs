//! How a transmission's in-band signalling reads on an embed: a fold over
//! what the decoders heard, in order.

use trunk_oxide_proto as proto;
use trunk_oxide_proto::in_band_signal::Signal;

/// One line of the "Signalling" field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    /// Whether it asks for attention (an emergency, a radio check, a call
    /// alert), so the embed can flag the call without anyone listening.
    pub alert: bool,
}

/// The lines for `heard`: consecutive DTMF keys read as one number, every
/// other signal as itself.
pub fn fold(heard: &[proto::InBandSignal]) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    let mut keys = String::new();
    let flush = |keys: &mut String, lines: &mut Vec<Line>| {
        if !keys.is_empty() {
            lines.push(Line {
                text: format!("DTMF {keys}"),
                alert: false,
            });
            keys.clear();
        }
    };
    for signal in heard.iter().filter_map(|h| h.signal.as_ref()) {
        match signal {
            Signal::Dtmf(key) => keys.push_str(&key.key),
            Signal::Mdc(packet) => {
                flush(&mut keys, &mut lines);
                let op = match packet.op() {
                    proto::MdcOp::PttId => "PTT ID".to_owned(),
                    proto::MdcOp::Emergency => "emergency".to_owned(),
                    proto::MdcOp::RadioCheck => "radio check".to_owned(),
                    proto::MdcOp::CallAlert => "call alert".to_owned(),
                    proto::MdcOp::Other => format!("op 0x{:02x}", packet.other_op),
                };
                lines.push(Line {
                    text: format!("MDC-1200 unit {:04X} {op}", packet.unit),
                    alert: matches!(
                        packet.op(),
                        proto::MdcOp::Emergency
                            | proto::MdcOp::RadioCheck
                            | proto::MdcOp::CallAlert
                    ),
                });
            }
        }
    }
    flush(&mut keys, &mut lines);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dtmf(key: &str) -> proto::InBandSignal {
        proto::InBandSignal {
            signal: Some(Signal::Dtmf(proto::DtmfKey { key: key.into() })),
        }
    }

    fn mdc(unit: u32, op: proto::MdcOp, other_op: u32) -> proto::InBandSignal {
        proto::InBandSignal {
            signal: Some(Signal::Mdc(proto::MdcPacket {
                unit,
                op: op.into(),
                other_op,
                arg: 0,
            })),
        }
    }

    #[test]
    fn dtmf_keys_in_a_row_read_as_one_number() {
        let heard = [
            dtmf("5"),
            dtmf("5"),
            dtmf("1"),
            mdc(1, proto::MdcOp::PttId, 0),
            dtmf("#"),
        ];
        let lines: Vec<_> = fold(&heard).into_iter().map(|l| l.text).collect();
        assert_eq!(lines, ["DTMF 551", "MDC-1200 unit 0001 PTT ID", "DTMF #"]);
    }

    #[test]
    fn only_the_ops_that_ask_for_attention_alert() {
        let heard = [
            mdc(0x1234, proto::MdcOp::PttId, 0),
            mdc(0x1234, proto::MdcOp::Emergency, 0),
            mdc(0x1234, proto::MdcOp::Other, 0x7f),
            dtmf("1"),
        ];
        let alerts: Vec<_> = fold(&heard).into_iter().map(|l| l.alert).collect();
        assert_eq!(alerts, [false, true, false, false]);
        assert_eq!(fold(&heard)[2].text, "MDC-1200 unit 1234 op 0x7f");
    }

    #[test]
    fn a_signal_from_a_newer_peer_is_skipped() {
        let unknown = proto::InBandSignal { signal: None };
        assert!(fold(&[unknown]).is_empty());
    }
}
