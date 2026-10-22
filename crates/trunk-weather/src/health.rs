//! Whether observations are arriving, as a gRPC health check.
//!
//! A process that is running and a station that is being heard are different
//! things: the Tempest cloud can be unreachable, or the token revoked, while
//! the process stays up retrying. So the check does not say "serving" because
//! the loop is alive, but because an observation recently arrived.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;

/// The Tempest reports every minute; this many missed is stale. It also bounds
/// how long a fresh start may wait for its first one.
// `from_mins` is not stable on our MSRV (1.94); keep `from_secs`.
#[allow(clippy::duration_suboptimal_units)]
const STALE: Duration = Duration::from_secs(10 * 60);

/// Milliseconds since the epoch.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// When an observation last arrived (the start, until one does).
pub struct Health {
    last_obs: AtomicU64,
}

impl Health {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            last_obs: AtomicU64::new(now_millis()),
        })
    }

    /// An observation arrived.
    pub fn observed(&self) {
        self.last_obs.store(now_millis(), Ordering::Relaxed);
    }

    fn serving(&self) -> bool {
        fresh(now_millis(), self.last_obs.load(Ordering::Relaxed))
    }
}

fn fresh(now: u64, last_obs: u64) -> bool {
    Duration::from_millis(now.saturating_sub(last_obs)) < STALE
}

/// Keeps the gRPC health status in step with [`Health`].
pub async fn watchdog(reporter: HealthReporter, health: Arc<Health>) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    let mut serving = true;
    loop {
        interval.tick().await;
        match (health.serving(), serving) {
            (false, true) => {
                tracing::error!(
                    stale_s = STALE.as_secs(),
                    "no observation lately; health -> NotServing"
                );
                reporter
                    .set_service_status("", ServingStatus::NotServing)
                    .await;
                serving = false;
            }
            (true, false) => {
                tracing::info!("observations are arriving again; health -> Serving");
                reporter
                    .set_service_status("", ServingStatus::Serving)
                    .await;
                serving = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = 60_000;

    #[test]
    fn it_serves_through_a_few_missed_minutes_and_not_past_ten() {
        let now = 100 * MINUTE;
        assert!(fresh(now, now));
        assert!(fresh(now, now - 9 * MINUTE));
        assert!(!fresh(now, now - 10 * MINUTE));
        assert!(!fresh(now, now - 90 * MINUTE));
    }

    #[test]
    fn a_clock_that_stepped_back_does_not_read_as_stale() {
        assert!(fresh(100 * MINUTE, 101 * MINUTE));
    }
}
