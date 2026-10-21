mod ab;
mod aggregator;
mod aggregator_state;
mod audio_shape;
mod call_handler;
mod call_state;
mod channel_manager;
mod channel_writer;
mod discord;
mod dispatcher;
mod dispatcher_state;
mod grpc_oxide;
mod grpc_transcriber;
mod metrics;
mod ogg_encoder;
mod reactions;
mod resampler;
mod signalling;
mod stats_chart;
mod tone_detect;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use opentelemetry::trace::TracerProvider;
use serenity::model::id::{ChannelId, GuildId};
use tokio::sync::Mutex;
use tracing_subscriber::prelude::*;
use trunk_config::Config;

#[derive(Parser)]
struct Args {
    /// Path to config file
    #[arg(short, long, default_value = "/etc/trunk-discord/config.toml")]
    config: String,

    /// Discord bot token (use @/path/to/file to read from file)
    #[arg(long)]
    discord_token: Option<String>,

    /// Directory for hourly `SQLite` databases
    #[arg(long, default_value = "/var/lib/trunk-discord/db")]
    db_path: PathBuf,

    /// One-off jobs instead of the service.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Blind A/B listening tests: post clips, tally votes.
    #[command(subcommand)]
    Ab(ab::Ab),
}

struct OtelProviders {
    tracer: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    meter: Option<opentelemetry_sdk::metrics::SdkMeterProvider>,
}

fn init_otel() -> OtelProviders {
    let otel_resource = opentelemetry_sdk::Resource::builder_empty()
        .with_service_name(env!("CARGO_PKG_NAME"))
        .build();

    // W3C trace-context propagation: inject the active context into outgoing
    // gRPC calls (e.g. the whisperer transcribe RPC) so downstream spans join
    // this service's traces.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    let tracer_provider = if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok() {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build()
            .ok();

        exporter.map(|exporter| {
            let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(otel_resource.clone())
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
        let exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .build()
            .ok();

        exporter.map(|exporter| {
            let reader = opentelemetry_sdk::metrics::PeriodicReader::builder(exporter).build();

            let provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
                .with_reader(reader)
                .with_resource(otel_resource)
                .build();

            opentelemetry::global::set_meter_provider(provider.clone());
            provider
        })
    } else {
        None
    };

    let otel_layer = tracer_provider.as_ref().map(|provider| {
        let tracer = provider.tracer(env!("CARGO_PKG_NAME"));
        // serenity's HTTP spans record the whole request (including uploaded
        // images, hundreds of KB of Debug text) as attributes. That inflates
        // span batches until Jaeger rejects them with a 400, losing every other
        // span in the batch, so keep them out of the exported traces.
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

async fn init_channel_manager(
    discord_client: &Arc<discord::DiscordClient>,
    config: &trunk_config::DiscordConfig,
) -> channel_manager::ChannelLookup {
    let lookup = channel_manager::new_lookup();
    if let Some(guild_id) = config.guild_id.map(GuildId::from) {
        if !config.groups.is_empty() {
            match channel_manager::resolve_existing(
                discord_client,
                guild_id,
                &config.groups,
                &lookup,
            )
            .await
            {
                Ok(existing_channels) => {
                    let reconcile_discord = Arc::clone(discord_client);
                    let reconcile_groups = config.groups.clone();
                    let reconcile_lookup = Arc::clone(&lookup);
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
    } else if !config.groups.is_empty() {
        tracing::warn!(
            "discord.groups configured but guild_id is missing, channel management disabled"
        );
    }
    lookup
}

/// Watches the dispatcher pump's heartbeat and makes the gRPC health check
/// meaningful: stale → report `NotServing`; hard-stall → exit so the container
/// is restarted. Without this, a frozen pump kept reporting `Serving` forever.
async fn health_watchdog(
    reporter: tonic_health::server::HealthReporter,
    heartbeat: Arc<AtomicU64>,
) {
    // Stale beyond SOFT: report unhealthy. Beyond HARD: force a restart.
    // The pump bumps its heartbeat at least once per second while alive, so
    // these thresholds never trip on normal (even idle) operation.
    const SOFT_STALL: u64 = 20_000;
    const HARD_STALL: u64 = 90_000;
    const CHECK_INTERVAL: Duration = Duration::from_secs(5);

    let mut interval = tokio::time::interval(CHECK_INTERVAL);
    let mut serving = true;

    loop {
        interval.tick().await;
        let stale = dispatcher::now_millis().saturating_sub(heartbeat.load(Ordering::Relaxed));

        if stale >= HARD_STALL {
            tracing::error!(
                stale_ms = stale,
                "dispatcher pump stalled past hard limit; exiting for restart"
            );
            std::process::exit(1);
        } else if stale >= SOFT_STALL && serving {
            tracing::error!(
                stale_ms = stale,
                "dispatcher pump heartbeat stale; health -> NotServing"
            );
            reporter
                .set_service_status("", tonic_health::ServingStatus::NotServing)
                .await;
            serving = false;
        } else if stale < SOFT_STALL && !serving {
            tracing::info!("dispatcher pump heartbeat recovered; health -> Serving");
            reporter
                .set_service_status("", tonic_health::ServingStatus::Serving)
                .await;
            serving = true;
        }
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
    let discord = config
        .discord
        .as_mut()
        .expect("missing [discord] section in config");
    if let Some(token) = args.discord_token.as_deref() {
        token.trim().clone_into(&mut discord.bot_token);
    } else if let Ok(token) = std::env::var("DISCORD_TOKEN") {
        token.trim().clone_into(&mut discord.bot_token);
    }
    if let Some(Command::Ab(ab)) = args.command {
        return ab::run(ab, discord).await;
    }

    let (reporter, health_service) = tonic_health::server::health_reporter();

    // Liveness: the dispatcher pump bumps `heartbeat` every loop iteration. The
    // watchdog flips the gRPC health status to NotServing if it goes stale, and
    // exits the process on a hard stall so the container restart policy
    // (`restart: unless-stopped`) recovers it — a frozen pump can no longer
    // masquerade as healthy.
    let heartbeat = Arc::new(AtomicU64::new(dispatcher::now_millis()));
    tokio::spawn(health_watchdog(reporter.clone(), Arc::clone(&heartbeat)));

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

    let discord_config = config
        .discord
        .as_ref()
        .expect("missing [discord] section in config");
    let whisperer_config = config
        .whisperer
        .as_ref()
        .expect("missing [whisperer] section in config");
    let oxide_config = config
        .oxide
        .as_ref()
        .expect("missing [oxide] section in config");
    let fallback_channel_id = ChannelId::from(discord_config.channel_id.get());

    let whisperer = grpc_transcriber::TranscriberConnection::connect(
        whisperer_config.socket_path.clone(),
        Duration::from_secs_f64(config.audio.transcription_timeout_s),
    )
    .await?;
    tracing::info!(
        path = %whisperer_config.socket_path,
        "connected to transcriber"
    );

    let hour = chrono::Utc::now().format("%Y-%m-%dT%H").to_string();
    let db = trunk_database::Database::open_hourly(&args.db_path, &hour)?;
    let db = Arc::new(Mutex::new(db));
    tracing::info!(path = %args.db_path.display(), hour, "opened initial database");

    let discord_client = Arc::new(discord::DiscordClient::new(discord_config));
    let channel_lookup = init_channel_manager(&discord_client, discord_config).await;

    // Register the bundled font before any chart renders (distroless has none).
    stats_chart::register_font();
    let stats_client = stats_chart::StatsClient::from_env();
    if stats_client.is_none() {
        tracing::info!("VM_QUERY_URL set empty; stats chart disabled");
    }

    let aggregator = Arc::new(aggregator::Aggregator::new(
        Arc::clone(&discord_client),
        fallback_channel_id,
        Arc::clone(&db),
        stats_client,
        discord_config
            .monitoring_channel_id
            .map(|id| ChannelId::from(id.get())),
    ));

    let meter = opentelemetry::global::meter("trunk-discord");
    let metrics = Arc::new(metrics::Metrics::new(&meter));

    // Reaction listener: ground truth for clipping / missing-lead-in reports.
    tokio::spawn(reactions::run(
        discord_config.bot_token.clone(),
        Arc::clone(&metrics),
    ));

    let pump = dispatcher::DispatchPump::new(
        whisperer,
        Arc::clone(&discord_client),
        aggregator,
        config.audio.max_segment_ms,
        fallback_channel_id,
        channel_lookup,
        config.audio.tone_detection,
        config.audio.tone_detection_duration_s,
        config.audio.audio_attachment,
        // Leaked once: the handlers' config is Copy.
        discord_config
            .jaeger_url
            .clone()
            .map(|u| &*Box::leak(u.into_boxed_str())),
        config.audio.coalesce_timeout_ms,
        Arc::clone(&db),
        args.db_path,
        metrics,
        Arc::clone(&heartbeat),
    );

    discord_client
        .send_message(fallback_channel_id, "trunk-discord is now online.")
        .await?;

    tracing::info!(
        oxide_socket = %oxide_config.socket_path,
        "connecting to trunk-oxide"
    );

    grpc_oxide::run(
        oxide_config.socket_path.clone(),
        pump,
        reporter,
        Arc::clone(&heartbeat),
    )
    .await?;

    if let Some(provider) = otel.meter {
        let _ = provider.shutdown();
    }
    if let Some(provider) = otel.tracer {
        let _ = provider.shutdown();
    }

    Ok(())
}
