//! Keeping the dashboard message up to date.
//!
//! What is cached is only the id of the message to edit, and any failure
//! forgets it. The next refresh looks the message up in the channel again:
//! if it is still there it is edited, and if it was deleted a new one is
//! posted. So a message that goes missing, a restart, and a failed edit all
//! heal on the next refresh, where holding on to a dead id for good would
//! leave the dashboard frozen until the process was restarted.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serenity::model::id::MessageId;
use tokio::time::{MissedTickBehavior, interval, timeout};

use crate::health::Health;
use crate::metrics::Metrics;

/// Draws the chart.
pub trait Chart {
    async fn render(&self) -> Result<Vec<u8>>;
}

/// The Discord message the chart is shown in.
pub trait Board {
    /// The newest dashboard message left in the channel, if any.
    async fn find_previous(&self) -> Result<Option<MessageId>>;
    async fn post(&self, png: Vec<u8>) -> Result<MessageId>;
    async fn edit(&self, id: MessageId, png: Vec<u8>) -> Result<()>;
    /// Delete every other dashboard in the channel's recent history, as
    /// leftovers of runs that no longer update them. How many were deleted.
    async fn clean(&self, keep: MessageId) -> Result<usize>;
}

/// Why a refresh failed, for the metric.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Render,
    Discord,
}

impl Failure {
    const fn label(self) -> &'static str {
        match self {
            Self::Render => "render",
            Self::Discord => "discord",
        }
    }
}

pub struct Dashboard<C, B> {
    chart: C,
    board: B,
    /// The message to edit, once known.
    message: Option<MessageId>,
    /// Whether the channel has been cleared of stale dashboards since start.
    cleaned: bool,
}

impl<C: Chart, B: Board> Dashboard<C, B> {
    pub const fn new(chart: C, board: B) -> Self {
        Self {
            chart,
            board,
            message: None,
            cleaned: false,
        }
    }

    /// Draw the chart and show it.
    pub async fn refresh(&mut self) -> Result<(), (Failure, anyhow::Error)> {
        let png = self
            .chart
            .render()
            .await
            .map_err(|e| (Failure::Render, e))?;
        self.show(png).await.map_err(|e| {
            self.message = None;
            (Failure::Discord, e)
        })
    }

    async fn show(&mut self, png: Vec<u8>) -> Result<()> {
        let existing = match self.message {
            Some(id) => Some(id),
            None => self.board.find_previous().await?,
        };
        let id = match existing {
            Some(id) => {
                self.board.edit(id, png).await?;
                id
            }
            None => self.board.post(png).await?,
        };
        self.message = Some(id);
        Ok(())
    }

    /// Once per run, after the dashboard is shown: delete the stale ones the
    /// channel holds. A failure is only logged, and tried again next time: a
    /// channel with leftovers is untidy, not broken, so it never touches health.
    pub async fn clean_once(&mut self) {
        if self.cleaned {
            return;
        }
        let Some(keep) = self.message else {
            return;
        };
        match self.board.clean(keep).await {
            Ok(deleted) => {
                self.cleaned = true;
                tracing::info!(deleted, "cleared stale dashboards from the channel");
            }
            Err(e) => tracing::warn!(error = %e, "could not clear stale dashboards; will retry"),
        }
    }

    /// Forget the message, as after a refresh that was abandoned part way.
    pub const fn forget(&mut self) {
        self.message = None;
    }
}

/// Refresh every `every`, for ever. A refresh that takes longer than
/// `cycle` is abandoned; the next one starts on schedule.
pub async fn run<C: Chart, B: Board>(
    mut dashboard: Dashboard<C, B>,
    every: Duration,
    cycle: Duration,
    metrics: Metrics,
    health: Arc<Health>,
) {
    let mut ticks = interval(every);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let started = Instant::now();
        let result = match timeout(cycle, dashboard.refresh()).await {
            Ok(Ok(())) => {
                health.ok();
                "ok"
            }
            Ok(Err((failure, error))) => {
                tracing::warn!(error = %error, kind = failure.label(), "dashboard refresh failed");
                health.beat();
                failure.label()
            }
            Err(_) => {
                tracing::warn!(cycle_s = cycle.as_secs(), "dashboard refresh timed out");
                dashboard.forget();
                health.beat();
                "timeout"
            }
        };
        metrics.refreshed(result, started.elapsed());
        if result == "ok" {
            // bounded like a refresh, and not counted as one
            let _ = timeout(cycle, dashboard.clean_once()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    /// What a fake board has been asked, and what it will answer.
    #[derive(Default)]
    struct Fake {
        /// The message the channel really holds.
        holds: RefCell<Option<MessageId>>,
        /// Older dashboards the channel still holds.
        stale: RefCell<Vec<MessageId>>,
        calls: RefCell<Vec<String>>,
        /// Fail the next calls of these kinds.
        fail: RefCell<VecDeque<&'static str>>,
        next_id: RefCell<u64>,
    }

    impl Fake {
        fn call(&self, what: &str) -> Result<()> {
            self.calls.borrow_mut().push(what.to_owned());
            if self.fail.borrow().front() == Some(&what.split(' ').next().unwrap()) {
                self.fail.borrow_mut().pop_front();
                anyhow::bail!("{what} failed");
            }
            Ok(())
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Board for &Fake {
        async fn find_previous(&self) -> Result<Option<MessageId>> {
            self.call("find")?;
            Ok(*self.holds.borrow())
        }

        async fn post(&self, _: Vec<u8>) -> Result<MessageId> {
            self.call("post")?;
            *self.next_id.borrow_mut() += 1;
            let id = MessageId::new(*self.next_id.borrow());
            *self.holds.borrow_mut() = Some(id);
            Ok(id)
        }

        async fn clean(&self, keep: MessageId) -> Result<usize> {
            self.call(&format!("clean {keep}"))?;
            let deleted = self.stale.borrow().len();
            self.stale.borrow_mut().clear();
            Ok(deleted)
        }

        async fn edit(&self, id: MessageId, _: Vec<u8>) -> Result<()> {
            self.call(&format!("edit {id}"))?;
            if *self.holds.borrow() == Some(id) {
                Ok(())
            } else {
                anyhow::bail!("unknown message")
            }
        }
    }

    struct Drawing(Result<(), &'static str>);

    impl Chart for Drawing {
        async fn render(&self) -> Result<Vec<u8>> {
            self.0.map(|()| vec![1]).map_err(|e| anyhow::anyhow!(e))
        }
    }

    fn dashboard(fake: &Fake) -> Dashboard<Drawing, &Fake> {
        Dashboard::new(Drawing(Ok(())), fake)
    }

    #[tokio::test]
    async fn the_first_refresh_posts_and_the_rest_edit_that_message() {
        let fake = Fake::default();
        let mut dash = dashboard(&fake);
        dash.refresh().await.unwrap();
        dash.refresh().await.unwrap();
        dash.refresh().await.unwrap();
        assert_eq!(fake.calls(), ["find", "post", "edit 1", "edit 1"]);
    }

    #[tokio::test]
    async fn a_restart_edits_the_message_the_last_run_left_and_posts_no_second() {
        let fake = Fake::default();
        *fake.holds.borrow_mut() = Some(MessageId::new(7));
        let mut dash = dashboard(&fake);
        dash.refresh().await.unwrap();
        assert_eq!(fake.calls(), ["find", "edit 7"]);
    }

    #[tokio::test]
    async fn a_deleted_message_is_replaced_on_the_next_refresh_not_never() {
        let fake = Fake::default();
        let mut dash = dashboard(&fake);
        dash.refresh().await.unwrap();
        *fake.holds.borrow_mut() = None; // someone deletes it
        let (failure, _) = dash.refresh().await.unwrap_err();
        assert_eq!(failure, Failure::Discord);
        dash.refresh().await.unwrap();
        // the edit of 1 failed; the next refresh looked again, found nothing, posted
        assert_eq!(fake.calls(), ["find", "post", "edit 1", "find", "post"]);
        dash.refresh().await.unwrap();
        assert_eq!(fake.calls().last().unwrap(), "edit 2");
    }

    #[tokio::test]
    async fn an_edit_that_merely_failed_edits_the_same_message_again_not_a_second_one() {
        let fake = Fake::default();
        let mut dash = dashboard(&fake);
        dash.refresh().await.unwrap();
        fake.fail.borrow_mut().push_back("edit");
        assert!(dash.refresh().await.is_err());
        dash.refresh().await.unwrap();
        // it looked the message up again, found it, and edited it
        assert_eq!(fake.calls(), ["find", "post", "edit 1", "find", "edit 1"]);
    }

    #[tokio::test]
    async fn a_failed_lookup_or_post_is_retried_whole() {
        let fake = Fake::default();
        fake.fail.borrow_mut().push_back("find");
        let mut dash = dashboard(&fake);
        assert!(dash.refresh().await.is_err());
        fake.fail.borrow_mut().push_back("post");
        assert!(dash.refresh().await.is_err());
        dash.refresh().await.unwrap();
        assert_eq!(fake.calls(), ["find", "find", "post", "find", "post"]);
    }

    #[tokio::test]
    async fn the_stale_dashboards_of_earlier_runs_are_deleted_once_and_the_live_one_kept() {
        let fake = Fake::default();
        *fake.holds.borrow_mut() = Some(MessageId::new(7));
        *fake.stale.borrow_mut() = vec![MessageId::new(3), MessageId::new(5)];
        let mut dash = dashboard(&fake);
        dash.refresh().await.unwrap();
        dash.clean_once().await;
        assert_eq!(fake.calls(), ["find", "edit 7", "clean 7"]);
        assert!(fake.stale.borrow().is_empty());
        assert_eq!(*fake.holds.borrow(), Some(MessageId::new(7)));
        // and not again: that was the once
        dash.refresh().await.unwrap();
        dash.clean_once().await;
        assert_eq!(fake.calls(), ["find", "edit 7", "clean 7", "edit 7"]);
    }

    #[tokio::test]
    async fn a_channel_with_no_live_dashboard_gets_one_and_loses_the_rest() {
        let fake = Fake::default();
        *fake.stale.borrow_mut() = vec![MessageId::new(1), MessageId::new(2)];
        let mut dash = dashboard(&fake);
        dash.refresh().await.unwrap();
        dash.clean_once().await;
        assert_eq!(fake.calls(), ["find", "post", "clean 1"]);
        assert!(fake.stale.borrow().is_empty());
    }

    #[tokio::test]
    async fn a_cleaning_that_fails_is_tried_again_and_never_fails_the_refresh() {
        let fake = Fake::default();
        *fake.holds.borrow_mut() = Some(MessageId::new(7));
        *fake.stale.borrow_mut() = vec![MessageId::new(3)];
        fake.fail.borrow_mut().push_back("clean");
        let mut dash = dashboard(&fake);
        dash.refresh().await.unwrap();
        dash.clean_once().await;
        assert_eq!(fake.stale.borrow().len(), 1, "the first try failed");
        dash.refresh().await.unwrap();
        dash.clean_once().await;
        assert!(fake.stale.borrow().is_empty(), "the second did it");
        dash.clean_once().await;
        assert_eq!(
            fake.calls()
                .iter()
                .filter(|c| c.starts_with("clean"))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn nothing_is_cleaned_before_a_dashboard_is_shown() {
        let fake = Fake::default();
        *fake.stale.borrow_mut() = vec![MessageId::new(3)];
        let mut dash = Dashboard::new(Drawing(Err("no font")), &fake);
        assert!(dash.refresh().await.is_err());
        dash.clean_once().await;
        assert!(fake.calls().is_empty());
        assert_eq!(fake.stale.borrow().len(), 1);
    }

    #[tokio::test]
    async fn a_chart_that_cannot_be_drawn_leaves_discord_alone() {
        let fake = Fake::default();
        let mut dash = Dashboard::new(Drawing(Err("no font")), &fake);
        let (failure, _) = dash.refresh().await.unwrap_err();
        assert_eq!(failure, Failure::Render);
        assert!(fake.calls().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_loop_refreshes_on_schedule_and_counts_what_it_did() {
        let fake = Fake::default();
        let health = Health::new();
        let meter = opentelemetry::global::meter("test");
        let run = run(
            dashboard(&fake),
            Duration::from_secs(300),
            Duration::from_secs(240),
            Metrics::new(&meter),
            health,
        );
        // three refreshes' worth of schedule: the first posts and, being the
        // run's first success, clears the channel of stale ones, once
        let _ = timeout(Duration::from_secs(650), run).await;
        assert_eq!(
            fake.calls(),
            ["find", "post", "clean 1", "edit 1", "edit 1"]
        );
    }
}
