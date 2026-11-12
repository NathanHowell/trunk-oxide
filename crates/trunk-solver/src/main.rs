use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;

use trunk_solver::config::SolverConfig;
use trunk_solver::device::Device;
use trunk_solver::solver::{Options, plan};

#[derive(Parser)]
#[command(about = "Optimally assign SDR devices to cover radio frequencies")]
struct Cli {
    /// Path to TOML config file
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,

    /// Device types to use (only rtlsdr). Repeat for multiple devices.
    /// If omitted, inferred from the existing sources in the config file.
    #[arg(long = "device")]
    devices: Vec<String>,

    /// Number of devices (only when a single device type is specified)
    #[arg(long = "count")]
    count: Option<usize>,

    /// How deep in a source's band a channel may sit, as a share of Nyquist.
    /// Loosened toward the roll-off (0.85) when it cannot cover every channel.
    #[arg(long, default_value_t = Options::default().margin)]
    margin: f64,
}

#[allow(clippy::cast_precision_loss)]
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

    let plan = plan(&channels, &devices, &Options { margin: cli.margin })?;
    let sources = &plan.sources;

    eprintln!(
        "\nMargin {:.2} (asked {:.2}); depth is the share of Nyquist a channel's far edge reaches:",
        plan.margin, cli.margin
    );
    for p in &plan.placements {
        let tag = match p.channel.tag {
            Some(trunk_solver::Tag::Primary) => " [primary]",
            Some(trunk_solver::Tag::Alternate) => " [alternate]",
            None => "",
        };
        match p.carried {
            Some((n, d)) => eprintln!(
                "  {:>11.5} MHz  source {n}  depth {d:.2}{}{tag}",
                p.channel.frequency as f64 / 1e6,
                if d > trunk_channelizer::passband::USABLE {
                    "  IN THE ROLL-OFF"
                } else {
                    ""
                },
            ),
            None => eprintln!(
                "  {:>11.5} MHz  NOT COVERED{tag}",
                p.channel.frequency as f64 / 1e6
            ),
        }
    }
    eprintln!(
        "Total {:.3} MS/s on {} device(s)\n",
        sources.iter().map(|s| s.rate).sum::<i64>() as f64 / 1e6,
        sources.len()
    );

    for source in sources {
        println!(
            "center={}.{:06} MHz  rate={}  device={}",
            source.center / 1_000_000,
            source.center % 1_000_000,
            source.rate,
            source.device,
        );
    }
    println!(
        "[recorder] max_channels = {}",
        trunk_solver::solver::max_channels(&channels, sources)
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
