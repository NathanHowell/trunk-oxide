use std::time::Duration;

use anyhow::Result;
use tonic::transport::Endpoint;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use trunk_whisperer_proto::TranscribeRequest;
use trunk_whisperer_proto::whisperer_client::WhispererClient;

/// Writes W3C trace-context headers into outgoing gRPC metadata so the
/// whisperer's transcribe span joins the active (call) trace.
struct MetadataInjector<'a>(&'a mut tonic::metadata::MetadataMap);

impl opentelemetry::propagation::Injector for MetadataInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let Ok(key) = tonic::metadata::MetadataKey::from_bytes(key.as_bytes())
            && let Ok(value) = tonic::metadata::MetadataValue::try_from(value.as_str())
        {
            self.0.insert(key, value);
        }
    }
}

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
    ///
    /// `short_name` / `is_conventional` become metric labels on the whisperer so
    /// decode CPU is attributable by system; `talkgroup` rides on the span (it
    /// is too high-cardinality for a metric label).
    pub async fn transcribe(
        &self,
        audio: Vec<f32>,
        short_name: &str,
        is_conventional: bool,
        talkgroup: i64,
        priority: i32,
    ) -> Result<Option<String>> {
        let bytes: Vec<u8> = audio.iter().flat_map(|s| s.to_le_bytes()).collect();

        let mut request = tonic::Request::new(TranscribeRequest {
            audio: bytes,
            short_name: short_name.to_string(),
            is_conventional,
            talkgroup,
            priority,
        });

        // Inject the active trace context (the call span) so the whisperer's
        // transcribe span is parented to it, stitching one end-to-end trace.
        let cx = tracing::Span::current().context();
        opentelemetry::global::get_text_map_propagator(|prop| {
            prop.inject_context(&cx, &mut MetadataInjector(request.metadata_mut()));
        });

        let mut client = self.client.clone();
        let rpc = client.transcribe(request);

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
