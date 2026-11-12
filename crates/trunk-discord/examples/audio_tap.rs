//! Dev tool: subscribe to a running trunk-oxide and write every audio sample
//! it emits to a raw file (int16 little-endian mono), for offline comparison
//! of DSP paths (e.g. `shared_channelizer` on/off against an `iqfile` source).
//!
//!   cargo run -p trunk-discord --example `audio_tap` -- <socket> <out.raw> [seconds]
//!
//! Prints `sample_rate` and the number of samples written on exit.

use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result};
use tonic::transport::Endpoint;
use trunk_oxide_proto as proto;

use proto::stream_message::Message;
use proto::trunk_oxide_client::TrunkOxideClient;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let socket = args
        .next()
        .context("usage: audio_tap <socket> <out.raw> [seconds]")?;
    let out = args
        .next()
        .context("usage: audio_tap <socket> <out.raw> [seconds]")?;
    let seconds: u64 = args.next().map_or(Ok(20), |s| s.parse())?;

    let channel = loop {
        let path = socket.clone();
        let connected = Endpoint::try_from("http://[::]:50051")?
            .connect_with_connector(tower::service_fn(move |_| {
                let path = path.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(path).await?;
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                }
            }))
            .await;
        match connected {
            Ok(c) => break c,
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    };
    let mut stream = TrunkOxideClient::new(channel)
        .subscribe(())
        .await?
        .into_inner();
    let mut file = std::io::BufWriter::new(std::fs::File::create(&out)?);

    let (mut samples, mut rate) = (0usize, 0);
    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => break,
            msg = stream.message() => {
                let Some(msg) = msg? else { break };
                if let Some(Message::Audio(a)) = msg.message {
                    rate = a.sample_rate;
                    samples += a.samples.len() / 2;
                    file.write_all(&a.samples)?;
                }
            }
        }
    }
    file.flush()?;
    println!("sample_rate={rate} samples={samples}");
    Ok(())
}
