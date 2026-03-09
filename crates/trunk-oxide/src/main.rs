//! trunk-oxide: Drives trunk-recorder as a linked library.
//!
//! This binary replaces the trunk-recorder executable. It links against
//! libtrunk-recorder.so, reads the TOML config file, and routes all events
//! through the gRPC EventBridge to downstream consumers.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use clap::Parser;
use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider;
use tracing_subscriber::prelude::*;
use trunk_oxide::TrunkRecorderBuilder;
use trunk_oxide_grpc::{EventBridge, TalkgroupMeta};

#[derive(Parser)]
struct Args {
    /// Path to TOML config file
    #[arg(default_value = "/app/config.toml")]
    config: String,
}

/// Observe a u64 block metric across all sources and recorders in the snapshot.
fn observe_block_u64(
    snap: &std::sync::Mutex<trunk_oxide::GrMetricsSnapshot>,
    observer: &dyn opentelemetry::metrics::AsyncInstrument<u64>,
    extract: fn(&trunk_recorder_sys::ffi::TrGrBlockMetrics) -> u64,
) {
    let Ok(snap) = snap.lock() else { return };
    for src in &snap.sources {
        let src_attr = KeyValue::new("source.id", i64::from(src.source_id));
        for blk in &src.blocks {
            observer.observe(
                extract(blk),
                &[
                    src_attr.clone(),
                    KeyValue::new("gr.block.name", blk.block_name.to_string()),
                    KeyValue::new("gr.block.alias", blk.block_alias.to_string()),
                ],
            );
        }
    }
    for rec in &snap.recorders {
        let attrs = [
            KeyValue::new("recorder.id", i64::from(rec.recorder_id)),
            KeyValue::new("recorder.type", rec.recorder_type.to_string()),
            KeyValue::new("source.id", i64::from(rec.source_id)),
        ];
        for blk in &rec.blocks {
            observer.observe(
                extract(blk),
                &[
                    attrs[0].clone(),
                    attrs[1].clone(),
                    attrs[2].clone(),
                    KeyValue::new("gr.block.name", blk.block_name.to_string()),
                    KeyValue::new("gr.block.alias", blk.block_alias.to_string()),
                ],
            );
        }
    }
}

/// Observe an f64 block metric across all sources and recorders in the snapshot.
fn observe_block_f64(
    snap: &std::sync::Mutex<trunk_oxide::GrMetricsSnapshot>,
    observer: &dyn opentelemetry::metrics::AsyncInstrument<f64>,
    extract: fn(&trunk_recorder_sys::ffi::TrGrBlockMetrics) -> f64,
) {
    let Ok(snap) = snap.lock() else { return };
    for src in &snap.sources {
        let src_attr = KeyValue::new("source.id", i64::from(src.source_id));
        for blk in &src.blocks {
            observer.observe(
                extract(blk),
                &[
                    src_attr.clone(),
                    KeyValue::new("gr.block.name", blk.block_name.to_string()),
                    KeyValue::new("gr.block.alias", blk.block_alias.to_string()),
                ],
            );
        }
    }
    for rec in &snap.recorders {
        let attrs = [
            KeyValue::new("recorder.id", i64::from(rec.recorder_id)),
            KeyValue::new("recorder.type", rec.recorder_type.to_string()),
            KeyValue::new("source.id", i64::from(rec.source_id)),
        ];
        for blk in &rec.blocks {
            observer.observe(
                extract(blk),
                &[
                    attrs[0].clone(),
                    attrs[1].clone(),
                    attrs[2].clone(),
                    KeyValue::new("gr.block.name", blk.block_name.to_string()),
                    KeyValue::new("gr.block.alias", blk.block_alias.to_string()),
                ],
            );
        }
    }
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
    tracing::info!(config = %config_path, "starting trunk-oxide driver");

    let config = trunk_config::Config::load(config_path).context("failed to load TOML config")?;

    // Build talkgroup metadata lookup from config
    let tg_map = {
        use std::collections::HashMap;
        let mut tg_map = HashMap::new();
        for (short_name, sys) in &config.system {
            for tg in &sys.talkgroup {
                tg_map.insert(
                    (short_name.clone(), tg.id),
                    TalkgroupMeta {
                        alpha_tag: tg.alpha_tag.clone(),
                        tag: tg.tag.clone(),
                        group: tg.group.clone(),
                    },
                );
            }
            for ch in &sys.channel {
                tg_map.insert(
                    (short_name.clone(), ch.id),
                    TalkgroupMeta {
                        alpha_tag: ch.alpha_tag.clone(),
                        tag: ch.tag.clone(),
                        group: ch.group.clone(),
                    },
                );
            }
        }
        tg_map
    };

    let (bridge, stream_tx) = EventBridge::new(tg_map);
    let bridge = Arc::new(bridge);

    let mut recorder = TrunkRecorderBuilder::from_config(&config, bridge.clone())
        .context("failed to initialize trunk-recorder")?
        .start();

    // Single runtime for gRPC server, timers, and signal handling
    let rt = tokio::runtime::Runtime::new()?;

    if meter_provider.is_some() {
        let depth = recorder.event_channel_depth();
        let meter = opentelemetry::global::meter(env!("CARGO_PKG_NAME"));
        let _depth_gauge = meter
            .u64_observable_gauge("trunk.oxide.event_channel.depth")
            .with_description("Number of GR events queued but not yet consumed")
            .with_callback({
                let depth = Arc::clone(&depth);
                move |observer| {
                    observer.observe(depth.load(Ordering::Relaxed) as u64, &[]);
                }
            })
            .build();
        let _utilization_gauge = meter
            .f64_observable_gauge("trunk.oxide.event_channel.utilization")
            .with_description("Event channel utilization (0.0-1.0)")
            .with_callback(move |observer| {
                observer.observe(depth.load(Ordering::Relaxed) as f64 / 256.0, &[]);
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

        // ── GR block performance counters ──
        let gr_metrics = recorder.gr_metrics();

        let _gr_items_read = meter
            .u64_observable_counter("gr.block.items_read")
            .with_description("Total items consumed on input port 0")
            .with_callback({
                let snap = Arc::clone(&gr_metrics);
                move |observer| observe_block_u64(&snap, observer, |b| b.items_read)
            })
            .build();

        let _gr_items_written = meter
            .u64_observable_counter("gr.block.items_written")
            .with_description("Total items produced on output port 0")
            .with_callback({
                let snap = Arc::clone(&gr_metrics);
                move |observer| observe_block_u64(&snap, observer, |b| b.items_written)
            })
            .build();

        let _gr_work_time = meter
            .f64_observable_counter("gr.block.work_time_total")
            .with_description("Cumulative time spent in work()")
            .with_callback({
                let snap = Arc::clone(&gr_metrics);
                move |observer| observe_block_f64(&snap, observer, |b| f64::from(b.work_time_total))
            })
            .build();

        let _gr_input_buf = meter
            .f64_observable_gauge("gr.block.input_buffer_full")
            .with_description("Input buffer saturation (0.0-1.0)")
            .with_callback({
                let snap = Arc::clone(&gr_metrics);
                move |observer| observe_block_f64(&snap, observer, |b| f64::from(b.input_buf_full))
            })
            .build();

        let _gr_output_buf = meter
            .f64_observable_gauge("gr.block.output_buffer_full")
            .with_description("Output buffer saturation (0.0-1.0)")
            .with_callback({
                let snap = Arc::clone(&gr_metrics);
                move |observer| observe_block_f64(&snap, observer, |b| f64::from(b.output_buf_full))
            })
            .build();

        // ── Conventional channel metrics ──
        let conv_metrics = recorder.conv_metrics();

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

        let _conv_adaptive_squelch_db = meter
            .f64_observable_gauge("trunk.oxide.conventional.adaptive_squelch_db")
            .with_description("Current adaptive squelch threshold (dBFS), only emitted when active")
            .with_callback({
                let snap = Arc::clone(&conv_metrics);
                move |observer| {
                    let Ok(snap) = snap.lock() else { return };
                    for ch in &snap.channels {
                        if let Some(db) = ch.adaptive_squelch_db {
                            observer.observe(
                                db,
                                &[
                                    KeyValue::new("system", ch.system.clone()),
                                    KeyValue::new("talkgroup", ch.talkgroup),
                                    KeyValue::new("freq_mhz", ch.freq_hz / 1_000_000.0),
                                ],
                            );
                        }
                    }
                }
            })
            .build();
    }

    // Spawn gRPC server as a background task on the runtime
    let socket_path = trunk_oxide_grpc::GrpcConfig::default().socket_path;
    rt.spawn(trunk_oxide_grpc::serve_grpc(socket_path, stream_tx));

    tracing::info!("trunk-recorder engine running, press Ctrl+C to stop");

    // The monitor loop makes FFI calls into C++ (TrContext), which is !Send and
    // must stay on a single OS thread. We spawn a dedicated thread and use
    // Handle::block_on to drive the async monitor future on that thread while
    // sharing the runtime's I/O reactor, timers, and signal handler.
    let monitor_handle = {
        let rt_handle = rt.handle().clone();
        std::thread::Builder::new()
            .name("monitor".into())
            .spawn(move || rt_handle.block_on(recorder.run_monitor()))
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
