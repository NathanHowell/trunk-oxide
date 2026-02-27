mod aggregator;
mod call_handler;
mod channel_manager;
mod config;
mod discord;
mod dispatcher;
mod grpc_oxide;
mod grpc_whisperer;
mod resampler;
mod tone_detect;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use opentelemetry::trace::TracerProvider;
use tokio::sync::Mutex;
use tracing_subscriber::prelude::*;

mod proto {
    tonic::include_proto!("whisperer");
}

#[derive(Parser)]
struct Args {
    /// Path to config file
    #[arg(short, long, default_value = "/etc/trunk-discord/config.toml")]
    config: String,

    /// Discord bot token (use @/path/to/file to read from file)
    #[arg(long)]
    discord_token: Option<String>,

    /// Directory for hourly SQLite databases
    #[arg(long, default_value = "/var/lib/trunk-discord/db")]
    db_path: PathBuf,
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

    let config = config::Config::load(&args.config, args.discord_token.as_deref())?;

    // Start health server on its own UDS
    let (reporter, health_service) = tonic_health::server::health_reporter();
    let health_listen_path = config.grpc.listen_path.clone();
    tokio::spawn(async move {
        let _ = std::fs::remove_file(&health_listen_path);
        let listener = match tokio::net::UnixListener::bind(&health_listen_path) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(path = %health_listen_path, error = %e, "failed to bind health UDS");
                return;
            }
        };
        let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
        tracing::info!(path = %health_listen_path, "trunk-discord health server listening");
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(health_service)
            .serve_with_incoming(incoming)
            .await
        {
            tracing::error!("health server error: {e}");
        }
    });

    let whisperer =
        grpc_whisperer::WhispererConnection::connect(config.whisperer.socket_path.clone()).await?;
    tracing::info!(
        path = %config.whisperer.socket_path,
        "connected to trunk-whisperer"
    );

    let hour = chrono::Utc::now().format("%Y-%m-%dT%H").to_string();
    let db = trunk_database::Database::open_hourly(&args.db_path, &hour)?;
    let db = Arc::new(Mutex::new(db));
    tracing::info!(path = %args.db_path.display(), hour, "opened initial database");

    let discord_client = Arc::new(discord::DiscordClient::new(&config.discord));

    // Resolve existing channel mappings and start background reconciliation
    let channel_lookup = channel_manager::new_lookup();
    if let Some(guild_id) = config.discord.guild_id {
        if !config.discord.groups.is_empty() {
            match channel_manager::resolve_existing(
                &discord_client,
                guild_id,
                &config.discord.groups,
                &channel_lookup,
            )
            .await
            {
                Ok(existing_channels) => {
                    let reconcile_discord = Arc::clone(&discord_client);
                    let reconcile_groups = config.discord.groups.clone();
                    let reconcile_lookup = Arc::clone(&channel_lookup);
                    tokio::spawn(channel_manager::reconcile(
                        reconcile_discord,
                        guild_id,
                        reconcile_groups,
                        reconcile_lookup,
                        existing_channels,
                    ));
                }
                Err(e) => {
                    tracing::error!(error = %e, "failed to resolve existing channels, using fallback");
                }
            }
        }
    } else if !config.discord.groups.is_empty() {
        tracing::warn!("discord.groups configured but guild_id is missing, channel management disabled");
    }

    let aggregator = Arc::new(aggregator::Aggregator::new(
        Arc::clone(&discord_client),
        config.discord.channel_id,
        Arc::clone(&db),
    ));

    let dispatcher = Arc::new(dispatcher::Dispatcher::new(
        whisperer,
        Arc::clone(&discord_client),
        aggregator,
        config.audio.buffer_duration_ms,
        config.discord.channel_id,
        channel_lookup,
        config.audio.tone_detection,
        config.audio.tone_detection_duration_s,
        Arc::clone(&db),
        args.db_path,
    ));

    discord_client
        .send_message(config.discord.channel_id, "trunk-discord is now online.")
        .await?;

    tracing::info!(
        oxide_socket = %config.oxide.socket_path,
        "connecting to trunk-oxide"
    );

    grpc_oxide::run(config.oxide.socket_path.clone(), dispatcher, reporter).await?;

    if let Some(provider) = otel_provider {
        let _ = provider.shutdown();
    }

    Ok(())
}
