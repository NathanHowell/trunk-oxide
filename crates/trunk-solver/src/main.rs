use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;

use trunk_solver::config::TrunkConfig;
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

    let mut config = TrunkConfig::load(&cli.config)?;
    let config_dir = cli
        .config
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));

    let channels = config.channels(config_dir)?;
    if channels.is_empty() {
        bail!("No channels found in config");
    }

    eprintln!("Solving for these frequencies:");
    for c in &channels {
        eprintln!("  {} {:?}", c.frequency, c.signal);
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

    config.patch_sources(&sources);
    println!("{}", config.to_json_pretty());

    Ok(())
}

fn build_device_list(cli: &Cli, config: &TrunkConfig) -> Result<Vec<Device>> {
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
