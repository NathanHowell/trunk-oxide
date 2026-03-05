use std::collections::HashMap;
use std::time::Instant;

use chrono::Utc;
use proto::types::{SysNum, Talkgroup};
use serenity::model::id::ChannelId;
use trunk_oxide_proto as proto;

use crate::aggregator::AggregatorMessage;

// ── Keys ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct CallKey {
    pub sys_num: SysNum,
    pub talkgroup: Talkgroup,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct ThreadCacheKey {
    pub sys_num: SysNum,
    pub talkgroup: Talkgroup,
    pub date: String,
}

// ── Effects ──────────────────────────────────────────────────────────────────

/// Side-effect commands returned by [`DispatcherState`].
#[derive(Debug)]
pub enum DispatchEffect {
    /// Send audio to an active call handler.
    SendAudio {
        key: CallKey,
        samples: Vec<u8>,
        sample_rate: i32,
    },
    /// Send an event text to an active call handler.
    SendEvent { key: CallKey, text: String },
    /// End an active call.
    EndCall {
        key: CallKey,
        call_data: Option<Box<proto::CallData>>,
    },
    /// Reap a stuck call (no CallEnd received).
    ReapCall { key: CallKey },
    /// Forward a message to the aggregator.
    ForwardToAggregator { message: AggregatorMessage },
    /// Rotate to a new hourly database.
    RotateDb,
    /// Resolve a channel for a new call. The executor resolves the channel
    /// (mapped lookup, thread cache, DB, or Discord create) then calls
    /// [`DispatcherState::on_channel_resolved`].
    ResolveChannel { call: proto::CallInfo },
    /// Spawn a call handler after channel resolution.
    SpawnCallHandler {
        key: CallKey,
        call: proto::CallInfo,
        post_channel: ChannelId,
    },
    /// Log a message.
    Log { message: String },
}

// ── Config ───────────────────────────────────────────────────────────────────

/// Maximum time a call can be active before being reaped.
const MAX_CALL_AGE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

// ── State machine ────────────────────────────────────────────────────────────

/// Pure state machine for the event dispatcher.
///
/// Single entry point for all events. Tracks active calls, thread cache,
/// and hourly rotation. Returns [`DispatchEffect`]s for the executor.
pub struct DispatcherState {
    active_calls: HashMap<CallKey, Instant>,
    thread_cache: HashMap<ThreadCacheKey, ChannelId>,
    current_hour: String,
}

impl DispatcherState {
    pub fn new() -> Self {
        let current_hour = Utc::now().format("%Y-%m-%dT%H").to_string();
        Self {
            active_calls: HashMap::new(),
            thread_cache: HashMap::new(),
            current_hour,
        }
    }

    /// Handle an audio sample. Routes to active call or drops silently.
    pub fn on_audio(&self, sample: &proto::AudioSample) -> Vec<DispatchEffect> {
        if sample.sample_rate == 0 {
            return vec![DispatchEffect::Log {
                message: format!(
                    "dropping audio with sample_rate=0 for tg {}",
                    sample.talkgroup
                ),
            }];
        }

        let key = CallKey {
            sys_num: SysNum(sample.sys_num),
            talkgroup: Talkgroup(sample.talkgroup),
        };

        if self.active_calls.contains_key(&key) {
            vec![DispatchEffect::SendAudio {
                key,
                samples: sample.samples.clone(),
                sample_rate: sample.sample_rate,
            }]
        } else {
            vec![]
        }
    }

    /// Handle a plugin event. Single entry point for all event routing.
    pub fn on_event(&mut self, event: proto::PluginEvent) -> Vec<DispatchEffect> {
        let Some(event) = event.event else {
            return vec![];
        };

        use proto::plugin_event::Event;
        match event {
            Event::CallStart(e) => self.on_call_start(e),
            Event::CallEnd(e) => self.on_call_end(e),
            Event::Signal(e) => self.on_signal(e),
            Event::Unit(e) => self.on_unit(e),
            Event::CallsActive(e) => vec![DispatchEffect::ForwardToAggregator {
                message: AggregatorMessage::CallsActive(e),
            }],
            Event::TrunkMessage(e) => vec![DispatchEffect::ForwardToAggregator {
                message: AggregatorMessage::TrunkMessage(e),
            }],
            Event::SystemRates(e) => vec![DispatchEffect::ForwardToAggregator {
                message: AggregatorMessage::SystemRates(e),
            }],
            Event::SetupRecorder(e) => vec![DispatchEffect::ForwardToAggregator {
                message: AggregatorMessage::SetupRecorder(e),
            }],
            Event::SetupSystem(e) => vec![DispatchEffect::ForwardToAggregator {
                message: AggregatorMessage::SetupSystem(e),
            }],
            Event::SetupSystems(e) => vec![DispatchEffect::ForwardToAggregator {
                message: AggregatorMessage::SetupSystems(e),
            }],
            Event::SetupSources(e) => vec![DispatchEffect::ForwardToAggregator {
                message: AggregatorMessage::SetupSources(e),
            }],
            Event::SetupConfig(e) => vec![DispatchEffect::ForwardToAggregator {
                message: AggregatorMessage::SetupConfig(e),
            }],
            Event::UnitAlias(e) => vec![DispatchEffect::Log {
                message: format!(
                    "OTA unit alias: radio_id={} alias={} source={} tg={}",
                    e.radio_id, e.alias, e.source, e.talkgroup_id
                ),
            }],
        }
    }

    fn on_call_start(&mut self, event: proto::CallStartEvent) -> Vec<DispatchEffect> {
        let Some(call) = event.call else {
            return vec![];
        };

        let mut effects = Vec::new();

        // Check hourly rotation
        let now_hour = Utc::now().format("%Y-%m-%dT%H").to_string();
        if self.current_hour != now_hour {
            self.current_hour = now_hour;
            effects.push(DispatchEffect::RotateDb);
        }

        // Emit channel resolution request — executor resolves async, then
        // calls on_channel_resolved() which emits SpawnCallHandler.
        effects.push(DispatchEffect::ResolveChannel { call });

        effects
    }

    /// Two-phase callback: executor resolved the post channel for a new call.
    /// Registers the call as active and emits SpawnCallHandler.
    pub fn on_channel_resolved(
        &mut self,
        call: proto::CallInfo,
        post_channel: ChannelId,
    ) -> Vec<DispatchEffect> {
        let key = CallKey {
            sys_num: SysNum(call.sys_num),
            talkgroup: Talkgroup(call.talkgroup),
        };
        self.active_calls.insert(key.clone(), Instant::now());

        vec![DispatchEffect::SpawnCallHandler {
            key,
            call,
            post_channel,
        }]
    }

    fn on_call_end(&mut self, event: proto::CallEndEvent) -> Vec<DispatchEffect> {
        let Some(ref call_data) = event.call_data else {
            return vec![];
        };

        let key = CallKey {
            sys_num: SysNum(call_data.sys_num),
            talkgroup: Talkgroup(call_data.talkgroup),
        };

        if self.active_calls.remove(&key).is_some() {
            vec![DispatchEffect::EndCall {
                key,
                call_data: event.call_data.map(Box::new),
            }]
        } else {
            vec![]
        }
    }

    fn on_signal(&self, event: proto::SignalEvent) -> Vec<DispatchEffect> {
        if let Some(call) = &event.call {
            let key = CallKey {
                sys_num: SysNum(call.sys_num),
                talkgroup: Talkgroup(call.talkgroup),
            };
            if self.active_calls.contains_key(&key) {
                let text = format_signal_event(&event);
                return vec![DispatchEffect::SendEvent { key, text }];
            }
        }
        vec![DispatchEffect::ForwardToAggregator {
            message: AggregatorMessage::Signal(Box::new(event)),
        }]
    }

    fn on_unit(&self, event: proto::UnitEvent) -> Vec<DispatchEffect> {
        if event.talkgroup != 0 {
            if let Some(system) = &event.system {
                let key = CallKey {
                    sys_num: SysNum(system.sys_num),
                    talkgroup: Talkgroup(event.talkgroup),
                };
                if self.active_calls.contains_key(&key) {
                    let text = format_unit_event(&event);
                    return vec![DispatchEffect::SendEvent { key, text }];
                }
            }
        }
        vec![DispatchEffect::ForwardToAggregator {
            message: AggregatorMessage::Unit(event),
        }]
    }

    /// Check for stuck calls and reap them.
    pub fn on_reap_tick(&mut self) -> Vec<DispatchEffect> {
        let now = Instant::now();
        let mut reap_keys = Vec::new();

        for (key, started_at) in &self.active_calls {
            let age = now.duration_since(*started_at);
            if age > MAX_CALL_AGE {
                reap_keys.push((key.clone(), age));
            }
        }

        let mut effects = Vec::new();
        for (key, age) in reap_keys {
            self.active_calls.remove(&key);
            effects.push(DispatchEffect::Log {
                message: format!(
                    "reaping stuck call sys_num={} tg={} age={}s",
                    key.sys_num.0,
                    key.talkgroup.0,
                    age.as_secs()
                ),
            });
            effects.push(DispatchEffect::ReapCall { key });
        }

        effects
    }

    /// Cache a resolved thread ID.
    pub fn cache_thread(&mut self, cache_key: ThreadCacheKey, channel: ChannelId) {
        self.thread_cache.insert(cache_key, channel);
    }

    /// Look up a cached thread.
    pub fn get_cached_thread(&self, cache_key: &ThreadCacheKey) -> Option<ChannelId> {
        self.thread_cache.get(cache_key).copied()
    }

    #[cfg(test)]
    pub fn is_active(&self, key: &CallKey) -> bool {
        self.active_calls.contains_key(key)
    }

}

// ── Formatting ───────────────────────────────────────────────────────────────

fn format_signal_event(event: &proto::SignalEvent) -> String {
    let signal_type = proto::SignalType::try_from(event.signal_type).map_or_else(
        |_| format!("UNKNOWN({})", event.signal_type),
        |st| {
            st.as_str_name()
                .strip_prefix("SIGNAL_TYPE_")
                .unwrap_or(st.as_str_name())
                .to_string()
        },
    );

    let unit = if event.unit_alias.is_empty() {
        format!("unit {}", event.unit_id)
    } else {
        format!("{} ({})", event.unit_alias, event.unit_id)
    };

    format!(
        "**Signal**: {unit} - {signal_type} ({})",
        event.signaling_type
    )
}

fn format_unit_event(event: &proto::UnitEvent) -> String {
    let event_type = proto::UnitEventType::try_from(event.r#type).map_or_else(
        |_| format!("UNKNOWN({})", event.r#type),
        |et| {
            et.as_str_name()
                .strip_prefix("UNIT_EVENT_TYPE_")
                .unwrap_or(et.as_str_name())
                .to_string()
        },
    );

    let source = if event.source_alias.is_empty() {
        format!("source {}", event.source_id)
    } else {
        format!("{} ({})", event.source_alias, event.source_id)
    };

    format!("**Unit**: {event_type} - {source} (tg {})", event.talkgroup)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_call_start(sys_num: i32, talkgroup: i64) -> proto::PluginEvent {
        proto::PluginEvent {
            event: Some(proto::plugin_event::Event::CallStart(
                proto::CallStartEvent {
                    call: Some(proto::CallInfo {
                        sys_num,
                        talkgroup,
                        short_name: "smco".into(),
                        freq: 851_125_000.0,
                        ..Default::default()
                    }),
                },
            )),
        }
    }

    fn make_call_end(sys_num: i32, talkgroup: i64) -> proto::PluginEvent {
        proto::PluginEvent {
            event: Some(proto::plugin_event::Event::CallEnd(
                proto::CallEndEvent {
                    call_data: Some(proto::CallData {
                        sys_num,
                        talkgroup,
                        length: 5.0,
                        ..Default::default()
                    }),
                },
            )),
        }
    }

    // ── Audio routing ────────────────────────────────────────────────────

    #[test]
    fn audio_for_active_call_routes() {
        let mut s = DispatcherState::new();
        let key = CallKey {
            sys_num: SysNum(1),
            talkgroup: Talkgroup(12345),
        };
        s.active_calls.insert(key.clone(), Instant::now());

        let sample = proto::AudioSample {
            sys_num: 1,
            talkgroup: 12345,
            samples: vec![1, 2, 3, 4],
            sample_rate: 16000,
            freq: 851_125_000.0,
        };
        let effects = s.on_audio(&sample);
        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0], DispatchEffect::SendAudio { .. }));
    }

    #[test]
    fn audio_for_unknown_call_dropped() {
        let s = DispatcherState::new();
        let sample = proto::AudioSample {
            sys_num: 1,
            talkgroup: 12345,
            samples: vec![1, 2],
            sample_rate: 16000,
            freq: 851_125_000.0,
        };
        let effects = s.on_audio(&sample);
        assert!(effects.is_empty());
    }

    #[test]
    fn audio_with_zero_sample_rate_logs() {
        let s = DispatcherState::new();
        let sample = proto::AudioSample {
            sys_num: 1,
            talkgroup: 12345,
            samples: vec![1, 2],
            sample_rate: 0,
            freq: 851_125_000.0,
        };
        let effects = s.on_audio(&sample);
        assert_eq!(effects.len(), 1);
        assert!(matches!(&effects[0], DispatchEffect::Log { .. }));
    }

    // ── Call lifecycle (two-phase) ───────────────────────────────────────

    #[test]
    fn call_start_emits_resolve_channel() {
        let mut s = DispatcherState::new();
        let effects = s.on_event(make_call_start(1, 12345));
        assert!(effects
            .iter()
            .any(|e| matches!(e, DispatchEffect::ResolveChannel { .. })));
        // Not yet active — only after channel resolution
        let key = CallKey {
            sys_num: SysNum(1),
            talkgroup: Talkgroup(12345),
        };
        assert!(!s.is_active(&key));
    }

    #[test]
    fn channel_resolved_registers_active_and_spawns() {
        let mut s = DispatcherState::new();
        let call = proto::CallInfo {
            sys_num: 1,
            talkgroup: 12345,
            ..Default::default()
        };
        let channel = ChannelId::from(999);

        let effects = s.on_channel_resolved(call, channel);
        assert!(effects
            .iter()
            .any(|e| matches!(e, DispatchEffect::SpawnCallHandler { .. })));

        let key = CallKey {
            sys_num: SysNum(1),
            talkgroup: Talkgroup(12345),
        };
        assert!(s.is_active(&key));
    }

    #[test]
    fn call_end_for_active_call_removes() {
        let mut s = DispatcherState::new();
        // Simulate full lifecycle: start → resolve → end
        s.on_event(make_call_start(1, 12345));
        s.on_channel_resolved(
            proto::CallInfo {
                sys_num: 1,
                talkgroup: 12345,
                ..Default::default()
            },
            ChannelId::from(999),
        );

        let effects = s.on_event(make_call_end(1, 12345));

        let key = CallKey {
            sys_num: SysNum(1),
            talkgroup: Talkgroup(12345),
        };
        assert!(!s.is_active(&key));
        assert!(effects
            .iter()
            .any(|e| matches!(e, DispatchEffect::EndCall { .. })));
    }

    #[test]
    fn call_end_for_unknown_call_no_effect() {
        let mut s = DispatcherState::new();
        let effects = s.on_event(make_call_end(1, 99999));
        assert!(!effects
            .iter()
            .any(|e| matches!(e, DispatchEffect::EndCall { .. })));
    }

    // ── Signal routing ───────────────────────────────────────────────────

    #[test]
    fn signal_during_active_call_routes_to_call() {
        let mut s = DispatcherState::new();
        s.on_channel_resolved(
            proto::CallInfo {
                sys_num: 1,
                talkgroup: 12345,
                ..Default::default()
            },
            ChannelId::from(999),
        );

        let event = proto::PluginEvent {
            event: Some(proto::plugin_event::Event::Signal(proto::SignalEvent {
                call: Some(proto::CallInfo {
                    sys_num: 1,
                    talkgroup: 12345,
                    ..Default::default()
                }),
                signal_type: 0,
                unit_id: 100,
                ..Default::default()
            })),
        };
        let effects = s.on_event(event);
        assert!(effects
            .iter()
            .any(|e| matches!(e, DispatchEffect::SendEvent { .. })));
    }

    #[test]
    fn signal_without_active_call_goes_to_aggregator() {
        let mut s = DispatcherState::new();
        let event = proto::PluginEvent {
            event: Some(proto::plugin_event::Event::Signal(proto::SignalEvent {
                call: Some(proto::CallInfo {
                    sys_num: 1,
                    talkgroup: 99999,
                    ..Default::default()
                }),
                ..Default::default()
            })),
        };
        let effects = s.on_event(event);
        assert!(effects
            .iter()
            .any(|e| matches!(e, DispatchEffect::ForwardToAggregator { .. })));
    }

    // ── Unit routing ─────────────────────────────────────────────────────

    #[test]
    fn unit_during_active_call_routes_to_call() {
        let mut s = DispatcherState::new();
        s.on_channel_resolved(
            proto::CallInfo {
                sys_num: 1,
                talkgroup: 12345,
                ..Default::default()
            },
            ChannelId::from(999),
        );

        let event = proto::PluginEvent {
            event: Some(proto::plugin_event::Event::Unit(proto::UnitEvent {
                talkgroup: 12345,
                system: Some(proto::SystemInfo {
                    sys_num: 1,
                    ..Default::default()
                }),
                source_id: 100,
                ..Default::default()
            })),
        };
        let effects = s.on_event(event);
        assert!(effects
            .iter()
            .any(|e| matches!(e, DispatchEffect::SendEvent { .. })));
    }

    #[test]
    fn unit_with_zero_talkgroup_goes_to_aggregator() {
        let mut s = DispatcherState::new();
        let event = proto::PluginEvent {
            event: Some(proto::plugin_event::Event::Unit(proto::UnitEvent {
                talkgroup: 0,
                system: Some(proto::SystemInfo {
                    sys_num: 1,
                    ..Default::default()
                }),
                ..Default::default()
            })),
        };
        let effects = s.on_event(event);
        assert!(effects
            .iter()
            .any(|e| matches!(e, DispatchEffect::ForwardToAggregator { .. })));
    }

    // ── Reaping ──────────────────────────────────────────────────────────

    #[test]
    fn reap_removes_old_calls() {
        let mut s = DispatcherState::new();
        let key = CallKey {
            sys_num: SysNum(1),
            talkgroup: Talkgroup(12345),
        };
        s.active_calls
            .insert(key.clone(), Instant::now() - std::time::Duration::from_secs(700));

        let effects = s.on_reap_tick();
        assert!(effects
            .iter()
            .any(|e| matches!(e, DispatchEffect::ReapCall { .. })));
        assert!(!s.is_active(&key));
    }

    #[test]
    fn reap_keeps_fresh_calls() {
        let mut s = DispatcherState::new();
        let key = CallKey {
            sys_num: SysNum(1),
            talkgroup: Talkgroup(12345),
        };
        s.active_calls.insert(key.clone(), Instant::now());

        let effects = s.on_reap_tick();
        assert!(effects.is_empty());
        assert!(s.is_active(&key));
    }

    // ── Aggregator forwarding ────────────────────────────────────────────

    #[test]
    fn calls_active_goes_to_aggregator() {
        let mut s = DispatcherState::new();
        let event = proto::PluginEvent {
            event: Some(proto::plugin_event::Event::CallsActive(
                proto::CallsActiveEvent { calls: vec![] },
            )),
        };
        let effects = s.on_event(event);
        assert!(effects
            .iter()
            .any(|e| matches!(e, DispatchEffect::ForwardToAggregator { .. })));
    }

    // ── Thread cache ─────────────────────────────────────────────────────

    #[test]
    fn thread_cache_round_trip() {
        let mut s = DispatcherState::new();
        let cache_key = ThreadCacheKey {
            sys_num: SysNum(1),
            talkgroup: Talkgroup(12345),
            date: "2025-03-05".into(),
        };
        let channel = ChannelId::from(999);

        assert!(s.get_cached_thread(&cache_key).is_none());
        s.cache_thread(cache_key.clone(), channel);
        assert_eq!(s.get_cached_thread(&cache_key), Some(channel));
    }
}
