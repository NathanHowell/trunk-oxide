//! Driver that links against `libtrunk-recorder.so` and drives it via FFI.
//!
//! This inverts the typical plugin relationship: instead of trunk-recorder
//! loading a Rust plugin via dlopen, this crate links against trunk-recorder
//! as a shared library and drives it directly.
//!
//! Events flow: SDR → C++ gnuradio graph → cxx callbacks → EventBridge

mod call_manager;
mod call_state;
mod dedup;

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use trunk_recorder_sys::{EventNotifier, GrEvent};

pub use call_manager::{CallManagerConfig, ConventionalChannel, SystemPolicy, TalkgroupPolicy};
pub use call_state::{Freq, Talkgroup};

/// Dedup key derived from a trunk message. Two identical TSBKs parsed by
/// different decoders on the same system produce the same key.
struct TrunkMsgKey {
    sys_num: i32,
    message_type: u8,
    opcode: u64,
    talkgroup: i64,
    source: i64,
    freq_bits: u64,
}

impl From<&ffi::TrTrunkMessage> for TrunkMsgKey {
    fn from(msg: &ffi::TrTrunkMessage) -> Self {
        Self {
            sys_num: msg.sys_num,
            message_type: msg.message_type.repr,
            opcode: msg.opcode,
            talkgroup: msg.talkgroup,
            source: msg.source,
            freq_bits: msg.freq.to_bits(),
        }
    }
}

impl Hash for TrunkMsgKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.sys_num.hash(state);
        self.message_type.hash(state);
        self.opcode.hash(state);
        self.talkgroup.hash(state);
        self.source.hash(state);
        self.freq_bits.hash(state);
    }
}

/// Create an interval whose first tick fires after `period` (not immediately).
fn delayed_interval(period: Duration) -> tokio::time::Interval {
    tokio::time::interval_at(tokio::time::Instant::now() + period, period)
}
use call_manager::CallManager;
use std::collections::HashMap;

use trunk_config::{Config, DefaultMode, Modulation, SourceDriver, SystemType};
use trunk_oxide_grpc::EventBridge;
use trunk_recorder_sys::ffi;

/// Configures and initializes a trunk-recorder instance without starting it.
///
/// Call [`start()`](TrunkRecorderBuilder::start) to consume the builder and
/// begin receiving radio traffic.
pub struct TrunkRecorderBuilder {
    ctx: cxx::UniquePtr<ffi::TrContext>,
    call_mgr_config: CallManagerConfig,
    bridge: Arc<EventBridge>,
}

/// A running trunk-recorder instance. Stops automatically on drop.
pub struct TrunkRecorder {
    ctx: cxx::UniquePtr<ffi::TrContext>,
    call_mgr_config: CallManagerConfig,
    bridge: Arc<EventBridge>,
    event_rx: tokio::sync::mpsc::Receiver<GrEvent>,
    pending: Arc<AtomicUsize>,
}

// Safety: TrContext is used exclusively from a single thread (the monitor thread).
// All FFI calls happen on that thread — no concurrent access.
#[allow(unsafe_code)]
unsafe impl Send for TrunkRecorder {}

impl TrunkRecorderBuilder {
    /// Build from a parsed TOML config using the builder FFI.
    ///
    /// This bypasses JSON config parsing entirely — Rust owns all config
    /// interpretation and drives C++ via structured setter calls.
    ///
    /// # Errors
    ///
    /// Returns an error if an SDR source fails to open or system setup fails.
    pub fn from_config(config: &Config, bridge: Arc<EventBridge>) -> anyhow::Result<Self> {
        let handler_box = trunk_recorder_sys::EventHandlerBox::new(bridge.clone());
        let mut ctx = ffi::tr_create_from_builder(Box::new(handler_box))?;

        // ── Config ──
        if let Some(rec) = &config.recorder {
            ffi::tr_config_set_temp_dir(ctx.pin_mut(), &rec.temp_dir);
            ffi::tr_config_set_soft_vocoder(ctx.pin_mut(), rec.soft_vocoder);
            ffi::tr_config_set_debug_recorder(
                ctx.pin_mut(),
                rec.debug.enabled,
                &rec.debug.address,
                rec.debug.port,
            );
        }

        // ── Sources ──
        for src in &config.source {
            if src.enabled == Some(false) {
                continue;
            }

            let driver_str = match src.driver {
                SourceDriver::Osmosdr => "osmosdr",
                SourceDriver::Usrp => "usrp",
                SourceDriver::Sigmf => "sigmf",
                SourceDriver::Iqfile => "iqfile",
            };

            // Collect all gain stages into a flat list
            let mut gain_stages: Vec<ffi::TrGainStage> = src
                .gain_settings
                .iter()
                .map(|(name, &value)| ffi::TrGainStage {
                    name: name.clone(),
                    value,
                })
                .collect();
            let named_gains = [
                ("IF", src.if_gain),
                ("BB", src.bb_gain),
                ("MIX", src.mix_gain),
                ("LNA", src.lna_gain),
                ("TIA", src.tia_gain),
                ("PGA", src.pga_gain),
                ("AMP", src.amp_gain),
                ("VGA", src.vga_gain),
                ("VGA1", src.vga1_gain),
                ("VGA2", src.vga2_gain),
            ];
            for (name, value) in named_gains {
                if value != 0.0 {
                    gain_stages.push(ffi::TrGainStage {
                        name: name.into(),
                        value,
                    });
                }
            }

            ffi::tr_add_source_from_config(
                ctx.pin_mut(),
                &ffi::TrSourceConfig {
                    driver: driver_str.into(),
                    device: src.device.clone(),
                    center: src.center,
                    rate: src.rate,
                    error: src.error,
                    gain: src.gain,
                    agc: src.agc,
                    ppm: src.ppm,
                    antenna: src.antenna.clone(),
                    silence_frames: src.silence_frames,
                    signal_detector_threshold: src.signal_detector_threshold.unwrap_or(0.0).into(),
                    has_signal_detector_threshold: src.signal_detector_threshold.is_some(),
                    gain_stages,
                    digital_recorders: src.digital_recorders,
                    analog_recorders: src.analog_recorders,
                    sigmf_recorders: src.sigmf_recorders,
                },
            )?;
        }

        // ── Systems ──
        let mut system_policies = HashMap::new();
        for (short_name, sys) in &config.system {
            let type_str = match sys.system_type {
                SystemType::Conventional => "conventional",
                SystemType::ConventionalP25 => "conventionalP25",
                SystemType::ConventionalDMR => "conventionalDMR",
                SystemType::ConventionalSigmf => "conventionalSIGMF",
                SystemType::Smartnet => "smartnet",
                SystemType::P25 => "p25",
            };

            let sys_idx = ffi::tr_add_system_from_config(
                ctx.pin_mut(),
                &ffi::TrSystemConfig {
                    short_name: short_name.clone(),
                    system_type: type_str.into(),
                    qpsk_mod: sys.modulation == Modulation::Qpsk,
                    squelch_db: sys.squelch,
                    analog_levels: sys.analog_levels,
                    digital_levels: sys.digital_levels,
                    tau: sys.deemphasis_tau.into(),
                    max_dev: sys.max_dev,
                    filter_width: sys.filter_width,
                    decode_mdc: sys.decode_mdc,
                    decode_fsync: sys.decode_fsync,
                    decode_star: sys.decode_star,
                    decode_tps: sys.decode_tps,
                    bandplan: sys.bandplan.clone().unwrap_or_default(),
                    bandplan_base: sys.bandplan_base,
                    bandplan_high: sys.bandplan_high,
                    bandplan_spacing: sys.bandplan_spacing,
                    bandplan_offset: sys.bandplan_offset,
                    bandfreq: 0.0,
                    sys_id: sys.sys_id,
                    wacn: sys.wacn,
                    nac: sys.nac,
                    control_channels: sys.control_channels.clone(),
                    channels: sys
                        .channel
                        .iter()
                        .map(|ch| ffi::TrChannelConfig {
                            id: ch.id,
                            freq: ch.freq,
                            tone: ch.tone,
                            squelch_db: ch.squelch_db,
                            signal_detection: ch.signal_detection,
                        })
                        .collect(),
                    talkgroups: sys
                        .talkgroup
                        .iter()
                        .map(|tg| ffi::TrTalkgroupConfig {
                            id: tg.id,
                            mode: tg.mode.as_str().into(),
                            priority: tg.priority,
                            preferred_nac: tg.preferred_nac,
                        })
                        .collect(),
                    unit_tags: sys
                        .unit_tag
                        .iter()
                        .map(|ut| ffi::TrUnitTagConfig {
                            pattern: ut.pattern.clone(),
                            tag: ut.tag.clone(),
                        })
                        .collect(),
                    unit_tags_mode: sys
                        .unit_tags
                        .as_ref()
                        .map(|s| s.mode.clone())
                        .unwrap_or_default(),
                },
            )?;

            // Build recording policy for this system
            let talkgroups = sys
                .talkgroup
                .iter()
                .map(|tg| {
                    (
                        Talkgroup(tg.id),
                        TalkgroupPolicy {
                            mode: tg.mode,
                            priority: tg.priority,
                            preferred_nac: tg.preferred_nac,
                        },
                    )
                })
                .collect();

            let channels = sys
                .channel
                .iter()
                .map(|ch| {
                    let is_analog = sys.system_type == SystemType::Conventional;
                    let priority = sys
                        .talkgroup
                        .iter()
                        .find(|tg| tg.id == ch.id)
                        .map_or(0, |tg| tg.priority);
                    ConventionalChannel {
                        talkgroup: Talkgroup(ch.id),
                        freq: Freq::from_hz(ch.freq),
                        is_analog,
                        priority,
                    }
                })
                .collect();

            system_policies.insert(
                sys_idx,
                SystemPolicy {
                    system_type: sys.system_type,
                    short_name: short_name.clone(),
                    record_unknown: sys.record_unknown,
                    hide_unknown: sys.hide_unknown_talkgroups,
                    hide_encrypted: sys.hide_encrypted,
                    monitor_encrypted: sys.monitor_encrypted,
                    multi_site: sys.multi_site,
                    multi_site_name: sys.multi_site_system_name.clone(),
                    conversation_mode: sys.conversation_mode,
                    max_duration: sys.max_duration,
                    control_channels: sys.control_channels.iter().map(|&f| Freq::from_hz(f)).collect(),
                    talkgroups,
                    channels,
                    sys_id: sys.sys_id,
                    wacn: sys.wacn,
                    nac: sys.nac,
                },
            );
        }

        // ── Finalize: setup_systems (C++) then fire events (Rust) ──
        ffi::tr_finalize(ctx.pin_mut())?;

        // Fire setup events now that systems are initialized
        {
            let infos: Vec<trunk_recorder_sys::SystemInfo> = system_policies
                .iter()
                .map(|(&sys_idx, policy)| {
                    let ffi_info = ffi::tr_context_get_system_info(ctx.pin_mut(), sys_idx);
                    let mut info = trunk_recorder_sys::SystemInfo::from_ffi(&ffi_info);
                    info.short_name = policy.short_name.clone();
                    info.conversation_mode = policy.conversation_mode;
                    info.sys_id = policy.sys_id;
                    info.wacn = policy.wacn;
                    info.nac = policy.nac;
                    info
                })
                .collect();
            bridge.setup_config(&infos);
            for info in &infos {
                bridge.setup_system(info);
            }
        }

        let rec = config.recorder.as_ref();
        let call_mgr_config = CallManagerConfig {
            call_timeout: rec.map_or(3.0, |r| r.call_timeout),
            new_call_from_update: rec.map_or(true, |r| r.new_call_from_update),
            record_uu_v_calls: rec.map_or(true, |r| r.record_uu_v_calls),
            default_mode: rec.map_or(DefaultMode::default(), |r| r.default_mode),
            control_warn_rate: rec.map_or(10, |r| r.control_warn_rate),
            systems: system_policies,
        };

        Ok(Self {
            ctx,
            call_mgr_config,
            bridge,
        })
    }

    /// Start the gnuradio flowgraph (without the C++ monitor thread).
    ///
    /// After calling this, use [`TrunkRecorder::run_monitor`] on a dedicated
    /// thread to drive the main loop from Rust.
    pub fn start(mut self) -> TrunkRecorder {
        // Create the event channel and wire notifiers before starting GR
        let pending = Arc::new(AtomicUsize::new(0));
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);
        let notifier = EventNotifier::new(event_tx, Arc::clone(&pending));
        ffi::tr_context_set_event_notifier(self.ctx.pin_mut(), Box::new(notifier));
        ffi::tr_context_wire_event_notifiers(self.ctx.pin_mut());

        ffi::tr_start_gnuradio(self.ctx.pin_mut());
        TrunkRecorder {
            ctx: self.ctx,
            call_mgr_config: self.call_mgr_config,
            bridge: self.bridge,
            event_rx,
            pending,
        }
    }
}

impl TrunkRecorder {
    /// Shared counter of events queued but not yet consumed by the monitor loop.
    pub fn event_channel_depth(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.pending)
    }

    /// Run the monitor loop until ctrl_c is received.
    ///
    /// Must be called from within a tokio runtime context (e.g. via
    /// `Handle::block_on`). All FFI calls are made on the calling thread,
    /// keeping `TrContext` (C++ objects) pinned.
    pub async fn run_monitor(&mut self) -> anyhow::Result<()> {
        let mut call_mgr = CallManager::new(self.call_mgr_config.clone(), self.bridge.clone());

        // Drain conventional calls created by C++ setup_systems during tr_finalize.
        // C++ creates one Call per channel in config order — match them up.
        for (&sys_num, policy) in &self.call_mgr_config.systems {
            for channel in &policy.channels {
                let call = ffi::tr_context_take_next_conventional_call(self.ctx.pin_mut());
                if call.is_null() {
                    break;
                }
                call_mgr.add_call(call, channel, sys_num);
            }
        }

        let mut trunk_dedup = dedup::WindowedDedup::new(64);

        let mut tick = delayed_interval(Duration::from_secs(1));
        let mut decode = delayed_interval(Duration::from_secs(3));
        let mut status = delayed_interval(Duration::from_secs(200));
        let mut autotune = delayed_interval(Duration::from_secs(200));

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down");
                    call_mgr.conclude_all(self.ctx.pin_mut());
                    return Ok(());
                },
                Some(event) = self.event_rx.recv() => {
                    self.pending.fetch_sub(1, Ordering::Relaxed);
                    match event {
                        GrEvent::TrunkMessages(messages) => {
                            let messages: Vec<_> = messages
                                .into_iter()
                                .filter(|m| trunk_dedup.admit(&TrunkMsgKey::from(&*m)))
                                .collect();
                            if !messages.is_empty() {
                                let mut by_system: HashMap<i32, Vec<usize>> = HashMap::new();
                                for (i, msg) in messages.iter().enumerate() {
                                    by_system.entry(msg.sys_num).or_default().push(i);
                                }
                                for (&sys_num, indices) in &by_system {
                                    let sys_msgs: Vec<ffi::TrTrunkMessage> =
                                        indices.iter().map(|&i| messages[i].clone()).collect();
                                    let info =
                                        call_mgr.get_system_info(self.ctx.pin_mut(), sys_num);
                                    self.bridge.trunk_message(&sys_msgs, &info);
                                }
                            }
                            for msg in &messages {
                                call_mgr.handle_message(self.ctx.pin_mut(), msg);
                            }
                        }
                        GrEvent::Squelch {
                            recorder_num,
                            muted,
                            pwr_db,
                        } => {
                            call_mgr.handle_squelch_event(recorder_num, muted, pwr_db);
                        }
                        GrEvent::SignalDetected => {
                            ffi::tr_context_check_conventional_detection(self.ctx.pin_mut());
                        }
                    }
                },
                _ = tick.tick() => {
                    call_mgr.tick(self.ctx.pin_mut());
                },
                _ = decode.tick() => {
                    let time_diff = decode.period().as_secs_f32();
                    call_mgr.check_decode_rates(self.ctx.pin_mut(), time_diff)?;
                    ffi::tr_context_check_sources(self.ctx.pin_mut())?;
                    call_mgr.clear_stale_patches();
                },
                _ = status.tick() => {
                    call_mgr.print_status(self.ctx.pin_mut());
                    call_mgr.print_active_patches();
                    call_mgr.print_recorder_status(&*self.ctx);
                },
                _ = autotune.tick() => {
                    ffi::tr_context_autotune_control_channels(self.ctx.pin_mut());
                },
            }

            // Cheap sync work after every wake
            call_mgr.process_recorder_queues();
        }
    }
}

impl Drop for TrunkRecorder {
    fn drop(&mut self) {
        ffi::tr_stop_gnuradio(self.ctx.pin_mut());
    }
}
