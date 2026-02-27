use anyhow::Result;
use tonic::transport::Endpoint;

use trunk_whisperer_proto::whisperer_client::WhispererClient;
use trunk_whisperer_proto::TranscribeRequest;

#[derive(Clone)]
pub struct WhispererConnection {
    client: WhispererClient<tonic::transport::Channel>,
}

impl WhispererConnection {
    pub async fn connect(socket_path: String) -> Result<Self> {
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
        })
    }

    pub async fn transcribe(&self, audio: Vec<f32>) -> Result<String> {
        let bytes: Vec<u8> = audio.iter().flat_map(|s| s.to_le_bytes()).collect();

        let response = self
            .client
            .clone()
            .transcribe(TranscribeRequest { audio: bytes })
            .await?;

        Ok(response.into_inner().text)
    }
}
