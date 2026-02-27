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
