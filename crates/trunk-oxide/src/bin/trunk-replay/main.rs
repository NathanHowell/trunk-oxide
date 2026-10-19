//! trunk-replay: run the real trunk-oxide pipeline against recorded IQ.
//!
//! `run` replays a capture directory (see `plan.rs`) in real time and writes
//! `events.jsonl`, per-call WAVs and `summary.json`. `sweep` runs a grid of
//! parameter overrides in parallel child processes and tabulates the results;
//! `compare` re-tabulates an existing sweep directory.

mod plan;
mod record;
mod sweep;
mod taps;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use tokio::sync::Notify;
use trunk_oxide::{MonitorOptions, ScannerBuilder};
use trunk_oxide_grpc::EventBridge;

use plan::Override;

#[derive(Parser)]
#[command(about = "Replay recorded IQ through the trunk-oxide pipeline")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

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
        /// Concurrent replays (each is real time and ~0.5-1 core).
        #[arg(short, long, default_value_t = 4)]
        jobs: usize,
        #[arg(long)]
        limit_s: Option<f64>,
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
        } => run(&capture, &out, &set, limit_s, grace_s, tap),
        Cmd::Sweep {
            capture,
            out,
            grid,
            set,
            jobs,
            limit_s,
        } => sweep::run(&sweep::SweepJob {
            capture: &capture,
            out: &out,
            base: &set,
            axes: &grid,
            jobs,
            limit_s,
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

fn run(
    capture: &std::path::Path,
    out: &std::path::Path,
    overrides: &[Override],
    limit_s: Option<f64>,
    grace_s: f64,
    tap: bool,
) -> Result<()> {
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

    let rt = tokio::runtime::Runtime::new()?;
    let stop_recorder = Arc::new(Notify::new());
    let recorder_task = {
        let mut rec = record::Recorder::new(out)?;
        let stop = Arc::clone(&stop_recorder);
        rt.spawn(async move {
            rec.run(events_rx, stop).await;
            rec
        })
    };

    let mut scanner = ScannerBuilder::from_config(&config, bridge)
        .context("building the scanner")?
        .start();

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

    let replay_s = limit_s.map_or(plan.duration_s, |l| l.min(plan.duration_s));
    tracing::info!(replay_s, grace_s, "replaying");
    let total = Duration::from_secs_f64(replay_s + grace_s);
    let channelizer = rt.block_on(async {
        let start = tokio::time::Instant::now();
        let mut report = tokio::time::interval(Duration::from_secs(60));
        report.tick().await;
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => { tracing::warn!("interrupted"); break; }
                () = tokio::time::sleep_until(start + total) => break,
                _ = report.tick() => tracing::info!(elapsed_s = start.elapsed().as_secs(), "replaying"),
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
