use std::collections::HashMap;
use std::fmt::Write;

use trunk_oxide_proto as proto;

use crate::call_state::EmbedData;
use crate::call_state::EmbedField;

// ── Effects ──────────────────────────────────────────────────────────────────

/// Side-effect commands returned by [`AggregatorState`].
#[derive(Debug)]
pub enum AggEffect {
    /// Post a new embed into the current hourly thread.
    PostEmbed { embed: EmbedData },
    /// Edit an existing pinned embed (recorder table or activity summary).
    EditEmbed {
        target: EmbedTarget,
        embed: EmbedData,
    },
    /// Create a new hourly thread with the given name.
    CreateThread { name: String },
    /// Persist an activity snapshot to the database.
    InsertActivity { snapshot: ActivitySnapshot },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedTarget {
    Recorder,
    Activity,
}

/// Activity snapshot for database persistence.
#[derive(Debug, Clone)]
pub struct ActivitySnapshot {
    pub timestamp: i64,
    pub signal_counts: HashMap<String, u64>,
    pub trunk_msg_counts: HashMap<String, (u64, u64)>,
    pub unit_counts: HashMap<String, u64>,
    pub active_calls: Option<usize>,
}

// ── FormatEmbed trait ────────────────────────────────────────────────────────

/// Extension trait for formatting proto events into embed-ready strings.
trait FormatEmbed {
    fn format_embed(&self) -> String;
}

impl FormatEmbed for proto::SetupConfigEvent {
    fn format_embed(&self) -> String {
        let mut text = String::new();
        for sys in &self.systems {
            let _ = writeln!(
                text,
                "System: {} - type: {}",
                sys.short_name,
                format_system_type(sys.system_type)
            );
        }
        text
    }
}

impl FormatEmbed for proto::SetupSystemsEvent {
    fn format_embed(&self) -> String {
        let mut text = String::new();
        for sys in &self.systems {
            let _ = writeln!(
                text,
                "{} (type={})",
                sys.short_name,
                format_system_type(sys.system_type)
            );
        }
        text
    }
}

impl FormatEmbed for proto::SetupSourcesEvent {
    fn format_embed(&self) -> String {
        let mut text = String::new();
        for src in &self.sources {
            let _ = writeln!(
                text,
                "Source {}: {} @ {:.3} MHz (analog={}, digital={})",
                src.src_num,
                src.driver,
                src.center_freq / 1_000_000.0,
                src.analog_recorders,
                src.digital_recorders
            );
        }
        text
    }
}

impl FormatEmbed for proto::SetupSystemEvent {
    fn format_embed(&self) -> String {
        self.system.as_ref().map_or_else(
            || "(no data)".to_string(),
            |sys| {
                format!(
                    "{} (type={})",
                    sys.short_name,
                    format_system_type(sys.system_type)
                )
            },
        )
    }
}

fn format_system_type(system_type: i32) -> &'static str {
    proto::SystemType::try_from(system_type).map_or("unknown", |st| {
        st.as_str_name()
            .strip_prefix("SYSTEM_TYPE_")
            .unwrap_or(st.as_str_name())
    })
}

fn format_signal_type(signal_type: i32) -> String {
    proto::SignalType::try_from(signal_type).map_or_else(
        |_| format!("UNKNOWN({signal_type})"),
        |st| {
            st.as_str_name()
                .strip_prefix("SIGNAL_TYPE_")
                .unwrap_or(st.as_str_name())
                .to_string()
        },
    )
}

fn format_unit_type(r#type: i32) -> String {
    proto::UnitEventType::try_from(r#type).map_or_else(
        |_| format!("UNKNOWN({type})"),
        |et| {
            et.as_str_name()
                .strip_prefix("UNIT_EVENT_TYPE_")
                .unwrap_or(et.as_str_name())
                .to_string()
        },
    )
}

fn build_recorder_table(recorders: &HashMap<i32, proto::RecorderInfo>) -> String {
    let mut ids: Vec<i32> = recorders.keys().copied().collect();
    ids.sort_unstable();

    let mut table = String::from("```\n");
    let _ = writeln!(table, "{:<4} {:>10}", "ID", "Freq Err");
    for id in ids {
        let rec = &recorders[&id];
        let _ = writeln!(table, "{:<4} {:>+10}", rec.id, rec.freq_error);
    }
    table.push_str("```");
    table
}

fn setup_embed(title: &str, description: &str) -> EmbedData {
    EmbedData {
        title: title.into(),
        colour: 0x0034_98db,
        description: Some(description.into()),
        fields: vec![],
        footer: String::new(),
    }
}

// ── State machine ────────────────────────────────────────────────────────────

/// Pure state machine for the system event aggregator.
///
/// Tracks setup event dedup, recorder state, counter aggregation, and hourly
/// thread rotation. Returns [`AggEffect`]s for the executor to perform.
pub struct AggregatorState {
    // Setup event dedup: only post when content changes
    last_setup_config: Option<String>,
    last_setup_systems: Option<String>,
    last_setup_sources: Option<String>,
    last_setup_system: HashMap<String, String>,

    // Recorder table: single embed edited in place
    recorder_state: HashMap<i32, proto::RecorderInfo>,
    has_recorder_embed: bool,

    // System activity: single embed edited in place
    has_activity_embed: bool,

    // Aggregated counters (flushed periodically)
    signal_counts: HashMap<String, u64>,
    trunk_message_counts: HashMap<String, (u64, u64)>,
    unit_counts: HashMap<String, u64>,
    latest_calls_active: Option<usize>,

    // Hourly thread tracking
    current_hour: String,
    has_thread: bool,
}

impl AggregatorState {
    pub fn new() -> Self {
        Self {
            last_setup_config: None,
            last_setup_systems: None,
            last_setup_sources: None,
            last_setup_system: HashMap::new(),
            recorder_state: HashMap::new(),
            has_recorder_embed: false,
            has_activity_embed: false,
            signal_counts: HashMap::new(),
            trunk_message_counts: HashMap::new(),
            unit_counts: HashMap::new(),
            latest_calls_active: None,
            current_hour: String::new(),
            has_thread: false,
        }
    }

    /// Handle an incoming aggregator message. Returns effects to execute.
    pub fn on_message(&mut self, msg: AggMessage) -> Vec<AggEffect> {
        let mut effects = self.ensure_thread_effects();

        match msg {
            AggMessage::SetupConfig(e) => {
                let text = e.format_embed();
                if self.last_setup_config.as_ref() != Some(&text) {
                    self.last_setup_config = Some(text.clone());
                    effects.push(AggEffect::PostEmbed {
                        embed: setup_embed("Setup Config", &text),
                    });
                }
            }
            AggMessage::SetupSystems(e) => {
                let text = e.format_embed();
                if self.last_setup_systems.as_ref() != Some(&text) {
                    self.last_setup_systems = Some(text.clone());
                    effects.push(AggEffect::PostEmbed {
                        embed: setup_embed("Systems", &text),
                    });
                }
            }
            AggMessage::SetupSources(e) => {
                let text = e.format_embed();
                if self.last_setup_sources.as_ref() != Some(&text) {
                    self.last_setup_sources = Some(text.clone());
                    effects.push(AggEffect::PostEmbed {
                        embed: setup_embed("Sources", &text),
                    });
                }
            }
            AggMessage::SetupSystem(e) => {
                let key = e
                    .system
                    .as_ref()
                    .map(|s| s.short_name.clone())
                    .unwrap_or_default();
                let text = e.format_embed();
                if self.last_setup_system.get(&key) != Some(&text) {
                    self.last_setup_system.insert(key, text.clone());
                    effects.push(AggEffect::PostEmbed {
                        embed: setup_embed("System", &text),
                    });
                }
            }
            AggMessage::SetupRecorder(e) => {
                if let Some(rec) = e.recorder {
                    self.recorder_state.insert(rec.id, rec);
                    let embed = self.build_recorder_embed();
                    if self.has_recorder_embed {
                        effects.push(AggEffect::EditEmbed {
                            target: EmbedTarget::Recorder,
                            embed,
                        });
                    } else {
                        self.has_recorder_embed = true;
                        effects.push(AggEffect::PostEmbed { embed });
                    }
                }
            }
            AggMessage::Signal(e) => {
                let signal_type = format_signal_type(e.signal_type);
                *self.signal_counts.entry(signal_type).or_insert(0) += 1;
            }
            AggMessage::TrunkMessage(e) => {
                let sys_name = e
                    .system
                    .as_ref()
                    .map_or_else(|| "unknown".to_string(), |s| s.short_name.clone());
                let msg_count = e.messages.len() as u64;
                let entry = self.trunk_message_counts.entry(sys_name).or_insert((0, 0));
                entry.0 += msg_count;
                entry.1 += 1;
            }
            AggMessage::Unit(e) => {
                let event_type = format_unit_type(e.r#type);
                *self.unit_counts.entry(event_type).or_insert(0) += 1;
            }
            AggMessage::CallsActive(e) => {
                self.latest_calls_active = Some(e.calls.len());
            }
        }

        effects
    }

    /// Periodic flush — emit activity embed and DB snapshot if there's data.
    pub fn on_flush(&mut self, timestamp: i64) -> Vec<AggEffect> {
        if !self.has_activity() {
            return vec![];
        }

        let mut effects = self.ensure_thread_effects();

        // Snapshot counters for DB before draining
        let snapshot = ActivitySnapshot {
            timestamp,
            signal_counts: self.signal_counts.clone(),
            trunk_msg_counts: self.trunk_message_counts.clone(),
            unit_counts: self.unit_counts.clone(),
            active_calls: self.latest_calls_active,
        };
        effects.push(AggEffect::InsertActivity { snapshot });

        let embed = self.build_activity_embed();
        if self.has_activity_embed {
            effects.push(AggEffect::EditEmbed {
                target: EmbedTarget::Activity,
                embed,
            });
        } else {
            self.has_activity_embed = true;
            effects.push(AggEffect::PostEmbed { embed });
        }

        effects
    }

    /// Notify the state machine that a new thread was created.
    pub fn on_thread_created(&mut self, hour: String) {
        self.current_hour = hour;
        self.has_thread = true;
        self.has_recorder_embed = false;
        self.recorder_state.clear();
        self.has_activity_embed = false;
    }

    fn has_activity(&self) -> bool {
        !self.signal_counts.is_empty()
            || !self.trunk_message_counts.is_empty()
            || !self.unit_counts.is_empty()
            || self.latest_calls_active.is_some()
    }

    fn ensure_thread_effects(&self) -> Vec<AggEffect> {
        if self.needs_new_thread() {
            let now = chrono::Utc::now();
            let hour_key = now.format("%Y-%m-%d %H:00").to_string();
            let thread_name = format!("System Events - {hour_key}");
            vec![AggEffect::CreateThread { name: thread_name }]
        } else {
            vec![]
        }
    }

    fn needs_new_thread(&self) -> bool {
        let now_hour = chrono::Utc::now().format("%Y-%m-%d %H:00").to_string();
        !self.has_thread || self.current_hour != now_hour
    }

    fn build_recorder_embed(&self) -> EmbedData {
        let table = build_recorder_table(&self.recorder_state);
        EmbedData {
            title: "Recorders".into(),
            colour: 0x0034_98db,
            description: Some(table),
            fields: vec![],
            footer: String::new(),
        }
    }

    fn build_activity_embed(&mut self) -> EmbedData {
        let mut fields = Vec::new();

        if !self.signal_counts.is_empty() {
            let mut types: Vec<_> = self.signal_counts.drain().collect();
            types.sort_by(|a, b| a.0.cmp(&b.0));
            let text: Vec<String> = types
                .into_iter()
                .map(|(typ, count)| format!("{count} {typ}"))
                .collect();
            fields.push(EmbedField {
                name: "Signals".into(),
                value: text.join(", "),
                inline: false,
            });
        }

        if !self.trunk_message_counts.is_empty() {
            let mut systems: Vec<_> = self.trunk_message_counts.drain().collect();
            systems.sort_by(|a, b| a.0.cmp(&b.0));
            let text: Vec<String> = systems
                .into_iter()
                .map(|(name, (msgs, batches))| format!("{name}: {msgs} msgs ({batches} batches)"))
                .collect();
            fields.push(EmbedField {
                name: "Trunk Messages".into(),
                value: text.join("\n"),
                inline: false,
            });
        }

        if !self.unit_counts.is_empty() {
            let mut types: Vec<_> = self.unit_counts.drain().collect();
            types.sort_by(|a, b| a.0.cmp(&b.0));
            let text: Vec<String> = types
                .into_iter()
                .map(|(typ, count)| format!("{count} {typ}"))
                .collect();
            fields.push(EmbedField {
                name: "Unit Events".into(),
                value: text.join(", "),
                inline: false,
            });
        }

        if let Some(count) = self.latest_calls_active.take() {
            fields.push(EmbedField {
                name: "Active Calls".into(),
                value: count.to_string(),
                inline: true,
            });
        }

        EmbedData {
            title: "System Activity".into(),
            colour: 0x0095_a5a6,
            description: None,
            fields,
            footer: String::new(),
        }
    }
}

// ── Message types ────────────────────────────────────────────────────────────

/// Input messages for the aggregator state machine.
pub enum AggMessage {
    Signal(Box<proto::SignalEvent>),
    TrunkMessage(proto::TrunkMessageEvent),
    Unit(proto::UnitEvent),
    CallsActive(proto::CallsActiveEvent),
    SetupRecorder(proto::SetupRecorderEvent),
    SetupSystem(proto::SetupSystemEvent),
    SetupSystems(proto::SetupSystemsEvent),
    SetupSources(proto::SetupSourcesEvent),
    SetupConfig(proto::SetupConfigEvent),
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> AggregatorState {
        let mut s = AggregatorState::new();
        // Pretend a thread already exists to avoid CreateThread effects in most tests
        s.on_thread_created(chrono::Utc::now().format("%Y-%m-%d %H:00").to_string());
        s
    }

    // ── Setup dedup ──────────────────────────────────────────────────────

    #[test]
    fn setup_config_dedup() {
        let mut s = make_state();
        let event = proto::SetupConfigEvent {
            systems: vec![proto::SystemInfo {
                short_name: "smco".into(),
                system_type: 0,
                ..Default::default()
            }],
        };

        let effects = s.on_message(AggMessage::SetupConfig(event.clone()));
        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0], AggEffect::PostEmbed { .. }));

        // Same event again — no effect (deduplicated)
        let effects = s.on_message(AggMessage::SetupConfig(event));
        assert!(effects.is_empty());
    }

    #[test]
    fn setup_system_dedup_by_key() {
        let mut s = make_state();
        let event1 = proto::SetupSystemEvent {
            system: Some(proto::SystemInfo {
                short_name: "smco".into(),
                system_type: 0,
                ..Default::default()
            }),
        };
        let event2 = proto::SetupSystemEvent {
            system: Some(proto::SystemInfo {
                short_name: "svrcs".into(),
                system_type: 0,
                ..Default::default()
            }),
        };

        let e1 = s.on_message(AggMessage::SetupSystem(event1.clone()));
        let e2 = s.on_message(AggMessage::SetupSystem(event2));
        assert_eq!(e1.len(), 1);
        assert_eq!(e2.len(), 1);

        // Same key, same content — deduped
        let e3 = s.on_message(AggMessage::SetupSystem(event1));
        assert!(e3.is_empty());
    }

    // ── Recorder embed ───────────────────────────────────────────────────

    #[test]
    fn recorder_first_posts_then_edits() {
        let mut s = make_state();
        let event1 = proto::SetupRecorderEvent {
            recorder: Some(proto::RecorderInfo {
                id: 1,
                ..Default::default()
            }),
        };

        let effects = s.on_message(AggMessage::SetupRecorder(event1));
        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0], AggEffect::PostEmbed { .. }));

        let event2 = proto::SetupRecorderEvent {
            recorder: Some(proto::RecorderInfo {
                id: 2,
                ..Default::default()
            }),
        };
        let effects = s.on_message(AggMessage::SetupRecorder(event2));
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0],
            AggEffect::EditEmbed {
                target: EmbedTarget::Recorder,
                ..
            }
        ));
    }

    // ── Counter aggregation ──────────────────────────────────────────────

    #[test]
    fn signal_counts_aggregate() {
        let mut s = make_state();
        let e1 = s.on_message(AggMessage::Signal(Box::new(proto::SignalEvent {
            signal_type: 0,
            ..Default::default()
        })));
        let e2 = s.on_message(AggMessage::Signal(Box::new(proto::SignalEvent {
            signal_type: 0,
            ..Default::default()
        })));
        assert!(e1.is_empty());
        assert!(e2.is_empty());
        assert_eq!(*s.signal_counts.values().next().unwrap(), 2);
    }

    #[test]
    fn trunk_message_counts_aggregate() {
        let mut s = make_state();
        let event = proto::TrunkMessageEvent {
            system: Some(proto::SystemInfo {
                short_name: "smco".into(),
                ..Default::default()
            }),
            messages: vec![Default::default(), Default::default()],
        };
        s.on_message(AggMessage::TrunkMessage(event));
        let (msgs, batches) = s.trunk_message_counts.get("smco").unwrap();
        assert_eq!(*msgs, 2);
        assert_eq!(*batches, 1);
    }

    // ── Flush ────────────────────────────────────────────────────────────

    #[test]
    fn flush_with_no_activity_no_effects() {
        let mut s = make_state();
        let effects = s.on_flush(1000);
        assert!(effects.is_empty());
    }

    #[test]
    fn flush_with_activity_emits_insert_and_post() {
        let mut s = make_state();
        s.on_message(AggMessage::Signal(Box::new(proto::SignalEvent {
            signal_type: 0,
            ..Default::default()
        })));

        let effects = s.on_flush(1000);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, AggEffect::InsertActivity { .. }))
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, AggEffect::PostEmbed { .. }))
        );
    }

    #[test]
    fn second_flush_edits_activity_embed() {
        let mut s = make_state();
        s.on_message(AggMessage::Signal(Box::new(proto::SignalEvent {
            signal_type: 0,
            ..Default::default()
        })));
        s.on_flush(1000);

        s.on_message(AggMessage::Signal(Box::new(proto::SignalEvent {
            signal_type: 0,
            ..Default::default()
        })));
        let effects = s.on_flush(2000);
        assert!(effects.iter().any(|e| matches!(
            e,
            AggEffect::EditEmbed {
                target: EmbedTarget::Activity,
                ..
            }
        )));
    }

    // ── Thread rotation ──────────────────────────────────────────────────

    #[test]
    fn new_state_needs_thread() {
        let s = AggregatorState::new();
        assert!(s.needs_new_thread());
    }

    #[test]
    fn after_thread_created_no_new_thread() {
        let s = make_state();
        assert!(!s.needs_new_thread());
    }

    #[test]
    fn thread_created_resets_embeds() {
        let mut s = make_state();
        s.has_recorder_embed = true;
        s.has_activity_embed = true;
        s.recorder_state.insert(
            1,
            proto::RecorderInfo {
                id: 1,
                ..Default::default()
            },
        );

        s.on_thread_created("2025-01-01 13:00".into());

        assert!(!s.has_recorder_embed);
        assert!(!s.has_activity_embed);
        assert!(s.recorder_state.is_empty());
    }
}
