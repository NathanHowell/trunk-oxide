use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct DiscordConfig {
    #[serde(default)]
    pub bot_token: String,
    pub channel_id: NonZeroU64,
    #[serde(default)]
    pub guild_id: Option<u64>,
    /// Base URL of the Jaeger UI (`http://host:16686`): a call's embed title
    /// links to its trace there. Unset: no links.
    #[serde(default)]
    pub jaeger_url: Option<String>,
    #[serde(default)]
    pub groups: Vec<ChannelGroup>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelGroup {
    pub name: String,
    #[serde(default)]
    pub id: Option<NonZeroU64>,
    pub channels: Vec<ChannelMapping>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelMapping {
    pub name: String,
    #[serde(default)]
    pub id: Option<NonZeroU64>,
    pub talkgroups: Vec<TalkgroupSelector>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TalkgroupSelector {
    pub system: String,
    pub ids: Vec<i64>,
}

/// A non-zero u64 that rejects 0 during deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NonZeroU64(std::num::NonZeroU64);

impl NonZeroU64 {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl std::fmt::Display for NonZeroU64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for NonZeroU64 {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let id = u64::deserialize(d)?;
        std::num::NonZeroU64::new(id)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("value must be non-zero"))
    }
}
