//! Whether the dashboard is being kept up to date, as a gRPC health check.
//!
//! A process that is running and a dashboard that is current are different
//! things: Discord or `VictoriaMetrics` can be unreachable for an hour while
//! the process stays up. So the check does not say "serving" because the
//! loop is alive, but because it recently *succeeded*.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;

/// Milliseconds since the epoch.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// What the dashboard loop has done lately.
pub struct Health {
    /// When a refresh last succeeded (the start, until one does).
    last_ok: AtomicU64,
    /// When the loop last finished a refresh, well or badly.
    last_beat: AtomicU64,
}

impl Health {
    pub fn new() -> Arc<Self> {
        let now = now_millis();
        Arc::new(Self {
            last_ok: AtomicU64::new(now),
            last_beat: AtomicU64::new(now),
        })
    }

    /// A refresh succeeded.
    pub fn ok(&self) {
        let now = now_millis();
        self.last_ok.store(now, Ordering::Relaxed);
        self.last_beat.store(now, Ordering::Relaxed);
    }

    /// A refresh failed, but the loop is alive.
    pub fn beat(&self) {
        self.last_beat.store(now_millis(), Ordering::Relaxed);
    }

    fn judge(&self, limits: Limits) -> Verdict {
        limits.judge(
            now_millis(),
            self.last_ok.load(Ordering::Relaxed),
            self.last_beat.load(Ordering::Relaxed),
        )
    }
}

/// How long without a success or a heartbeat is too long.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Without a successful refresh for this long: not serving.
    pub stale: Duration,
    /// Without the loop finishing a refresh for this long: it is wedged, and
    /// the process exits so the container is restarted.
    pub stalled: Duration,
}

/// The least that counts as stale, and as wedged, whatever the refresh.
// `from_mins` is not stable on our MSRV (1.94); keep `from_secs`.
#[allow(clippy::duration_suboptimal_units)]
const MIN_STALE: Duration = Duration::from_secs(15 * 60);
#[allow(clippy::duration_suboptimal_units)]
const MIN_STALLED: Duration = Duration::from_secs(20 * 60);

impl Limits {
    /// For a dashboard refreshed every `refresh`, whose refresh gives up
    /// after `cycle`: three missed refreshes (at least 15 minutes) is stale,
    /// and a loop that has not finished one in three refreshes plus a cycle
    /// (at least 20 minutes) is wedged.
    pub fn for_refresh(refresh: Duration, cycle: Duration) -> Self {
        Self {
            stale: (3 * refresh).max(MIN_STALE),
            stalled: (3 * refresh + cycle).max(MIN_STALLED),
        }
    }

    fn judge(self, now: u64, last_ok: u64, last_beat: u64) -> Verdict {
        let since = |then: u64| Duration::from_millis(now.saturating_sub(then));
        if since(last_beat) >= self.stalled {
            Verdict::Stalled
        } else if since(last_ok) >= self.stale {
            Verdict::Stale
        } else {
            Verdict::Serving
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Serving,
    Stale,
    Stalled,
}

/// Keeps the gRPC health status in step with [`Health`]: not serving while
/// the dashboard is stale, serving again when it recovers, and exit when the
/// loop is wedged.
pub async fn watchdog(reporter: HealthReporter, health: Arc<Health>, limits: Limits) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    let mut serving = true;
    loop {
        interval.tick().await;
        match health.judge(limits) {
            Verdict::Stalled => {
                tracing::error!("the dashboard loop is wedged; exiting for a restart");
                std::process::exit(1);
            }
            Verdict::Stale if serving => {
                tracing::error!(
                    stale_s = limits.stale.as_secs(),
                    "no successful dashboard update; health -> NotServing"
                );
                reporter
                    .set_service_status("", ServingStatus::NotServing)
                    .await;
                serving = false;
            }
            Verdict::Serving if !serving => {
                tracing::info!("the dashboard is updating again; health -> Serving");
                reporter
                    .set_service_status("", ServingStatus::Serving)
                    .await;
                serving = true;
            }
            Verdict::Serving | Verdict::Stale => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = 60_000;

    fn limits() -> Limits {
        Limits::for_refresh(Duration::from_secs(300), Duration::from_secs(240))
    }

    #[test]
    fn the_limits_are_a_few_refreshes_and_never_under_a_quarter_hour() {
        assert_eq!(limits().stale, Duration::from_secs(15 * 60));
        assert_eq!(limits().stalled, Duration::from_secs(20 * 60));
        // a slow refresh stretches them
        let slow = Limits::for_refresh(Duration::from_secs(900), Duration::from_secs(240));
        assert_eq!(slow.stale, Duration::from_secs(45 * 60));
        assert_eq!(slow.stalled, Duration::from_secs(2_940));
        // a fast one does not shrink them
        let fast = Limits::for_refresh(Duration::from_secs(30), Duration::from_secs(20));
        assert_eq!(fast.stale, Duration::from_secs(15 * 60));
    }

    #[test]
    fn it_serves_while_refreshes_succeed_and_after_a_short_failure() {
        let now = 100 * MINUTE;
        assert_eq!(
            limits().judge(now, now - MINUTE, now - MINUTE),
            Verdict::Serving
        );
        // five minutes of failures, the loop still beating
        assert_eq!(
            limits().judge(now, now - 5 * MINUTE, now - MINUTE),
            Verdict::Serving
        );
    }

    #[test]
    fn it_is_not_serving_once_nothing_has_succeeded_for_a_quarter_hour() {
        let now = 100 * MINUTE;
        assert_eq!(
            limits().judge(now, now - 14 * MINUTE, now),
            Verdict::Serving
        );
        assert_eq!(limits().judge(now, now - 15 * MINUTE, now), Verdict::Stale);
        assert_eq!(
            limits().judge(now, now - 90 * MINUTE, now - MINUTE),
            Verdict::Stale
        );
    }

    #[test]
    fn a_loop_that_has_stopped_beating_is_stalled_whatever_it_did_before() {
        let now = 100 * MINUTE;
        assert_eq!(
            limits().judge(now, now, now - 19 * MINUTE),
            Verdict::Serving
        );
        assert_eq!(
            limits().judge(now, now, now - 20 * MINUTE),
            Verdict::Stalled
        );
        assert_eq!(
            limits().judge(now, now - MINUTE, now - 30 * MINUTE),
            Verdict::Stalled
        );
    }

    #[test]
    fn a_fresh_start_is_serving_and_a_success_resets_the_clock() {
        let health = Health::new();
        assert_eq!(health.judge(limits()), Verdict::Serving);
        health.beat();
        health.ok();
        assert_eq!(health.judge(limits()), Verdict::Serving);
    }
}
