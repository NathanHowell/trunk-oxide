mod config;
mod grpc;
mod transcriber;

use std::sync::Arc;

use anyhow::Result;

mod proto {
    tonic::include_proto!("whisperer");
}

#[tokio::main]
async fn main() -> Result<()> {
    pid1::Pid1Settings::new().launch()?;
    tracing_subscriber::fmt::init();

    let config = config::Config::load()?;

    let transcriber = Arc::new(transcriber::Transcriber::new(&config.whisper)?);

    let listen_path = &config.grpc.listen_path;
    let _ = std::fs::remove_file(listen_path);
    let listener = tokio::net::UnixListener::bind(listen_path)?;
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);

    let service = grpc::WhispererService::new(transcriber);

    tracing::info!(path = %listen_path, "trunk-whisperer listening");

    tonic::transport::Server::builder()
        .add_service(proto::whisperer_server::WhispererServer::new(service))
        .serve_with_incoming_shutdown(incoming, shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
    tracing::info!("shutdown signal received");
}
