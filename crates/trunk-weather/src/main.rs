//! trunk-weather: streams a Tempest weather station's observations into
//! `VictoriaMetrics`, so rain and temperature can be lined up with how well the
//! radio is decoding. It shares nothing with the other services but the metrics
//! store.

mod feed;
mod health;
mod metrics;
mod obs;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tracing_subscriber::prelude::*;

#[derive(Parser)]
struct Args {
    /// Tempest personal access token (use @/path/to/file to read from file;
    /// default: the `TEMPEST_TOKEN` environment variable)
    #[arg(long)]
    tempest_token: Option<String>,

    /// The Tempest's device id, when the account has more than one
    /// (default: the first Tempest on the account)
    #[arg(long)]
    device_id: Option<u64>,

    /// Where to serve the gRPC health check
    #[arg(long, default_value = "/run/service/grpc.sock")]
    health_socket: String,
}

fn init_meter() -> Option<opentelemetry_sdk::metrics::SdkMeterProvider> {
    if std::env::var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT").is_err()
        && std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_err()
    {
        return None;
    }
    let resource = opentelemetry_sdk::Resource::builder_empty()
        .with_service_name(env!("CARGO_PKG_NAME"))
        .build();
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
}

#[tokio::main]
async fn main() -> Result<()> {
    pid1::Pid1Settings::new().launch()?;

    let args = Args::parse_from(
        argfile::expand_args_from(wild::args_os(), argfile::parse_fromfile, argfile::PREFIX)
            .map_err(|e| anyhow::anyhow!(e))?,
    );

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let meter_provider = init_meter();
    if meter_provider.is_none() {
        tracing::warn!("no OTLP metrics endpoint is set; nothing will be recorded");
    }

    let token = args
        .tempest_token
        .or_else(|| std::env::var("TEMPEST_TOKEN").ok())
        .unwrap_or_default();
    let token = token.trim();
    if token.is_empty() {
        bail!("no Tempest token: pass --tempest-token @/path/to/file or set TEMPEST_TOKEN");
    }

    let health = health::Health::new();
    let (reporter, health_service) = tonic_health::server::health_reporter();
    tokio::spawn(health::watchdog(reporter, std::sync::Arc::clone(&health)));

    let _ = std::fs::remove_file(&args.health_socket);
    let listener = tokio::net::UnixListener::bind(&args.health_socket)
        .with_context(|| format!("binding {}", args.health_socket))?;
    tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(health_service)
            .serve_with_incoming(incoming)
            .await
        {
            tracing::error!("health server error: {e}");
            std::process::exit(1);
        }
    });

    let meter = opentelemetry::global::meter("trunk-weather");
    let feed = feed::Feed::new(
        token.to_owned(),
        args.device_id,
        metrics::Metrics::new(&meter),
        health,
    )?;
    tracing::info!(device = ?args.device_id, "trunk-weather started");
    feed.run().await;
    Ok(())
}
