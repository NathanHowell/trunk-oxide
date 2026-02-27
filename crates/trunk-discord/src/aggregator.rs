use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use serenity::model::id::ChannelId;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};
use trunk_oxide_proto as proto;

use crate::discord::DiscordClient;

pub enum AggregatorMessage {
    Signal(Box<proto::SignalEvent>),
    TrunkMessage(proto::TrunkMessageEvent),
    Unit(proto::UnitEvent),
    SystemRates(proto::SystemRatesEvent),
    CallsActive(proto::CallsActiveEvent),
    SetupRecorder(proto::SetupRecorderEvent),
    SetupSystem(proto::SetupSystemEvent),
    SetupSystems(proto::SetupSystemsEvent),
    SetupSources(proto::SetupSourcesEvent),
    SetupConfig(proto::SetupConfigEvent),
}

pub struct Aggregator {
    tx: mpsc::UnboundedSender<AggregatorMessage>,
}

impl Aggregator {
    pub fn new(discord: Arc<DiscordClient>, channel_id: u64) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();

        let inner = AggregatorInner {
            discord,
            channel_id,
            rx,
            buffer: Vec::new(),
            current_thread_id: None,
            current_hour: String::new(),
            // Setup event dedup
            last_setup_config: None,
            last_setup_systems: None,
            last_setup_sources: None,
            last_setup_system: HashMap::new(),
            last_setup_recorder: HashMap::new(),
            // Aggregated counters
            trunk_message_counts: HashMap::new(),
            unit_counts: HashMap::new(),
            latest_rates: None,
            latest_calls_active: None,
        };

        tokio::spawn(inner.run());

        Self { tx }
    }

    pub fn send(&self, msg: AggregatorMessage) {
        let _ = self.tx.send(msg);
    }
}

struct AggregatorInner {
    discord: Arc<DiscordClient>,
    channel_id: u64,
    rx: mpsc::UnboundedReceiver<AggregatorMessage>,
    buffer: Vec<String>,
    current_thread_id: Option<ChannelId>,
    current_hour: String,

    // Setup event dedup: only post when content changes
    last_setup_config: Option<String>,
    last_setup_systems: Option<String>,
    last_setup_sources: Option<String>,
    last_setup_system: HashMap<i32, String>,
    last_setup_recorder: HashMap<i32, String>,

    // Aggregated counters (flushed periodically)
    trunk_message_counts: HashMap<String, (u64, u64)>, // sys_name -> (messages, batches)
    unit_counts: HashMap<String, u64>,                  // event_type -> count
    latest_rates: Option<String>,
    latest_calls_active: Option<String>,
}

impl AggregatorInner {
    async fn run(mut self) {
        let mut flush_interval = time::interval(Duration::from_secs(60));

        loop {
            tokio::select! {
                Some(msg) = self.rx.recv() => {
                    self.handle_message(msg).await;
                }
                _ = flush_interval.tick() => {
                    self.flush().await;
                }
            }
        }
    }

    async fn handle_message(&mut self, msg: AggregatorMessage) {
        match msg {
            // Setup events: deduplicate by content
            AggregatorMessage::SetupConfig(e) => {
                let text = format_setup_config(&e);
                if self.last_setup_config.as_ref() != Some(&text) {
                    self.last_setup_config = Some(text.clone());
                    self.post_immediately(&text).await;
                }
            }
            AggregatorMessage::SetupSystems(e) => {
                let text = format_setup_systems(&e);
                if self.last_setup_systems.as_ref() != Some(&text) {
                    self.last_setup_systems = Some(text.clone());
                    self.post_immediately(&text).await;
                }
            }
            AggregatorMessage::SetupSources(e) => {
                let text = format_setup_sources(&e);
                if self.last_setup_sources.as_ref() != Some(&text) {
                    self.last_setup_sources = Some(text.clone());
                    self.post_immediately(&text).await;
                }
            }
            AggregatorMessage::SetupSystem(e) => {
                let key = e.system.as_ref().map_or(0, |s| s.sys_num);
                let text = format_setup_system(&e);
                if self.last_setup_system.get(&key) != Some(&text) {
                    self.last_setup_system.insert(key, text.clone());
                    self.post_immediately(&text).await;
                }
            }
            AggregatorMessage::SetupRecorder(e) => {
                let key = e.recorder.as_ref().map_or(0, |r| r.id);
                let text = format_setup_recorder(&e);
                if self.last_setup_recorder.get(&key) != Some(&text) {
                    self.last_setup_recorder.insert(key, text.clone());
                    self.post_immediately(&text).await;
                }
            }
            // Signals are kept as individual lines (relatively rare)
            AggregatorMessage::Signal(e) => {
                self.buffer.push(format_signal(&e));
            }
            // Trunk messages: aggregate counts per system
            AggregatorMessage::TrunkMessage(e) => {
                let sys_name = e
                    .system
                    .as_ref()
                    .map_or_else(|| "unknown".to_string(), |s| s.short_name.clone());
                let msg_count = e.messages.len() as u64;
                let entry = self.trunk_message_counts.entry(sys_name).or_insert((0, 0));
                entry.0 += msg_count;
                entry.1 += 1;
            }
            // Unit events: aggregate by type
            AggregatorMessage::Unit(e) => {
                let event_type = format_unit_type(e.r#type);
                *self.unit_counts.entry(event_type).or_insert(0) += 1;
            }
            // Rates / calls active: keep only latest
            AggregatorMessage::SystemRates(e) => {
                self.latest_rates = Some(format_system_rates(&e));
            }
            AggregatorMessage::CallsActive(e) => {
                self.latest_calls_active = Some(format_calls_active(&e));
            }
        }
    }

    async fn post_immediately(&mut self, text: &str) {
        if let Err(e) = self.ensure_hourly_thread().await {
            tracing::error!(error = %e, "failed to ensure hourly thread");
            return;
        }
        if let Some(thread_id) = self.current_thread_id {
            if let Err(e) = self.discord.send_message(thread_id, text).await {
                tracing::error!(error = %e, "failed to post setup event");
            }
        }
    }

    fn drain_aggregated(&mut self) {
        // Trunk message summaries
        if !self.trunk_message_counts.is_empty() {
            let mut systems: Vec<_> = self.trunk_message_counts.drain().collect();
            systems.sort_by(|a, b| a.0.cmp(&b.0));
            let summaries: Vec<String> = systems
                .into_iter()
                .map(|(name, (msgs, batches))| format!("{name}: {msgs} msgs ({batches} batches)"))
                .collect();
            self.buffer.push(format!("Trunk: {}", summaries.join(", ")));
        }

        // Unit event summaries
        if !self.unit_counts.is_empty() {
            let mut types: Vec<_> = self.unit_counts.drain().collect();
            types.sort_by(|a, b| a.0.cmp(&b.0));
            let summaries: Vec<String> = types
                .into_iter()
                .map(|(typ, count)| format!("{count} {typ}"))
                .collect();
            self.buffer.push(format!("Units: {}", summaries.join(", ")));
        }

        // Latest rates snapshot
        if let Some(rates) = self.latest_rates.take() {
            self.buffer.push(rates);
        }

        // Latest calls active snapshot
        if let Some(calls) = self.latest_calls_active.take() {
            self.buffer.push(calls);
        }
    }

    async fn flush(&mut self) {
        self.drain_aggregated();

        if self.buffer.is_empty() {
            return;
        }

        if let Err(e) = self.ensure_hourly_thread().await {
            tracing::error!(error = %e, "failed to ensure hourly thread");
            return;
        }

        let Some(thread_id) = self.current_thread_id else {
            return;
        };

        // Batch messages under Discord's 2000-char limit
        let mut batch = String::new();
        for line in self.buffer.drain(..) {
            if batch.len() + line.len() + 1 > 2000 && !batch.is_empty() {
                if let Err(e) = self.discord.send_message(thread_id, &batch).await {
                    tracing::error!(error = %e, "failed to post aggregated events");
                }
                batch.clear();
            }
            if !batch.is_empty() {
                batch.push('\n');
            }
            batch.push_str(&line);
        }

        if !batch.is_empty() {
            if let Err(e) = self.discord.send_message(thread_id, &batch).await {
                tracing::error!(error = %e, "failed to post aggregated events");
            }
        }
    }

    async fn ensure_hourly_thread(&mut self) -> Result<()> {
        let now = Utc::now();
        let hour_key = now.format("%Y-%m-%d %H:00").to_string();

        if self.current_hour == hour_key && self.current_thread_id.is_some() {
            return Ok(());
        }

        let thread_name = format!("System Events - {hour_key}");
        let thread_id = self
            .discord
            .create_call_thread(self.channel_id, &thread_name)
            .await?;

        self.current_thread_id = Some(thread_id);
        self.current_hour = hour_key;

        tracing::info!(thread = %thread_id, "created hourly summary thread");

        Ok(())
    }
}

fn format_setup_config(e: &proto::SetupConfigEvent) -> String {
    let mut text = String::from("**Setup Config**\n");
    for sys in &e.systems {
        let _ = writeln!(
            text,
            "  System: {} ({}) - type: {}",
            sys.short_name, sys.sys_num, sys.system_type
        );
    }
    for src in &e.sources {
        let _ = writeln!(
            text,
            "  Source {}: {} @ {:.3} MHz",
            src.src_num,
            src.driver,
            src.center_freq / 1_000_000.0
        );
    }
    text
}

fn format_setup_systems(e: &proto::SetupSystemsEvent) -> String {
    let mut text = String::from("**Systems**\n");
    for sys in &e.systems {
        let _ = writeln!(
            text,
            "  {} (sys_num={}, type={})",
            sys.short_name, sys.sys_num, sys.system_type
        );
    }
    text
}

fn format_setup_sources(e: &proto::SetupSourcesEvent) -> String {
    let mut text = String::from("**Sources**\n");
    for src in &e.sources {
        let _ = writeln!(
            text,
            "  Source {}: {} @ {:.3} MHz (analog={}, digital={})",
            src.src_num,
            src.driver,
            src.center_freq / 1_000_000.0,
            src.analog_recorders,
            src.digital_recorders
        );
    }
    text
}

fn format_setup_system(e: &proto::SetupSystemEvent) -> String {
    e.system.as_ref().map_or_else(
        || "**System**: (no data)".to_string(),
        |sys| {
            format!(
                "**System**: {} (sys_num={}, type={})",
                sys.short_name, sys.sys_num, sys.system_type
            )
        },
    )
}

fn format_setup_recorder(e: &proto::SetupRecorderEvent) -> String {
    e.recorder.as_ref().map_or_else(
        || "**Recorder**: (no data)".to_string(),
        |rec| {
            format!(
                "**Recorder**: id={} type={} freq={:.4} MHz",
                rec.id,
                rec.r#type,
                rec.freq / 1_000_000.0
            )
        },
    )
}

fn format_signal(e: &proto::SignalEvent) -> String {
    let signal_type = proto::SignalType::try_from(e.signal_type).map_or_else(
        |_| format!("UNKNOWN({})", e.signal_type),
        |st| {
            st.as_str_name()
                .strip_prefix("SIGNAL_TYPE_")
                .unwrap_or(st.as_str_name())
                .to_string()
        },
    );
    format!("Signal: unit {} - {signal_type}", e.unit_id)
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

fn format_system_rates(e: &proto::SystemRatesEvent) -> String {
    let rates: Vec<String> = e
        .systems
        .iter()
        .map(|s| format!("{}: {}msg/s", s.short_name, s.decode_rate))
        .collect();
    format!("Rates: {}", rates.join(", "))
}

fn format_calls_active(e: &proto::CallsActiveEvent) -> String {
    format!("Active calls: {}", e.calls.len())
}
