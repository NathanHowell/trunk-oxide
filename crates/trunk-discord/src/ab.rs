//! Blind A/B listening tests in Discord: posts clips (two versions of the
//! same audio in one file, `examples/vocoder_ab.rs` in trunk-p25) to the
//! current hourly thread with ⬅️, ➡️ and 🤷 (can't tell) to vote with, and
//! tallies the votes against the key.
//!
//! `trunk-discord ab post <dir>` posts every clip the directory's
//! `key.json` lists and writes `posted.json`; `trunk-discord ab tally <dir>`
//! reads the reactions back. A clip the key pairs with its mirror (the
//! same halves the other way round) scores a listener's preference only
//! when it holds both ways round: picking the same side twice is a side,
//! not a policy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serenity::all::{
    ChannelId, CreateAttachment, CreateMessage, GuildId, Http, MessageId, ReactionType,
};

use crate::ogg_encoder::OggOpusEncoder;
use crate::resampler::Resampler;

const LEFT: &str = "\u{2b05}\u{fe0f}";
const RIGHT: &str = "\u{27a1}\u{fe0f}";
const UNSURE: &str = "\u{1f937}";
const HOURLY_THREAD_PREFIX: &str = "System Events - ";

#[derive(Subcommand)]
pub enum Ab {
    /// Post the clips of `dir` (its `key.json`) to the current hourly thread.
    Post { dir: PathBuf },
    /// Tally the votes on clips posted from `dir`.
    Tally { dir: PathBuf },
}

/// One clip as the generator describes it (the key, with the sides).
#[derive(Deserialize, Serialize)]
struct Clip {
    file: String,
    /// Which comparison it belongs to, if the key names them.
    #[serde(default)]
    test: String,
    /// The clip and its mirror share a pair.
    #[serde(default)]
    pair: Option<usize>,
    phase: u8,
    sigma: f64,
    call: usize,
    left: String,
    right: String,
    #[serde(flatten)]
    stats: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
struct Posted {
    channel: u64,
    messages: Vec<(String, u64)>,
}

pub async fn run(ab: Ab, config: &trunk_config::DiscordConfig) -> Result<()> {
    let http = Http::new(&config.bot_token);
    match ab {
        Ab::Post { dir } => post(&http, config, &dir).await,
        Ab::Tally { dir } => tally(&http, &dir).await,
    }
}

fn key(dir: &Path) -> Result<Vec<Clip>> {
    let text = std::fs::read_to_string(dir.join("key.json")).context("key.json")?;
    Ok(serde_json::from_str(&text)?)
}

/// The newest hourly thread under the feed channel, else the channel.
async fn hourly_thread(http: &Http, config: &trunk_config::DiscordConfig) -> Result<ChannelId> {
    let channel = ChannelId::new(config.channel_id.get());
    let Some(guild) = config.guild_id else {
        return Ok(channel);
    };
    let threads = GuildId::new(guild).get_active_threads(http).await?;
    Ok(threads
        .threads
        .iter()
        .filter(|t| t.parent_id == Some(channel) && t.name.starts_with(HOURLY_THREAD_PREFIX))
        .max_by_key(|t| t.id)
        .map_or(channel, |t| t.id))
}

/// A 16-bit mono WAV's samples and rate.
fn read_wav(path: &Path) -> Result<(u32, Vec<f32>)> {
    let bytes = std::fs::read(path)?;
    if bytes.len() < 44 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("{}: not a WAV", path.display());
    }
    let mut rate = 0;
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into()?) as usize;
        let body = &bytes[pos + 8..(pos + 8 + len).min(bytes.len())];
        if id == b"fmt " {
            if u16::from_le_bytes([body[0], body[1]]) != 1
                || u16::from_le_bytes([body[2], body[3]]) != 1
            {
                bail!("{}: not PCM mono", path.display());
            }
            rate = u32::from_le_bytes(body[4..8].try_into()?);
        } else if id == b"data" {
            let samples = body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| f32::from(i16::from_le_bytes(*c)) / 32768.0)
                .collect();
            return Ok((rate, samples));
        }
        pos += 8 + len + (len & 1);
    }
    bail!("{}: no data chunk", path.display())
}

fn ogg(path: &Path) -> Result<Vec<u8>> {
    let (rate, samples) = read_wav(path)?;
    let mut resampler = Resampler::new(rate)?;
    let mut encoder = OggOpusEncoder::new()?;
    encoder.process(&resampler.process(&samples)?)?;
    encoder.process(&resampler.flush()?)?;
    encoder.finish()
}

async fn post(http: &Http, config: &trunk_config::DiscordConfig, dir: &Path) -> Result<()> {
    let clips = key(dir)?;
    let channel = hourly_thread(http, config).await?;
    let mut posted = Posted {
        channel: channel.get(),
        messages: Vec::new(),
    };
    for (i, clip) in clips.iter().enumerate() {
        let text = format!(
            "**Listening test {}/{}** — phase {}, noise σ {:.1}. Two seconds of one transmission decoded two ways, one after the other with a pause between; they differ somewhere in those two seconds. React {LEFT} if the first sounds better, {RIGHT} if the second, {UNSURE} if you can't tell.",
            i + 1,
            clips.len(),
            clip.phase,
            clip.sigma,
        );
        let attachment = CreateAttachment::bytes(
            ogg(&dir.join(&clip.file))?,
            clip.file.replace(".wav", ".ogg"),
        );
        let message = channel
            .send_message(
                http,
                CreateMessage::new().content(text).add_file(attachment),
            )
            .await?;
        for emoji in [LEFT, RIGHT, UNSURE] {
            http.create_reaction(
                channel,
                message.id,
                &ReactionType::Unicode(emoji.to_owned()),
            )
            .await?;
        }
        posted.messages.push((clip.file.clone(), message.id.get()));
        println!("posted {} as {}", clip.file, message.id);
    }
    std::fs::write(
        dir.join("posted.json"),
        serde_json::to_string_pretty(&posted)?,
    )?;
    Ok(())
}

/// A listener's vote on one clip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Vote {
    Left,
    Right,
    Unsure,
}

/// Who voted what on a message (people only; both sides at once is no
/// vote).
async fn votes(
    http: &Http,
    channel: ChannelId,
    message: MessageId,
) -> Result<BTreeMap<String, Vote>> {
    let mut by: BTreeMap<String, Vec<Vote>> = BTreeMap::new();
    for (emoji, vote) in [
        (LEFT, Vote::Left),
        (RIGHT, Vote::Right),
        (UNSURE, Vote::Unsure),
    ] {
        let users = http
            .get_reaction_users(
                channel,
                message,
                &ReactionType::Unicode(emoji.to_owned()),
                100,
                None,
            )
            .await?;
        for u in users.into_iter().filter(|u| !u.bot) {
            by.entry(u.name).or_default().push(vote);
        }
    }
    Ok(by
        .into_iter()
        .filter_map(|(who, v)| match v[..] {
            [one] => Some((who, one)),
            _ => None,
        })
        .collect())
}

/// Per test and pair, each listener's choices: the policy each
/// presentation's vote chose (`None`: unsure).
type Pairs = BTreeMap<(String, usize), BTreeMap<String, Vec<Option<String>>>>;

/// What a listener's two votes on a pair say.
#[derive(Default)]
struct Verdicts {
    /// Pairs a policy won both ways round (or once, unsure the other time).
    preferred: BTreeMap<String, u64>,
    /// The same side both times.
    side: u64,
    /// Unsure both times.
    unsure: u64,
}

async fn tally(http: &Http, dir: &Path) -> Result<()> {
    let clips = key(dir)?;
    let posted: Posted = serde_json::from_str(&std::fs::read_to_string(dir.join("posted.json"))?)?;
    let channel = ChannelId::new(posted.channel);
    let mut pairs = Pairs::new();
    let mut totals: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut sides: BTreeMap<&str, u64> = BTreeMap::new();
    for (file, id) in &posted.messages {
        let Some(clip) = clips.iter().find(|c| &c.file == file) else {
            continue;
        };
        let votes = votes(http, channel, MessageId::new(*id)).await?;
        let mut line = Vec::new();
        for (who, &vote) in &votes {
            let chose = match vote {
                Vote::Left => Some(clip.left.clone()),
                Vote::Right => Some(clip.right.clone()),
                Vote::Unsure => None,
            };
            let label = chose.clone().unwrap_or_else(|| "unsure".to_owned());
            *totals
                .entry((clip.test.clone(), label.clone()))
                .or_default() += 1;
            *sides
                .entry(match vote {
                    Vote::Left => "left",
                    Vote::Right => "right",
                    Vote::Unsure => "unsure",
                })
                .or_default() += 1;
            line.push(format!("{who}={label}"));
            if let Some(pair) = clip.pair {
                pairs
                    .entry((clip.test.clone(), pair))
                    .or_default()
                    .entry(who.clone())
                    .or_default()
                    .push(chose);
            }
        }
        println!(
            "{file}: {} phase {} σ {:.1} call {} pair {:?} ({} | {}): {}",
            clip.test,
            clip.phase,
            clip.sigma,
            clip.call,
            clip.pair,
            clip.left,
            clip.right,
            line.join(" ")
        );
    }
    println!("votes by side: {sides:?}");
    println!("votes by policy: {totals:?}");
    let mut verdicts: BTreeMap<(String, String), Verdicts> = BTreeMap::new();
    for ((test, _), voters) in &pairs {
        for (who, v) in voters {
            let [a, b] = &v[..] else {
                continue;
            };
            let entry = verdicts.entry((test.clone(), who.clone())).or_default();
            match (a, b) {
                (None, None) => entry.unsure += 1,
                (Some(x), Some(y)) if x != y => entry.side += 1,
                (Some(x), _) | (None, Some(x)) => {
                    *entry.preferred.entry(x.clone()).or_default() += 1;
                }
            }
        }
    }
    for ((test, who), v) in &verdicts {
        println!(
            "{test} {who}: preferred {:?}, same side twice {}, unsure twice {}",
            v.preferred, v.side, v.unsure
        );
    }
    Ok(())
}
