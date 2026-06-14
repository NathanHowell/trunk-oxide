mod config;
mod grpc;
mod priority_semaphore;
mod transcriber;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use opentelemetry::trace::TracerProvider;
use tracing_subscriber::prelude::*;

use trunk_transcriber_proto as proto;

#[derive(Parser)]
struct Args {
    /// Path to config file
    #[arg(short, long, default_value = "/etc/trunk-parakeet/config.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    pid1::Pid1Settings::new().launch()?;

    let args = Args::parse_from(
        argfile::expand_args_from(wild::args_os(), argfile::parse_fromfile, argfile::PREFIX)
            .map_err(|e| anyhow::anyhow!(e))?,
    );

    let otel_resource = opentelemetry_sdk::Resource::builder_empty()
        .with_service_name(env!("CARGO_PKG_NAME"))
        .build();

    // W3C trace-context propagation: extract the parent context injected by the
    // gRPC client (trunk-discord) so transcribe spans join the caller's trace.
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

    let config = config::Config::load(&args.config)?;

    let transcriber = Arc::new(transcriber::ParakeetTranscriber::new(&config.parakeet)?);

    if meter_provider.is_some() {
        let semaphore = Arc::clone(transcriber.semaphore());
        let max_concurrent = transcriber.max_concurrent();
        let meter = opentelemetry::global::meter(env!("CARGO_PKG_NAME"));
        let _gauge = meter
            .f64_observable_gauge("trunk.parakeet.semaphore.utilization")
            .with_description("Fraction of transcription semaphore permits in use")
            .with_callback(move |observer| {
                let available = semaphore.available_permits();
                #[allow(clippy::cast_precision_loss)]
                let utilization = (max_concurrent - available) as f64 / max_concurrent as f64;
                observer.observe(utilization, &[]);
            })
            .build();

        let semaphore = Arc::clone(transcriber.semaphore());
        let _depth = meter
            .u64_observable_gauge("trunk.parakeet.queue.depth")
            .with_description("Requests parked in the priority admission queue (backlog depth)")
            .with_callback(move |observer| {
                observer.observe(semaphore.waiting() as u64, &[]);
            })
            .build();
    }

    let listen_path = &config.grpc.listen_path;
    let _ = std::fs::remove_file(listen_path);
    let listener = tokio::net::UnixListener::bind(listen_path)?;
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);

    let service = grpc::ParakeetService::new(transcriber);

    let (reporter, health_service) = tonic_health::server::health_reporter();
    reporter
        .set_serving::<proto::transcriber_server::TranscriberServer<grpc::ParakeetService>>()
        .await;

    tracing::info!(path = %listen_path, "trunk-parakeet listening");

    tonic::transport::Server::builder()
        .add_service(health_service)
        .add_service(proto::transcriber_server::TranscriberServer::new(service))
        .serve_with_incoming_shutdown(incoming, shutdown_signal())
        .await?;

    if let Some(provider) = meter_provider {
        let _ = provider.shutdown();
    }
    if let Some(provider) = tracer_provider {
        let _ = provider.shutdown();
    }

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
    tracing::info!("shutdown signal received");
}
