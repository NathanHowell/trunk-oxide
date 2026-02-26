use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;

use trunk_solver::Source;
use trunk_solver::config::{parse_config, parse_devices_from_config};
use trunk_solver::device::Device;
use trunk_solver::solver::solve;

#[derive(Parser)]
#[command(about = "Optimally assign SDR devices to cover radio frequencies")]
struct Cli {
    /// Path to trunk-recorder config file
    #[arg(long, default_value = "trunk-recorder.conf")]
    config: PathBuf,

    /// Device types to use (e.g., rtlsdr, airspy). Repeat for multiple devices.
    /// If omitted, inferred from the existing sources in the config file.
    #[arg(long = "device")]
    devices: Vec<String>,

    /// Number of devices (only when a single device type is specified)
    #[arg(long = "count")]
    count: Option<usize>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let channels = parse_config(&cli.config)?;

    if channels.is_empty() {
        bail!("No channels found in config");
    }

    eprintln!("Solving for these frequencies:");
    for c in &channels {
        eprintln!("  {} {:?}", c.frequency, c.signal);
    }

    let devices = build_device_list(&cli)?;

    eprintln!(
        "Using {} device(s): {}",
        devices.len(),
        devices
            .iter()
            .map(|d| d.device_template.split('=').next().unwrap_or("?"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let sources = solve(&channels, &devices)?;

    let output = sources_to_json(&sources)?;
    println!("{output}");

    Ok(())
}

fn build_device_list(cli: &Cli) -> Result<Vec<Device>> {
    if cli.devices.is_empty() && cli.count.is_none() {
        // Infer from existing config sources
        let devices = parse_devices_from_config(&cli.config)?;
        if devices.is_empty() {
            bail!("No sources in config and no --device specified");
        }
        return Ok(devices);
    }

    let count = cli.count.unwrap_or(1);

    if cli.devices.is_empty() {
        bail!("--count requires at least one --device");
    } else if cli.devices.len() == 1 {
        let device = Device::from_name(&cli.devices[0])
            .ok_or_else(|| anyhow::anyhow!("Unknown device: {}", cli.devices[0]))?;
        Ok(vec![device; count])
    } else {
        if count > 1 {
            bail!("--count is only supported for a single device type");
        }
        cli.devices
            .iter()
            .map(|name| {
                Device::from_name(name)
                    .ok_or_else(|| anyhow::anyhow!("Unknown device: {name}"))
            })
            .collect()
    }
}

fn sources_to_json(sources: &[Source]) -> Result<String> {
    let sources_json: Vec<serde_json::Value> = sources
        .iter()
        .map(|s| {
            serde_json::json!({
                "center": s.center,
                "rate": s.rate,
                "gain": s.gain,
                "error": s.error,
                "ppm": s.ppm,
                "driver": s.driver,
                "device": s.device,
                "debugRecorders": 0,
                "digitalRecorders": s.digital_recorders,
                "analogRecorders": s.analog_recorders,
            })
        })
        .collect();

    let output = serde_json::json!({ "sources": sources_json });
    Ok(serde_json::to_string_pretty(&output)?)
}
