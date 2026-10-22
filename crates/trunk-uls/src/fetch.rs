//! The bulk file, cached outside `/tmp` (which is RAM here).

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

pub const URL: &str = "https://data.fcc.gov/download/pub/uls/complete/l_LMpriv.zip";

/// The FCC refreshes the file weekly.
pub const MAX_AGE: Duration = Duration::from_hours(7 * 24);

/// `$XDG_CACHE_HOME/trunk-oxide`, or `~/.cache/trunk-oxide`.
pub fn cache_root() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(
            std::env::var_os("HOME").context("neither XDG_CACHE_HOME nor HOME is set")?,
        )
        .join(".cache"),
    };
    Ok(base.join("trunk-oxide"))
}

/// Where the licence zip is kept.
pub fn cache_dir() -> Result<PathBuf> {
    Ok(cache_root()?.join("uls"))
}

fn fresh(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < MAX_AGE)
}

/// The cached zip, downloaded first when there is none less than a week old
/// (or `force`).
pub fn ensure(dir: &Path, force: bool) -> Result<PathBuf> {
    let zip = dir.join("l_LMpriv.zip");
    if !force && fresh(&zip) {
        return Ok(zip);
    }
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    eprintln!("downloading {URL}");
    let mut response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_mins(30))
        .build()?
        .get(URL)
        .send()?
        .error_for_status()?;
    let part = dir.join("l_LMpriv.zip.part");
    let bytes = io::copy(&mut response, &mut File::create(&part)?)?;
    fs::rename(&part, &zip)?;
    eprintln!("{bytes} bytes to {}", zip.display());
    Ok(zip)
}
