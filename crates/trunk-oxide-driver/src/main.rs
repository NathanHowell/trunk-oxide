//! trunk-oxide: Drives trunk-recorder as a linked library.
//!
//! This binary replaces the trunk-recorder executable. It links against
//! libtrunk-recorder.so, reads the same JSON config, and routes all events
//! through the gRPC plugin to trunk-whisperer (or any other gRPC consumer).

use anyhow::{Context, Result};
use trunk_oxide_driver::TrunkRecorderDriver;
use trunk_oxide_grpc::GrpcPlugin;

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

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

    Ok(())
}
