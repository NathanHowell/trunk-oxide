use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use serenity::model::id::ChannelId;
use tokio::sync::mpsc;
use tokio::time::{self, Duration};

use crate::discord::DiscordClient;
use crate::proto;

pub enum AggregatorMessage {
    Signal(proto::SignalEvent),
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
            // Setup events are posted immediately
            AggregatorMessage::SetupConfig(e) => {
                let text = format_setup_config(&e);
                self.post_immediately(&text).await;
            }
            AggregatorMessage::SetupSystems(e) => {
                let text = format_setup_systems(&e);
                self.post_immediately(&text).await;
            }
            AggregatorMessage::SetupSources(e) => {
                let text = format_setup_sources(&e);
                self.post_immediately(&text).await;
            }
            AggregatorMessage::SetupSystem(e) => {
                let text = format_setup_system(&e);
                self.post_immediately(&text).await;
            }
            AggregatorMessage::SetupRecorder(e) => {
                let text = format_setup_recorder(&e);
                self.post_immediately(&text).await;
            }
            // Everything else is buffered
            AggregatorMessage::Signal(e) => {
                self.buffer.push(format_signal(&e));
            }
            AggregatorMessage::TrunkMessage(e) => {
                self.buffer.push(format_trunk_message(&e));
            }
            AggregatorMessage::Unit(e) => {
                self.buffer.push(format_unit(&e));
            }
            AggregatorMessage::SystemRates(e) => {
                self.buffer.push(format_system_rates(&e));
            }
            AggregatorMessage::CallsActive(e) => {
                self.buffer.push(format_calls_active(&e));
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

    async fn flush(&mut self) {
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
            if batch.len() + line.len() + 1 > 2000 {
                if !batch.is_empty() {
                    if let Err(e) = self.discord.send_message(thread_id, &batch).await {
                        tracing::error!(error = %e, "failed to post aggregated events");
                    }
                    batch.clear();
                }
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

        let thread_name = format!("System Events - {}", hour_key);
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
        text.push_str(&format!(
            "  System: {} ({}) - type: {}\n",
            sys.short_name, sys.sys_num, sys.system_type
        ));
    }
    for src in &e.sources {
        text.push_str(&format!(
            "  Source {}: {} @ {:.3} MHz\n",
            src.src_num,
            src.driver,
            src.center_freq / 1_000_000.0
        ));
    }
    text
}

fn format_setup_systems(e: &proto::SetupSystemsEvent) -> String {
    let mut text = String::from("**Systems**\n");
    for sys in &e.systems {
        text.push_str(&format!(
            "  {} (sys_num={}, type={})\n",
            sys.short_name, sys.sys_num, sys.system_type
        ));
    }
    text
}

fn format_setup_sources(e: &proto::SetupSourcesEvent) -> String {
    let mut text = String::from("**Sources**\n");
    for src in &e.sources {
        text.push_str(&format!(
            "  Source {}: {} @ {:.3} MHz (analog={}, digital={})\n",
            src.src_num,
            src.driver,
            src.center_freq / 1_000_000.0,
            src.analog_recorders,
            src.digital_recorders
        ));
    }
    text
}

fn format_setup_system(e: &proto::SetupSystemEvent) -> String {
    if let Some(sys) = &e.system {
        format!(
            "**System**: {} (sys_num={}, type={})",
            sys.short_name, sys.sys_num, sys.system_type
        )
    } else {
        "**System**: (no data)".to_string()
    }
}

fn format_setup_recorder(e: &proto::SetupRecorderEvent) -> String {
    if let Some(rec) = &e.recorder {
        format!(
            "**Recorder**: id={} type={} freq={:.4} MHz",
            rec.id,
            rec.r#type,
            rec.freq / 1_000_000.0
        )
    } else {
        "**Recorder**: (no data)".to_string()
    }
}

fn format_signal(e: &proto::SignalEvent) -> String {
    let signal_type = proto::SignalType::try_from(e.signal_type)
        .map(|st| {
            st.as_str_name()
                .strip_prefix("SIGNAL_TYPE_")
                .unwrap_or(st.as_str_name())
                .to_string()
        })
        .unwrap_or_else(|_| format!("UNKNOWN({})", e.signal_type));
    format!("Signal: unit {} - {}", e.unit_id, signal_type)
}

fn format_trunk_message(e: &proto::TrunkMessageEvent) -> String {
    let count = e.messages.len();
    let sys_name = e
        .system
        .as_ref()
        .map(|s| s.short_name.as_str())
        .unwrap_or("unknown");
    format!("Trunk: {} messages from {}", count, sys_name)
}

fn format_unit(e: &proto::UnitEvent) -> String {
    let event_type = proto::UnitEventType::try_from(e.r#type)
        .map(|et| {
            et.as_str_name()
                .strip_prefix("UNIT_EVENT_TYPE_")
                .unwrap_or(et.as_str_name())
                .to_string()
        })
        .unwrap_or_else(|_| format!("UNKNOWN({})", e.r#type));
    format!("Unit: {} - source {}", event_type, e.source_id)
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
