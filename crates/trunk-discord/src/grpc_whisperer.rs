use std::time::Duration;

use anyhow::Result;
use tonic::transport::Endpoint;

use trunk_whisperer_proto::TranscribeRequest;
use trunk_whisperer_proto::whisperer_client::WhispererClient;

#[derive(Clone)]
pub struct WhispererConnection {
    client: WhispererClient<tonic::transport::Channel>,
    /// Hard ceiling on a single transcription. Exceeding it abandons the
    /// request (treated as empty) so a wedged/runaway whisper server cannot
    /// block the call handler — and through it, the dispatcher — indefinitely.
    timeout: Duration,
}

impl WhispererConnection {
    pub async fn connect(socket_path: String, timeout: Duration) -> Result<Self> {
        let channel = Endpoint::try_from("http://[::]:50051")?
            .connect_with_connector(tower::service_fn(move |_| {
                let path = socket_path.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(path).await?;
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                }
            }))
            .await?;

        Ok(Self {
            client: WhispererClient::new(channel),
            timeout,
        })
    }

    /// Transcribe `audio`, bounded by [`WhispererConnection::timeout`]. Returns
    /// `Ok(Some(text))` on completion (the text may be empty for silence) and
    /// `Ok(None)` when the request was abandoned after exceeding the timeout, so
    /// the caller can record a distinct timeout metric.
    pub async fn transcribe(&self, audio: Vec<f32>) -> Result<Option<String>> {
        let bytes: Vec<u8> = audio.iter().flat_map(|s| s.to_le_bytes()).collect();

        let mut client = self.client.clone();
        let rpc = client.transcribe(TranscribeRequest { audio: bytes });

        match tokio::time::timeout(self.timeout, rpc).await {
            Ok(response) => Ok(Some(response?.into_inner().text)),
            Err(_elapsed) => {
                // Abandon the request and let the call finalize without a
                // transcript rather than hanging the handler forever.
                tracing::warn!(
                    timeout_s = self.timeout.as_secs_f64(),
                    "transcription timed out, abandoning request"
                );
                Ok(None)
            }
        }
    }
}
