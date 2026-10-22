use std::fs::File;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use zip::ZipArchive;

use trunk_config::Config;
use trunk_uls::Point;
use trunk_uls::fetch;
use trunk_uls::reach::{link, profile, received_dbm};
use trunk_uls::report::{band_of, render, systems_of, write_csv};
use trunk_uls::select::{Query, find};
use trunk_uls::terrain::Terrain;

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
        #[command(flatten)]
        around: Around,

        /// Also write a row per site to this CSV file
        #[arg(long)]
        csv: Option<PathBuf>,
    },
    /// Rank those sites by the signal they should put into the receiver's
    /// antenna over the terrain between them
    Reach {
        #[command(flatten)]
        around: Around,

        /// Height of the receiving antenna above the ground, m
        #[arg(long, default_value_t = 8.0)]
        rx_height: f64,

        /// Loss the terrain does not show (trees, buildings, cable, an antenna
        /// under 0 dBi), dB; set it from a measurement
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        extra_loss_db: f64,

        /// The weakest signal worth decoding, dBm (thermal noise in 12.5 kHz
        /// is -133, a dongle adds ~6 dB, P25 wants ~10 dB over that)
        #[arg(long, default_value_t = -117.0, allow_negative_numbers = true)]
        floor_dbm: f64,

        /// Only sites at least this many dB over the floor
        #[arg(long, default_value_t = f64::NEG_INFINITY, allow_negative_numbers = true)]
        min_margin: f64,
    },
}

#[derive(clap::Args)]
struct Around {
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
}

struct Found {
    home: Point,
    systems: Vec<trunk_uls::report::SystemChannels>,
    licensees: Vec<trunk_uls::Licensee>,
}

impl Around {
    fn find(&self) -> Result<Found> {
        let home = Point::parse(&self.home)?;
        let config = Config::load(
            self.config
                .to_str()
                .context("the config path is not UTF-8")?,
        )
        .with_context(|| format!("reading the config {}", self.config.display()))?;
        let systems = systems_of(&config);
        let band_hz = match &self.band {
            Some(text) => band(text)?,
            None => band_of(&systems, 3_000_000).context("the config has no P25 channels")?,
        };
        let zip = match &self.zip {
            Some(path) => path.clone(),
            None => fetch::ensure(&fetch::cache_dir()?, false)?,
        };
        let mut archive =
            ZipArchive::new(File::open(&zip).with_context(|| zip.display().to_string())?)?;
        let licensees = find(
            &mut archive,
            &Query {
                home,
                radius_km: self.radius,
                band_hz,
            },
        )?;
        Ok(Found {
            home,
            systems,
            licensees,
        })
    }
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
        Command::Sites { around, csv } => {
            let found = around.find()?;
            print!("{}", render(&found.licensees, &found.systems));
            if let Some(path) = csv {
                write_csv(File::create(&path)?, &found.licensees, &found.systems)?;
                eprintln!("wrote {}", path.display());
            }
        }
        Command::Reach {
            around,
            rx_height,
            extra_loss_db,
            floor_dbm,
            min_margin,
        } => {
            let found = around.find()?;
            print_reach(&found, rx_height, extra_loss_db, floor_dbm, min_margin)?;
        }
    }
    Ok(())
}

/// The sites by margin over the floor, strongest first.
#[allow(clippy::cast_precision_loss)]
fn print_reach(
    found: &Found,
    rx_height: f64,
    extra_loss_db: f64,
    floor_dbm: f64,
    min_margin: f64,
) -> Result<()> {
    let mut terrain = Terrain::new(fetch::cache_root()?.join("srtm"));
    let rx_ground = terrain.height(found.home)?;
    println!(
        "receiver at {:.4},{:.4}: ground {rx_ground:.0} m, antenna {rx_height:.0} m up, {extra_loss_db:.0} dB of unmodelled loss, floor {floor_dbm:.0} dBm\n\
         (terrain, earth curve and free space only: read the order and the diffraction column, not the absolute margins)\n",
        found.home.lat, found.home.lon
    );
    let in_config: std::collections::BTreeSet<i64> = found
        .systems
        .iter()
        .flat_map(|s| s.channels.iter().copied())
        .collect();
    let mut rows = Vec::new();
    for m in trunk_uls::report::matches(&found.licensees, &found.systems) {
        for site in &m.licensee.sites {
            let ground = match site.ground_m {
                Some(g) => g,
                None => terrain.height(site.at)?,
            };
            let tx = ground + site.structure_m.unwrap_or(0.0).max(5.0);
            let freq_mhz = site
                .channels
                .iter()
                .nth(site.channels.len() / 2)
                .map_or(772.0, |&hz| hz as f64 / 1e6);
            let path = profile(&mut terrain, site.at, found.home)?;
            let l = link(&path, tx, rx_ground + rx_height, freq_mhz);
            let erp = site.erp_w.unwrap_or(100.0);
            let rx = received_dbm(erp, l.loss_db() + extra_loss_db);
            let scanned = site
                .channels
                .iter()
                .filter(|c| in_config.contains(c))
                .count();
            rows.push((
                rx - floor_dbm,
                rx,
                l,
                m.licensee.name.clone(),
                site,
                scanned,
            ));
        }
    }
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!("margin  rx dBm  km    loss dB (diffraction)  clearance m  channels(scanned)  site");
    for (margin, rx, l, licensee, site, scanned) in rows.iter().filter(|r| r.0 >= min_margin) {
        println!(
            "{margin:+6.1}  {rx:6.1}  {:4.1}  {:5.1} ({:4.1})         {:+7.0}      {:2}({scanned:2})            {}/{} {} [{}]",
            l.distance_km,
            l.loss_db(),
            l.diffraction_db,
            l.worst_above_line_m,
            site.channels.len(),
            site.call,
            site.number,
            site.name,
            licensee
        );
    }
    Ok(())
}
