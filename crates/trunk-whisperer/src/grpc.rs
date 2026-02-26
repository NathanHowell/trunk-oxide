use std::sync::Arc;

use tonic::{Request, Response, Status};

use crate::proto::whisperer_server::Whisperer;
use crate::proto::{TranscribeRequest, TranscribeResponse};
use crate::transcriber::Transcriber;

pub struct WhispererService {
    transcriber: Arc<Transcriber>,
}

impl WhispererService {
    pub const fn new(transcriber: Arc<Transcriber>) -> Self {
        Self { transcriber }
    }
}

#[tonic::async_trait]
impl Whisperer for WhispererService {
    async fn transcribe(
        &self,
        request: Request<TranscribeRequest>,
    ) -> Result<Response<TranscribeResponse>, Status> {
        let req = request.into_inner();

        // Decode f32 LE bytes
        if req.audio.len() % 4 != 0 {
            return Err(Status::invalid_argument(
                "audio bytes length must be a multiple of 4 (f32 LE)",
            ));
        }
        let audio: Vec<f32> = req
            .audio
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();

        if audio.is_empty() {
            return Ok(Response::new(TranscribeResponse {
                text: String::new(),
            }));
        }

        let text = self
            .transcriber
            .transcribe(audio)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        Ok(Response::new(TranscribeResponse { text }))
    }
}
