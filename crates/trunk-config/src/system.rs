use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SystemType {
    Conventional,
    ConventionalP25,
    ConventionalDMR,
    #[serde(rename = "conventionalSIGMF")]
    ConventionalSigmf,
    Smartnet,
    P25,
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
    #[serde(default = "default_deemphasis_tau")]
    pub deemphasis_tau: f32,
    #[serde(default = "default_max_dev")]
    pub max_dev: i32,
    #[serde(default = "default_filter_width")]
    pub filter_width: f64,
    #[serde(default = "default_true")]
    pub conversation_mode: bool,
    #[serde(default = "default_true")]
    pub record_unknown: bool,

    // Talkgroup display (for C++ console logging)
    #[serde(default)]
    pub talkgroup_display_format: TalkgroupDisplayFormat,

    // Talkgroups — CSV file (RadioReference export) with optional TOML overrides
    #[serde(default)]
    pub talkgroup_file: Option<String>,

    // Talkgroups (inline)
    #[serde(default)]
    pub talkgroup: Vec<TalkgroupConfig>,

    // Unit tags
    #[serde(default)]
    pub unit_tag: Vec<UnitTagEntry>,
    #[serde(default)]
    pub unit_tags: Option<UnitTagsSettings>,

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
    #[serde(default)]
    pub bandplan_base: f64,
    #[serde(default)]
    pub bandplan_high: f64,
    #[serde(default)]
    pub bandplan_spacing: f64,
    #[serde(default)]
    pub bandplan_offset: i32,

    // Filtering
    #[serde(default)]
    pub hide_encrypted: bool,
    #[serde(default)]
    pub monitor_encrypted: bool,
    #[serde(default)]
    pub hide_unknown_talkgroups: bool,

    // Duration limits
    #[serde(default)]
    pub min_duration: f64,
    #[serde(default)]
    pub max_duration: f64,
    #[serde(default)]
    pub min_transmission_duration: f64,

    // Multi-site
    #[serde(default)]
    pub multi_site: bool,
    #[serde(default)]
    pub multi_site_system_name: String,
    #[serde(default)]
    pub multi_site_system_number: i32,

    // Decoder options
    #[serde(default)]
    pub decode_mdc: bool,
    #[serde(default)]
    pub decode_fsync: bool,
    #[serde(default)]
    pub decode_star: bool,
    #[serde(default)]
    pub decode_tps: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TalkgroupDisplayFormat {
    #[default]
    Id,
    IdTag,
    TagId,
}

/// Talkgroup entry for trunked systems.
/// Descriptive fields stay in Rust for enrichment.
/// Operational fields are passed to C++ for recording decisions.
#[derive(Debug, Clone, Deserialize)]
pub struct TalkgroupConfig {
    pub id: i64,
    #[serde(default = "default_talkgroup_mode")]
    pub mode: String,
    #[serde(default)]
    pub alpha_tag: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub preferred_nac: u64,
}

fn default_talkgroup_mode() -> String {
    "D".to_string()
}

/// Channel entry for conventional systems.
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelConfig {
    pub id: i64,
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
    #[serde(default = "default_squelch")]
    pub squelch_db: f64,
    #[serde(default)]
    pub signal_detection: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnitTagEntry {
    pub pattern: String,
    pub tag: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UnitTagsSettings {
    #[serde(default = "default_unit_tags_mode")]
    pub mode: String,
}

fn default_unit_tags_mode() -> String {
    "user".to_string()
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
const fn default_deemphasis_tau() -> f32 {
    0.000_750
}
const fn default_max_dev() -> i32 {
    5000
}
const fn default_filter_width() -> f64 {
    1.0
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

    let col_decimal =
        col_decimal.context("talkgroup CSV missing required 'Decimal' column")?;

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
                default_talkgroup_mode()
            } else {
                m
            }
        };

        let priority: i32 = col_priority
            .and_then(|i| record.get(i))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

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
        tg.mode = v.to_string();
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
    if let Some(v) = table.get("priority").and_then(|v| v.as_integer()) {
        tg.priority = v as i32;
    }
    if let Some(v) = table.get("preferred_nac").and_then(|v| v.as_integer()) {
        tg.preferred_nac = v as u64;
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
            .and_then(|v| v.as_integer())
            .context("talkgroup entry missing required 'id' field")?;

        let tg = map.entry(id).or_insert_with(|| TalkgroupConfig {
            id,
            mode: default_talkgroup_mode(),
            alpha_tag: String::new(),
            description: String::new(),
            tag: String::new(),
            group: String::new(),
            priority: 0,
            preferred_nac: 0,
        });

        apply_overlay(tg, table);
    }

    // Sort by ID for deterministic output
    let mut talkgroups: Vec<TalkgroupConfig> = map.into_values().collect();
    talkgroups.sort_by_key(|tg| tg.id);
    Ok(talkgroups)
}
