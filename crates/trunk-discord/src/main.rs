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

mod proto {
    tonic::include_proto!("whisperer");
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

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

    Ok(())
}
