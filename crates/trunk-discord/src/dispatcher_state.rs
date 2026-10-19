use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use proto::types::Talkgroup;
use trunk_oxide_proto as proto;

use crate::aggregator::AggregatorMessage;

// ── Keys ─────────────────────────────────────────────────────────────────────

/// Identifies a live call. The system is identified by `short_name` (stable),
/// not `sys_num`, which is assigned at startup and changes across restarts.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct CallKey {
    pub system: Arc<str>,
    pub talkgroup: Talkgroup,
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
    /// Reap a stuck call (no `CallEnd` received).
    ReapCall { key: CallKey },
    /// Forward a message to the aggregator.
    ForwardToAggregator { message: AggregatorMessage },
    /// Rotate to a new hourly database.
    RotateDb,
    /// Resolve a channel for a new call and run its handler. The executor
    /// creates the handler's message channel up front, registers the sender as
    /// the live handler, then spawns the handler — which owns the receiver from
    /// creation and resolves its post channel (mapped lookup, thread cache, DB,
    /// or Discord create) off the pump loop. Inbound messages buffer in the
    /// channel until resolution completes.
    ResolveChannel { key: CallKey, call: proto::CallInfo },
    /// PTT boundary within a coalesced call — handler stays alive.
    PttEnd {
        key: CallKey,
        call_data: Option<Box<proto::CallData>>,
    },
    /// Resume a coalescing handler with a new PTT.
    CoalesceResume { key: CallKey, call: proto::CallInfo },
    /// Coalesce timeout expired — finalize the handler.
    CoalesceTimeout { key: CallKey },
    /// Log a message.
    Log { message: String },
}

// ── Config ───────────────────────────────────────────────────────────────────

/// Maximum time a call can be active before being reaped.
const MAX_CALL_AGE: Duration = Duration::from_secs(10 * 60);

/// The line an evicted call's embed gets.
const EVICTED: &str = "✂️ cut short: a higher-priority call took the channel";

// ── State machine ────────────────────────────────────────────────────────────

/// Pure state machine for the event dispatcher.
///
/// Single entry point for all events. Tracks active calls, thread cache,
/// and hourly rotation. Returns [`DispatchEffect`]s for the executor.
pub struct DispatcherState {
    active_calls: HashMap<CallKey, Instant>,
    coalescing_calls: HashMap<CallKey, Instant>,
    current_hour: String,
    coalesce_timeout: Duration,
}

impl DispatcherState {
    pub fn new(coalesce_timeout_ms: u64) -> Self {
        let current_hour = Utc::now().format("%Y-%m-%dT%H").to_string();
        Self {
            active_calls: HashMap::new(),
            coalescing_calls: HashMap::new(),
            current_hour,
            coalesce_timeout: Duration::from_millis(coalesce_timeout_ms),
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
            system: Arc::from(sample.short_name.as_str()),
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
        use proto::plugin_event::Event;

        let Some(event) = event.event else {
            return vec![];
        };

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
            Event::SystemRates(_) => vec![],
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

        let key = CallKey {
            system: Arc::from(call.short_name.as_str()),
            talkgroup: Talkgroup(call.talkgroup),
        };

        // Check if this talkgroup is coalescing (between PTTs)
        if self.coalescing_calls.remove(&key).is_some() {
            self.active_calls.insert(key.clone(), Instant::now());
            effects.push(DispatchEffect::CoalesceResume { key, call });
            return effects;
        }

        // Overlapping start: a handler is already active for this talkgroup (a
        // new grant arrived before the previous call ended — e.g. a duplicate
        // call_start on source change, or genuinely back-to-back calls).
        // Explicitly finalize the old handler so its embed closes (turns green)
        // before the new one takes over the slot, and log the transition. The
        // ResolveChannel below also ends any lingering handler for the key, so
        // this is belt-and-suspenders, but the log is useful diagnostics.
        if self.active_calls.contains_key(&key) {
            effects.push(DispatchEffect::Log {
                message: format!(
                    "overlapping call_start for tg {} sys {}: finalizing previous handler",
                    key.talkgroup.0, key.system
                ),
            });
            effects.push(DispatchEffect::EndCall {
                key: key.clone(),
                call_data: None,
            });
        }

        // Normal path: register as active immediately, then resolve channel
        self.active_calls.insert(key.clone(), Instant::now());
        effects.push(DispatchEffect::ResolveChannel { key, call });

        effects
    }

    /// A handler could not resolve its post channel — remove it from
    /// `active_calls`. The executor drops the matching `self.calls` entry.
    pub fn on_channel_resolution_failed(&mut self, key: &CallKey) {
        self.active_calls.remove(key);
    }

    fn on_call_end(&mut self, event: proto::CallEndEvent) -> Vec<DispatchEffect> {
        let Some(ref call_data) = event.call_data else {
            return vec![];
        };

        let key = CallKey {
            system: Arc::from(call_data.short_name.as_str()),
            talkgroup: Talkgroup(call_data.talkgroup),
        };

        if self.active_calls.remove(&key).is_none() {
            return vec![];
        }
        // A call that outranks it took its channel: say so on the embed.
        let mut effects = Vec::new();
        if event.reason() == proto::CallEndReason::Evicted {
            effects.push(DispatchEffect::SendEvent {
                key: key.clone(),
                text: EVICTED.to_owned(),
            });
        }

        // Coalescing enabled: transition to coalescing state. Both trunked and
        // conventional calls coalesce identically — trunk-oxide now emits one
        // CallStart/CallEnd per keyed transmission for conventional channels
        // (squelch open/close), exactly like a trunked grant, so consecutive
        // transmissions on a talkgroup group into a single embed with the same
        // PTT splitting. (Conventional calls used to be excluded here because a
        // free-running recorder restart loop emitted CallStarts during dead air
        // that would have kept the handler alive forever; that loop is gone.)
        if !self.coalesce_timeout.is_zero() {
            self.coalescing_calls.insert(key.clone(), Instant::now());
            effects.push(DispatchEffect::PttEnd {
                key,
                call_data: event.call_data.map(Box::new),
            });
            return effects;
        }

        // No coalescing: end the call immediately
        effects.push(DispatchEffect::EndCall {
            key,
            call_data: event.call_data.map(Box::new),
        });
        effects
    }

    fn on_signal(&self, event: proto::SignalEvent) -> Vec<DispatchEffect> {
        if let Some(call) = &event.call {
            let key = CallKey {
                system: Arc::from(call.short_name.as_str()),
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
        if event.talkgroup != 0
            && let Some(system) = &event.system
        {
            let key = CallKey {
                system: Arc::from(system.short_name.as_str()),
                talkgroup: Talkgroup(event.talkgroup),
            };
            if self.active_calls.contains_key(&key) {
                let text = format_unit_event(&event);
                return vec![DispatchEffect::SendEvent { key, text }];
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
                    key.system,
                    key.talkgroup.0,
                    age.as_secs()
                ),
            });
            effects.push(DispatchEffect::ReapCall { key });
        }

        effects
    }

    /// Check for expired coalescing calls and finalize them.
    pub fn on_coalesce_tick(&mut self) -> Vec<DispatchEffect> {
        if self.coalesce_timeout.is_zero() {
            return vec![];
        }

        let now = Instant::now();
        let mut effects = Vec::new();
        self.coalescing_calls.retain(|key, entered_at| {
            if now.duration_since(*entered_at) > self.coalesce_timeout {
                effects.push(DispatchEffect::CoalesceTimeout { key: key.clone() });
                false
            } else {
                true
            }
        });
        effects
    }

    pub fn active_call_count(&self) -> usize {
        self.active_calls.len()
    }

    pub fn coalescing_call_count(&self) -> usize {
        self.coalescing_calls.len()
    }

    #[cfg(test)]
    pub fn is_active(&self, key: &CallKey) -> bool {
        self.active_calls.contains_key(key)
    }

    #[cfg(test)]
    pub fn is_coalescing(&self, key: &CallKey) -> bool {
        self.coalescing_calls.contains_key(key)
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

    fn make_call_start(system: &str, talkgroup: i64) -> proto::PluginEvent {
        proto::PluginEvent {
            event: Some(proto::plugin_event::Event::CallStart(
                proto::CallStartEvent {
                    call: Some(proto::CallInfo {
                        talkgroup,
                        short_name: system.into(),
                        freq: 851_125_000.0,
                        ..Default::default()
                    }),
                },
            )),
        }
    }

    fn make_call_end(system: &str, talkgroup: i64) -> proto::PluginEvent {
        proto::PluginEvent {
            event: Some(proto::plugin_event::Event::CallEnd(proto::CallEndEvent {
                call_data: Some(proto::CallData {
                    short_name: system.into(),
                    talkgroup,
                    length: 5.0,
                    ..Default::default()
                }),
                ..Default::default()
            })),
        }
    }

    // ── Audio routing ────────────────────────────────────────────────────

    #[test]
    fn audio_for_active_call_routes() {
        let mut s = DispatcherState::new(0);
        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        s.active_calls.insert(key, Instant::now());

        let sample = proto::AudioSample {
            short_name: "smco".into(),
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
        let s = DispatcherState::new(0);
        let sample = proto::AudioSample {
            short_name: "smco".into(),
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
        let s = DispatcherState::new(0);
        let sample = proto::AudioSample {
            short_name: "smco".into(),
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
        let mut s = DispatcherState::new(0);
        let effects = s.on_event(make_call_start("smco", 12345));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::ResolveChannel { .. }))
        );
        // Active immediately so audio can buffer during resolution
        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        assert!(s.is_active(&key));
    }

    #[test]
    fn call_start_registers_active_and_resolves() {
        let mut s = DispatcherState::new(0);
        // A fresh call_start registers the call as active immediately (so audio
        // can buffer) and asks the executor to resolve a channel + run a handler.
        let effects = s.on_event(make_call_start("smco", 12345));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::ResolveChannel { .. }))
        );

        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        assert!(s.is_active(&key));
    }

    #[test]
    fn channel_resolution_failed_removes_active() {
        let mut s = DispatcherState::new(0);
        s.on_event(make_call_start("smco", 12345));
        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        assert!(s.is_active(&key));
        s.on_channel_resolution_failed(&key);
        assert!(!s.is_active(&key));
    }

    #[test]
    fn call_end_for_active_call_removes() {
        let mut s = DispatcherState::new(0);
        // Simulate full lifecycle: start → resolve → end
        s.on_event(make_call_start("smco", 12345));

        let effects = s.on_event(make_call_end("smco", 12345));

        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        assert!(!s.is_active(&key));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::EndCall { .. }))
        );
    }

    #[test]
    fn call_end_for_unknown_call_no_effect() {
        let mut s = DispatcherState::new(0);
        let effects = s.on_event(make_call_end("smco", 99999));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::EndCall { .. }))
        );
    }

    #[test]
    fn overlapping_call_start_finalizes_previous_handler() {
        let mut s = DispatcherState::new(0);
        // First call_start for the talkgroup, resolved into an active handler.
        s.on_event(make_call_start("smco", 12345));

        // Second call_start arrives before the first ended (overlap). It must
        // end the previous handler before resolving the new one, otherwise the
        // first embed is orphaned grey when its sender is overwritten.
        let effects = s.on_event(make_call_start("smco", 12345));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::EndCall { .. })),
            "overlapping call_start must finalize the previous handler"
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::ResolveChannel { .. })),
            "overlapping call_start must still resolve the new call"
        );

        // The talkgroup is still active (now owned by the new call).
        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        assert!(s.is_active(&key));
    }

    #[test]
    fn coalescing_call_start_does_not_finalize() {
        // A call_start that resumes a coalescing call must NOT emit EndCall —
        // it resumes the same handler rather than replacing it.
        let mut s = DispatcherState::new(5000);
        s.on_event(make_call_start("smco", 12345));
        // End moves it to coalescing (between PTTs).
        s.on_event(make_call_end("smco", 12345));

        let effects = s.on_event(make_call_start("smco", 12345));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::CoalesceResume { .. })),
            "coalescing resume expected"
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::EndCall { .. })),
            "coalescing resume must not finalize the handler"
        );
    }

    // ── Signal routing ───────────────────────────────────────────────────

    #[test]
    fn signal_during_active_call_routes_to_call() {
        let mut s = DispatcherState::new(0);
        s.on_event(make_call_start("smco", 12345));

        let event = proto::PluginEvent {
            event: Some(proto::plugin_event::Event::Signal(proto::SignalEvent {
                call: Some(proto::CallInfo {
                    talkgroup: 12345,
                    short_name: "smco".into(),
                    ..Default::default()
                }),
                signal_type: 0,
                unit_id: 100,
                ..Default::default()
            })),
        };
        let effects = s.on_event(event);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::SendEvent { .. }))
        );
    }

    #[test]
    fn signal_without_active_call_goes_to_aggregator() {
        let mut s = DispatcherState::new(0);
        let event = proto::PluginEvent {
            event: Some(proto::plugin_event::Event::Signal(proto::SignalEvent {
                call: Some(proto::CallInfo {
                    talkgroup: 99999,
                    ..Default::default()
                }),
                ..Default::default()
            })),
        };
        let effects = s.on_event(event);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::ForwardToAggregator { .. }))
        );
    }

    // ── Unit routing ─────────────────────────────────────────────────────

    #[test]
    fn unit_during_active_call_routes_to_call() {
        let mut s = DispatcherState::new(0);
        s.on_event(make_call_start("smco", 12345));

        let event = proto::PluginEvent {
            event: Some(proto::plugin_event::Event::Unit(proto::UnitEvent {
                talkgroup: 12345,
                system: Some(proto::SystemInfo {
                    short_name: "smco".into(),
                    ..Default::default()
                }),
                source_id: 100,
                ..Default::default()
            })),
        };
        let effects = s.on_event(event);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::SendEvent { .. }))
        );
    }

    #[test]
    fn unit_with_zero_talkgroup_goes_to_aggregator() {
        let mut s = DispatcherState::new(0);
        let event = proto::PluginEvent {
            event: Some(proto::plugin_event::Event::Unit(proto::UnitEvent {
                talkgroup: 0,
                system: Some(proto::SystemInfo {
                    ..Default::default()
                }),
                ..Default::default()
            })),
        };
        let effects = s.on_event(event);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::ForwardToAggregator { .. }))
        );
    }

    // ── Reaping ──────────────────────────────────────────────────────────

    #[test]
    fn reap_removes_old_calls() {
        let mut s = DispatcherState::new(0);
        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        s.active_calls.insert(
            key.clone(),
            Instant::now() - std::time::Duration::from_secs(700),
        );

        let effects = s.on_reap_tick();
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::ReapCall { .. }))
        );
        assert!(!s.is_active(&key));
    }

    #[test]
    fn reap_keeps_fresh_calls() {
        let mut s = DispatcherState::new(0);
        let key = CallKey {
            system: Arc::from("smco"),
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
        let mut s = DispatcherState::new(0);
        let event = proto::PluginEvent {
            event: Some(proto::plugin_event::Event::CallsActive(
                proto::CallsActiveEvent { calls: vec![] },
            )),
        };
        let effects = s.on_event(event);
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::ForwardToAggregator { .. }))
        );
    }

    // ── Coalescing ───────────────────────────────────────────────────────

    #[test]
    fn coalesce_off_call_end_emits_end_call() {
        let mut s = DispatcherState::new(0);
        s.on_event(make_call_start("smco", 12345));
        let effects = s.on_event(make_call_end("smco", 12345));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::EndCall { .. }))
        );
        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        assert!(!s.is_coalescing(&key));
    }

    #[test]
    fn coalesce_on_call_end_emits_ptt_end() {
        let mut s = DispatcherState::new(10_000);
        s.on_event(make_call_start("smco", 12345));
        let effects = s.on_event(make_call_end("smco", 12345));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::PttEnd { .. }))
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::EndCall { .. }))
        );
        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        assert!(!s.is_active(&key));
        assert!(s.is_coalescing(&key));
    }

    #[test]
    fn coalesce_call_start_during_gap_emits_resume() {
        let mut s = DispatcherState::new(10_000);
        // Start → resolve → end (enters coalescing)
        s.on_event(make_call_start("smco", 12345));
        s.on_event(make_call_end("smco", 12345));

        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        assert!(s.is_coalescing(&key));

        // New CallStart while coalescing
        let effects = s.on_event(make_call_start("smco", 12345));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::CoalesceResume { .. }))
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::ResolveChannel { .. }))
        );
        assert!(s.is_active(&key));
        assert!(!s.is_coalescing(&key));
    }

    #[test]
    fn coalesce_timeout_fires_on_tick() {
        let mut s = DispatcherState::new(100); // 100ms timeout
        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        // Simulate an entry that's already expired
        s.coalescing_calls
            .insert(key.clone(), Instant::now() - Duration::from_millis(200));

        let effects = s.on_coalesce_tick();
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::CoalesceTimeout { .. }))
        );
        assert!(!s.is_coalescing(&key));
    }

    #[test]
    fn coalesce_fresh_entry_not_timed_out() {
        let mut s = DispatcherState::new(10_000);
        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        s.coalescing_calls.insert(key.clone(), Instant::now());

        let effects = s.on_coalesce_tick();
        assert!(effects.is_empty());
        assert!(s.is_coalescing(&key));
    }

    #[test]
    fn coalesce_tick_noop_when_disabled() {
        let mut s = DispatcherState::new(0);
        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        // This shouldn't happen in practice, but verify it's safe
        s.coalescing_calls
            .insert(key, Instant::now() - Duration::from_secs(100));
        let effects = s.on_coalesce_tick();
        assert!(effects.is_empty());
    }

    #[test]
    fn audio_during_coalesce_gap_not_routed() {
        let mut s = DispatcherState::new(10_000);
        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        s.coalescing_calls.insert(key, Instant::now());

        let sample = proto::AudioSample {
            short_name: "smco".into(),
            talkgroup: 12345,
            samples: vec![1, 2],
            sample_rate: 16000,
            freq: 851_125_000.0,
        };
        let effects = s.on_audio(&sample);
        assert!(effects.is_empty());
    }

    // ── Conventional calls coalesce like trunked ─────────────────────

    fn make_conventional_call_end(system: &str, talkgroup: i64) -> proto::PluginEvent {
        proto::PluginEvent {
            event: Some(proto::plugin_event::Event::CallEnd(proto::CallEndEvent {
                call_data: Some(proto::CallData {
                    short_name: system.into(),
                    talkgroup,
                    length: 5.0,
                    is_conventional: true,
                    ..Default::default()
                }),
                ..Default::default()
            })),
        }
    }

    #[test]
    fn an_evicted_call_says_so_on_its_embed_then_ends() {
        let mut s = DispatcherState::new(10_000);
        s.on_event(make_call_start("smco", 12345));
        let mut end = make_call_end("smco", 12345);
        if let Some(proto::plugin_event::Event::CallEnd(e)) = &mut end.event {
            e.reason = proto::CallEndReason::Evicted as i32;
        }
        let effects = s.on_event(end);
        assert!(matches!(
            effects.as_slice(),
            [
                DispatchEffect::SendEvent { text, .. },
                DispatchEffect::PttEnd { .. }
            ] if text == EVICTED
        ));
        // any other end says nothing
        s.on_event(make_call_start("smco", 12345));
        let effects = s.on_event(make_call_end("smco", 12345));
        assert!(matches!(
            effects.as_slice(),
            [DispatchEffect::PttEnd { .. }]
        ));
    }

    #[test]
    fn conventional_call_coalesces_like_trunked() {
        let mut s = DispatcherState::new(10_000); // coalescing enabled
        s.on_event(make_call_start("smco", 12345));

        // trunk-oxide now emits one CallStart/CallEnd per keyed transmission for
        // conventional channels, so CallEnd coalesces (PttEnd), not EndCall.
        let effects = s.on_event(make_conventional_call_end("smco", 12345));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::PttEnd { .. })),
            "conventional call should coalesce"
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::EndCall { .. })),
            "conventional call should not end immediately when coalescing"
        );
        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        assert!(s.is_coalescing(&key));
        assert!(!s.is_active(&key));
    }

    #[test]
    fn trunked_call_still_coalesces() {
        let mut s = DispatcherState::new(10_000); // coalescing enabled
        s.on_event(make_call_start("smco", 12345));

        // CallEnd for a trunked call should still coalesce
        let effects = s.on_event(make_call_end("smco", 12345));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, DispatchEffect::PttEnd { .. })),
            "trunked call should coalesce"
        );
        let key = CallKey {
            system: Arc::from("smco"),
            talkgroup: Talkgroup(12345),
        };
        assert!(s.is_coalescing(&key));
    }
}
