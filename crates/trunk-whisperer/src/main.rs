mod config;
mod grpc;
mod transcriber;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use opentelemetry::trace::TracerProvider;
use tracing_subscriber::prelude::*;

use trunk_whisperer_proto as proto;

#[derive(Parser)]
struct Args {
    /// Path to config file
    #[arg(short, long, default_value = "/etc/trunk-whisperer/config.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    pid1::Pid1Settings::new().launch()?;

    let args = Args::parse_from(
        argfile::expand_args_from(wild::args_os(), argfile::parse_fromfile, argfile::PREFIX)
            .map_err(|e| anyhow::anyhow!(e))?,
    );

    let otel_provider = if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build()?;

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                opentelemetry_sdk::Resource::builder_empty()
                    .with_service_name("trunk-whisperer")
                    .build(),
            )
            .build();

        opentelemetry::global::set_tracer_provider(provider.clone());
        Some(provider)
    } else {
        None
    };

    let otel_layer = otel_provider.as_ref().map(|provider| {
        let tracer = provider.tracer("trunk-whisperer");
        tracing_opentelemetry::layer().with_tracer(tracer)
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(otel_layer)
        .init();

    let config = config::Config::load(&args.config)?;

    let transcriber = Arc::new(transcriber::Transcriber::new(&config.whisper)?);

    let listen_path = &config.grpc.listen_path;
    let _ = std::fs::remove_file(listen_path);
    let listener = tokio::net::UnixListener::bind(listen_path)?;
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);

    let service = grpc::WhispererService::new(transcriber);

    let (reporter, health_service) = tonic_health::server::health_reporter();
    reporter
        .set_serving::<proto::whisperer_server::WhispererServer<grpc::WhispererService>>()
        .await;

    tracing::info!(path = %listen_path, "trunk-whisperer listening");

    tonic::transport::Server::builder()
        .add_service(health_service)
        .add_service(proto::whisperer_server::WhispererServer::new(service))
        .serve_with_incoming_shutdown(incoming, shutdown_signal())
        .await?;

    if let Some(provider) = otel_provider {
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
