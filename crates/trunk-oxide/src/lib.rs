//! The scanner: opens the sources, builds the radio
//! ([`trunk_radio::Radio`]) and runs the call state machine over its events.
//!
//! Events flow: SDR → trunk-radio (channelizer, decoders) → `RadioEvent` →
//! call state machine → `EventBridge` (gRPC)

mod allocation;
pub mod calibrate;
mod call_manager;
mod call_state;
pub mod clock;
pub mod cpu_weight;
mod dedup;
mod fm_metrics;
mod p25_metrics;
pub mod plan;
pub mod proc_stats;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use trunk_radio::{
    AnalogFm, AnalogParams, ConditioningSettings, EventSender, Keying, P25Control, P25Voice, Radio,
    RadioConfig, RadioEvent, Recipe, SourceSetup, SystemSetup, TrunkMessage, Trunking,
};

pub use allocation::Priority;
pub use call_manager::{CallManagerConfig, ConventionalChannel, SystemPolicy, TalkgroupPolicy};
pub use call_state::{Freq, Talkgroup};

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
}

/// Snapshot of conventional-channel metrics, updated on the monitor thread
/// and read by `OTel` observable callbacks.
#[derive(Default)]
pub struct ConventionalMetricsSnapshot {
    pub channels: Vec<ConventionalChannelMetrics>,
}

/// What makes two trunk messages the same packet: the same kind, opcode,
/// channel, talkgroup and unit on one system. Two decoders of the same
/// system hearing one TSBK produce equal keys.
#[derive(PartialEq, Eq, Hash)]
struct TrunkMsgKey {
    sys_num: i32,
    kind: std::mem::Discriminant<Trunking>,
    opcode: u64,
    talkgroup: u32,
    source: Option<u32>,
    freq_bits: u64,
}

impl From<&TrunkMessage> for TrunkMsgKey {
    fn from(msg: &TrunkMessage) -> Self {
        use Trunking as K;
        let (talkgroup, source, freq_hz) = match msg.kind {
            K::Grant(v)
            | K::Update(v)
            | K::UnitGrant(v)
            | K::UnitUpdate(v)
            | K::AnswerRequest(v) => (v.talkgroup, v.source, v.channel.freq_hz),
            K::DataGrant { freq_hz, unit, .. } => (0, Some(unit), freq_hz),
            K::Affiliation { group, unit } | K::Location { group, unit } => {
                (u32::from(group), Some(unit), 0.0)
            }
            K::Acknowledge { unit } | K::Registration { unit } | K::Deregistration { unit } => {
                (0, Some(unit), 0.0)
            }
            K::ControlChannel { freq_hz } | K::Network { freq_hz, .. } => (0, None, freq_hz),
            K::Site { .. }
            | K::PatchAdd(_)
            | K::PatchDelete(_)
            | K::Tdulc
            | K::Invalid
            | K::Unknown => (0, None, 0.0),
        };
        Self {
            sys_num: msg.sys_num,
            kind: std::mem::discriminant(&msg.kind),
            opcode: msg.opcode,
            talkgroup,
            source,
            freq_bits: freq_hz.to_bits(),
        }
    }
}

/// A configured talkgroup (or conventional channel) id, as the air names it.
fn talkgroup_id(system: &str, id: i64) -> anyhow::Result<Talkgroup> {
    u32::try_from(id)
        .map(Talkgroup)
        .with_context(|| format!("system '{system}': talkgroup {id} is not a valid id"))
}

/// The monitor loop's housekeeping period, and its status report's.
const TICK: Duration = Duration::from_secs(1);
const STATUS: Duration = Duration::from_secs(200);

/// The monitor loop's tick: timeouts, stale patches, source health.
fn on_tick(call_mgr: &mut CallManager, radio: &Radio, check_sources: bool) {
    call_mgr.tick();
    call_mgr.clear_stale_patches();
    if check_sources && let Err(e) = radio.check_sources() {
        tracing::error!(%e, "source health check failed, exiting");
        std::process::exit(1);
    }
}

fn on_status(call_mgr: &CallManager) {
    call_mgr.print_status();
    call_mgr.print_active_patches();
    call_mgr.print_channel_status();
}

/// Create an interval whose first tick fires after `period` (not immediately).
fn delayed_interval(period: Duration) -> tokio::time::Interval {
    tokio::time::interval_at(tokio::time::Instant::now() + period, period)
}
use call_manager::CallManager;
use std::collections::{BTreeMap, HashMap};

use trunk_config::{
    ConditionerReset, Config, DefaultMode, Modulation, SourceConfig, SourceDriver, SystemType,
};
use trunk_oxide_grpc::EventBridge;

/// What a source's gain calibration came to.
#[derive(Clone, Debug)]
struct Calibrated {
    gain_db: f64,
    noise_floor_db: Option<f64>,
    /// As `trunk.oxide.source.gain_db` labels it.
    reason: &'static str,
    /// A fresh sweep, to cache.
    swept: Option<calibrate::Sweep>,
    /// The noise model fitted to the device's sweep (fresh or cached).
    model: Option<calibrate::select::NoiseModel>,
}

impl Calibrated {
    const fn configured(gain_db: f64) -> Self {
        Self {
            gain_db,
            noise_floor_db: None,
            reason: "configured",
            swept: None,
            model: None,
        }
    }
}

/// Run one pre-flight gain-calibration sweep (or re-check of the cached
/// one) on an RTL-SDR and return the chosen gain. Falls back to
/// `fallback_gain` on any failure so calibration can't block startup. Runs
/// on its own thread via [`calibrate_sources_parallel`].
fn run_calibration(
    spec: &trunk_sdr::DeviceSpec,
    tuning: &trunk_sdr::Tuning,
    policy: &calibrate::CalibrationPolicy,
    fallback_gain: f64,
    device: &str,
    cached: Option<&calibrate::cache::Cached>,
) -> Calibrated {
    // The probe holds the device open for the sweep and releases it on drop
    // (end of this function), before the real source is opened.
    let mut probe = match calibrate::rtl::RtlProbe::open(
        spec,
        tuning,
        policy.capture_samples,
        policy.settle_samples,
    ) {
        Ok(probe) => probe,
        Err(e) => {
            tracing::warn!(
                device = %device, error = %e,
                "gain calibration: could not open device; using the fallback gain"
            );
            return Calibrated::configured(fallback_gain);
        }
    };

    // The last sweep's pick stands while the floor it was made at holds.
    if let Some(c) = cached {
        match calibrate::recheck(&mut probe, policy, c) {
            Ok(r) if r.keep => {
                tracing::info!(
                    device = %device,
                    gain_db = c.gain_db,
                    noise_floor_moved_db = r.moved_db,
                    penalty_db = r.penalty_db,
                    "gain calibration: keeping the last sweep's gain"
                );
                return Calibrated {
                    gain_db: c.gain_db,
                    noise_floor_db: Some(c.noise_floor_db),
                    reason: "cached",
                    swept: None,
                    model: calibrate::model(policy, c),
                };
            }
            Ok(r) => tracing::info!(
                device = %device,
                cached_gain_db = c.gain_db,
                noise_floor_moved_db = r.moved_db,
                penalty_db = r.penalty_db,
                "gain calibration: the last sweep's gain now costs too much SNR; sweeping again"
            ),
            Err(e) => tracing::warn!(
                device = %device, error = %e,
                "gain calibration: could not re-check the last sweep; sweeping again"
            ),
        }
    }

    tracing::info!(device = %device, "starting gain auto-calibration sweep");
    match calibrate::calibrate(&mut probe, policy, fallback_gain) {
        Ok(sweep) => {
            let sel = sweep.selection;
            tracing::info!(
                device = %device,
                gain_db = sel.gain_db,
                noise_floor_db = sel.noise_floor_db,
                reason = sel.reason.label(),
                penalty_db = sel.penalty_db,
                fallback_gain,
                curve = %sweep.curve(policy.sat_limit),
                "gain auto-calibration complete"
            );
            Calibrated {
                gain_db: sel.gain_db,
                noise_floor_db: sel.noise_floor_db.is_finite().then_some(sel.noise_floor_db),
                reason: sel.reason.label(),
                swept: Some(sweep),
                model: None,
            }
        }
        Err(e) => {
            tracing::warn!(
                device = %device, error = %e,
                "gain calibration failed; using the fallback gain"
            );
            Calibrated::configured(fallback_gain)
        }
    }
}

/// Calibrate every `gain = "auto"` source concurrently — one thread per device —
/// and return a map from source index to the outcome. Sources absent from
/// the map keep their configured gain.
///
/// Each thread opens and releases its own device, and all threads are joined
/// before any source is opened for real, so the devices are free again by the
/// time the sources are built. Each device's last sweep is read from, and a
/// fresh one written to, the cache in `state_dir`.
fn calibrate_sources_parallel(
    sources: &[SourceConfig],
    state_dir: &std::path::Path,
) -> HashMap<usize, Calibrated> {
    let policy = calibrate::CalibrationPolicy::default();
    let mut cache = calibrate::cache::Cache::load(state_dir);
    let mut handles = Vec::new();

    for (idx, src) in sources.iter().enumerate() {
        // Only an RTL-SDR's gain is tuned; a file has none.
        if src.enabled == Some(false) || !src.gain.is_auto() || src.driver != SourceDriver::Rtlsdr {
            continue;
        }
        let spec = trunk_sdr::DeviceSpec::parse(&src.device);
        let device = src.device.clone();
        let cached = cache.get(&device).cloned();
        // Should calibration fail: this dongle's last pick, else the default.
        let fallback = cached.as_ref().map_or(DEFAULT_GAIN_DB, |c| c.gain_db);
        let tuning = rtl_tuning(src, trunk_sdr::Gain::Manual(fallback));
        handles.push(std::thread::spawn(move || {
            let c = run_calibration(&spec, &tuning, &policy, fallback, &device, cached.as_ref());
            (idx, device, c)
        }));
    }

    let mut gains = HashMap::new();
    let mut swept = false;
    for handle in handles {
        if let Ok((idx, device, mut c)) = handle.join() {
            if let Some(sweep) = &c.swept
                && sweep.selection.reason != calibrate::SelectReason::Fallback
            {
                let sel = sweep.selection;
                cache.put(
                    &device,
                    calibrate::cache::Cached {
                        gain_db: sel.gain_db,
                        noise_floor_db: sel.noise_floor_db,
                        reason: sel.reason.label().to_owned(),
                        at: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_secs()),
                        algorithm: calibrate::cache::ALGORITHM,
                        sweep: sweep
                            .points
                            .iter()
                            .map(|p| calibrate::cache::SweepPoint {
                                gain_db: p.gain_db,
                                noise_floor_db: p.measurement.noise_floor_db,
                                sat_frac: p.measurement.sat_frac,
                            })
                            .collect(),
                    },
                );
                swept = true;
                c.model = cache
                    .get(&device)
                    .and_then(|k| calibrate::model(&policy, k));
            }
            gains.insert(idx, c);
        } else {
            tracing::error!(
                "a gain calibration thread panicked; that source will use its configured gain"
            );
        }
    }
    if swept && let Err(e) = cache.save() {
        tracing::warn!(error = %e, "gain calibration: could not save the cache; the next start sweeps again");
    }
    gains
}

/// An `rtlsdr` source's tuning: its centre with the software `error`
/// folded in, and the whole-ppm hardware correction (the fraction is the
/// radio's per-source software correction, see `trunk_radio::correction`).
#[allow(clippy::cast_possible_truncation)]
/// Warn about every configured channel no source carries cleanly: one no
/// source covers, or one that only lands in a band's roll-off
/// ([`trunk_radio::passband`]).
fn warn_on_placement(radio: &Radio, config: &Config) {
    for (short_name, sys) in &config.system {
        let freqs = sys
            .control_channels
            .iter()
            .chain(&sys.voice_channels)
            .copied()
            .chain(sys.channel.iter().map(|c| c.freq));
        for freq_hz in freqs {
            match radio.placement(freq_hz) {
                None => {
                    tracing::warn!(system = %short_name, freq_hz, "no source covers this channel");
                }
                Some((source, depth)) if depth > trunk_radio::passband::USABLE => {
                    tracing::warn!(
                        system = %short_name,
                        freq_hz,
                        source,
                        nyquist_pct = (depth * 100.0).round(),
                        "channel sits in its source's roll-off: raise the rate or move the centre"
                    );
                }
                Some(_) => {}
            }
        }
    }
}

/// An RTL-SDR's gain when `gain = "auto"` has nothing better: calibration
/// failed and the dongle has no earlier pick (an R820T step mid-range, near
/// what the live dongles' sweeps choose).
const DEFAULT_GAIN_DB: f64 = 40.2;

/// A source's gain when it is not tuned: the pinned value, else the default.
fn configured_gain(src: &SourceConfig) -> f64 {
    src.gain.fixed().unwrap_or(DEFAULT_GAIN_DB)
}

fn rtl_tuning(src: &SourceConfig, gain: trunk_sdr::Gain) -> trunk_sdr::Tuning {
    trunk_sdr::Tuning {
        center_hz: src.center + src.error,
        rate_hz: src.rate,
        ppm: src.ppm.round() as i32,
        gain,
    }
}

/// Samples per block a source stream queues (a quarter of a USB transfer).
const STREAM_BLOCK: usize = 32_768;
/// How much of a stream may wait for the channelizer: one second. A dongle
/// drops blocks beyond that; a file player waits.
const STREAM_QUEUE_SECS: f64 = 1.0;

/// Radio events the monitor loop may fall behind by before the radio drops them.
const EVENT_QUEUE: usize = 256;

/// The radio's event queue as its gauges see it. How full it is comes from
/// the channel itself, so no count is kept beside it.
#[derive(Clone)]
pub struct EventQueue(tokio::sync::mpsc::WeakSender<RadioEvent>);

impl EventQueue {
    /// Events queued and not yet taken by the monitor loop (none once the
    /// radio is gone).
    pub fn depth(&self) -> usize {
        self.0
            .upgrade()
            .map_or(0, |tx| tx.max_capacity() - tx.capacity())
    }

    /// [`Self::depth`] as a fraction of the queue's room.
    #[expect(clippy::cast_precision_loss, reason = "at most EVENT_QUEUE")]
    pub fn utilization(&self) -> f64 {
        self.depth() as f64 / EVENT_QUEUE as f64
    }
}

/// Configures and initializes the radio without starting the call state
/// machine.
///
/// Call [`start()`](ScannerBuilder::start) to consume the builder and
/// begin receiving radio traffic.
pub struct ScannerBuilder {
    radio: Arc<Radio>,
    event_rx: tokio::sync::mpsc::Receiver<RadioEvent>,
    event_queue: EventQueue,
    call_mgr_config: CallManagerConfig,
    bridge: Arc<EventBridge>,
    /// The call state machine's time ([`clock::Clock::system`] unless
    /// [`Self::with_clock`] says otherwise).
    clock: clock::Clock,
    /// A replay faster than real time (see [`trunk_radio::pace`]).
    pacer: Option<trunk_radio::Pacer>,
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
                    // Discord's transcription order: 0 sorts last.
                    priority: Some(tg.priority.unwrap_or(0)),
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
                    priority: Some(ch.priority.unwrap_or(0)),
                },
            );
        }
    }
    map
}

/// Options for [`Scanner::run_monitor_with`].
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

/// A running radio. Stops automatically on drop.
pub struct Scanner {
    radio: Arc<Radio>,
    call_mgr_config: CallManagerConfig,
    bridge: Arc<EventBridge>,
    event_rx: tokio::sync::mpsc::Receiver<RadioEvent>,
    event_queue: EventQueue,
    clock: clock::Clock,
    pacer: Option<trunk_radio::Pacer>,
    /// Conventional channels whose calls the [`CallManager`] creates when
    /// it starts: (system, channel index).
    pending_conventional_calls: Vec<(i32, usize)>,
    /// Shared snapshot of conventional-channel metrics, updated on the
    /// monitor thread and read by `OTel` observable callbacks.
    conv_metrics: Arc<Mutex<ConventionalMetricsSnapshot>>,
}

impl ScannerBuilder {
    /// Build from a parsed TOML config: calibrate and open the sources,
    /// start their channelizers, and set up every system's decoders.
    ///
    /// # Errors
    ///
    /// Returns an error if an SDR source fails to open or system setup fails.
    pub fn from_config(config: &Config, bridge: Arc<EventBridge>) -> anyhow::Result<Self> {
        Self::from_config_paced(config, bridge, None)
    }

    /// [`Self::from_config`] for a replay faster than real time: IQ files
    /// play unpaced and `pacer` keeps capture time (see
    /// [`trunk_radio::pace`]); run the state machine on a manual clock
    /// ([`Self::with_clock`]) the monitor loop moves to it. `None` is
    /// [`Self::from_config`].
    ///
    /// # Errors
    ///
    /// As [`Self::from_config`].
    pub fn from_config_paced(
        config: &Config,
        bridge: Arc<EventBridge>,
        pacer: Option<trunk_radio::Pacer>,
    ) -> anyhow::Result<Self> {
        let rec = config.recorder.as_ref();
        let max_channels = rec
            .and_then(|r| r.max_channels)
            .context("set [recorder] max_channels: how many channels may be open at once")?;
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(EVENT_QUEUE);
        let event_queue = EventQueue(event_tx.downgrade());
        let events = EventSender::new(event_tx);
        let conditioning = rec.map_or_else(ConditioningSettings::default, |r| {
            let c = &r.conditioner;
            ConditioningSettings {
                params: trunk_radio::ConditionerParams {
                    agc_alpha: c.agc_alpha,
                    agc_reference: c.agc_reference,
                    fll_bw_scale: c.fll_bw_scale,
                    ..trunk_radio::ConditionerParams::DEFAULT
                },
                reset: match c.reset {
                    ConditionerReset::Never => trunk_radio::ResetPolicy::Never,
                    ConditionerReset::Enable => trunk_radio::ResetPolicy::Enable,
                    ConditionerReset::Retune => trunk_radio::ResetPolicy::Retune,
                },
            }
        });
        let sink: Arc<dyn trunk_radio::AudioSink> = bridge.clone();
        let radio = Arc::new(Radio::new(
            RadioConfig {
                tap_dir: rec.and_then(|r| r.tap_dir.clone()),
                adaptive_correction: rec.is_some_and(|r| r.adaptive_freq_correction),
                conditioning,
                pacer: pacer.clone(),
                snapshots: Some(
                    calibrate::live::start(calibrate::CalibrationPolicy::default())
                        .context("starting the live noise floor")?,
                ),
            },
            sink,
            events,
        ));

        // ── Sources ──
        // Pre-flight gain calibration for every gain = "auto" source runs first, in
        // parallel (one thread per device), so the devices are free before any
        // source is opened below. Maps source index → calibrated gain.
        let state_dir = rec.map_or("/var/lib/trunk-oxide", |r| r.state_dir.as_str());
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
                        .unwrap_or(Calibrated::configured(configured_gain(src)));
                    calibrate::report::SourceGain {
                        device: src.device.clone(),
                        source: idx,
                        gain_db: c.gain_db,
                        noise_floor_db: c.noise_floor_db,
                        reason: c.reason,
                        model: c.model,
                    }
                })
                .collect(),
        );

        if let Some(p) = &pacer {
            p.expect(
                config
                    .source
                    .iter()
                    .filter(|s| s.enabled != Some(false))
                    .count(),
            );
        }
        for (idx, src) in config.source.iter().enumerate() {
            if src.enabled == Some(false) {
                continue;
            }
            // Use the pre-flight calibrated gain if this source was swept,
            // otherwise the configured gain.
            let gain = calibrated_gains
                .get(&idx)
                .map_or_else(|| configured_gain(src), |c| c.gain_db);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let capacity = ((src.rate * STREAM_QUEUE_SECS) as usize).max(4 * STREAM_BLOCK);
            let (stream, running, rate) = match src.driver {
                SourceDriver::Rtlsdr => {
                    let gain = if src.agc {
                        trunk_sdr::Gain::Auto
                    } else {
                        trunk_sdr::Gain::Manual(gain)
                    };
                    let spec = trunk_sdr::DeviceSpec::parse(&src.device);
                    let rtl = trunk_sdr::Rtl::open(&spec, &rtl_tuning(src, gain))
                        .with_context(|| format!("opening RTL-SDR source '{}'", src.device))?;
                    let info = rtl.info().clone();
                    tracing::info!(
                        device = %src.device,
                        serial = %info.serial,
                        product = %info.product,
                        tuner = ?info.tuner,
                        rate_hz = info.rate_hz,
                        requested_rate_hz = src.rate,
                        center_hz = src.center + src.error,
                        gain_db = rtl.tuner_gain_db(),
                        agc = src.agc,
                        "opened RTL-SDR"
                    );
                    let (stream, streaming) = rtl
                        .stream(STREAM_BLOCK, capacity)
                        .with_context(|| format!("streaming RTL-SDR source '{}'", src.device))?;
                    (
                        stream,
                        Box::new(streaming) as Box<dyn trunk_sdr::Running>,
                        info.rate_hz.round(),
                    )
                }
                SourceDriver::Iqfile => {
                    let spec = trunk_sdr::FileSpec {
                        path: src.device.clone().into(),
                        format: trunk_sdr::Format::parse(&src.iq_format)?,
                        rate_hz: src.rate,
                        repeat: src.iq_repeat,
                        paced: pacer.is_none(),
                    };
                    let (stream, playing) = trunk_sdr::file::play(&spec, STREAM_BLOCK, capacity)
                        .with_context(|| format!("playing IQ file '{}'", src.device))?;
                    (stream, playing, src.rate)
                }
                SourceDriver::Osmosdr | SourceDriver::Usrp => anyhow::bail!(
                    "source '{}': the {:?} driver is no longer supported; use driver = \"rtlsdr\" (an RTL-SDR by serial) or \"iqfile\"",
                    src.device,
                    src.driver
                ),
            };
            radio
                .add_source(SourceSetup {
                    device: src.device.clone(),
                    center_hz: src.center + src.error,
                    rate_hz: rate,
                    stream,
                    running,
                    ppm: src.ppm,
                    correction_hz: src.freq_correction_hz,
                    signal_detector_threshold: src.signal_detector_threshold,
                })
                .with_context(|| format!("starting source '{}'", src.device))?;
        }

        warn_on_placement(&radio, config);

        // ── Systems ──
        // Numbered in short-name order, so a system keeps its number across
        // restarts and config edits that add or remove others after it.
        let mut systems: Vec<_> = config.system.iter().collect();
        systems.sort_by_key(|(name, _)| name.as_str());
        let mut system_policies = HashMap::new();
        for (sys_idx, (short_name, sys)) in (0i32..).zip(systems) {
            if !matches!(
                sys.system_type,
                SystemType::P25 | SystemType::Conventional | SystemType::ConventionalP25
            ) {
                anyhow::bail!(
                    "system '{short_name}': {:?} is not supported by the Rust radio (see beads trunk-oxide-a2m and trunk-oxide-map)",
                    sys.system_type
                );
            }
            radio.add_system(SystemSetup {
                sys_num: sys_idx,
                short_name: short_name.clone(),
                qpsk: sys.modulation == Modulation::Qpsk,
                max_dev: sys.max_dev,
                noise_suppression: sys
                    .noise_suppression_db
                    .map(|db| {
                        trunk_radio::NoiseSuppression::from_db(db).ok_or_else(|| {
                            anyhow::anyhow!(
                                "system '{short_name}': noise_suppression_db = {db}; it is 6, 12, 18 or 21"
                            )
                        })
                    })
                    .transpose()?,
                analog_levels: sys.analog_levels,
                digital_levels: sys.digital_levels,
                nac: sys.nac,
                sys_id: sys.sys_id,
                wacn: sys.wacn,
            })?;

            // How each kind of call is opened: chosen here, once.
            let p25 = Recipe::<P25Voice>::opener(Keying::Trunked);
            let analog = Recipe::<AnalogFm>::opener(AnalogParams::default());
            let default_priority = Priority::or_unset(sys.default_priority);
            let talkgroups = sys
                .talkgroup
                .iter()
                .map(|tg| {
                    Ok((
                        talkgroup_id(short_name, tg.id)?,
                        TalkgroupPolicy {
                            mode: tg.mode,
                            priority: tg.priority.map_or(default_priority, Priority::new).level(),
                            preferred_nac: tg.preferred_nac,
                            recipe: Arc::clone(if tg.mode == trunk_config::TalkgroupMode::Analog {
                                &analog
                            } else {
                                &p25
                            }),
                        },
                    ))
                })
                .collect::<anyhow::Result<_>>()?;

            let channels = sys
                .channel
                .iter()
                .map(|ch| {
                    let squelch_db = ch.squelch_db.unwrap_or(sys.squelch);
                    let recipe = if sys.system_type == SystemType::Conventional {
                        let params = AnalogParams::with_tone_hz(ch.tone);
                        if ch.tone > 0.0 && params.tone.is_none() {
                            tracing::warn!(
                                system = %short_name, channel = ch.id, tone_hz = ch.tone,
                                "not a CTCSS tone; the channel is gated without one"
                            );
                        }
                        Recipe::<AnalogFm>::opener(params)
                    } else {
                        Recipe::<P25Voice>::opener(Keying::Carrier { squelch_db })
                    };
                    let row = sys.talkgroup.iter().find(|tg| tg.id == ch.id);
                    Ok(ConventionalChannel {
                        talkgroup: talkgroup_id(short_name, ch.id)?,
                        freq: Freq::from_hz(ch.freq),
                        priority: ch
                            .priority
                            .or_else(|| row.and_then(|tg| tg.priority))
                            .map_or(default_priority, Priority::new),
                        signal_detection: ch.signal_detection,
                        squelch_db,
                        recipe,
                    })
                })
                .collect::<anyhow::Result<_>>()?;

            let trunked = sys.system_type == SystemType::P25;
            let default_mode = rec.map_or(DefaultMode::default(), |r| r.default_mode);
            system_policies.insert(
                sys_idx,
                SystemPolicy {
                    system_type: sys.system_type,
                    short_name: short_name.clone(),
                    record_unknown: sys.record_unknown,
                    control_only: sys.control_only,
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
                    control_recipe: trunked.then(|| Recipe::<P25Control>::opener(())),
                    // Followed for metrics only, they give way to anything
                    // that records.
                    control_priority: if sys.control_only {
                        Priority::new(sys.control_channel_priority).background()
                    } else {
                        Priority::new(sys.control_channel_priority)
                    },
                    emergency_priority: Priority::new(sys.emergency_priority),
                    default_recipe: trunked.then(|| {
                        if default_mode == DefaultMode::Analog {
                            Arc::clone(&analog)
                        } else {
                            Arc::clone(&p25)
                        }
                    }),
                    default_priority,
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

        // Fire setup events now that systems are initialized
        {
            let mut systems: Vec<_> = system_policies.iter().collect();
            systems.sort_by_key(|(sys_num, _)| **sys_num);
            let infos: Vec<_> = systems
                .into_iter()
                .map(|(_, policy)| trunk_oxide_grpc::proto::SystemInfo {
                    system_type: call_manager::proto_system_type(policy.system_type) as i32,
                    short_name: policy.short_name.clone(),
                    sys_id: policy.sys_id,
                    wacn: policy.wacn,
                    nac: policy.nac,
                    bandplan: policy.bandplan.clone(),
                    analog_levels: policy.analog_levels,
                    digital_levels: policy.digital_levels,
                    squelch_db: policy.squelch_db,
                    conversation_mode: policy.conversation_mode,
                    ..Default::default()
                })
                .collect();
            for info in &infos {
                bridge.setup_system(info.clone());
            }
            bridge.setup_config(infos);
        }

        let temp_dir = rec.map_or_else(
            || std::env::temp_dir().display().to_string(),
            |r| r.temp_dir.clone(),
        );
        let call_mgr_config = CallManagerConfig {
            call_timeout: rec.map_or(3.0, |r| r.call_timeout),
            new_call_from_update: rec.is_none_or(|r| r.new_call_from_update),
            record_uu_v_calls: rec.is_none_or(|r| r.record_uu_v_calls),
            max_channels,
            temp_dir,
            systems: system_policies,
        };

        Ok(Self {
            radio,
            event_rx,
            event_queue,
            call_mgr_config,
            bridge,
            clock: clock::Clock::system(),
            pacer,
        })
    }

    /// Run the call state machine on `clock` (a replay's, say) instead of
    /// the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: clock::Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Hand over to the running state: the sources are already streaming;
    /// use [`Scanner::run_monitor`] on a dedicated thread to drive the
    /// main loop.
    pub fn start(self) -> Scanner {
        let mut pending_conventional_calls = Vec::new();
        // In system order, so channel numbers are the same run to run. (A
        // channel no source covers is rejected when it cannot be opened.)
        let mut systems: Vec<_> = self.call_mgr_config.systems.iter().collect();
        systems.sort_by_key(|(sys_num, _)| **sys_num);
        for (&sys_num, policy) in systems {
            pending_conventional_calls.extend((0..policy.channels.len()).map(|i| (sys_num, i)));
        }

        Scanner {
            radio: self.radio,
            call_mgr_config: self.call_mgr_config,
            bridge: self.bridge,
            event_rx: self.event_rx,
            event_queue: self.event_queue,
            clock: self.clock,
            pacer: self.pacer,
            pending_conventional_calls,
            conv_metrics: Arc::new(Mutex::new(ConventionalMetricsSnapshot::default())),
        }
    }
}

impl Scanner {
    /// The queue of radio events the monitor loop takes from, for its gauges.
    pub fn event_queue(&self) -> EventQueue {
        self.event_queue.clone()
    }

    /// Shared conventional-channel metrics snapshot.
    pub fn conv_metrics(&self) -> Arc<Mutex<ConventionalMetricsSnapshot>> {
        Arc::clone(&self.conv_metrics)
    }

    /// Run the monitor loop until `ctrl_c` is received.
    ///
    /// Must be called from within a tokio runtime context (e.g. via
    /// `Handle::block_on`).
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
            Arc::clone(&self.radio),
            Arc::clone(&self.conv_metrics),
            self.clock.clone(),
        );

        // The control channels, then the conventional channels' calls.
        call_mgr.start();
        let pending_calls = std::mem::take(&mut self.pending_conventional_calls);
        for (sys_num, channel_idx) in pending_calls {
            let channel = &self.call_mgr_config.systems[&sys_num].channels[channel_idx];
            call_mgr.add_conventional_call(channel, sys_num);
        }
        if let Some(p) = &self.pacer {
            p.start();
        }

        // The same packet decoded by several control channel decoders arrives
        // within milliseconds; the control channel's own repeats (call
        // refreshes) a second or so apart.
        let mut trunk_dedup = dedup::TimedDedup::new(Duration::from_millis(500));

        // Live, the 1 s tick and the 200 s status run on tokio's clock; in a
        // paced replay they fall due as capture time passes their marks.
        let paced = self.pacer.clone();
        let mut tick = delayed_interval(TICK);
        let mut status = delayed_interval(STATUS);
        let (mut next_tick, mut next_status) = (TICK, STATUS);

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down");
                    call_mgr.conclude_all();
                    return Ok(());
                },
                () = async {
                    match &shutdown {
                        Some(n) => n.notified().await,
                        None => std::future::pending().await,
                    }
                } => {
                    tracing::info!("shutdown requested");
                    call_mgr.conclude_all();
                    return Ok(());
                },
                Some(event) = self.event_rx.recv() => {
                    match event {
                        RadioEvent::TrunkMessages { control_hz, messages } => {
                            let now = call_mgr.clock().now();
                            // Before the duplicates go: each control channel
                            // is judged on what it decoded itself.
                            call_mgr.control_heard(control_hz, &messages);
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
                                let mut by_system: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
                                for (i, msg) in messages.iter().enumerate() {
                                    by_system.entry(msg.sys_num).or_default().push(i);
                                }
                                for (&sys_num, indices) in &by_system {
                                    let sys_msgs: Vec<TrunkMessage> =
                                        indices.iter().map(|&i| messages[i].clone()).collect();
                                    let info = call_mgr.get_system_info(sys_num);
                                    self.bridge.trunk_message(&sys_msgs, info);
                                }
                            }
                            for msg in &messages {
                                call_mgr.handle_message(msg);
                            }
                        }
                        RadioEvent::Squelch {
                            call_id,
                            squelch,
                            power_db,
                        } => {
                            call_mgr.handle_squelch_event(call_id, squelch, power_db);
                        }
                        RadioEvent::SourceChange { call_id, unit } => {
                            call_mgr.handle_source_change(call_id, unit);
                        }
                        RadioEvent::Signalling {
                            call_id,
                            heard,
                            muted,
                        } => {
                            call_mgr.handle_signalling(call_id, heard, muted);
                        }
                        RadioEvent::Crypto {
                            call_id,
                            encryption,
                        } => {
                            call_mgr.handle_crypto(call_id, encryption);
                        }
                        RadioEvent::SignalsDetected {
                            source_center_freq,
                            signals,
                            ..
                        } => {
                            call_mgr.handle_signals_detected(source_center_freq, &signals);
                        }
                        RadioEvent::Paced { slot, at } => {
                            if let Some(p) = &paced {
                                let now = p.capture_time();
                                call_mgr.clock().advance_to(now);
                                while now >= next_tick {
                                    on_tick(&mut call_mgr, &self.radio, check_sources);
                                    next_tick += TICK;
                                }
                                while now >= next_status {
                                    on_status(&call_mgr);
                                    next_status += STATUS;
                                }
                                p.acknowledge(slot, at);
                            }
                        }
                        RadioEvent::UnitAlias { sys_num, alias } => {
                            let info = call_mgr.get_system_info(sys_num);
                            self.bridge.unit_alias_discovered(
                                &info.short_name,
                                alias.radio_id,
                                &alias.alias,
                                &alias.source,
                                alias.talkgroup_id,
                            );
                        }
                    }
                },
                _ = tick.tick(), if paced.is_none() => {
                    on_tick(&mut call_mgr, &self.radio, check_sources);
                },
                _ = status.tick(), if paced.is_none() => on_status(&call_mgr),
            }
        }
    }
}

impl Drop for Scanner {
    fn drop(&mut self) {
        self.radio.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::{EVENT_QUEUE, EventQueue};
    use trunk_radio::{EventSender, RadioEvent};

    #[test]
    fn the_event_queues_depth_is_the_channels() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(EVENT_QUEUE);
        let queue = EventQueue(tx.downgrade());
        let events = EventSender::new(tx);
        assert_eq!(queue.depth(), 0);
        let empty = || RadioEvent::TrunkMessages {
            control_hz: 0.0,
            messages: Vec::new(),
        };
        events.send(empty());
        events.send(empty());
        assert_eq!(queue.depth(), 2);
        rx.try_recv().unwrap();
        assert_eq!(queue.depth(), 1);
        assert!((queue.utilization() - 1.0 / 256.0).abs() < f64::EPSILON);
        drop(events);
        assert_eq!(queue.depth(), 0);
    }
}
