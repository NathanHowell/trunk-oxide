//! Index of a run's baseline taps (`trunk-replay run --tap`).
//!
//! Every tap is `<base>.json` (sidecar) plus some of:
//! - `<base>.in.cf32`  conditioner input (only in baselines from before the
//!   conditioner moved into the Rust channelizer; `in_rate`)
//! - `<base>.out.cf32` the channelizer's conditioned output: after resample,
//!   squelch, AGC and FLL (`out_rate`)
//! - `<base>.dibits`   control channels: slicer output, one byte (0..3) per dibit, 4800/s
//! - `<base>.symbols.f32` voice calls: demodulator output, f32 symbols
//! - `<base>.msgs.jsonl`  control channels: every raw decoder message (golden output)

use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

fn size(base: &Path, ext: &str) -> Option<u64> {
    std::fs::metadata(format!("{}.{ext}", base.display()))
        .ok()
        .map(|m| m.len())
}

fn lines(base: &Path, ext: &str) -> Option<usize> {
    std::fs::read_to_string(format!("{}.{ext}", base.display()))
        .ok()
        .map(|t| t.lines().count())
}

/// Write `index.json` into `dir` and return a markdown table of the taps.
pub fn index(dir: &Path) -> Result<String> {
    let mut sidecars: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().is_some_and(|e| e == "json")
                && p.file_name().is_some_and(|n| n != "index.json")
        })
        .collect();
    sidecars.sort();

    let mut entries = Vec::new();
    let mut table = String::from(
        "| tap | kind | system | tg | MHz | rate Hz | seconds | dibits | symbols | raw msgs |\n|---|---|---|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for sc in sidecars {
        let meta: Value = serde_json::from_str(&std::fs::read_to_string(&sc)?)?;
        let base = sc.with_extension("");
        let name = base
            .file_name()
            .map_or_default(|n| n.to_string_lossy().into_owned());
        let out_rate = meta["out_rate"].as_f64().unwrap_or(0.0);
        #[allow(clippy::cast_precision_loss)]
        let seconds = size(&base, "out.cf32").map(|b| b as f64 / 8.0 / out_rate.max(1.0));
        let dibits = size(&base, "dibits");
        #[allow(clippy::cast_precision_loss)]
        let symbols = size(&base, "symbols.f32").map(|b| b / 4);
        let msgs = lines(&base, "msgs.jsonl");

        let f = |x: Option<String>| x.unwrap_or_else(|| "-".into());
        table.push_str(&format!(
            "| {name} | {} | {} | {} | {} | {out_rate:.0} | {} | {} | {} | {} |\n",
            meta["kind"].as_str().unwrap_or("?"),
            meta["system"].as_str().unwrap_or("-"),
            meta["talkgroup"]
                .as_i64()
                .map_or_else(|| "-".into(), |t| t.to_string()),
            meta["freq"]
                .as_f64()
                .map_or_else(|| "-".into(), |v| format!("{:.4}", v / 1e6)),
            f(seconds.map(|s| format!("{s:.1}"))),
            f(dibits.map(|d| d.to_string())),
            f(symbols.map(|d| d.to_string())),
            f(msgs.map(|d| d.to_string())),
        ));
        entries.push(json!({
            "name": name, "meta": meta, "seconds": seconds,
            "files": {
                "in_cf32": size(&base, "in.cf32").map(|_| format!("{name}.in.cf32")),
                "out_cf32": size(&base, "out.cf32").map(|_| format!("{name}.out.cf32")),
                "dibits": dibits.map(|_| format!("{name}.dibits")),
                "symbols_f32": symbols.map(|_| format!("{name}.symbols.f32")),
                "messages": msgs.map(|_| format!("{name}.msgs.jsonl")),
            },
            "dibits": dibits, "symbols": symbols, "raw_messages": msgs,
        }));
    }
    std::fs::write(
        dir.join("index.json"),
        serde_json::to_string_pretty(&entries)?,
    )?;
    Ok(table)
}
