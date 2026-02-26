//! trunk-oxide: Drives trunk-recorder as a linked library.
//!
//! This binary replaces the trunk-recorder executable. It links against
//! libtrunk-recorder.so, reads the same JSON config, and routes all events
//! through the gRPC plugin to trunk-whisperer (or any other gRPC consumer).

use anyhow::{Context, Result};
use trunk_oxide::TrunkRecorderDriver;
use trunk_oxide_grpc::GrpcPlugin;
use opentelemetry::trace::TracerProvider;
use tracing_subscriber::prelude::*;

fn main() -> Result<()> {
    pid1::Pid1Settings::new().launch()?;

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

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "trunk-recorder.conf".to_string());

    let config_json =
        std::fs::read_to_string(&config_path).context("failed to read config file")?;

    tracing::info!(config = %config_path, "starting trunk-oxide driver");

    let mut driver = TrunkRecorderDriver::new(&config_json);
    driver.start(GrpcPlugin::new());

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
    driver.stop();

    if let Some(provider) = otel_provider {
        let _ = provider.shutdown();
    }

    Ok(())
}
