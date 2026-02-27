#[allow(clippy::all, clippy::pedantic, clippy::nursery)]
mod generated {
    tonic::include_proto!("trunk_oxide");
}
pub use generated::*;

pub mod types;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    /// Check that encoded_len() matches actual encode() output for a message.
    fn check_encoded_len(msg: &impl Message, label: &str) {
        let predicted = msg.encoded_len();
        let mut buf = Vec::new();
        msg.encode(&mut buf).unwrap();
        assert_eq!(
            predicted,
            buf.len(),
            "{label}: encoded_len()={predicted} but encode() wrote {} bytes",
            buf.len()
        );
    }

    #[test]
    fn trunk_message_encoded_len_basic() {
        let msg = TrunkMessage {
            message_type: MessageType::Grant as i32,
            meta: "2C".to_string(),
            freq: 772_406_250.0,
            talkgroup: 1001,
            encrypted: false,
            emergency: false,
            duplex: false,
            mode: false,
            priority: 0,
            tdma_slot: 0,
            phase2_tdma: false,
            source: -1,
            sys_num: 1,
            sys_id: 0,
            rfss: 1,
            site_id: 1,
            nac: 0,
            wacn: 0,
            patch_data: Some(PatchData {
                sg: 0,
                ga1: 0,
                ga2: 0,
                ga3: 0,
            }),
            opcode: 0,
        };
        check_encoded_len(&msg, "TrunkMessage");
    }

    #[test]
    fn trunk_message_encoded_len_with_patch_data() {
        let msg = TrunkMessage {
            message_type: MessageType::PatchAdd as i32,
            meta: String::new(),
            freq: 0.0,
            talkgroup: 500,
            encrypted: false,
            emergency: false,
            duplex: false,
            mode: false,
            priority: 3,
            tdma_slot: 0,
            phase2_tdma: false,
            source: 12345,
            sys_num: 2,
            sys_id: 0x1234,
            rfss: 1,
            site_id: 1,
            nac: 0x5AB,
            wacn: 0xBEEF0,
            patch_data: Some(PatchData {
                sg: 100,
                ga1: 200,
                ga2: 300,
                ga3: 0,
            }),
            opcode: 42,
        };
        check_encoded_len(&msg, "TrunkMessage with patch data");
    }

    #[test]
    fn trunk_message_event_encoded_len() {
        let msgs: Vec<TrunkMessage> = (0..5)
            .map(|i| TrunkMessage {
                message_type: MessageType::Grant as i32,
                meta: format!("{i}C"),
                freq: 772_000_000.0 + i as f64 * 100_000.0,
                talkgroup: 1000 + i,
                encrypted: i % 2 == 0,
                emergency: false,
                duplex: false,
                mode: false,
                priority: i as i32,
                tdma_slot: 0,
                phase2_tdma: false,
                source: -1,
                sys_num: 1,
                sys_id: 0,
                rfss: 1,
                site_id: 1,
                nac: 0,
                wacn: 0,
                patch_data: Some(PatchData {
                    sg: 0,
                    ga1: 0,
                    ga2: 0,
                    ga3: 0,
                }),
                opcode: 0,
            })
            .collect();

        let event = TrunkMessageEvent {
            messages: msgs,
            system: Some(SystemInfo {
                sys_num: 1,
                sys_id: 0,
                short_name: "smco".to_string(),
                system_type: SystemType::P25 as i32,
                wacn: 0,
                nac: 0,
                rfss: 1,
                site_id: 1,
                bandplan: String::new(),
                analog_levels: 0.0,
                digital_levels: 1.0,
                squelch_db: -50.0,
                conversation_mode: false,
                message_count: 42,
                decode_rate: 10,
            }),
        };
        check_encoded_len(&event, "TrunkMessageEvent");

        // Also check wrapped in PluginEvent
        let plugin_event = PluginEvent {
            event: Some(plugin_event::Event::TrunkMessage(event)),
        };
        check_encoded_len(&plugin_event, "PluginEvent(TrunkMessage)");
    }

    #[test]
    fn plugin_event_all_variants_encoded_len() {
        let events: Vec<(&str, PluginEvent)> = vec![
            (
                "Signal",
                PluginEvent {
                    event: Some(plugin_event::Event::Signal(SignalEvent {
                        unit_id: 12345,
                        signaling_type: "normal".to_string(),
                        signal_type: SignalType::Normal as i32,
                        call: Some(CallInfo {
                            talkgroup: 1001,
                            call_num: 42,
                            sys_num: 1,
                            freq: 772_406_250.0,
                            ..Default::default()
                        }),
                        system: Some(SystemInfo {
                            sys_num: 1,
                            short_name: "smco".to_string(),
                            ..Default::default()
                        }),
                        recorder: None,
                    })),
                },
            ),
            (
                "CallStart",
                PluginEvent {
                    event: Some(plugin_event::Event::CallStart(CallStartEvent {
                        call: Some(CallInfo {
                            talkgroup: 1001,
                            call_num: 1,
                            sys_num: 1,
                            freq: 772_406_250.0,
                            short_name: "smco".to_string(),
                            ..Default::default()
                        }),
                    })),
                },
            ),
            (
                "SystemRates",
                PluginEvent {
                    event: Some(plugin_event::Event::SystemRates(SystemRatesEvent {
                        systems: vec![SystemInfo {
                            sys_num: 1,
                            short_name: "smco".to_string(),
                            decode_rate: 42,
                            ..Default::default()
                        }],
                        time_diff: 1.5,
                    })),
                },
            ),
        ];

        for (name, event) in &events {
            check_encoded_len(event, name);
        }
    }

    /// Test edge-case values that might come from C++ FFI: negative int64,
    /// large uint64, special floats, long strings, etc.
    #[test]
    fn trunk_message_encoded_len_edge_cases() {
        let cases: Vec<(&str, TrunkMessage)> = vec![
            (
                "source=-1 (10-byte varint)",
                TrunkMessage {
                    source: -1,
                    talkgroup: 1001,
                    freq: 772_406_250.0,
                    message_type: MessageType::Grant as i32,
                    sys_num: 1,
                    patch_data: Some(PatchData::default()),
                    ..Default::default()
                },
            ),
            (
                "large uint64 values",
                TrunkMessage {
                    sys_id: 0xFFFF_FFFF_FFFF_FFFF,
                    nac: 0xBEEF_DEAD_CAFE_BABE,
                    wacn: 0x1234_5678_9ABC_DEF0,
                    opcode: u64::MAX,
                    talkgroup: i64::MAX,
                    source: i64::MIN,
                    patch_data: Some(PatchData {
                        sg: u64::MAX,
                        ga1: u64::MAX,
                        ga2: u64::MAX,
                        ga3: u64::MAX,
                    }),
                    ..Default::default()
                },
            ),
            (
                "long meta string (hex P25 message)",
                TrunkMessage {
                    meta: "A".repeat(500),
                    freq: 772_406_250.0,
                    message_type: MessageType::Grant as i32,
                    talkgroup: 1001,
                    source: -1,
                    sys_num: 1,
                    sys_id: 0x1234,
                    nac: 0x5AB,
                    wacn: 0xBEEF0,
                    patch_data: Some(PatchData::default()),
                    ..Default::default()
                },
            ),
            (
                "all defaults (zero values)",
                TrunkMessage::default(),
            ),
            (
                "all bools true",
                TrunkMessage {
                    encrypted: true,
                    emergency: true,
                    duplex: true,
                    mode: true,
                    phase2_tdma: true,
                    patch_data: Some(PatchData::default()),
                    ..Default::default()
                },
            ),
            (
                "special floats",
                TrunkMessage {
                    freq: f64::NAN,
                    talkgroup: 1,
                    patch_data: Some(PatchData::default()),
                    ..Default::default()
                },
            ),
            (
                "negative zero freq",
                TrunkMessage {
                    freq: -0.0,
                    talkgroup: 1,
                    patch_data: Some(PatchData::default()),
                    ..Default::default()
                },
            ),
            (
                "high tag-number fields only",
                TrunkMessage {
                    nac: 0x5AB,
                    wacn: 0xBEEF0,
                    opcode: 42,
                    patch_data: Some(PatchData { sg: 100, ga1: 200, ga2: 300, ga3: 400 }),
                    ..Default::default()
                },
            ),
        ];

        for (label, msg) in &cases {
            check_encoded_len(msg, label);

            // Also wrap in TrunkMessageEvent and PluginEvent
            let event = TrunkMessageEvent {
                messages: vec![msg.clone()],
                system: Some(SystemInfo {
                    sys_num: 1,
                    short_name: "test".to_string(),
                    ..Default::default()
                }),
            };
            check_encoded_len(&event, &format!("Event({label})"));

            let plugin_event = PluginEvent {
                event: Some(plugin_event::Event::TrunkMessage(event)),
            };
            check_encoded_len(&plugin_event, &format!("PluginEvent({label})"));
        }
    }

    /// Test batch of multiple TrunkMessages (mimics real trunk_message callback
    /// which sends a batch of messages for each control channel decode).
    #[test]
    fn trunk_message_batch_encoded_len() {
        let messages: Vec<TrunkMessage> = (0..10)
            .map(|i| TrunkMessage {
                message_type: if i % 3 == 0 {
                    MessageType::Grant as i32
                } else {
                    MessageType::Status as i32
                },
                meta: format!("{:02X}", i * 17),
                freq: 770_000_000.0 + i as f64 * 500_000.0,
                talkgroup: 1000 + i,
                source: if i % 2 == 0 { -1 } else { 10000 + i },
                sys_num: 1,
                sys_id: 0x1234,
                rfss: 1,
                site_id: 1,
                nac: 0x5AB,
                wacn: 0xBEEF0,
                priority: i as i32 % 5,
                encrypted: i % 4 == 0,
                patch_data: Some(PatchData {
                    sg: if i > 5 { 100 } else { 0 },
                    ga1: if i > 5 { 200 + i as u64 } else { 0 },
                    ga2: 0,
                    ga3: 0,
                }),
                opcode: i as u64 * 7,
                ..Default::default()
            })
            .collect();

        let event = TrunkMessageEvent {
            messages,
            system: Some(SystemInfo {
                sys_num: 1,
                sys_id: 0x1234,
                short_name: "smco".to_string(),
                system_type: SystemType::P25 as i32,
                wacn: 0xBEEF0,
                nac: 0x5AB,
                rfss: 1,
                site_id: 1,
                message_count: 500,
                decode_rate: 15,
                ..Default::default()
            }),
        };
        check_encoded_len(&event, "TrunkMessageEvent batch of 10");

        let plugin_event = PluginEvent {
            event: Some(plugin_event::Event::TrunkMessage(event)),
        };
        check_encoded_len(&plugin_event, "PluginEvent(batch of 10)");
    }
}
