use std::sync::Arc;

use anyhow::Result;
use tonic::transport::Endpoint;
use trunk_oxide_proto as proto;

use proto::trunk_oxide_client::TrunkOxideClient;

use crate::dispatcher::Dispatcher;

pub async fn run(socket_path: String, dispatcher: Arc<Dispatcher>) -> Result<()> {
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

    let dispatcher_events = Arc::clone(&dispatcher);
    let event_handle = tokio::spawn(async move {
        while let Ok(Some(event)) = event_stream.message().await {
            dispatcher_events.handle_event(event).await;
        }
        tracing::info!("event stream ended");
    });

    let audio_handle = tokio::spawn(async move {
        while let Ok(Some(sample)) = audio_stream.message().await {
            dispatcher.handle_audio(sample).await;
        }
        tracing::info!("audio stream ended");
    });

    let (event_result, audio_result) = tokio::join!(event_handle, audio_handle);
    if let Err(e) = event_result {
        tracing::error!("event stream task join error: {e}");
    }
    if let Err(e) = audio_result {
        tracing::error!("audio stream task join error: {e}");
    }

    Ok(())
}
