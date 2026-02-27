use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use serenity::all::CreateEmbed;
use serenity::model::id::{ChannelId, MessageId};
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
            current_thread_id: None,
            current_hour: String::new(),
            // Setup event dedup
            last_setup_config: None,
            last_setup_systems: None,
            last_setup_sources: None,
            last_setup_system: HashMap::new(),
            // Recorder table
            recorder_state: HashMap::new(),
            recorder_embed_id: None,
            // System activity embed
            activity_embed_id: None,
            // Aggregated counters
            signal_counts: HashMap::new(),
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
    current_thread_id: Option<ChannelId>,
    current_hour: String,

    // Setup event dedup: only post when content changes
    last_setup_config: Option<String>,
    last_setup_systems: Option<String>,
    last_setup_sources: Option<String>,
    last_setup_system: HashMap<i32, String>,

    // Recorder table: single embed edited in place
    recorder_state: HashMap<i32, proto::RecorderInfo>,
    recorder_embed_id: Option<MessageId>,

    // System activity: single embed edited in place
    activity_embed_id: Option<MessageId>,

    // Aggregated counters (flushed periodically)
    signal_counts: HashMap<String, u64>,               // signal_type -> count
    trunk_message_counts: HashMap<String, (u64, u64)>, // sys_name -> (messages, batches)
    unit_counts: HashMap<String, u64>,                  // event_type -> count
    latest_rates: Option<proto::SystemRatesEvent>,
    latest_calls_active: Option<usize>,
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
                    let embed = CreateEmbed::new()
                        .title("Setup Config")
                        .colour(0x3498db_u32)
                        .description(text);
                    self.post_embed(embed).await;
                }
            }
            AggregatorMessage::SetupSystems(e) => {
                let text = format_setup_systems(&e);
                if self.last_setup_systems.as_ref() != Some(&text) {
                    self.last_setup_systems = Some(text.clone());
                    let embed = CreateEmbed::new()
                        .title("Systems")
                        .colour(0x3498db_u32)
                        .description(text);
                    self.post_embed(embed).await;
                }
            }
            AggregatorMessage::SetupSources(e) => {
                let text = format_setup_sources(&e);
                if self.last_setup_sources.as_ref() != Some(&text) {
                    self.last_setup_sources = Some(text.clone());
                    let embed = CreateEmbed::new()
                        .title("Sources")
                        .colour(0x3498db_u32)
                        .description(text);
                    self.post_embed(embed).await;
                }
            }
            AggregatorMessage::SetupSystem(e) => {
                let key = e.system.as_ref().map_or(0, |s| s.sys_num);
                let text = format_setup_system(&e);
                if self.last_setup_system.get(&key) != Some(&text) {
                    self.last_setup_system.insert(key, text.clone());
                    let embed = CreateEmbed::new()
                        .title("System")
                        .colour(0x3498db_u32)
                        .description(text);
                    self.post_embed(embed).await;
                }
            }
            AggregatorMessage::SetupRecorder(e) => {
                if let Some(rec) = e.recorder {
                    self.recorder_state.insert(rec.id, rec);
                    self.update_recorder_embed().await;
                }
            }
            // Signals: aggregate by type
            AggregatorMessage::Signal(e) => {
                let signal_type = format_signal_type(e.signal_type);
                *self.signal_counts.entry(signal_type).or_insert(0) += 1;
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
                self.latest_rates = Some(e);
            }
            AggregatorMessage::CallsActive(e) => {
                self.latest_calls_active = Some(e.calls.len());
            }
        }
    }

    async fn post_embed(&mut self, embed: CreateEmbed) {
        if let Err(e) = self.ensure_hourly_thread().await {
            tracing::error!(error = %e, "failed to ensure hourly thread");
            return;
        }
        if let Some(thread_id) = self.current_thread_id {
            if let Err(e) = self.discord.send_embed(thread_id, embed).await {
                tracing::error!(error = %e, "failed to post setup event");
            }
        }
    }

    async fn update_recorder_embed(&mut self) {
        if let Err(e) = self.ensure_hourly_thread().await {
            tracing::error!(error = %e, "failed to ensure hourly thread");
            return;
        }
        let Some(thread_id) = self.current_thread_id else {
            return;
        };

        let table = build_recorder_table(&self.recorder_state);
        let embed = CreateEmbed::new()
            .title("Recorders")
            .colour(0x3498db_u32)
            .description(table);

        if let Some(msg_id) = self.recorder_embed_id {
            if let Err(e) = self.discord.edit_embed(thread_id, msg_id, embed).await {
                tracing::error!(error = %e, "failed to edit recorder embed");
            }
        } else {
            match self.discord.send_embed(thread_id, embed).await {
                Ok(msg_id) => self.recorder_embed_id = Some(msg_id),
                Err(e) => tracing::error!(error = %e, "failed to post recorder embed"),
            }
        }
    }

    fn has_activity(&self) -> bool {
        !self.signal_counts.is_empty()
            || !self.trunk_message_counts.is_empty()
            || !self.unit_counts.is_empty()
            || self.latest_rates.is_some()
            || self.latest_calls_active.is_some()
    }

    fn build_activity_embed(&mut self) -> CreateEmbed {
        let mut embed = CreateEmbed::new()
            .title("System Activity")
            .colour(0x95a5a6_u32); // grey

        // Signals: "12 NORMAL, 2 EMERGENCY"
        if !self.signal_counts.is_empty() {
            let mut types: Vec<_> = self.signal_counts.drain().collect();
            types.sort_by(|a, b| a.0.cmp(&b.0));
            let text: Vec<String> = types
                .into_iter()
                .map(|(typ, count)| format!("{count} {typ}"))
                .collect();
            embed = embed.field("Signals", text.join(", "), false);
        }

        // Trunk messages: "smco: 1364 msgs (1280 batches)"
        if !self.trunk_message_counts.is_empty() {
            let mut systems: Vec<_> = self.trunk_message_counts.drain().collect();
            systems.sort_by(|a, b| a.0.cmp(&b.0));
            let text: Vec<String> = systems
                .into_iter()
                .map(|(name, (msgs, batches))| format!("{name}: {msgs} msgs ({batches} batches)"))
                .collect();
            embed = embed.field("Trunk Messages", text.join("\n"), false);
        }

        // Unit events: "4 ACKNOWLEDGE_RESPONSE, 3 DATA_GRANT"
        if !self.unit_counts.is_empty() {
            let mut types: Vec<_> = self.unit_counts.drain().collect();
            types.sort_by(|a, b| a.0.cmp(&b.0));
            let text: Vec<String> = types
                .into_iter()
                .map(|(typ, count)| format!("{count} {typ}"))
                .collect();
            embed = embed.field("Unit Events", text.join(", "), false);
        }

        // Rates: inline fields per system
        if let Some(rates) = self.latest_rates.take() {
            let text: Vec<String> = rates
                .systems
                .iter()
                .map(|s| format!("{}: {} msg/s", s.short_name, s.decode_rate))
                .collect();
            embed = embed.field("Decode Rates", text.join(", "), true);
        }

        // Active calls
        if let Some(count) = self.latest_calls_active.take() {
            embed = embed.field("Active Calls", count.to_string(), true);
        }

        embed
    }

    async fn flush(&mut self) {
        if !self.has_activity() {
            return;
        }

        if let Err(e) = self.ensure_hourly_thread().await {
            tracing::error!(error = %e, "failed to ensure hourly thread");
            return;
        }

        let Some(thread_id) = self.current_thread_id else {
            return;
        };

        let embed = self.build_activity_embed();

        if let Some(msg_id) = self.activity_embed_id {
            if let Err(e) = self.discord.edit_embed(thread_id, msg_id, embed).await {
                tracing::error!(error = %e, "failed to edit activity embed");
            }
        } else {
            match self.discord.send_embed(thread_id, embed).await {
                Ok(msg_id) => self.activity_embed_id = Some(msg_id),
                Err(e) => tracing::error!(error = %e, "failed to post activity embed"),
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

        // Reset embed IDs so new embeds are posted in the new thread
        self.recorder_embed_id = None;
        self.recorder_state.clear();
        self.activity_embed_id = None;

        tracing::info!(thread = %thread_id, "created hourly summary thread");

        Ok(())
    }
}

fn format_setup_config(e: &proto::SetupConfigEvent) -> String {
    let mut text = String::new();
    for sys in &e.systems {
        let _ = writeln!(
            text,
            "System: {} ({}) - type: {}",
            sys.short_name, sys.sys_num, sys.system_type
        );
    }
    for src in &e.sources {
        let _ = writeln!(
            text,
            "Source {}: {} @ {:.3} MHz",
            src.src_num,
            src.driver,
            src.center_freq / 1_000_000.0
        );
    }
    text
}

fn format_setup_systems(e: &proto::SetupSystemsEvent) -> String {
    let mut text = String::new();
    for sys in &e.systems {
        let _ = writeln!(
            text,
            "{} (sys_num={}, type={})",
            sys.short_name, sys.sys_num, sys.system_type
        );
    }
    text
}

fn format_setup_sources(e: &proto::SetupSourcesEvent) -> String {
    let mut text = String::new();
    for src in &e.sources {
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

fn format_setup_system(e: &proto::SetupSystemEvent) -> String {
    e.system.as_ref().map_or_else(
        || "(no data)".to_string(),
        |sys| {
            format!(
                "{} (sys_num={}, type={})",
                sys.short_name, sys.sys_num, sys.system_type
            )
        },
    )
}

fn build_recorder_table(recorders: &HashMap<i32, proto::RecorderInfo>) -> String {
    let mut ids: Vec<i32> = recorders.keys().copied().collect();
    ids.sort();

    let mut table = String::from("```\n");
    let _ = writeln!(table, "{:<4} {:<8} {:>12} {:>10} {}", "ID", "Type", "Freq (MHz)", "TG", "State");
    for id in ids {
        let rec = &recorders[&id];
        let state = if rec.is_active {
            "active"
        } else if rec.is_idle {
            "idle"
        } else {
            "—"
        };
        let tg = if rec.talkgroup != 0 {
            rec.talkgroup.to_string()
        } else {
            "—".to_string()
        };
        let _ = writeln!(
            table,
            "{:<4} {:<8} {:>12.4} {:>10} {}",
            rec.id,
            rec.r#type,
            rec.freq / 1_000_000.0,
            tg,
            state,
        );
    }
    table.push_str("```");
    table
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

