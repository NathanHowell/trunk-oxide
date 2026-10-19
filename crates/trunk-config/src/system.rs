use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SystemType {
    Conventional,
    ConventionalP25,
    ConventionalDMR,
    #[serde(rename = "conventionalSIGMF")]
    ConventionalSigmf,
    Smartnet,
    #[default]
    P25,
}

impl SystemType {
    pub const fn is_trunked(self) -> bool {
        matches!(self, Self::P25 | Self::Smartnet)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modulation {
    #[default]
    Qpsk,
    Fsk4,
}

#[derive(Debug, Deserialize)]
#[serde(rename = "type")]
#[allow(clippy::struct_excessive_bools)]
pub struct SystemConfig {
    #[serde(rename = "type")]
    pub system_type: SystemType,

    // Trunked system fields
    #[serde(default)]
    pub control_channels: Vec<f64>,
    #[serde(default)]
    pub preferred_control_channel: Option<f64>,
    #[serde(default)]
    pub voice_channels: Vec<f64>,

    // Conventional system channels — CSV file with optional TOML overrides
    #[serde(default)]
    pub channel_file: Option<String>,

    // Conventional system channels (inline)
    #[serde(default)]
    pub channel: Vec<ChannelConfig>,

    // Common fields
    #[serde(default)]
    pub modulation: Modulation,
    #[serde(default = "default_digital_levels")]
    pub digital_levels: f64,
    #[serde(default = "default_analog_levels")]
    pub analog_levels: f64,
    #[serde(default = "default_squelch")]
    pub squelch: f64,
    #[serde(default = "default_max_dev")]
    pub max_dev: i32,
    #[serde(default = "default_true")]
    pub conversation_mode: bool,
    #[serde(default = "default_true")]
    pub record_unknown: bool,

    // Talkgroups — CSV file (RadioReference export) with optional TOML overrides
    #[serde(default)]
    pub talkgroup_file: Option<String>,

    // Talkgroups (inline)
    #[serde(default)]
    pub talkgroup: Vec<TalkgroupConfig>,

    // Unit tags

    // P25 identity
    #[serde(default)]
    pub sys_id: u64,
    #[serde(default)]
    pub nac: u64,
    #[serde(default)]
    pub wacn: u64,

    // Smartnet bandplan
    #[serde(default)]
    pub bandplan: Option<String>,

    // Channel claims (lower wins)
    /// Its control channels' claim on a channel.
    #[serde(default)]
    pub control_channel_priority: i32,
    /// An emergency call's claim, when better than its talkgroup's.
    #[serde(default)]
    pub emergency_priority: i32,
    /// The claim of talkgroups and channels that set none (unset: after
    /// every configured one).
    #[serde(default)]
    pub default_priority: Option<i32>,

    // Filtering
    #[serde(default)]
    pub hide_encrypted: bool,
    #[serde(default)]
    pub monitor_encrypted: bool,
    #[serde(default)]
    pub hide_unknown_talkgroups: bool,

    // Duration limits
    #[serde(default)]
    pub max_duration: f64,

    // Multi-site
    #[serde(default)]
    pub multi_site: bool,
    #[serde(default)]
    pub multi_site_system_name: String,
    // Decoder options
}

/// Recording mode for a talkgroup.
///
/// Values match the `RadioReference` CSV "Mode" column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TalkgroupMode {
    /// Analog voice ("A")
    Analog,
    /// Digital voice ("D") — default
    #[default]
    Digital,
    /// TDMA / P25 Phase II ("T")
    Tdma,
    /// Encrypted ("E")
    Encrypted,
    /// Trunked encrypted ("TE")
    TrunkedEncrypted,
    /// Digital encrypted ("DE")
    DigitalEncrypted,
    /// Mixed analog/digital ("M")
    Mixed,
}

impl TalkgroupMode {
    pub const fn is_encrypted(self) -> bool {
        matches!(
            self,
            Self::Encrypted | Self::TrunkedEncrypted | Self::DigitalEncrypted
        )
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Analog => "A",
            Self::Digital => "D",
            Self::Tdma => "T",
            Self::Encrypted => "E",
            Self::TrunkedEncrypted => "TE",
            Self::DigitalEncrypted => "DE",
            Self::Mixed => "M",
        }
    }
}

impl fmt::Display for TalkgroupMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TalkgroupMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_str(&s))
    }
}

impl TalkgroupMode {
    fn from_str(s: &str) -> Self {
        match s {
            "A" => Self::Analog,
            "D" => Self::Digital,
            "T" => Self::Tdma,
            "E" => Self::Encrypted,
            "M" => Self::Mixed,
            "TE" => Self::TrunkedEncrypted,
            "DE" => Self::DigitalEncrypted,
            other => {
                tracing::warn!(
                    mode = other,
                    "unknown talkgroup mode, defaulting to digital"
                );
                Self::Digital
            }
        }
    }
}

/// Talkgroup entry for trunked systems.
/// Descriptive fields stay in Rust for enrichment.
/// Operational fields decide whether and how its calls are recorded.
#[derive(Debug, Clone, Deserialize)]
pub struct TalkgroupConfig {
    pub id: i64,
    #[serde(default)]
    pub mode: TalkgroupMode,
    #[serde(default)]
    pub alpha_tag: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub group: String,
    /// Its calls' claim on a channel (lower wins; -1: never recorded).
    /// Unset, the system's `default_priority`. Discord also orders
    /// transcription by it.
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub preferred_nac: u64,
}

/// Channel entry for conventional systems.
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelConfig {
    pub id: i64,
    #[serde(default)]
    pub freq: f64,
    #[serde(default)]
    pub tone: f64,
    #[serde(default)]
    pub alpha_tag: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub group: String,
    /// Per-channel squelch override.  `None` inherits the system-level squelch.
    #[serde(default)]
    pub squelch_db: Option<f64>,
    #[serde(default)]
    pub signal_detection: bool,
    /// The channel's claim on a channel to record on (lower wins), and its
    /// transcription admission order at Discord. Conventional channels carry
    /// no CSV priority, so this is set via TOML overlay only; unset, the
    /// channel's talkgroup row's, else the system's `default_priority`.
    #[serde(default)]
    pub priority: Option<i32>,
}

const fn default_digital_levels() -> f64 {
    1.0
}
const fn default_analog_levels() -> f64 {
    8.0
}
const fn default_squelch() -> f64 {
    -160.0
}
const fn default_max_dev() -> i32 {
    5000
}
const fn default_true() -> bool {
    true
}

// ── CSV talkgroup parsing ──

/// Parse a RadioReference-format talkgroup CSV into a map keyed by talkgroup ID.
pub(crate) fn load_talkgroups_csv(path: &Path) -> Result<HashMap<i64, TalkgroupConfig>> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .trim(csv::Trim::All)
        .from_path(path)
        .with_context(|| format!("failed to open talkgroup CSV: {}", path.display()))?;

    // Map header names to column indices
    let headers = reader.headers()?.clone();
    let mut col_decimal = None;
    let mut col_mode = None;
    let mut col_alpha_tag = None;
    let mut col_description = None;
    let mut col_tag = None;
    let mut col_category = None;
    let mut col_priority = None;
    let mut col_preferred_nac = None;

    for (i, h) in headers.iter().enumerate() {
        match h {
            "Decimal" => col_decimal = Some(i),
            "Mode" => col_mode = Some(i),
            "Alpha Tag" => col_alpha_tag = Some(i),
            "Description" => col_description = Some(i),
            "Tag" => col_tag = Some(i),
            "Category" => col_category = Some(i),
            "Priority" => col_priority = Some(i),
            "Preferred NAC" => col_preferred_nac = Some(i),
            _ => {} // Hex, Comment, etc. — ignored
        }
    }

    let col_decimal = col_decimal.context("talkgroup CSV missing required 'Decimal' column")?;

    let mut talkgroups = HashMap::new();

    for result in reader.records() {
        let record = result?;
        let id_str = record.get(col_decimal).unwrap_or("").trim();
        if id_str.is_empty() {
            continue;
        }
        let id: i64 = id_str
            .parse()
            .with_context(|| format!("invalid talkgroup ID: {id_str:?}"))?;

        let get = |col: Option<usize>| -> String {
            col.and_then(|i| record.get(i))
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };

        let mode = {
            let m = get(col_mode);
            if m.is_empty() {
                TalkgroupMode::default()
            } else {
                TalkgroupMode::from_str(&m)
            }
        };

        let priority: Option<i32> = col_priority
            .and_then(|i| record.get(i))
            .and_then(|s| s.trim().parse().ok());

        let preferred_nac: u64 = col_preferred_nac
            .and_then(|i| record.get(i))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        talkgroups.insert(
            id,
            TalkgroupConfig {
                id,
                mode,
                alpha_tag: get(col_alpha_tag),
                description: get(col_description),
                tag: get(col_tag),
                group: get(col_category),
                priority,
                preferred_nac,
            },
        );
    }

    Ok(talkgroups)
}

/// Apply a TOML table overlay onto a `TalkgroupConfig`, setting only
/// the fields that are explicitly present in the table.
fn apply_overlay(tg: &mut TalkgroupConfig, table: &toml::Table) {
    if let Some(v) = table.get("mode").and_then(|v| v.as_str()) {
        tg.mode = TalkgroupMode::from_str(v);
    }
    if let Some(v) = table.get("alpha_tag").and_then(|v| v.as_str()) {
        tg.alpha_tag = v.to_string();
    }
    if let Some(v) = table.get("description").and_then(|v| v.as_str()) {
        tg.description = v.to_string();
    }
    if let Some(v) = table.get("tag").and_then(|v| v.as_str()) {
        tg.tag = v.to_string();
    }
    if let Some(v) = table.get("group").and_then(|v| v.as_str()) {
        tg.group = v.to_string();
    }
    if let Some(v) = table.get("priority").and_then(toml::Value::as_integer) {
        // An out-of-range value is a config typo: unset.
        tg.priority = i32::try_from(v).ok();
    }
    if let Some(v) = table.get("preferred_nac").and_then(toml::Value::as_integer) {
        tg.preferred_nac = v.cast_unsigned();
    }
}

/// Resolve talkgroups from a CSV base (if any) with TOML overlays applied on top.
pub(crate) fn resolve_talkgroups(
    config_dir: &Path,
    talkgroup_file: Option<&str>,
    overlays: &[toml::Value],
) -> Result<Vec<TalkgroupConfig>> {
    // Load CSV base
    let mut map: HashMap<i64, TalkgroupConfig> = match talkgroup_file {
        Some(file) => {
            let csv_path = config_dir.join(file);
            load_talkgroups_csv(&csv_path)?
        }
        None => HashMap::new(),
    };

    // Apply TOML overlays
    for overlay in overlays {
        let table = overlay
            .as_table()
            .context("talkgroup entry must be a TOML table")?;

        let id = table
            .get("id")
            .and_then(toml::Value::as_integer)
            .context("talkgroup entry missing required 'id' field")?;

        let tg = map.entry(id).or_insert_with(|| TalkgroupConfig {
            id,
            mode: TalkgroupMode::default(),
            alpha_tag: String::new(),
            description: String::new(),
            tag: String::new(),
            group: String::new(),
            priority: None,
            preferred_nac: 0,
        });

        apply_overlay(tg, table);
    }

    // Sort by ID for deterministic output
    let mut talkgroups: Vec<TalkgroupConfig> = map.into_values().collect();
    talkgroups.sort_by_key(|tg| tg.id);
    Ok(talkgroups)
}

// ── CSV conventional channel parsing ──

/// Parse a conventional channel CSV into a map keyed by channel ID (TG Number).
pub(crate) fn load_channels_csv(path: &Path) -> Result<HashMap<i64, ChannelConfig>> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .trim(csv::Trim::All)
        .from_path(path)
        .with_context(|| format!("failed to open channel CSV: {}", path.display()))?;

    let headers = reader.headers()?.clone();
    let mut col_tg_number = None;
    let mut col_frequency = None;
    let mut col_tone = None;
    let mut col_alpha_tag = None;
    let mut col_description = None;
    let mut col_tag = None;
    let mut col_category = None;

    for (i, h) in headers.iter().enumerate() {
        match h {
            "TG Number" => col_tg_number = Some(i),
            "Frequency" => col_frequency = Some(i),
            "Tone" => col_tone = Some(i),
            "Alpha Tag" => col_alpha_tag = Some(i),
            "Description" => col_description = Some(i),
            "Tag" => col_tag = Some(i),
            "Category" => col_category = Some(i),
            _ => {}
        }
    }

    let col_tg_number = col_tg_number.context("channel CSV missing required 'TG Number' column")?;
    let col_frequency = col_frequency.context("channel CSV missing required 'Frequency' column")?;

    let mut channels = HashMap::new();

    for result in reader.records() {
        let record = result?;
        let id_str = record.get(col_tg_number).unwrap_or("").trim();
        if id_str.is_empty() {
            continue;
        }
        let id: i64 = id_str
            .parse()
            .with_context(|| format!("invalid channel ID: {id_str:?}"))?;

        let freq: f64 = record
            .get(col_frequency)
            .unwrap_or("0")
            .trim()
            .parse()
            .unwrap_or(0.0);

        let get = |col: Option<usize>| -> String {
            col.and_then(|i| record.get(i))
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };

        let tone: f64 = col_tone
            .and_then(|i| record.get(i))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0.0);

        channels.insert(
            id,
            ChannelConfig {
                id,
                freq,
                tone,
                alpha_tag: get(col_alpha_tag),
                description: get(col_description),
                tag: get(col_tag),
                group: get(col_category),
                squelch_db: None,
                signal_detection: false,
                priority: None,
            },
        );
    }

    Ok(channels)
}

/// Apply a TOML table overlay onto a `ChannelConfig`.
fn apply_channel_overlay(ch: &mut ChannelConfig, table: &toml::Table) {
    if let Some(v) = table.get("freq").and_then(toml::Value::as_float) {
        ch.freq = v;
    }
    if let Some(v) = table.get("tone").and_then(toml::Value::as_float) {
        ch.tone = v;
    }
    if let Some(v) = table.get("alpha_tag").and_then(|v| v.as_str()) {
        ch.alpha_tag = v.to_string();
    }
    if let Some(v) = table.get("description").and_then(|v| v.as_str()) {
        ch.description = v.to_string();
    }
    if let Some(v) = table.get("tag").and_then(|v| v.as_str()) {
        ch.tag = v.to_string();
    }
    if let Some(v) = table.get("group").and_then(|v| v.as_str()) {
        ch.group = v.to_string();
    }
    if let Some(v) = table.get("squelch_db").and_then(toml::Value::as_float) {
        ch.squelch_db = Some(v);
    }
    if let Some(v) = table.get("signal_detection").and_then(toml::Value::as_bool) {
        ch.signal_detection = v;
    }
    if let Some(v) = table.get("priority").and_then(toml::Value::as_integer) {
        // An out-of-range value is a config typo: unset.
        ch.priority = i32::try_from(v).ok();
    }
}

/// Resolve channels from a CSV base (if any) with TOML overlays applied on top.
pub(crate) fn resolve_channels(
    config_dir: &Path,
    channel_file: Option<&str>,
    overlays: &[toml::Value],
) -> Result<Vec<ChannelConfig>> {
    let mut map: HashMap<i64, ChannelConfig> = match channel_file {
        Some(file) => {
            let csv_path = config_dir.join(file);
            load_channels_csv(&csv_path)?
        }
        None => HashMap::new(),
    };

    for overlay in overlays {
        let table = overlay
            .as_table()
            .context("channel entry must be a TOML table")?;

        let id = table
            .get("id")
            .and_then(toml::Value::as_integer)
            .context("channel entry missing required 'id' field")?;

        let ch = map.entry(id).or_insert_with(|| ChannelConfig {
            id,
            freq: 0.0,
            tone: 0.0,
            alpha_tag: String::new(),
            description: String::new(),
            tag: String::new(),
            group: String::new(),
            squelch_db: None,
            signal_detection: false,
            priority: None,
        });

        apply_channel_overlay(ch, table);
    }

    let mut channels: Vec<ChannelConfig> = map.into_values().collect();
    channels.sort_by_key(|ch| ch.id);
    Ok(channels)
}
