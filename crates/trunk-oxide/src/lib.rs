//! Driver that links against `libtrunk-recorder.so` and drives it via FFI.
//!
//! This inverts the typical plugin relationship: instead of trunk-recorder
//! loading a Rust plugin via dlopen, this crate links against trunk-recorder
//! as a shared library and drives it directly.
//!
//! Events flow: SDR → C++ gnuradio graph → cxx callbacks → EventBridge

mod adaptive_squelch;
pub mod calibrate;
mod call_manager;
mod call_state;
pub mod clock;
mod dedup;
mod p25_lfsr;
mod p25_metrics;
pub mod proc_stats;

use std::hash::{Hash, Hasher};
use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use trunk_recorder_sys::{EventNotifier, GrEvent};

pub use call_manager::{CallManagerConfig, ConventionalChannel, SystemPolicy, TalkgroupPolicy};
pub use call_state::{Freq, Talkgroup};

/// Snapshot of GNU Radio block-level performance counters, collected
/// periodically on the monitor thread and read by OTel observable callbacks.
#[derive(Default)]
pub struct GrMetricsSnapshot {
    pub recorders: Vec<ffi::TrRecorderMetrics>,
    pub sources: Vec<ffi::TrSourceMetrics>,
}

/// Per-channel metrics for a single conventional channel.
pub struct ConventionalChannelMetrics {
    pub system: String,
    pub talkgroup: i64,
    pub freq_hz: f64,
    pub state: &'static str,
    pub signal_power_db: f64,
    pub noise_floor_db: f64,
    pub detection_rssi_db: f64,
    pub squelch_open_count: u64,
    pub squelch_close_count: u64,
    pub enable_count: u64,
    pub disable_count: u64,
    pub idle_timeout_count: u64,
    pub recordings_completed: u64,
    /// Configured (static) squelch threshold in dB.
    pub squelch_db: f64,
    /// Current adaptive squelch threshold, if active.
    pub adaptive_squelch_db: Option<f64>,
    /// Latest FFT power reading fed to the adaptive squelch tracker (dB).
    pub last_power_reading_db: Option<f64>,
}

/// Snapshot of conventional-channel metrics, updated on the monitor thread
/// and read by OTel observable callbacks.
#[derive(Default)]
pub struct ConventionalMetricsSnapshot {
    pub channels: Vec<ConventionalChannelMetrics>,
}

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

use trunk_config::{
    Config, ConditionerReset, DefaultMode, Modulation, SourceConfig, SourceDriver, SystemType,
};
use trunk_oxide_grpc::EventBridge;
use trunk_recorder_sys::ffi;

/// Run one pre-flight gain-calibration sweep and return the chosen gain.
/// Falls back to `fallback_gain` on any failure so calibration can't block
/// startup. Runs on its own thread via [`calibrate_sources_parallel`].
/// What a source's gain calibration came to.
#[derive(Clone, Debug)]
struct Calibrated {
    gain_db: f64,
    noise_floor_db: Option<f64>,
    /// As `trunk.oxide.source.gain_db` labels it.
    reason: &'static str,
    /// A fresh sweep, to cache.
    swept: Option<calibrate::GainSelection>,
}

impl Calibrated {
    const fn configured(gain_db: f64) -> Self {
        Self {
            gain_db,
            noise_floor_db: None,
            reason: "configured",
            swept: None,
        }
    }
}

fn run_calibration(
    probe_cfg: &ffi::TrProbeConfig,
    policy: &calibrate::CalibrationPolicy,
    fallback_gain: f64,
    device: &str,
    cached: Option<&calibrate::cache::Cached>,
) -> Calibrated {
    // The probe holds the device open for the sweep and releases it on drop
    // (end of this function), before the real source is opened.
    let mut probe = match calibrate::preflight::PreflightProbe::open(
        probe_cfg,
        policy.capture_samples,
        policy.settle_samples,
    ) {
        Ok(probe) => probe,
        Err(e) => {
            tracing::warn!(
                device = %device, error = %e,
                "gain calibration: could not open device; using configured gain"
            );
            return Calibrated::configured(fallback_gain);
        }
    };

    // The last sweep's pick stands while the floor it was made at holds.
    if let Some(c) = cached {
        match calibrate::recheck(&mut probe, policy, c) {
            Ok((true, moved)) => {
                tracing::info!(
                    device = %device,
                    gain_db = c.gain_db,
                    noise_floor_moved_db = moved,
                    "gain calibration: keeping the last sweep's gain"
                );
                return Calibrated {
                    gain_db: c.gain_db,
                    noise_floor_db: Some(c.noise_floor_db),
                    reason: "cached",
                    swept: None,
                };
            }
            Ok((false, moved)) => tracing::info!(
                device = %device,
                cached_gain_db = c.gain_db,
                noise_floor_moved_db = moved,
                "gain calibration: the noise floor moved since the last sweep; sweeping again"
            ),
            Err(e) => tracing::warn!(
                device = %device, error = %e,
                "gain calibration: could not re-check the last sweep; sweeping again"
            ),
        }
    }

    tracing::info!(device = %device, "starting gain auto-calibration sweep");
    match calibrate::calibrate(&mut probe, policy, fallback_gain) {
        Ok(sel) => {
            tracing::info!(
                device = %device,
                gain_db = sel.gain_db,
                noise_floor_db = sel.noise_floor_db,
                reason = ?sel.reason,
                configured_gain = fallback_gain,
                "gain auto-calibration complete"
            );
            Calibrated {
                gain_db: sel.gain_db,
                noise_floor_db: sel.noise_floor_db.is_finite().then_some(sel.noise_floor_db),
                reason: sel.reason.label(),
                swept: Some(sel),
            }
        }
        Err(e) => {
            tracing::warn!(
                device = %device, error = %e,
                "gain calibration failed; using configured gain"
            );
            Calibrated::configured(fallback_gain)
        }
    }
}

/// Calibrate every `auto_gain` source concurrently — one thread per device —
/// and return a map from source index to the outcome. Sources absent from
/// the map keep their configured gain.
///
/// Each thread opens and releases its own device, and all threads are joined
/// before any source is opened for real, so the devices are free again by the
/// time the main flowgraph is built. Non-osmosdr drivers are skipped with a
/// warning. Each device's last sweep is read from, and a fresh one written
/// to, the cache in `state_dir`.
fn calibrate_sources_parallel(
    sources: &[SourceConfig],
    state_dir: &std::path::Path,
) -> HashMap<usize, Calibrated> {
    let policy = calibrate::CalibrationPolicy::default();
    let mut cache = calibrate::cache::Cache::load(state_dir);
    let mut handles = Vec::new();

    for (idx, src) in sources.iter().enumerate() {
        if src.enabled == Some(false) || !src.auto_gain {
            continue;
        }
        if !matches!(src.driver, SourceDriver::Osmosdr) {
            tracing::warn!(
                device = %src.device,
                "auto_gain is only supported for the osmosdr driver; using configured gain"
            );
            continue;
        }

        let probe_cfg = ffi::TrProbeConfig {
            driver: "osmosdr".into(),
            device: src.device.clone(),
            center: src.center,
            rate: src.rate,
            error: src.error,
            ppm: src.ppm,
            antenna: src.antenna.clone(),
        };
        let device = src.device.clone();
        let fallback = src.gain;
        let cached = cache.get(&device).cloned();
        handles.push(std::thread::spawn(move || {
            let c = run_calibration(&probe_cfg, &policy, fallback, &device, cached.as_ref());
            (idx, device, c)
        }));
    }

    let mut gains = HashMap::new();
    let mut swept = false;
    for handle in handles {
        match handle.join() {
            Ok((idx, device, c)) => {
                if let Some(sel) = &c.swept
                    && sel.reason != calibrate::SelectReason::Fallback
                {
                    cache.put(
                        &device,
                        calibrate::cache::Cached {
                            gain_db: sel.gain_db,
                            noise_floor_db: sel.noise_floor_db,
                            reason: sel.reason.label().to_owned(),
                            at: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_or(0, |d| d.as_secs()),
                        },
                    );
                    swept = true;
                }
                gains.insert(idx, c);
            }
            Err(_) => tracing::error!(
                "a gain calibration thread panicked; that source will use its configured gain"
            ),
        }
    }
    if swept && let Err(e) = cache.save() {
        tracing::warn!(error = %e, "gain calibration: could not save the cache; the next start sweeps again");
    }
    gains
}

/// Configures and initializes a trunk-recorder instance without starting it.
///
/// Call [`start()`](TrunkRecorderBuilder::start) to consume the builder and
/// begin receiving radio traffic.
pub struct TrunkRecorderBuilder {
    ctx: cxx::UniquePtr<ffi::TrContext>,
    call_mgr_config: CallManagerConfig,
    bridge: Arc<EventBridge>,
    /// Per-source frequency coverage, indexed by source position.
    source_ranges: Vec<RangeInclusive<f64>>,
    /// Per-source configured ppm correction, indexed by source position.
    source_ppms: Vec<f64>,
    /// Per-source software Hz tune correction, indexed by source position.
    source_freq_corrections_hz: Vec<f64>,
    /// The call state machine's time ([`clock::Clock::system`] unless
    /// [`Self::with_clock`] says otherwise).
    clock: clock::Clock,
}

/// Talkgroup/channel metadata from a parsed config, keyed by `(short_name, id)`,
/// for [`trunk_oxide_grpc::EventBridge::new`].
pub fn talkgroup_meta_map(
    config: &Config,
) -> HashMap<(String, i64), trunk_oxide_grpc::TalkgroupMeta> {
    let mut map = HashMap::new();
    for (short_name, sys) in &config.system {
        for tg in &sys.talkgroup {
            map.insert(
                (short_name.clone(), tg.id),
                trunk_oxide_grpc::TalkgroupMeta {
                    alpha_tag: tg.alpha_tag.clone(),
                    tag: tg.tag.clone(),
                    group: tg.group.clone(),
                    priority: Some(tg.priority),
                },
            );
        }
        for ch in &sys.channel {
            map.insert(
                (short_name.clone(), ch.id),
                trunk_oxide_grpc::TalkgroupMeta {
                    alpha_tag: ch.alpha_tag.clone(),
                    tag: ch.tag.clone(),
                    group: ch.group.clone(),
                    priority: Some(ch.priority),
                },
            );
        }
    }
    map
}

/// Options for [`TrunkRecorder::run_monitor_with`].
pub struct MonitorOptions {
    /// Exit the process when a source stops delivering samples (production
    /// behaviour). Disable for finite IQ-file replay.
    pub check_sources: bool,
    /// When notified, conclude all calls and return.
    pub shutdown: Option<Arc<tokio::sync::Notify>>,
}

impl Default for MonitorOptions {
    fn default() -> Self {
        Self {
            check_sources: true,
            shutdown: None,
        }
    }
}

/// A running trunk-recorder instance. Stops automatically on drop.
pub struct TrunkRecorder {
    ctx: cxx::UniquePtr<ffi::TrContext>,
    call_mgr_config: CallManagerConfig,
    bridge: Arc<EventBridge>,
    event_rx: tokio::sync::mpsc::Receiver<GrEvent>,
    pending: Arc<AtomicUsize>,
    /// Per-source frequency coverage, indexed by source position.
    source_ranges: Vec<RangeInclusive<f64>>,
    /// Per-source configured ppm correction, indexed by source position.
    source_ppms: Vec<f64>,
    /// Per-source software Hz tune correction, indexed by source position.
    source_freq_corrections_hz: Vec<f64>,
    clock: clock::Clock,
    /// Conventional calls created before the flowgraph starts, pending
    /// registration with the [`CallManager`].
    pending_conventional_calls: Vec<PendingConventionalCall>,
    /// Shared snapshot of GR block metrics, updated periodically on the
    /// monitor thread and read by OTel observable callbacks.
    gr_metrics: Arc<Mutex<GrMetricsSnapshot>>,
    /// Shared snapshot of conventional-channel metrics, updated on the
    /// monitor thread and read by OTel observable callbacks.
    conv_metrics: Arc<Mutex<ConventionalMetricsSnapshot>>,
}

struct PendingConventionalCall {
    call: cxx::UniquePtr<ffi::TrCall>,
    sys_num: i32,
    channel_idx: usize,
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
            let c = &rec.conditioner;
            trunk_recorder_sys::set_conditioning_settings(trunk_recorder_sys::ConditioningSettings {
                params: trunk_recorder_sys::ConditionerParams {
                    agc_alpha: c.agc_alpha,
                    agc_reference: c.agc_reference,
                    fll_bw_scale: c.fll_bw_scale,
                    ..trunk_recorder_sys::ConditionerParams::DEFAULT
                },
                reset: match c.reset {
                    ConditionerReset::Never => trunk_recorder_sys::ResetPolicy::Never,
                    ConditionerReset::Enable => trunk_recorder_sys::ResetPolicy::Enable,
                    ConditionerReset::Retune => trunk_recorder_sys::ResetPolicy::Retune,
                },
            });
            if let Some(dir) = &rec.tap_dir {
                ffi::tr_config_set_tap_dir(ctx.pin_mut(), dir);
            }
            ffi::tr_config_set_debug_recorder(
                ctx.pin_mut(),
                rec.debug.enabled,
                &rec.debug.address,
                rec.debug.port,
            );
        }

        // ── Sources ──
        // Pre-flight gain calibration for all auto_gain sources runs first, in
        // parallel (one thread per device), so the devices are free before any
        // source is opened below. Maps source index → calibrated gain.
        let state_dir = config
            .recorder
            .as_ref()
            .map_or("/var/lib/trunk-oxide", |r| r.state_dir.as_str());
        let calibrated_gains =
            calibrate_sources_parallel(&config.source, std::path::Path::new(state_dir));
        calibrate::report::record(
            config
                .source
                .iter()
                .enumerate()
                .filter(|(_, src)| src.enabled != Some(false))
                .map(|(idx, src)| {
                    let c = calibrated_gains
                        .get(&idx)
                        .cloned()
                        .unwrap_or(Calibrated::configured(src.gain));
                    calibrate::report::SourceGain {
                        device: src.device.clone(),
                        source: idx,
                        gain_db: c.gain_db,
                        noise_floor_db: c.noise_floor_db,
                        reason: c.reason,
                    }
                })
                .collect(),
        );

        let mut source_ranges = Vec::new();
        let mut source_ppms = Vec::new();
        let mut source_freq_corrections_hz = Vec::new();
        for (idx, src) in config.source.iter().enumerate() {
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

            // Use the pre-flight calibrated gain if this source was swept,
            // otherwise the configured gain.
            let gain = calibrated_gains.get(&idx).map_or(src.gain, |c| c.gain_db);

            ffi::tr_add_source_from_config(
                ctx.pin_mut(),
                &ffi::TrSourceConfig {
                    driver: driver_str.into(),
                    device: src.device.clone(),
                    center: src.center,
                    rate: src.rate,
                    error: src.error,
                    gain,
                    agc: src.agc,
                    // Hybrid clock correction: the hardware knob is integer-ppm
                    // only (rtl-sdr truncates), so it takes the *coarse* rounded
                    // ppm here — a whole-dongle correction (incl. control
                    // channel) for free. The fractional remainder (and any
                    // real-time adaptive delta) is applied in software as a
                    // per-recorder tune offset in
                    // CallManager::build_recorder_config_from_view, so sub-ppm
                    // precision works. Total = round(ppm) + (ppm - round(ppm)).
                    ppm: src.ppm.round(),
                    antenna: src.antenna.clone(),
                    silence_frames: src.silence_frames,
                    signal_detector_threshold: src.signal_detector_threshold.unwrap_or(0.0).into(),
                    has_signal_detector_threshold: src.signal_detector_threshold.is_some(),
                    gain_stages,
                    digital_recorders: src.digital_recorders,
                    analog_recorders: src.analog_recorders,
                    sigmf_recorders: src.sigmf_recorders,
                    iq_format: src.iq_format.clone(),
                    iq_repeat: src.iq_repeat,
                },
            )?;
            let half_rate = src.rate / 2.0;
            source_ranges.push((src.center - half_rate)..=(src.center + half_rate));
            source_ppms.push(src.ppm);
            source_freq_corrections_hz.push(src.freq_correction_hz);
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
                    analog_levels: sys.analog_levels,
                    tau: sys.deemphasis_tau.into(),
                    max_dev: sys.max_dev,
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
                            squelch_db: ch.squelch_db.unwrap_or(sys.squelch),
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
                        signal_detection: ch.signal_detection,
                        squelch_db: ch.squelch_db.unwrap_or(sys.squelch),
                        tone: ch.tone,
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
                    control_channels: sys
                        .control_channels
                        .iter()
                        .map(|&f| Freq::from_hz(f))
                        .collect(),
                    talkgroups,
                    channels,
                    squelch_db: sys.squelch,
                    analog_levels: sys.analog_levels,
                    digital_levels: sys.digital_levels,
                    bandplan: sys.bandplan.clone().unwrap_or_default(),
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
                    use trunk_oxide_grpc::proto::SystemType as P;
                    let system_type = match policy.system_type {
                        trunk_config::SystemType::Conventional => P::Conventional as i32,
                        trunk_config::SystemType::ConventionalP25 => P::ConventionalP25 as i32,
                        trunk_config::SystemType::ConventionalDMR => P::ConventionalDmr as i32,
                        trunk_config::SystemType::ConventionalSigmf => P::ConventionalSigmf as i32,
                        trunk_config::SystemType::Smartnet => P::Smartnet as i32,
                        trunk_config::SystemType::P25 => P::P25 as i32,
                    };
                    trunk_recorder_sys::SystemInfo {
                        sys_num: sys_idx,
                        system_type,
                        short_name: policy.short_name.clone(),
                        sys_id: policy.sys_id,
                        wacn: policy.wacn,
                        nac: policy.nac,
                        rfss: 0,
                        site_id: 0,
                        bandplan: policy.bandplan.clone(),
                        analog_levels: policy.analog_levels,
                        digital_levels: policy.digital_levels,
                        squelch_db: policy.squelch_db,
                        conversation_mode: policy.conversation_mode,
                        message_count: 0,
                        decode_rate: 0,
                    }
                })
                .collect();
            bridge.setup_config(&infos);
            for info in &infos {
                bridge.setup_system(info);
            }
        }

        let rec = config.recorder.as_ref();
        let temp_dir = rec.map_or_else(
            || std::env::temp_dir().display().to_string(),
            |r| r.temp_dir.clone(),
        );
        let call_mgr_config = CallManagerConfig {
            call_timeout: rec.map_or(3.0, |r| r.call_timeout),
            new_call_from_update: rec.is_none_or(|r| r.new_call_from_update),
            record_uu_v_calls: rec.is_none_or(|r| r.record_uu_v_calls),
            default_mode: rec.map_or(DefaultMode::default(), |r| r.default_mode),
            temp_dir,
            systems: system_policies,
            adaptive_freq_correction: rec.is_some_and(|r| r.adaptive_freq_correction),
        };

        Ok(Self {
            ctx,
            call_mgr_config,
            bridge,
            source_ranges,
            source_ppms,
            source_freq_corrections_hz,
            clock: clock::Clock::system(),
        })
    }

    /// Run the call state machine on `clock` (a replay's, say) instead of
    /// the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: clock::Clock) -> Self {
        self.clock = clock;
        self
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

        // Create conventional recorders BEFORE starting the flowgraph so the
        // blocks are part of the initial graph topology. Adding hier_block2
        // blocks to a running GNU Radio graph via lock/unlock crashes in
        // block_detail::output() during the graph restart.
        let mut pending_conventional_calls = Vec::new();
        for (&sys_num, policy) in &self.call_mgr_config.systems {
            for (ch_idx, channel) in policy.channels.iter().enumerate() {
                let freq_hz = channel.freq.as_hz();
                let Some(source_idx) = self
                    .source_ranges
                    .iter()
                    .position(|r| r.contains(&freq_hz))
                    .map(|i| i as i32)
                else {
                    tracing::error!(
                        sys_num,
                        freq = freq_hz,
                        talkgroup = channel.talkgroup.0,
                        "no source covers conventional channel frequency"
                    );
                    continue;
                };
                let ffi_channel = ffi::TrChannelConfig {
                    id: channel.talkgroup.0,
                    freq: freq_hz,
                    tone: channel.tone,
                    squelch_db: channel.squelch_db,
                    signal_detection: channel.signal_detection,
                };
                let call = ffi::tr_create_conventional_call(
                    self.ctx.pin_mut(),
                    sys_num,
                    source_idx,
                    &ffi_channel,
                );
                if call.is_null() {
                    tracing::error!(
                        sys_num,
                        freq = freq_hz,
                        "failed to create conventional call"
                    );
                    continue;
                }
                pending_conventional_calls.push(PendingConventionalCall {
                    call,
                    sys_num,
                    channel_idx: ch_idx,
                });
            }
        }

        ffi::tr_start_gnuradio(self.ctx.pin_mut());
        TrunkRecorder {
            ctx: self.ctx,
            call_mgr_config: self.call_mgr_config,
            bridge: self.bridge,
            event_rx,
            pending,
            source_ranges: self.source_ranges,
            source_ppms: self.source_ppms,
            source_freq_corrections_hz: self.source_freq_corrections_hz,
            clock: self.clock,
            pending_conventional_calls,
            gr_metrics: Arc::new(Mutex::new(GrMetricsSnapshot::default())),
            conv_metrics: Arc::new(Mutex::new(ConventionalMetricsSnapshot::default())),
        }
    }
}

impl TrunkRecorder {
    /// Shared counter of events queued but not yet consumed by the monitor loop.
    pub fn event_channel_depth(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.pending)
    }

    /// Shared GR block metrics snapshot, updated every 15s on the monitor thread.
    pub fn gr_metrics(&self) -> Arc<Mutex<GrMetricsSnapshot>> {
        Arc::clone(&self.gr_metrics)
    }

    /// Shared conventional-channel metrics snapshot.
    pub fn conv_metrics(&self) -> Arc<Mutex<ConventionalMetricsSnapshot>> {
        Arc::clone(&self.conv_metrics)
    }

    /// Run the monitor loop until ctrl_c is received.
    ///
    /// Must be called from within a tokio runtime context (e.g. via
    /// `Handle::block_on`). All FFI calls are made on the calling thread,
    /// keeping `TrContext` (C++ objects) pinned.
    pub async fn run_monitor(&mut self) -> anyhow::Result<()> {
        self.run_monitor_with(MonitorOptions::default()).await
    }

    /// [`run_monitor`](Self::run_monitor) with options: an explicit shutdown
    /// trigger and control over the source-liveness check (replay of a finite
    /// IQ file reaches end-of-file, which must not be treated as a dead dongle).
    pub async fn run_monitor_with(&mut self, opts: MonitorOptions) -> anyhow::Result<()> {
        let MonitorOptions {
            check_sources,
            shutdown,
        } = opts;
        let mut call_mgr = CallManager::new(
            self.call_mgr_config.clone(),
            self.bridge.clone(),
            self.source_ranges.clone(),
            self.source_ppms.clone(),
            self.source_freq_corrections_hz.clone(),
            Arc::clone(&self.conv_metrics),
            self.clock.clone(),
        );

        // Register conventional calls that were pre-created before the
        // flowgraph started (see TrunkRecorderBuilder::start).
        let pending_calls = std::mem::take(&mut self.pending_conventional_calls);
        for pending in pending_calls {
            let channel =
                &self.call_mgr_config.systems[&pending.sys_num].channels[pending.channel_idx];
            call_mgr.add_call(self.ctx.pin_mut(), pending.call, channel, pending.sys_num);
        }

        // The same packet decoded by several control channel decoders arrives
        // within milliseconds; the control channel's own repeats (call
        // refreshes) a second or so apart.
        let mut trunk_dedup = dedup::TimedDedup::new(Duration::from_millis(500));

        let mut tick = delayed_interval(Duration::from_secs(1));
        let mut status = delayed_interval(Duration::from_secs(200));
        let mut metrics_tick = delayed_interval(Duration::from_secs(1));

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down");
                    call_mgr.conclude_all(self.ctx.pin_mut());
                    return Ok(());
                },
                () = async {
                    match &shutdown {
                        Some(n) => n.notified().await,
                        None => std::future::pending().await,
                    }
                } => {
                    tracing::info!("shutdown requested");
                    call_mgr.conclude_all(self.ctx.pin_mut());
                    return Ok(());
                },
                Some(event) = self.event_rx.recv() => {
                    self.pending.fetch_sub(1, Ordering::Relaxed);
                    match event {
                        GrEvent::TrunkMessages(messages) => {
                            let now = call_mgr.clock().now();
                            let messages: Vec<_> = messages
                                .into_iter()
                                .filter(|m| {
                                    let fresh = trunk_dedup.admit(&TrunkMsgKey::from(m), now);
                                    if !fresh {
                                        call_mgr.note_duplicate(m);
                                    }
                                    fresh
                                })
                                .collect();
                            if !messages.is_empty() {
                                let mut by_system: HashMap<i32, Vec<usize>> = HashMap::new();
                                for (i, msg) in messages.iter().enumerate() {
                                    by_system.entry(msg.sys_num).or_default().push(i);
                                }
                                for (&sys_num, indices) in &by_system {
                                    let sys_msgs: Vec<ffi::TrTrunkMessage> =
                                        indices.iter().map(|&i| messages[i].clone()).collect();
                                    let info = call_mgr.get_system_info(sys_num);
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
                            call_mgr.handle_squelch_event(
                                self.ctx.pin_mut(),
                                recorder_num,
                                muted,
                                pwr_db,
                            );
                        }
                        GrEvent::SourceChange {
                            rust_call_id,
                            src_id,
                        } => {
                            call_mgr.handle_source_change(
                                self.ctx.pin_mut(),
                                rust_call_id,
                                src_id,
                            );
                        }
                        GrEvent::Crypto {
                            rust_call_id,
                            alg_id,
                            key_id,
                        } => {
                            call_mgr.handle_crypto(rust_call_id, alg_id, key_id);
                        }
                        GrEvent::SignalsDetected {
                            source_center_freq,
                            signals,
                            ..
                        } => {
                            call_mgr.handle_signals_detected(
                                self.ctx.pin_mut(),
                                source_center_freq,
                                &signals,
                            );
                        }

                    }
                },
                _ = tick.tick() => {
                    call_mgr.tick(self.ctx.pin_mut());
                    call_mgr.clear_stale_patches();
                    if check_sources
                        && let Err(e) = ffi::tr_context_check_sources(self.ctx.pin_mut())
                    {
                        tracing::error!(%e, "source health check failed, exiting");
                        std::process::exit(1);
                    }
                },
                _ = status.tick() => {
                    call_mgr.print_status(self.ctx.pin_mut());
                    call_mgr.print_active_patches();
                    call_mgr.print_recorder_status(&self.ctx);
                },
                _ = metrics_tick.tick() => {
                    let recorders = ffi::tr_collect_recorder_metrics(&self.ctx);
                    let sources = ffi::tr_collect_source_metrics(&self.ctx);
                    if let Ok(mut snap) = self.gr_metrics.lock() {
                        snap.recorders = recorders;
                        snap.sources = sources;
                    }
                },
            }
        }
    }
}

impl Drop for TrunkRecorder {
    fn drop(&mut self) {
        ffi::tr_stop_gnuradio(self.ctx.pin_mut());
    }
}
