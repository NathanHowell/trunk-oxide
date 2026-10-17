//! Parameter sweeps: run a grid of replays in parallel child processes and
//! tabulate the results.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::plan::Override;

/// `key=v1,v2,v3` → one axis of the grid.
#[derive(Clone)]
pub struct Axis {
    pub key: String,
    pub values: Vec<String>,
}

impl std::str::FromStr for Axis {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (key, vals) = s
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("expected key=v1,v2,..., got '{s}'"))?;
        let values: Vec<String> = vals.split(',').map(|v| v.trim().to_string()).collect();
        if values.iter().any(String::is_empty) {
            bail!("empty value in grid axis '{s}'");
        }
        Ok(Self {
            key: key.trim().to_string(),
            values,
        })
    }
}

/// Cartesian product of the axes, each point as `key=value` strings.
pub fn expand(axes: &[Axis]) -> Vec<Vec<String>> {
    let mut points: Vec<Vec<String>> = vec![Vec::new()];
    for axis in axes {
        points = points
            .into_iter()
            .flat_map(|p| {
                axis.values.iter().map(move |v| {
                    let mut q = p.clone();
                    q.push(format!("{}={v}", axis.key));
                    q
                })
            })
            .collect();
    }
    points
}

fn variant_name(point: &[String]) -> String {
    if point.is_empty() {
        return "baseline".into();
    }
    point
        .iter()
        .map(|kv| {
            kv.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("__")
}

pub struct SweepJob<'a> {
    pub capture: &'a Path,
    pub out: &'a Path,
    pub base: &'a [Override],
    pub axes: &'a [Axis],
    pub jobs: usize,
    pub limit_s: Option<f64>,
    /// Run children in separate IPC namespaces (see [`run`]).
    pub isolate_ipc: bool,
}

/// GNU Radio backs every circular buffer with a SysV shared-memory segment, and
/// one replay uses ~900 of the host's 4096, so more than four concurrent
/// children fail with "shmget: No space left on device". An IPC namespace per
/// child (unprivileged, via a user namespace that maps the current user) gives
/// each its own limit.
fn child_command(exe: &Path, isolate_ipc: bool) -> Command {
    if isolate_ipc {
        let mut cmd = Command::new("unshare");
        cmd.args(["--user", "--map-current-user", "--ipc"]).arg(exe);
        cmd
    } else {
        Command::new(exe)
    }
}

pub fn run(job: &SweepJob<'_>) -> Result<()> {
    let exe = std::env::current_exe()?;
    let points = expand(job.axes);
    std::fs::create_dir_all(job.out)?;
    eprintln!("sweep: {} variants, {} at a time", points.len(), job.jobs);

    let mut pending: Vec<(String, Vec<String>)> =
        points.into_iter().map(|p| (variant_name(&p), p)).collect();
    pending.reverse();
    let mut running: Vec<(String, Child)> = Vec::new();
    while !pending.is_empty() || !running.is_empty() {
        while running.len() < job.jobs.max(1)
            && let Some((name, point)) = pending.pop()
        {
            let dir = job.out.join(&name);
            std::fs::create_dir_all(&dir)?;
            let log = std::fs::File::create(dir.join("run.log"))?;
            let mut cmd = child_command(&exe, job.isolate_ipc);
            cmd.arg("run")
                .arg("--capture")
                .arg(job.capture)
                .arg("--out")
                .arg(&dir);
            for ov in job.base {
                cmd.arg("--set").arg(ov.to_string());
            }
            for kv in &point {
                cmd.arg("--set").arg(kv);
            }
            if let Some(l) = job.limit_s {
                cmd.arg("--limit-s").arg(l.to_string());
            }
            let child = cmd
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log))
                .spawn()
                .with_context(|| format!("spawning variant {name}"))?;
            eprintln!("sweep: started {name}");
            running.push((name, child));
        }
        std::thread::sleep(Duration::from_secs(2));
        let mut still = Vec::new();
        for (name, mut child) in running.drain(..) {
            match child.try_wait()? {
                Some(status) => eprintln!("sweep: finished {name} ({status})"),
                None => still.push((name, child)),
            }
        }
        running = still;
    }
    let table = compare(job.out)?;
    std::fs::write(job.out.join("comparison.md"), &table)?;
    println!("{table}");
    Ok(())
}

fn get_f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64)
}

/// Markdown table of every `*/summary.json` under `dir`.
pub fn compare(dir: &Path) -> Result<String> {
    let mut rows: Vec<(String, Value)> = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("summary.json").is_file())
        .collect();
    entries.sort();
    for p in entries {
        let text = std::fs::read_to_string(p.join("summary.json"))?;
        rows.push((
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            serde_json::from_str(&text)?,
        ));
    }
    let mut out = String::from(
        "| variant | cc msgs | calls start | calls end | audio s | mean abs ferr Hz | signal dB | noise dB | decode errs | overruns |\n\
         |---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    let f = |x: Option<f64>, p: usize| x.map_or_else(|| "-".to_string(), |x| format!("{x:.p$}"));
    for (name, s) in &rows {
        let errs: f64 = s
            .get("systems")
            .and_then(Value::as_object)
            .map_or(0.0, |m| {
                m.values().filter_map(|x| get_f(x, "decode_errors")).sum()
            });
        let overruns: f64 = s
            .get("channelizer")
            .and_then(Value::as_array)
            .map_or(0.0, |a| a.iter().filter_map(|c| get_f(c, "overruns")).sum());
        out.push_str(&format!(
            "| {name} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            f(get_f(s, "trunk_messages_total"), 0),
            f(get_f(s, "calls_started_total"), 0),
            f(get_f(s, "calls_ended_total"), 0),
            f(get_f(s, "audio_seconds_total"), 1),
            f(get_f(s, "mean_abs_freq_error_hz"), 1),
            f(get_f(s, "mean_signal_db"), 1),
            f(get_f(s, "mean_noise_db"), 1),
            f(Some(errs), 0),
            f(Some(overruns), 0),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_is_a_cartesian_product() {
        let axes: Vec<Axis> = ["squelch=-50,-45", "freq_correction_hz=0,185,300"]
            .iter()
            .map(|s| s.parse().unwrap())
            .collect();
        let pts = expand(&axes);
        assert_eq!(pts.len(), 6);
        assert_eq!(pts[0], ["squelch=-50", "freq_correction_hz=0"]);
        assert_eq!(pts[5], ["squelch=-45", "freq_correction_hz=300"]);
        assert_eq!(expand(&[]), vec![Vec::<String>::new()]);
    }

    #[test]
    fn variant_names_are_filesystem_safe() {
        assert_eq!(
            variant_name(&["system.conv.squelch=-48".into()]),
            "system.conv.squelch_-48"
        );
        assert_eq!(variant_name(&[]), "baseline");
    }
}
