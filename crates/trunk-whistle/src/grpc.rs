use std::sync::Arc;

use tonic::{Request, Response, Status};
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::proto::transcriber_server::Transcriber as TranscriberService;
use crate::proto::{TranscribeRequest, TranscribeResponse};
use crate::transcriber::WhistleTranscriber;

pub struct WhistleService {
    transcriber: Arc<WhistleTranscriber>,
}

impl WhistleService {
    pub const fn new(transcriber: Arc<WhistleTranscriber>) -> Self {
        Self { transcriber }
    }
}

/// Reads W3C trace-context headers out of incoming gRPC metadata so a
/// transcribe span can be parented to the caller's (trunk-discord) trace.
struct MetadataExtractor<'a>(&'a tonic::metadata::MetadataMap);

impl opentelemetry::propagation::Extractor for MetadataExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        // TraceContextPropagator::extract only reads traceparent/tracestate via
        // get(); it never enumerates keys, so an empty list is sufficient.
        Vec::new()
    }
}

#[tonic::async_trait]
impl TranscriberService for WhistleService {
    async fn transcribe(
        &self,
        request: Request<TranscribeRequest>,
    ) -> Result<Response<TranscribeResponse>, Status> {
        // Extract the caller's trace context before consuming the request.
        let parent_cx = opentelemetry::global::get_text_map_propagator(|prop| {
            prop.extract(&MetadataExtractor(request.metadata()))
        });

        let req = request.into_inner();

        if !req.audio.len().is_multiple_of(4) {
            return Err(Status::invalid_argument(
                "audio bytes length must be a multiple of 4 (f32 LE)",
            ));
        }
        let (samples, _) = req.audio.as_chunks::<4>();
        let audio: Vec<f32> = samples.iter().map(|c| f32::from_le_bytes(*c)).collect();

        if audio.is_empty() {
            return Ok(Response::new(TranscribeResponse {
                text: String::new(),
            }));
        }

        // One span per decode, parented to the discord call span and carrying
        // the high-cardinality talkgroup (which is deliberately not a metric
        // label). Queue wait vs decode time is visible per-request in the trace.
        let span = tracing::info_span!(
            "transcribe",
            talkgroup = req.talkgroup,
            short_name = %req.short_name,
            is_conventional = req.is_conventional,
            priority = req.priority,
            audio_samples = audio.len(),
        );
        if let Err(e) = span.set_parent(parent_cx) {
            tracing::debug!(?e, "failed to parent transcribe span to caller trace");
        }

        let text = self
            .transcriber
            .transcribe(audio, &req.short_name, req.is_conventional, req.priority)
            .instrument(span)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(TranscribeResponse { text }))
    }
}
