use std::time::Duration;

use anyhow::Result;
use tonic::transport::Endpoint;
use trunk_oxide_proto as proto;

use proto::trunk_oxide_client::TrunkOxideClient;

use crate::dispatcher::DispatchPump;

/// Connect to trunk-oxide with retries (exponential backoff, 1s → 2s → 4s … 30s cap).
async fn connect(socket_path: &str) -> Result<tonic::transport::Channel> {
    let mut delay = Duration::from_secs(1);
    let max_delay = Duration::from_secs(30);

    loop {
        let path = socket_path.to_string();
        let result = Endpoint::try_from("http://[::]:50051")?
            .connect_with_connector(tower::service_fn(move |_| {
                let path = path.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(path).await?;
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                }
            }))
            .await;

        match result {
            Ok(channel) => return Ok(channel),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    retry_in = ?delay,
                    "failed to connect to trunk-oxide, retrying"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(max_delay);
            }
        }
    }
}

pub async fn run(
    socket_path: String,
    mut pump: DispatchPump,
    reporter: tonic_health::server::HealthReporter,
) -> Result<()> {
    let channel = connect(&socket_path).await?;
    let mut client = TrunkOxideClient::new(channel);

    let response = client.subscribe(()).await?;
    let mut stream = response.into_inner();

    reporter
        .set_service_status("", tonic_health::ServingStatus::Serving)
        .await;

    pump.run(&mut stream).await;

    reporter
        .set_service_status("", tonic_health::ServingStatus::NotServing)
        .await;

    Ok(())
}
