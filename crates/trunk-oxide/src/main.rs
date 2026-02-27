//! trunk-oxide: Drives trunk-recorder as a linked library.
//!
//! This binary replaces the trunk-recorder executable. It links against
//! libtrunk-recorder.so, reads the TOML config file, and routes all events
//! through the gRPC plugin to downstream consumers.

use anyhow::{Context, Result};
use clap::Parser;
use trunk_oxide::TrunkRecorderBuilder;
use trunk_oxide_grpc::GrpcPlugin;
use opentelemetry::trace::TracerProvider;
use tracing_subscriber::prelude::*;

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

    let otel_provider = if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build()?;

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                opentelemetry_sdk::Resource::builder_empty()
                    .with_service_name("trunk-oxide")
                    .build(),
            )
            .build();

        opentelemetry::global::set_tracer_provider(provider.clone());
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

    let config = trunk_config::Config::load(config_path)
        .context("failed to load TOML config")?;
    let recorder = TrunkRecorderBuilder::from_config(&config, GrpcPlugin::new())
        .context("failed to initialize trunk-recorder")?
        .start();

    tracing::info!("trunk-recorder engine running, press Ctrl+C to stop");

    // Block until SIGINT
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install signal handler");
    });

    tracing::info!("shutting down");
    drop(recorder);

    if let Some(provider) = otel_provider {
        let _ = provider.shutdown();
    }

    Ok(())
}
