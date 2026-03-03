//! Driver that links against `libtrunk-recorder.so` and drives it via FFI.
//!
//! This inverts the typical plugin relationship: instead of trunk-recorder
//! loading a Rust plugin via dlopen, this crate links against trunk-recorder
//! as a shared library and drives it directly.
//!
//! Events flow: SDR → C++ gnuradio graph → cxx callbacks → Plugin trait impl

mod call_manager;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use trunk_recorder_sys::{EventNotifier, GrEvent};

pub use call_manager::{CallManagerConfig, SystemPolicy, TalkgroupPolicy};
use call_manager::CallManager;
use std::collections::HashMap;

use trunk_config::{Config, DefaultMode, Modulation, SourceDriver, SystemType};
use trunk_recorder_sys::ffi;
use trunk_recorder_sys::EventHandler;

pub(crate) type Handler = Arc<Mutex<Box<dyn EventHandler>>>;

pub(crate) fn lock_handler(handler: &Handler) -> MutexGuard<'_, Box<dyn EventHandler>> {
    match handler.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Configures and initializes a trunk-recorder instance without starting it.
///
/// Call [`start()`](TrunkRecorderBuilder::start) to consume the builder and
/// begin receiving radio traffic.
pub struct TrunkRecorderBuilder {
    ctx: cxx::UniquePtr<ffi::TrContext>,
    call_mgr_config: CallManagerConfig,
    handler: Handler,
}

/// A running trunk-recorder instance. Stops automatically on drop.
pub struct TrunkRecorder {
    ctx: cxx::UniquePtr<ffi::TrContext>,
    call_mgr_config: CallManagerConfig,
    handler: Handler,
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
    pub fn from_config<P: EventHandler + 'static>(
        config: &Config,
        handler: P,
    ) -> anyhow::Result<Self> {
        let handler: Handler = Arc::new(Mutex::new(Box::new(handler)));
        let handler_box = trunk_recorder_sys::EventHandlerBox::new(handler.clone());
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

            let idx = ffi::tr_add_source(
                ctx.pin_mut(),
                driver_str,
                &src.device,
                src.center,
                src.rate,
                src.error,
            )?;

            if src.agc {
                ffi::tr_source_set_gain_mode(ctx.pin_mut(), idx, true);
            }
            if src.gain != 0.0 {
                ffi::tr_source_set_gain(ctx.pin_mut(), idx, src.gain);
            }
            if src.ppm != 0.0 {
                ffi::tr_source_set_freq_corr(ctx.pin_mut(), idx, src.ppm);
            }
            if !src.antenna.is_empty() {
                ffi::tr_source_set_antenna(ctx.pin_mut(), idx, &src.antenna);
            }
            if src.silence_frames != 0 {
                ffi::tr_source_set_silence_frames(ctx.pin_mut(), idx, src.silence_frames);
            }
            if let Some(threshold) = src.signal_detector_threshold {
                ffi::tr_source_set_signal_detector_threshold(ctx.pin_mut(), idx, threshold);
            }

            // Named gain stages
            for (name, &value) in &src.gain_settings {
                ffi::tr_source_set_gain_by_name(ctx.pin_mut(), idx, name, value);
            }

            // Individual gain fields (shorthand for common stages)
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
                    ffi::tr_source_set_gain_by_name(ctx.pin_mut(), idx, name, value);
                }
            }

            // Recorder slots
            if src.digital_recorders > 0 {
                ffi::tr_source_create_digital_recorders(ctx.pin_mut(), idx, src.digital_recorders);
            }
            if src.analog_recorders > 0 {
                ffi::tr_source_create_analog_recorders(ctx.pin_mut(), idx, src.analog_recorders);
            }
            if src.sigmf_recorders > 0 {
                ffi::tr_source_create_sigmf_recorders(ctx.pin_mut(), idx, src.sigmf_recorders);
            }
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

            let sys_idx = ffi::tr_add_system(ctx.pin_mut(), short_name, type_str)?;

            // Control channels (trunked systems)
            for &freq in &sys.control_channels {
                ffi::tr_system_add_control_channel(ctx.pin_mut(), sys_idx, freq);
            }

            // Modulation
            ffi::tr_system_set_qpsk_mod(
                ctx.pin_mut(),
                sys_idx,
                sys.modulation == Modulation::Qpsk,
            );

            // Audio/filter settings
            ffi::tr_system_set_squelch_db(ctx.pin_mut(), sys_idx, sys.squelch);
            ffi::tr_system_set_analog_levels(ctx.pin_mut(), sys_idx, sys.analog_levels);
            ffi::tr_system_set_digital_levels(ctx.pin_mut(), sys_idx, sys.digital_levels);
            ffi::tr_system_set_tau(ctx.pin_mut(), sys_idx, sys.deemphasis_tau);
            ffi::tr_system_set_max_dev(ctx.pin_mut(), sys_idx, sys.max_dev);
            ffi::tr_system_set_filter_width(ctx.pin_mut(), sys_idx, sys.filter_width);

            // Decoder options
            ffi::tr_system_set_mdc_enabled(ctx.pin_mut(), sys_idx, sys.decode_mdc);
            ffi::tr_system_set_fsync_enabled(ctx.pin_mut(), sys_idx, sys.decode_fsync);
            ffi::tr_system_set_star_enabled(ctx.pin_mut(), sys_idx, sys.decode_star);
            ffi::tr_system_set_tps_enabled(ctx.pin_mut(), sys_idx, sys.decode_tps);

            // Bandplan
            if let Some(bp) = &sys.bandplan {
                ffi::tr_system_set_bandplan(ctx.pin_mut(), sys_idx, bp);
            }
            if sys.bandplan_base > 0.0 {
                ffi::tr_system_set_bandplan_base(ctx.pin_mut(), sys_idx, sys.bandplan_base);
            }
            if sys.bandplan_high > 0.0 {
                ffi::tr_system_set_bandplan_high(ctx.pin_mut(), sys_idx, sys.bandplan_high);
            }
            if sys.bandplan_spacing > 0.0 {
                ffi::tr_system_set_bandplan_spacing(ctx.pin_mut(), sys_idx, sys.bandplan_spacing);
            }
            if sys.bandplan_offset != 0 {
                ffi::tr_system_set_bandplan_offset(ctx.pin_mut(), sys_idx, sys.bandplan_offset);
            }

            // XOR mask (P25 identity)
            if sys.sys_id != 0 || sys.wacn != 0 || sys.nac != 0 {
                ffi::tr_system_set_xor_mask(
                    ctx.pin_mut(),
                    sys_idx,
                    sys.sys_id,
                    sys.wacn,
                    sys.nac,
                );
            }

            // Talkgroups (trunked systems)
            for tg in &sys.talkgroup {
                ffi::tr_system_add_talkgroup(
                    ctx.pin_mut(),
                    sys_idx,
                    tg.id,
                    tg.mode.as_str(),
                    tg.priority,
                    tg.preferred_nac,
                );
            }

            // Channels (conventional systems)
            for ch in &sys.channel {
                ffi::tr_system_add_conventional_channel(
                    ctx.pin_mut(),
                    sys_idx,
                    ch.id,
                    ch.freq,
                    ch.tone,
                    ch.squelch_db,
                    ch.signal_detection,
                );
            }

            // Unit tags
            for ut in &sys.unit_tag {
                ffi::tr_system_add_unit_tag(ctx.pin_mut(), sys_idx, &ut.pattern, &ut.tag);
            }
            if let Some(settings) = &sys.unit_tags {
                ffi::tr_system_set_unit_tags_mode(ctx.pin_mut(), sys_idx, &settings.mode);
            }

            // Build recording policy for this system
            let talkgroups = sys
                .talkgroup
                .iter()
                .map(|tg| {
                    (
                        tg.id,
                        TalkgroupPolicy {
                            mode: tg.mode,
                            priority: tg.priority,
                            preferred_nac: tg.preferred_nac,
                        },
                    )
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
                    control_channels: sys.control_channels.clone(),
                    talkgroups,
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
            let mut guard = lock_handler(&handler);
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
            guard.setup_config(&infos);
            for info in &infos {
                guard.setup_system(info);
            }
            guard.start();
        }

        let rec = config.recorder.as_ref();
        let call_mgr_config = CallManagerConfig {
            call_timeout: rec.map_or(3.0, |r| r.call_timeout),
            new_call_from_update: rec.map_or(true, |r| r.new_call_from_update),
            record_uu_v_calls: rec.map_or(true, |r| r.record_uu_v_calls),
            default_mode: rec.map_or(DefaultMode::default(), |r| r.default_mode),
            control_retune_limit: rec.map_or(0, |r| r.control_retune_limit),
            control_warn_rate: rec.map_or(10, |r| r.control_warn_rate),
            systems: system_policies,
        };

        Ok(Self {
            ctx,
            call_mgr_config,
            handler,
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
            handler: self.handler,
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

    /// Run the monitor loop on the current thread until `shutdown` is set.
    ///
    /// Uses a single-threaded tokio runtime so `TrContext` (C++ objects) stays
    /// pinned to this thread. GR blocks push events through the mpsc channel;
    /// periodic work runs on `tokio::time::interval` timers.
    pub fn run_monitor(&mut self, shutdown: &AtomicBool) -> anyhow::Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?;
        rt.block_on(self.monitor_loop(shutdown))
    }

    async fn monitor_loop(&mut self, shutdown: &AtomicBool) -> anyhow::Result<()> {
        let mut call_mgr = CallManager::new(self.call_mgr_config.clone(), self.handler.clone());

        // Drain conventional calls created by C++ setup_systems during tr_finalize
        loop {
            let call = ffi::tr_context_take_next_conventional_call(self.ctx.pin_mut());
            if call.is_null() {
                break;
            }
            call_mgr.add_call(call);
        }

        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut decode = tokio::time::interval(Duration::from_secs(3));
        let mut status = tokio::time::interval(Duration::from_secs(200));
        let mut autotune = tokio::time::interval(Duration::from_secs(200));

        // First ticks fire immediately — consume them so the first real tick
        // happens after the interval elapses.
        tick.tick().await;
        decode.tick().await;
        status.tick().await;
        autotune.tick().await;

        loop {
            if shutdown.load(Ordering::Relaxed) {
                tracing::info!("shutting down");
                call_mgr.conclude_all(self.ctx.pin_mut());
                lock_handler(&self.handler).stop();
                tokio::time::sleep(Duration::from_secs(5)).await;
                return Ok(());
            }

            tokio::select! {
                Some(event) = self.event_rx.recv() => {
                    self.pending.fetch_sub(1, Ordering::Relaxed);
                    match event {
                        GrEvent::TrunkMessages(messages) => {
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
                                    lock_handler(&self.handler).trunk_message(&sys_msgs, &info);
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
                    ffi::tr_context_print_status(self.ctx.pin_mut());
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
