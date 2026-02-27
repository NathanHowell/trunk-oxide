use std::sync::Arc;

use anyhow::Result;
use tonic::transport::Endpoint;
use trunk_oxide_proto as proto;

use proto::trunk_oxide_client::TrunkOxideClient;

use crate::dispatcher::Dispatcher;

pub async fn run(
    socket_path: String,
    dispatcher: Arc<Dispatcher>,
    reporter: tonic_health::server::HealthReporter,
) -> Result<()> {
    let channel = Endpoint::try_from("http://[::]:50051")?
        .connect_with_connector(tower::service_fn(move |_| {
            let path = socket_path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await?;

    let mut event_client = TrunkOxideClient::new(channel.clone());
    let mut audio_client = TrunkOxideClient::new(channel);

    let event_response = event_client.subscribe_events(()).await?;
    let audio_response = audio_client.subscribe_audio(()).await?;

    let mut event_stream = event_response.into_inner();
    let mut audio_stream = audio_response.into_inner();

    reporter.set_service_status("", tonic_health::ServingStatus::Serving).await;

    let dispatcher_events = Arc::clone(&dispatcher);
    let event_handle = tokio::spawn(async move {
        loop {
            match event_stream.message().await {
                Ok(Some(event)) => dispatcher_events.handle_event(event).await,
                Ok(None) => {
                    tracing::info!("event stream closed by server");
                    break;
                }
                Err(status) => {
                    tracing::error!(%status, "event stream error");
                    break;
                }
            }
        }
    });

    let audio_handle = tokio::spawn(async move {
        loop {
            match audio_stream.message().await {
                Ok(Some(sample)) => dispatcher.handle_audio(sample).await,
                Ok(None) => {
                    tracing::info!("audio stream closed by server");
                    break;
                }
                Err(status) => {
                    tracing::error!(%status, "audio stream error");
                    break;
                }
            }
        }
    });

    let (event_result, audio_result) = tokio::join!(event_handle, audio_handle);
    if let Err(e) = event_result {
        tracing::error!("event stream task join error: {e}");
    }
    if let Err(e) = audio_result {
        tracing::error!("audio stream task join error: {e}");
    }

    reporter.set_service_status("", tonic_health::ServingStatus::NotServing).await;

    Ok(())
}
