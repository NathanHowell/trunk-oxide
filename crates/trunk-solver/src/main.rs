use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;

use trunk_solver::config::SolverConfig;
use trunk_solver::device::Device;
use trunk_solver::solver::solve;

#[derive(Parser)]
#[command(about = "Optimally assign SDR devices to cover radio frequencies")]
struct Cli {
    /// Path to TOML config file
    #[arg(long, default_value = "config.toml")]
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

    let config = SolverConfig::load(&cli.config)?;
    let channels = config.channels();
    if channels.is_empty() {
        bail!("No channels found in config");
    }

    eprintln!("Solving for these frequencies:");
    for c in &channels {
        let tag_str = match c.tag {
            Some(trunk_solver::Tag::Primary) => " [primary]",
            Some(trunk_solver::Tag::Alternate) => " [alternate]",
            None => "",
        };
        eprintln!("  {} {:?}{}", c.frequency, c.signal, tag_str);
    }

    let devices = build_device_list(&cli, &config)?;

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

    for source in &sources {
        println!(
            "center={}.{:06} MHz  rate={}  gain={}  device={}",
            source.center / 1_000_000,
            source.center % 1_000_000,
            source.rate,
            source.gain,
            source.device,
        );
    }
    println!(
        "[recorder] max_channels = {}",
        trunk_solver::solver::max_channels(&channels, &sources)
    );

    Ok(())
}

fn build_device_list(cli: &Cli, config: &SolverConfig) -> Result<Vec<Device>> {
    if cli.devices.is_empty() && cli.count.is_none() {
        let devices = config.devices()?;
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
                Device::from_name(name).ok_or_else(|| anyhow::anyhow!("Unknown device: {name}"))
            })
            .collect()
    }
}
