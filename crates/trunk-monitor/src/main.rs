//! trunk-monitor: keeps a live metrics dashboard up to date in a Discord
//! channel. It shares nothing with trunk-discord but the bot's identity: it
//! has its own process, its own health check and its own failures.

mod board;
mod chart;
mod dashboard;
mod health;
mod metrics;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use opentelemetry::trace::TracerProvider;
use serenity::model::id::ChannelId;
use tracing_subscriber::prelude::*;
use trunk_config::Config;

#[derive(Parser)]
struct Args {
    /// Path to config file
    #[arg(short, long, default_value = "/etc/trunk-monitor/config.toml")]
    config: String,

    /// Discord bot token (use @/path/to/file to read from file)
    #[arg(long)]
    discord_token: Option<String>,
}

/// A refresh that takes longer than this is abandoned.
const CYCLE: Duration = Duration::from_secs(240);

struct OtelProviders {
    tracer: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    meter: Option<opentelemetry_sdk::metrics::SdkMeterProvider>,
}

fn init_otel() -> OtelProviders {
    let resource = opentelemetry_sdk::Resource::builder_empty()
        .with_service_name(env!("CARGO_PKG_NAME"))
        .build();

    let tracer_provider = if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build()
            .ok()
            .map(|exporter| {
                let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                    .with_batch_exporter(exporter)
                    .with_resource(resource.clone())
                    .build();
                opentelemetry::global::set_tracer_provider(provider.clone());
                provider
            })
    } else {
        None
    };

    let meter_provider = if std::env::var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT").is_ok()
        || std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok()
    {
        opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .build()
            .ok()
            .map(|exporter| {
                let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(exporter).build();
                let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
                    .with_reader(reader)
                    .with_resource(resource)
                    .build();
                opentelemetry::global::set_meter_provider(provider.clone());
                provider
            })
    } else {
        None
    };

    let otel_layer = tracer_provider.as_ref().map(|provider| {
        let tracer = provider.tracer(env!("CARGO_PKG_NAME"));
        // serenity's HTTP spans record the whole request, uploaded picture
        // included, as attributes: hundreds of KB that make Jaeger reject the
        // batch they are in.
        tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_filter(tracing_subscriber::filter::filter_fn(|meta| {
                !meta.target().starts_with("serenity")
            }))
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(otel_layer)
        .init();

    OtelProviders {
        tracer: tracer_provider,
        meter: meter_provider,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    pid1::Pid1Settings::new().launch()?;

    let args = Args::parse_from(
        argfile::expand_args_from(wild::args_os(), argfile::parse_fromfile, argfile::PREFIX)
            .map_err(|e| anyhow::anyhow!(e))?,
    );

    let otel = init_otel();

    let mut config = Config::load(&args.config)?;
    let monitor = config
        .monitor
        .as_mut()
        .context("missing [monitor] section in config")?;
    if let Some(token) = args.discord_token.as_deref() {
        token.trim().clone_into(&mut monitor.bot_token);
    } else if let Ok(token) = std::env::var("DISCORD_TOKEN") {
        token.trim().clone_into(&mut monitor.bot_token);
    }
    let monitor = config
        .monitor
        .as_ref()
        .context("missing [monitor] section in config")?;

    let every = Duration::from_secs(monitor.refresh_secs.max(10));
    let limits = health::Limits::for_refresh(every, CYCLE);
    let health = health::Health::new();

    let (reporter, health_service) = tonic_health::server::health_reporter();
    tokio::spawn(health::watchdog(reporter, Arc::clone(&health), limits));

    let listen_path = config.grpc.listen_path.clone();
    tokio::spawn(async move {
        let _ = std::fs::remove_file(&listen_path);
        let listener = match tokio::net::UnixListener::bind(&listen_path) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(path = %listen_path, error = %e, "failed to bind health UDS");
                std::process::exit(1);
            }
        };
        let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
        tracing::info!(path = %listen_path, "trunk-monitor health server listening");
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(health_service)
            .serve_with_incoming(incoming)
            .await
        {
            tracing::error!("health server error: {e}");
            std::process::exit(1);
        }
    });

    // Register the bundled font before any chart renders (distroless has none).
    chart::register_font();
    let stats = chart::StatsClient::new(&monitor.victoriametrics_url)?;
    let channel = board::Channel::new(&monitor.bot_token, ChannelId::new(monitor.channel_id.get()));
    let meter = opentelemetry::global::meter("trunk-monitor");

    tracing::info!(
        channel = %monitor.channel_id,
        victoriametrics = %monitor.victoriametrics_url,
        every_s = every.as_secs(),
        "trunk-monitor started"
    );
    dashboard::run(
        dashboard::Dashboard::new(stats, channel),
        every,
        CYCLE,
        metrics::Metrics::new(&meter),
        health,
    )
    .await;

    if let Some(provider) = otel.meter {
        let _ = provider.shutdown();
    }
    if let Some(provider) = otel.tracer {
        let _ = provider.shutdown();
    }
    Ok(())
}
