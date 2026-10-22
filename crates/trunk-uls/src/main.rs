use std::fs::File;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use zip::ZipArchive;

use trunk_config::Config;
use trunk_uls::Point;
use trunk_uls::fetch;
use trunk_uls::report::{band_of, render, systems_of, write_csv};
use trunk_uls::select::{Query, find};

#[derive(Parser)]
#[command(about = "The FCC's licensed sites and channels around the receiver, against the config")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download the FCC's weekly land-mobile licence file into the cache
    Fetch {
        /// Download even when the cached file is under a week old
        #[arg(long)]
        force: bool,
    },
    /// List the licensed sites near a point that the config's channels belong to
    Sites {
        /// Where the receiver is, `lat,lon` in degrees
        #[arg(long)]
        home: String,

        /// Path to the TOML config file
        #[arg(long, default_value = "config.toml")]
        config: PathBuf,

        /// How far from home to look, km
        #[arg(long, default_value_t = 60.0)]
        radius: f64,

        /// Licensed band to look in, MHz `low:high`; default is the config's
        /// P25 channels widened by 3 MHz
        #[arg(long)]
        band: Option<String>,

        /// The ULS zip; default is the cached download (fetched if needed)
        #[arg(long)]
        zip: Option<PathBuf>,

        /// Also write a row per site to this CSV file
        #[arg(long)]
        csv: Option<PathBuf>,
    },
}

#[allow(clippy::cast_possible_truncation)]
fn band(text: &str) -> Result<std::ops::RangeInclusive<i64>> {
    let (low, high) = text
        .split_once(':')
        .context("a band is `low:high` in MHz")?;
    let (low, high): (f64, f64) = (low.parse()?, high.parse()?);
    if low >= high {
        bail!("the band {text} is empty");
    }
    Ok((low * 1e6).round() as i64..=(high * 1e6).round() as i64)
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Fetch { force } => {
            let zip = fetch::ensure(&fetch::cache_dir()?, force)?;
            println!("{}", zip.display());
        }
        Command::Sites {
            home,
            config,
            radius,
            band: wanted,
            zip,
            csv,
        } => {
            let home = Point::parse(&home)?;
            let config = Config::load(config.to_str().context("the config path is not UTF-8")?)
                .with_context(|| format!("reading the config {}", config.display()))?;
            let systems = systems_of(&config);
            let band_hz = match wanted {
                Some(text) => band(&text)?,
                None => band_of(&systems, 3_000_000).context("the config has no P25 channels")?,
            };
            let zip = match zip {
                Some(path) => path,
                None => fetch::ensure(&fetch::cache_dir()?, false)?,
            };
            let mut archive =
                ZipArchive::new(File::open(&zip).with_context(|| zip.display().to_string())?)?;
            let licensees = find(
                &mut archive,
                &Query {
                    home,
                    radius_km: radius,
                    band_hz,
                },
            )?;
            print!("{}", render(&licensees, &systems));
            if let Some(path) = csv {
                write_csv(File::create(&path)?, &licensees, &systems)?;
                eprintln!("wrote {}", path.display());
            }
        }
    }
    Ok(())
}
