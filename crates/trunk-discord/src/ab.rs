//! Blind A/B listening tests in Discord: posts clips (two versions of the
//! same audio in one file, `examples/vocoder_ab.rs` in trunk-p25) to the
//! current hourly thread with ⬅️ and ➡️ to vote with, and tallies the votes
//! against the key.
//!
//! `trunk-discord ab post <dir>` posts every clip the directory's
//! `key.json` lists and writes `posted.json`; `trunk-discord ab tally <dir>`
//! reads the reactions back.

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
    phase: u8,
    sigma: f64,
    call: usize,
    why: String,
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
            "**Listening test {}/{}** — phase {}, noise σ {:.1}. The same transmission decoded two ways, one after the other with a pause between. React {LEFT} if the first sounds better, {RIGHT} if the second.",
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
        for emoji in [LEFT, RIGHT] {
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

async fn tally(http: &Http, dir: &Path) -> Result<()> {
    let clips = key(dir)?;
    let posted: Posted = serde_json::from_str(&std::fs::read_to_string(dir.join("posted.json"))?)?;
    let channel = ChannelId::new(posted.channel);
    let mut totals = std::collections::BTreeMap::<String, u64>::new();
    for (file, id) in &posted.messages {
        let Some(clip) = clips.iter().find(|c| &c.file == file) else {
            continue;
        };
        let message = http.get_message(channel, MessageId::new(*id)).await?;
        // Less the bot's own reaction on each side.
        let votes = |emoji: &str| {
            message
                .reactions
                .iter()
                .find(|r| r.reaction_type == ReactionType::Unicode(emoji.to_owned()))
                .map_or(0, |r| r.count.saturating_sub(u64::from(r.me)))
        };
        let (l, r) = (votes(LEFT), votes(RIGHT));
        *totals.entry(clip.left.clone()).or_default() += l;
        *totals.entry(clip.right.clone()).or_default() += r;
        println!(
            "{file}: phase {} σ {:.1} ({}): {LEFT} {l} = {}, {RIGHT} {r} = {}",
            clip.phase, clip.sigma, clip.why, clip.left, clip.right
        );
    }
    println!("totals: {totals:?}");
    Ok(())
}
