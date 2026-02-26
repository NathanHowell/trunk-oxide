mod aggregator;
mod call_handler;
mod config;
mod discord;
mod dispatcher;
mod grpc_oxide;
mod grpc_whisperer;
mod resampler;
mod tone_detect;

use std::sync::Arc;

use anyhow::Result;
use opentelemetry::trace::TracerProvider;
use tracing_subscriber::prelude::*;

mod proto {
    tonic::include_proto!("whisperer");
}

#[tokio::main]
async fn main() -> Result<()> {
    pid1::Pid1Settings::new().launch()?;

    let otel_provider = if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build()?;

        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                opentelemetry_sdk::Resource::builder_empty()
                    .with_service_name("trunk-discord")
                    .build(),
            )
            .build();

        opentelemetry::global::set_tracer_provider(provider.clone());
        Some(provider)
    } else {
        None
    };

    let otel_layer = otel_provider.as_ref().map(|provider| {
        let tracer = provider.tracer("trunk-discord");
        tracing_opentelemetry::layer().with_tracer(tracer)
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(otel_layer)
        .init();

    let config = config::Config::load()?;

    let whisperer =
        grpc_whisperer::WhispererConnection::connect(config.whisperer.socket_path.clone()).await?;
    tracing::info!(
        path = %config.whisperer.socket_path,
        "connected to trunk-whisperer"
    );

    let discord_client = Arc::new(discord::DiscordClient::new(&config.discord));
    let aggregator = Arc::new(aggregator::Aggregator::new(
        Arc::clone(&discord_client),
        config.discord.channel_id,
    ));

    let dispatcher = Arc::new(dispatcher::Dispatcher::new(
        whisperer,
        discord_client,
        aggregator,
        config.audio.buffer_duration_ms,
        config.discord.channel_id,
        config.audio.tone_detection,
        config.audio.tone_detection_duration_s,
    ));

    tracing::info!(
        oxide_socket = %config.oxide.socket_path,
        "connecting to trunk-oxide"
    );

    grpc_oxide::run(config.oxide.socket_path.clone(), dispatcher).await?;

    if let Some(provider) = otel_provider {
        let _ = provider.shutdown();
    }

    Ok(())
}
