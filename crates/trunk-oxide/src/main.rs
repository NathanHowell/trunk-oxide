//! trunk-oxide: Drives trunk-recorder as a linked library.
//!
//! This binary replaces the trunk-recorder executable. It links against
//! libtrunk-recorder.so, reads the TOML config file, and routes all events
//! through the gRPC EventBridge to downstream consumers.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use clap::Parser;
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

fn main() -> Result<()> {
    pid1::Pid1Settings::new().launch()?;

    let args = Args::parse_from(
        argfile::expand_args_from(wild::args_os(), argfile::parse_fromfile, argfile::PREFIX)
            .map_err(|e| anyhow::anyhow!(e))?,
    );

    let otel_resource = opentelemetry_sdk::Resource::builder_empty()
        .with_service_name("trunk-oxide")
        .build();

    let otel_provider = if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
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

    let meter_provider = if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
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

    let otel_layer = otel_provider.as_ref().map(|provider| {
        let tracer = provider.tracer("trunk-oxide");
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

    let (bridge, audio_tx, event_tx) = EventBridge::new(tg_map);
    let bridge = Arc::new(bridge);

    let mut recorder = TrunkRecorderBuilder::from_config(&config, bridge.clone())
        .context("failed to initialize trunk-recorder")?
        .start();

    // Single runtime for gRPC server, timers, and signal handling
    let rt = tokio::runtime::Runtime::new()?;

    // Register observable gauge for event channel depth
    if meter_provider.is_some() {
        let depth = recorder.event_channel_depth();
        let meter = opentelemetry::global::meter("trunk-oxide");
        let _gauge = meter
            .u64_observable_gauge("trunk.event_channel.depth")
            .with_description("Number of GR events queued but not yet consumed")
            .with_callback(move |observer| {
                observer.observe(depth.load(Ordering::Relaxed) as u64, &[]);
            })
            .build();
    }

    // Spawn gRPC server as a background task on the runtime
    let socket_path = trunk_oxide_grpc::GrpcConfig::default().socket_path;
    rt.spawn(trunk_oxide_grpc::serve_grpc(
        socket_path,
        audio_tx,
        event_tx,
    ));

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
    if let Some(provider) = otel_provider {
        let _ = provider.shutdown();
    }

    monitor_result
}
