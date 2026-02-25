mod aggregator;
mod call_handler;
mod config;
mod discord;
mod dispatcher;
mod grpc;
mod resampler;
mod tone_detect;
mod transcriber;

use std::sync::Arc;

use anyhow::Result;

pub mod proto {
    tonic::include_proto!("trunk_oxide");
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let config = config::Config::load()?;

    let transcriber = Arc::new(transcriber::Transcriber::new(&config.whisper)?);
    let discord_client = Arc::new(discord::DiscordClient::new(&config.discord));
    let aggregator = Arc::new(aggregator::Aggregator::new(
        Arc::clone(&discord_client),
        config.discord.channel_id,
    ));

    let dispatcher = Arc::new(dispatcher::Dispatcher::new(
        transcriber,
        discord_client,
        aggregator,
        config.audio.buffer_duration_ms,
        config.discord.channel_id,
        config.audio.tone_detection,
        config.audio.tone_detection_duration_s,
    ));

    let addr = config.grpc.listen_addr.parse()?;
    let service = grpc::TrunkRecorderService::new(dispatcher);

    tracing::info!(%addr, "trunk-whisperer listening");

    tonic::transport::Server::builder()
        .add_service(
            proto::trunk_recorder_plugin_server::TrunkRecorderPluginServer::new(service),
        )
        .serve_with_shutdown(addr, shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
    tracing::info!("shutdown signal received");
}
