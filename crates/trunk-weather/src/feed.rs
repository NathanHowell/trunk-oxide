//! The connection to the Tempest cloud: find the device, listen to it, and
//! start over whenever the stream drops or goes quiet.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as Frame;

use crate::health::Health;
use crate::metrics::Metrics;
use crate::obs::{self, Message};

const STATIONS_URL: &str = "https://swd.weatherflow.com/swd/rest/stations";
const STREAM_URL: &str = "wss://ws.weatherflow.com/swd/data";

/// A stream with no observation for this long (they come every minute) is
/// dead whatever the socket says.
// `from_mins` is not stable on our MSRV (1.94); keep `from_secs`.
#[allow(clippy::duration_suboptimal_units)]
const SILENCE: Duration = Duration::from_secs(5 * 60);
/// How often to ping, which keeps the server from calling the link idle, and
/// to check for silence.
const BEAT: Duration = Duration::from_secs(60);
const RETRY_MIN: Duration = Duration::from_secs(5);
// `from_mins` is not stable on our MSRV (1.94); keep `from_secs`.
#[allow(clippy::duration_suboptimal_units)]
const RETRY_MAX: Duration = Duration::from_secs(5 * 60);

pub struct Feed {
    token: String,
    device: Option<u64>,
    http: reqwest::Client,
    metrics: Metrics,
    health: Arc<Health>,
}

impl Feed {
    pub fn new(
        token: String,
        device: Option<u64>,
        metrics: Metrics,
        health: Arc<Health>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            token,
            device,
            http,
            metrics,
            health,
        })
    }

    /// Stream for as long as the process lives, backing off between failures.
    pub async fn run(mut self) {
        let mut wait = RETRY_MIN;
        loop {
            let (result, heard) = self.session().await;
            match result {
                Ok(()) => tracing::warn!("the stream closed; reconnecting"),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "the stream failed; reconnecting");
                }
            }
            // a session that delivered something was working: start the
            // backoff over
            wait = if heard {
                RETRY_MIN
            } else {
                (wait * 2).min(RETRY_MAX)
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// One connection. Whether it heard an observation is returned beside how it ended.
    async fn session(&mut self) -> (Result<()>, bool) {
        let device = match self.device().await {
            Ok(id) => id,
            Err(e) => {
                self.metrics.connection("lookup");
                return (Err(e), false);
            }
        };
        let mut heard = false;
        let result = self.stream(device, &mut heard).await;
        self.metrics
            .connection(if result.is_ok() { "ok" } else { "stream" });
        (result, heard)
    }

    /// The device to listen to: the one configured, else the account's Tempest.
    async fn device(&mut self) -> Result<u64> {
        if let Some(id) = self.device {
            return Ok(id);
        }
        // errors name the URL, and the token is not in it, but be sure
        let reply = self
            .http
            .get(STATIONS_URL)
            .bearer_auth(&self.token)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(reqwest::Error::without_url)
            .context("listing the account's stations")?
            .text()
            .await
            .map_err(reqwest::Error::without_url)
            .context("reading the stations")?;
        let id = obs::tempest_device(&reply)?;
        tracing::info!(device = id, "found the Tempest");
        self.device = Some(id);
        Ok(id)
    }

    async fn stream(&self, device: u64, heard: &mut bool) -> Result<()> {
        // the token rides in the URL, so the URL is never logged
        let url = format!("{STREAM_URL}?token={}", self.token);
        let (mut socket, _) = connect_async(url)
            .await
            .map_err(|e| anyhow::anyhow!("connecting to the stream: {e}"))?;
        let listen = serde_json::json!({
            "type": "listen_start",
            "device_id": device,
            "id": "trunk-weather",
        });
        socket.send(Frame::Text(listen.to_string())).await?;
        tracing::info!(device, "listening");

        let mut last = Instant::now();
        let mut beat = tokio::time::interval(BEAT);
        loop {
            tokio::select! {
                frame = socket.next() => match frame {
                    None | Some(Ok(Frame::Close(_))) => return Ok(()),
                    Some(Err(e)) => bail!("reading the stream: {e}"),
                    Some(Ok(Frame::Text(text))) => match obs::parse(&text) {
                        Ok(Message::Observation(obs)) => {
                            last = Instant::now();
                            *heard = true;
                            self.health.observed();
                            self.metrics.observed(&obs);
                        }
                        Ok(Message::RainStart) => self.metrics.rain_started(),
                        Ok(Message::Other) => {}
                        Err(e) => tracing::debug!(error = %e, "skipping a frame"),
                    },
                    Some(Ok(_)) => {}
                },
                _ = beat.tick() => {
                    if last.elapsed() >= SILENCE {
                        bail!("no observation for {} s", last.elapsed().as_secs());
                    }
                    socket.send(Frame::Ping(Vec::new())).await?;
                }
            }
        }
    }
}
