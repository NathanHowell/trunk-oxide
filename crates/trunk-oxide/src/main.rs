//! trunk-oxide: the scanner. Reads the TOML config, builds the radio and the
//! call state machine ([`trunk_oxide::ScannerBuilder`]), and serves calls,
//! audio and events over the gRPC `EventBridge` to downstream consumers.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider;
use tracing_subscriber::prelude::*;
use trunk_oxide::ScannerBuilder;
use trunk_oxide_grpc::EventBridge;

#[derive(Parser)]
struct Args {
    /// Path to TOML config file
    #[arg(default_value = "/app/config.toml")]
    config: String,
}

/// Observe an f64 conventional-channel metric across all channels in the snapshot.
fn observe_conv_f64(
    snap: &std::sync::Mutex<trunk_oxide::ConventionalMetricsSnapshot>,
    observer: &dyn opentelemetry::metrics::AsyncInstrument<f64>,
    extract: fn(&trunk_oxide::ConventionalChannelMetrics) -> f64,
) {
    let Ok(snap) = snap.lock() else { return };
    for ch in &snap.channels {
        observer.observe(
            extract(ch),
            &[
                KeyValue::new("system", ch.system.clone()),
                KeyValue::new("talkgroup", ch.talkgroup),
                KeyValue::new("freq_mhz", ch.freq_hz / 1_000_000.0),
            ],
        );
    }
}

/// Observe a u64 conventional-channel metric across all channels in the snapshot.
fn observe_conv_u64(
    snap: &std::sync::Mutex<trunk_oxide::ConventionalMetricsSnapshot>,
    observer: &dyn opentelemetry::metrics::AsyncInstrument<u64>,
    extract: fn(&trunk_oxide::ConventionalChannelMetrics) -> u64,
) {
    let Ok(snap) = snap.lock() else { return };
    for ch in &snap.channels {
        observer.observe(
            extract(ch),
            &[
                KeyValue::new("system", ch.system.clone()),
                KeyValue::new("talkgroup", ch.talkgroup),
                KeyValue::new("freq_mhz", ch.freq_hz / 1_000_000.0),
            ],
        );
    }
}

fn main() -> Result<()> {
    pid1::Pid1Settings::new().launch()?;

    let args = Args::parse_from(
        argfile::expand_args_from(wild::args_os(), argfile::parse_fromfile, argfile::PREFIX)
            .map_err(|e| anyhow::anyhow!(e))?,
    );

    let otel_resource = opentelemetry_sdk::Resource::builder_empty()
        .with_service_name(env!("CARGO_PKG_NAME"))
        .build();

    // W3C trace-context propagation: lets the per-call span's context be
    // serialized into the CallStart event so trunk-discord hangs the work for
    // each transmission (and the downstream transcribe span) off that call's
    // span, and links its embed's span back to it.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    let tracer_provider = if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build()?;

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(otel_resource.clone())
            .build();

        opentelemetry::global::set_tracer_provider(provider.clone());
        Some(provider)
    } else {
        None
    };

    let meter_provider = if std::env::var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT").is_ok()
        || std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok()
    {
        let exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .build()?;

        let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(exporter).build();

        let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
            .with_reader(reader)
            .with_resource(otel_resource)
            .build();

        opentelemetry::global::set_meter_provider(provider.clone());
        Some(provider)
    } else {
        None
    };

    let otel_layer = tracer_provider.as_ref().map(|provider| {
        let tracer = provider.tracer(env!("CARGO_PKG_NAME"));
        tracing_opentelemetry::layer().with_tracer(tracer)
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(otel_layer)
        .init();

    let config_path = &args.config;
    tracing::info!(config = %config_path, "starting trunk-oxide");

    let config = trunk_config::Config::load(config_path).context("failed to load TOML config")?;
    trunk_oxide::plan::check(&config.source).context("the sources are not planned")?;

    let min_cpu_weight = config.oxide.as_ref().map_or_else(
        || trunk_config::OxideConfig::default().min_cpu_weight,
        |o| o.min_cpu_weight,
    );
    let cgroups = trunk_oxide::cpu_weight::require(min_cpu_weight)?;
    tracing::info!(?cgroups, "cpu weight");

    let tg_map = trunk_oxide::talkgroup_meta_map(&config);

    let (bridge, stream_tx) = EventBridge::new(tg_map);
    let bridge = Arc::new(bridge);

    let mut scanner = ScannerBuilder::from_config(&config, bridge.clone())
        .context("failed to build the scanner")?
        .start();

    // Single runtime for gRPC server, timers, and signal handling
    let rt = tokio::runtime::Runtime::new()?;

    if meter_provider.is_some() {
        let events = scanner.event_queue();
        let meter = opentelemetry::global::meter(env!("CARGO_PKG_NAME"));
        // ── The process and its sources, for the chart to explain steps ──
        let started = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        )
        .unwrap_or(0);
        let _start_time = meter
            .i64_observable_gauge("trunk.oxide.process.start_time")
            .with_description("When this process started (Unix seconds); a change is a restart")
            .with_unit("s")
            .with_callback(move |observer| observer.observe(started, &[]))
            .build();
        let _source_gain = meter
            .f64_observable_gauge("trunk.oxide.source.gain_db")
            .with_description("Each SDR source's tuner gain as opened, with how it was chosen")
            .with_unit("dB")
            .with_callback(|observer| {
                for g in trunk_oxide::calibrate::report::gains() {
                    observer.observe(
                        g.gain_db,
                        &[
                            KeyValue::new("device", g.device.clone()),
                            KeyValue::new("source", i64::try_from(g.source).unwrap_or(-1)),
                            KeyValue::new("reason", g.reason),
                        ],
                    );
                }
            })
            .build();
        let _source_floor = meter
            .f64_observable_gauge("trunk.oxide.source.noise_floor_db")
            .with_description(
                "The noise floor the gain calibration measured at the chosen gain (relative dB)",
            )
            .with_unit("dB")
            .with_callback(|observer| {
                for g in trunk_oxide::calibrate::report::gains() {
                    if let Some(floor) = g.noise_floor_db {
                        observer.observe(
                            floor,
                            &[
                                KeyValue::new("device", g.device.clone()),
                                KeyValue::new("source", i64::try_from(g.source).unwrap_or(-1)),
                            ],
                        );
                    }
                }
            })
            .build();
        let live_floor = |pick: fn(&trunk_oxide::calibrate::report::LiveFloor) -> Option<f64>| {
            move |observer: &dyn opentelemetry::metrics::AsyncInstrument<f64>| {
                for l in trunk_oxide::calibrate::report::live() {
                    if let Some(v) = pick(&l) {
                        observer.observe(
                            v,
                            &[
                                KeyValue::new("device", l.device.clone()),
                                KeyValue::new("source", i64::try_from(l.source).unwrap_or(-1)),
                            ],
                        );
                    }
                }
            }
        };
        let _live_floor = meter
            .f64_observable_gauge("trunk.oxide.source.live_floor_db")
            .with_description(
                "The noise floor measured on the running stream each minute (relative dB, the sweep's scale)",
            )
            .with_unit("dB")
            .with_callback(live_floor(|l| Some(l.floor_db)))
            .build();
        let _floor_drift = meter
            .f64_observable_gauge("trunk.oxide.source.floor_drift_db")
            .with_description(
                "How far the live noise floor moved from the gain calibration's at the same gain (heat drift)",
            )
            .with_unit("dB")
            .with_callback(live_floor(|l| l.drift_db))
            .build();
        let _adc_penalty = meter
            .f64_observable_gauge("trunk.oxide.source.adc_penalty_db")
            .with_description(
                "The SNR the ADC's noise takes at the running gain, by the calibration's noise model and the live floor",
            )
            .with_unit("dB")
            .with_callback(live_floor(|l| l.penalty_db))
            .build();
        let _depth_gauge = meter
            .u64_observable_gauge("trunk.oxide.event_channel.depth")
            .with_description("Radio events queued for the monitor loop")
            .with_callback({
                let events = events.clone();
                move |observer| observer.observe(events.depth() as u64, &[])
            })
            .build();
        let _utilization_gauge = meter
            .f64_observable_gauge("trunk.oxide.event_channel.utilization")
            .with_description("Event channel utilization (0.0-1.0)")
            .with_callback(move |observer| {
                observer.observe(events.utilization(), &[]);
            })
            .build();

        let bridge_for_gauge = Arc::clone(&bridge);
        let _broadcast_util = meter
            .f64_observable_gauge("trunk.oxide.broadcast.utilization")
            .with_description("Broadcast channel utilization (0.0-1.0)")
            .with_callback(move |observer| {
                let depth = bridge_for_gauge.broadcast_depth();
                let cap = bridge_for_gauge.broadcast_capacity();
                observer.observe(depth as f64 / cap as f64, &[]);
            })
            .build();

        // ── Process CPU (from /proc; the DSP threads are named per source) ──
        let _process_cpu = meter
            .f64_observable_counter("process.cpu.time")
            .with_description("CPU seconds used by the process")
            .with_unit("s")
            .with_callback(|observer| {
                if let Some(t) = trunk_oxide::proc_stats::process_cpu() {
                    observer.observe(t.user, &[KeyValue::new("state", "user")]);
                    observer.observe(t.system, &[KeyValue::new("state", "system")]);
                }
            })
            .build();

        let _process_thread_cpu = meter
            .f64_observable_counter("process.thread.cpu.time")
            .with_description("CPU seconds used by live threads, grouped by thread name")
            .with_unit("s")
            .with_callback(|observer| {
                for (thread, secs) in trunk_oxide::proc_stats::thread_cpu_by_group() {
                    observer.observe(secs, &[KeyValue::new("thread", thread)]);
                }
            })
            .build();

        // ── Conventional channel metrics ──
        let conv_metrics = scanner.conv_metrics();

        let _conv_state = meter
            .f64_observable_gauge("trunk.oxide.conventional.state")
            .with_description("Conventional channel state (1.0 = active, state in attributes)")
            .with_callback({
                let snap = Arc::clone(&conv_metrics);
                move |observer| {
                    let Ok(snap) = snap.lock() else { return };
                    for ch in &snap.channels {
                        observer.observe(
                            1.0,
                            &[
                                KeyValue::new("system", ch.system.clone()),
                                KeyValue::new("talkgroup", ch.talkgroup),
                                KeyValue::new("freq_mhz", ch.freq_hz / 1_000_000.0),
                                KeyValue::new("state", ch.state),
                            ],
                        );
                    }
                }
            })
            .build();

        let _conv_signal_power = meter
            .f64_observable_gauge("trunk.oxide.conventional.signal_power_db")
            .with_description("Last squelch-open signal power (dBFS)")
            .with_callback({
                let snap = Arc::clone(&conv_metrics);
                move |observer| observe_conv_f64(&snap, observer, |ch| ch.signal_power_db)
            })
            .build();

        let _conv_noise_floor = meter
            .f64_observable_gauge("trunk.oxide.conventional.noise_floor_db")
            .with_description("Last squelch-close noise floor (dBFS)")
            .with_callback({
                let snap = Arc::clone(&conv_metrics);
                move |observer| observe_conv_f64(&snap, observer, |ch| ch.noise_floor_db)
            })
            .build();

        let _conv_detection_rssi = meter
            .f64_observable_gauge("trunk.oxide.conventional.signal_detection_rssi_db")
            .with_description("Last FFT signal detection RSSI")
            .with_callback({
                let snap = Arc::clone(&conv_metrics);
                move |observer| observe_conv_f64(&snap, observer, |ch| ch.detection_rssi_db)
            })
            .build();

        let _conv_squelch_open = meter
            .u64_observable_counter("trunk.oxide.conventional.squelch_open")
            .with_description("Total squelch open events")
            .with_callback({
                let snap = Arc::clone(&conv_metrics);
                move |observer| observe_conv_u64(&snap, observer, |ch| ch.squelch_open_count)
            })
            .build();

        let _conv_squelch_close = meter
            .u64_observable_counter("trunk.oxide.conventional.squelch_close")
            .with_description("Total squelch close events")
            .with_callback({
                let snap = Arc::clone(&conv_metrics);
                move |observer| observe_conv_u64(&snap, observer, |ch| ch.squelch_close_count)
            })
            .build();

        let _conv_enable = meter
            .u64_observable_counter("trunk.oxide.conventional.signal_detection_enable")
            .with_description("Total signal detection enable cycles")
            .with_callback({
                let snap = Arc::clone(&conv_metrics);
                move |observer| observe_conv_u64(&snap, observer, |ch| ch.enable_count)
            })
            .build();

        let _conv_disable = meter
            .u64_observable_counter("trunk.oxide.conventional.signal_detection_disable")
            .with_description("Total signal detection disable cycles")
            .with_callback({
                let snap = Arc::clone(&conv_metrics);
                move |observer| observe_conv_u64(&snap, observer, |ch| ch.disable_count)
            })
            .build();

        let _conv_idle_timeout = meter
            .u64_observable_counter("trunk.oxide.conventional.idle_timeouts")
            .with_description("Total idle timeout restarts")
            .with_callback({
                let snap = Arc::clone(&conv_metrics);
                move |observer| observe_conv_u64(&snap, observer, |ch| ch.idle_timeout_count)
            })
            .build();

        let _conv_recordings = meter
            .u64_observable_counter("trunk.oxide.conventional.recordings_completed")
            .with_description("Total recordings concluded with audio")
            .with_callback({
                let snap = Arc::clone(&conv_metrics);
                move |observer| observe_conv_u64(&snap, observer, |ch| ch.recordings_completed)
            })
            .build();

        let _conv_squelch_db = meter
            .f64_observable_gauge("trunk.oxide.conventional.squelch_db")
            .with_description("Configured (static) squelch threshold (dBFS)")
            .with_callback({
                let snap = Arc::clone(&conv_metrics);
                move |observer| observe_conv_f64(&snap, observer, |ch| ch.squelch_db)
            })
            .build();
    }

    // Spawn gRPC server as a background task on the runtime
    let socket_path = config.grpc.listen_path;
    rt.spawn(trunk_oxide_grpc::serve_grpc(socket_path, stream_tx));

    tracing::info!("scanner running, press Ctrl+C to stop");

    // The monitor loop (the call state machine and the radio it drives) gets
    // a thread of its own, so a slow tick never holds up the runtime's
    // workers serving gRPC; Handle::block_on drives it there while sharing
    // the runtime's I/O reactor, timers and signal handler.
    let monitor_handle = {
        let rt_handle = rt.handle().clone();
        std::thread::Builder::new()
            .name("monitor".into())
            .spawn(move || rt_handle.block_on(scanner.run_monitor()))
            .context("failed to spawn monitor thread")?
    };

    // Block until the monitor loop finishes (on ctrl_c or error).
    // join() returns Err only if the thread panicked.
    let monitor_result = monitor_handle
        .join()
        .map_err(|_| anyhow::anyhow!("monitor thread panicked"))?;

    if let Some(provider) = meter_provider {
        let _ = provider.shutdown();
    }
    if let Some(provider) = tracer_provider {
        let _ = provider.shutdown();
    }

    monitor_result
}
