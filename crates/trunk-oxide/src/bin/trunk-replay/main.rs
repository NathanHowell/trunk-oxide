//! trunk-replay: run the real trunk-oxide pipeline against recorded IQ.
//!
//! `run` replays a capture directory (see `plan.rs`) in real time, or as
//! fast as it decodes (`--speed max`), and writes `events.jsonl`, per-call
//! WAVs and `summary.json`. `sweep` runs a grid of
//! parameter overrides in parallel child processes and tabulates the results;
//! `compare` re-tabulates an existing sweep directory.

mod plan;
mod record;
mod sweep;
mod taps;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use tokio::sync::Notify;
use trunk_oxide::clock::Clock;
use trunk_oxide::{MonitorOptions, ScannerBuilder};
use trunk_oxide_grpc::EventBridge;
use trunk_radio::Pacer;

use plan::Override;

#[derive(Parser)]
#[command(about = "Replay recorded IQ through the trunk-oxide pipeline")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// How fast a replay runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Speed {
    /// The capture's own rate, on the wall clock, as live.
    #[value(name = "1x")]
    RealTime,
    /// As fast as it decodes: the sources keep the time.
    Max,
}

impl Speed {
    pub const fn name(self) -> &'static str {
        match self {
            Self::RealTime => "1x",
            Self::Max => "max",
        }
    }
}

/// Seconds of capture time per paced coasting step after the sources end.
const COAST_STEP: Duration = Duration::from_millis(100);

#[derive(Subcommand)]
enum Cmd {
    /// Replay one capture with optional config overrides.
    Run {
        /// Capture directory (capture.json, config.toml, CSVs, *.cu8).
        #[arg(long)]
        capture: PathBuf,
        /// Output directory for this run.
        #[arg(long)]
        out: PathBuf,
        /// Config override `key=value`, e.g. `system.conv.squelch=-48`,
        /// `source.2.freq_correction_hz=0`. Aliases: `squelch`,
        /// `freq_correction_hz` (all sources).
        #[arg(long = "set")]
        set: Vec<Override>,
        /// Replay only the first N seconds.
        #[arg(long)]
        limit_s: Option<f64>,
        /// Seconds to keep running after the last sample so calls can finish.
        #[arg(long, default_value_t = 8.0)]
        grace_s: f64,
        /// Write decoder baseline taps to `<out>/taps` (conditioner IQ,
        /// symbols/dibits, raw control messages); see REPLAY.md.
        #[arg(long)]
        tap: bool,
        /// `1x` (real time, as live) or `max` (as fast as it decodes).
        #[arg(long, value_enum, default_value_t = Speed::RealTime)]
        speed: Speed,
    },
    /// Run a grid of overrides in parallel and print a comparison table.
    Sweep {
        #[arg(long)]
        capture: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Grid axis `key=v1,v2,...`; repeat for a cartesian product.
        #[arg(long = "grid")]
        grid: Vec<sweep::Axis>,
        /// Override applied to every variant.
        #[arg(long = "set")]
        set: Vec<Override>,
        /// Concurrent replays (each is real time and ~0.5-1 core at 1x,
        /// a few cores at max).
        #[arg(short, long, default_value_t = 4)]
        jobs: usize,
        #[arg(long)]
        limit_s: Option<f64>,
        #[arg(long, value_enum, default_value_t = Speed::RealTime)]
        speed: Speed,
    },
    /// Tabulate the summary.json of every run under a directory.
    Compare { dir: PathBuf },
    /// Index a run's baseline taps (writes index.json, prints a table).
    Taps { dir: PathBuf },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    match Cli::parse().cmd {
        Cmd::Run {
            capture,
            out,
            set,
            limit_s,
            grace_s,
            tap,
            speed,
        } => run(&Job {
            capture: &capture,
            out: &out,
            overrides: &set,
            limit_s,
            grace_s,
            tap,
            speed,
        }),
        Cmd::Sweep {
            capture,
            out,
            grid,
            set,
            jobs,
            limit_s,
            speed,
        } => sweep::run(&sweep::SweepJob {
            capture: &capture,
            out: &out,
            base: &set,
            axes: &grid,
            jobs,
            limit_s,
            speed,
        }),
        Cmd::Compare { dir } => {
            println!("{}", sweep::compare(&dir)?);
            Ok(())
        }
        Cmd::Taps { dir } => {
            println!("{}", taps::index(&dir)?);
            Ok(())
        }
    }
}

struct Job<'a> {
    capture: &'a std::path::Path,
    out: &'a std::path::Path,
    overrides: &'a [Override],
    limit_s: Option<f64>,
    grace_s: f64,
    tap: bool,
    speed: Speed,
}

#[allow(clippy::too_many_lines)]
fn run(job: &Job<'_>) -> Result<()> {
    let &Job {
        capture,
        out,
        overrides,
        limit_s,
        grace_s,
        tap,
        speed,
    } = job;
    std::fs::create_dir_all(out)?;
    let plan = plan::build(capture, out, overrides, tap)?;
    for s in &plan.skipped_sources {
        tracing::warn!(device = %s, "no capture for source; disabled");
    }
    let config = trunk_config::Config::load(&plan.config_path.display().to_string())
        .context("loading generated replay config")?;

    let (bridge, stream_tx) = EventBridge::new(trunk_oxide::talkgroup_meta_map(&config));
    let bridge = Arc::new(bridge);
    let events_rx = stream_tx.subscribe();

    let replay_s = limit_s.map_or(plan.duration_s, |l| l.min(plan.duration_s));
    // A real-time replay stops on the wall clock after this, the files
    // playing on through the grace if they last that long.
    let total = Duration::from_secs_f64(replay_s + grace_s);
    // At max speed the sources keep the time: the state machine runs on a
    // manual clock the monitor moves to the capture time, and a source
    // waits while the event recorder is behind (it would drop events). The
    // sources stop where real time would have, and capture time coasts on
    // to there if the files end first.
    let (clock, pacer) = match speed {
        Speed::RealTime => (Clock::system(), None),
        Speed::Max => {
            let b = Arc::clone(&bridge);
            let room = b.broadcast_capacity() / 4;
            let pacer = Pacer::new(SystemTime::now(), Some(total), move || {
                b.broadcast_depth() < room
            });
            (Clock::manual(pacer.wall_epoch()), Some(pacer))
        }
    };

    let rt = tokio::runtime::Runtime::new()?;
    let stop_recorder = Arc::new(Notify::new());
    let recorder_task = {
        let mut rec = record::Recorder::new(out, clock.clone())?;
        let stop = Arc::clone(&stop_recorder);
        rt.spawn(async move {
            rec.run(events_rx, stop).await;
            rec
        })
    };

    let mut builder = ScannerBuilder::from_config_paced(&config, bridge, pacer.clone())
        .context("building the scanner")?;
    if pacer.is_some() {
        builder = builder.with_clock(clock.clone());
    }
    let mut scanner = builder.start();

    let shutdown = Arc::new(Notify::new());
    let monitor = {
        let handle = rt.handle().clone();
        let opts = MonitorOptions {
            check_sources: false,
            shutdown: Some(Arc::clone(&shutdown)),
        };
        std::thread::Builder::new()
            .name("monitor".into())
            .spawn(move || handle.block_on(scanner.run_monitor_with(opts)))?
    };

    tracing::info!(replay_s, grace_s, speed = speed.name(), "replaying");
    let channelizer = rt.block_on(async {
        let start = tokio::time::Instant::now();
        let mut report = tokio::time::interval(Duration::from_secs(60));
        report.tick().await;
        // Real time: the replay ends on the wall clock. Max: once every
        // source has ended, capture time coasts on to the same end.
        let finished = async {
            match &pacer {
                None => tokio::time::sleep_until(start + total).await,
                Some(p) => {
                    while !p.all_ended() {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    let p = p.clone();
                    let rest = total.saturating_sub(p.capture_time());
                    let _ = tokio::task::spawn_blocking(move || p.coast(rest, COAST_STEP)).await;
                }
            }
        };
        tokio::pin!(finished);
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => { tracing::warn!("interrupted"); break; }
                () = &mut finished => break,
                _ = report.tick() => tracing::info!(
                    elapsed_s = start.elapsed().as_secs(),
                    capture_s = clock.elapsed().as_secs(),
                    "replaying"
                ),
            }
        }
        trunk_radio::channelizer_stats()
            .into_iter()
            .map(|s| {
                json!({
                    "source": s.source, "blocks": s.counters.blocks,
                    "overruns": s.counters.overruns, "rejected": s.counters.rejected,
                })
            })
            .collect::<Vec<_>>()
    });

    shutdown.notify_one();
    monitor
        .join()
        .map_err(|_| anyhow::anyhow!("monitor thread panicked"))??;
    stop_recorder.notify_one();
    let rec = rt.block_on(recorder_task)?;
    let summary = rec.finish(
        speed.name(),
        overrides.iter().map(ToString::to_string).collect(),
        channelizer,
    )?;
    std::fs::write(
        out.join("summary.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;
    tracing::info!(
        calls = summary.calls_ended_total,
        messages = summary.trunk_messages_total,
        audio_s = summary.audio_seconds_total,
        "done"
    );
    Ok(())
}
