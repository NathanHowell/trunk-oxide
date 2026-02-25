use std::sync::Arc;

use tonic::{Request, Response, Status, Streaming};

use crate::dispatcher::Dispatcher;
use crate::proto;
use crate::proto::trunk_recorder_plugin_server::TrunkRecorderPlugin;

pub struct TrunkRecorderService {
    dispatcher: Arc<Dispatcher>,
}

impl TrunkRecorderService {
    pub fn new(dispatcher: Arc<Dispatcher>) -> Self {
        Self { dispatcher }
    }
}

#[tonic::async_trait]
impl TrunkRecorderPlugin for TrunkRecorderService {
    async fn stream_audio(
        &self,
        request: Request<Streaming<proto::AudioSample>>,
    ) -> Result<Response<()>, Status> {
        let mut stream = request.into_inner();

        while let Some(sample) = stream.message().await? {
            self.dispatcher.handle_audio(sample).await;
        }

        Ok(Response::new(()))
    }

    async fn stream_events(
        &self,
        request: Request<Streaming<proto::PluginEvent>>,
    ) -> Result<Response<()>, Status> {
        let mut stream = request.into_inner();

        while let Some(event) = stream.message().await? {
            self.dispatcher.handle_event(event).await;
        }

        Ok(Response::new(()))
    }
}
