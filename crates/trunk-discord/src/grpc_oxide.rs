use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use tonic::transport::Endpoint;
use trunk_oxide_proto as proto;

use proto::trunk_oxide_client::TrunkOxideClient;

use crate::dispatcher::{DispatchPump, now_millis};

/// The longest wait between attempts: under the watchdog's soft stall, so a
/// trunk-oxide that is down for a while does not read as a stalled pump.
const MAX_RETRY: Duration = Duration::from_secs(10);

/// Connect to trunk-oxide with retries (exponential backoff, 1s → 2s → 4s … 10s cap).
///
/// Bumps `heartbeat` on each attempt so the liveness watchdog does not mistake a
/// legitimately slow/unavailable upstream for a stalled pump.
async fn connect(socket_path: &str, heartbeat: &AtomicU64) -> Result<tonic::transport::Channel> {
    let mut delay = Duration::from_secs(1);

    loop {
        heartbeat.store(now_millis(), Ordering::Relaxed);
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
                heartbeat.store(now_millis(), Ordering::Relaxed);
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(MAX_RETRY);
            }
        }
    }
}

/// Subscribe to trunk-oxide and pump its stream, for as long as the process
/// runs: when the stream ends (trunk-oxide restarted, say) the calls it
/// carried are ended and the subscription is made again. Returns only on an
/// error that retrying cannot fix.
pub async fn run(
    socket_path: String,
    mut pump: DispatchPump,
    reporter: tonic_health::server::HealthReporter,
    heartbeat: Arc<AtomicU64>,
) -> Result<()> {
    loop {
        let channel = connect(&socket_path, &heartbeat).await?;
        let mut stream = match TrunkOxideClient::new(channel).subscribe(()).await {
            Ok(response) => response.into_inner(),
            Err(status) => {
                tracing::warn!(%status, retry_in = ?MAX_RETRY, "subscribe to trunk-oxide failed, retrying");
                tokio::time::sleep(MAX_RETRY).await;
                continue;
            }
        };
        tracing::info!("subscribed to trunk-oxide");
        reporter
            .set_service_status("", tonic_health::ServingStatus::Serving)
            .await;

        pump.run(&mut stream).await;
        pump.upstream_lost().await;
        tracing::warn!("trunk-oxide stream lost, reconnecting");
    }
}
