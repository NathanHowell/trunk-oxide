use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use serenity::all::{
    AutoArchiveDuration, ChannelType, CreateAttachment, CreateChannel, CreateEmbed, CreateMessage,
    CreateThread, EditAttachments, EditMessage, GuildChannel, ReactionType,
};
use serenity::http::Http;
use serenity::model::id::{ChannelId, GuildId, MessageId};

/// Hard cap on a grey-critical Discord HTTP call (embed create/edit, reactions).
/// Discord rate-limits message edits per-channel and serenity sleeps on 429s
/// with no upper bound, so a hot thread can otherwise wedge a call handler for
/// minutes (observed: `edit_embed` tail in the 250–500s range, with global
/// request rate only ~2/s — the bottleneck is the per-channel edit bucket, not
/// the global limit). Bounding these lets the handler abandon a stuck edit and
/// keep going, so the grey→green transition is never held hostage by the bucket.
const DISCORD_API_TIMEOUT: Duration = Duration::from_secs(45);

/// Longer cap for audio attachment, whichever path it takes. Safe to wait long
/// because in `on_end` the green embed edit runs *before* the attach, so the
/// embed is already finalized (not grey) by the time we get here — the attach is
/// purely best-effort and post-transition, so a generous bound only delays the
/// audio file appearing, never the grey→green flip.
// `from_mins` is not stable on our MSRV (1.94); keep `from_secs`.
#[allow(clippy::duration_suboptimal_units)]
const DISCORD_ATTACH_TIMEOUT: Duration = Duration::from_secs(180);

/// How the call audio is attached to its embed.
///
/// [`AttachMode::Edit`] puts the OGG inline on the embed (the nicer UX) via
/// `edit_message`, but that shares the per-channel *edit* rate-limit bucket with
/// the transcript edits — on a hot channel the bucket saturates and the upload
/// stalls in serenity's 429 queue until it times out, so the audio never lands.
/// [`AttachMode::Reply`] posts the audio as a separate reply on the per-channel
/// *message-create* bucket, which is independent of (and far more generous than)
/// the edit bucket, at the cost of the audio appearing as a reply rather than
/// inline. The uploader chooses per-channel from observed edit-bucket congestion
/// (see [`crate::call_handler::AttachQueue`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachMode {
    Edit,
    Reply,
}

/// Emoji used as the in-flight transcription indicator. A reaction lives in a
/// different per-channel rate-limit bucket than message edits, so toggling it
/// does not compete with (or stall behind) the embed edits.
pub const TRANSCRIBING_EMOJI: &str = "⏳";

/// Run a single grey-critical Discord HTTP call under [`DISCORD_API_TIMEOUT`].
async fn with_timeout<T>(op: &str, fut: impl Future<Output = serenity::Result<T>>) -> Result<T> {
    with_timeout_for(op, DISCORD_API_TIMEOUT, fut).await
}

/// Run a single Discord HTTP call under an explicit timeout, turning an expiry
/// into an error the caller can log and move past.
async fn with_timeout_for<T>(
    op: &str,
    timeout: Duration,
    fut: impl Future<Output = serenity::Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(timeout, fut).await {
        Ok(res) => Ok(res?),
        Err(_) => Err(anyhow::anyhow!(
            "discord {op} timed out after {}s",
            timeout.as_secs()
        )),
    }
}

pub struct DiscordClient {
    http: Http,
}

impl DiscordClient {
    pub fn new(config: &trunk_config::DiscordConfig) -> Self {
        Self {
            http: Http::new(&config.bot_token),
        }
    }

    pub async fn create_call_thread(&self, channel_id: ChannelId, name: &str) -> Result<ChannelId> {
        let thread = with_timeout(
            "create_thread",
            channel_id.create_thread(
                &self.http,
                CreateThread::new(name)
                    .kind(ChannelType::PublicThread)
                    .auto_archive_duration(AutoArchiveDuration::OneDay),
            ),
        )
        .await?;
        Ok(thread.id)
    }

    pub async fn send_message(&self, channel_id: ChannelId, text: &str) -> Result<MessageId> {
        let msg = with_timeout(
            "send_message",
            channel_id.send_message(&self.http, CreateMessage::new().content(text)),
        )
        .await?;
        Ok(msg.id)
    }

    pub async fn send_embed(&self, channel_id: ChannelId, embed: CreateEmbed) -> Result<MessageId> {
        let msg = with_timeout(
            "send_embed",
            channel_id.send_message(&self.http, CreateMessage::new().embed(embed)),
        )
        .await?;
        Ok(msg.id)
    }

    pub async fn edit_embed(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        embed: CreateEmbed,
    ) -> Result<()> {
        with_timeout(
            "edit_embed",
            channel_id.edit_message(&self.http, message_id, EditMessage::new().embed(embed)),
        )
        .await?;
        Ok(())
    }

    /// Attach the call audio to its embed, via [`AttachMode::Edit`] (inline on
    /// the embed) or [`AttachMode::Reply`] (a separate reply message). See
    /// [`AttachMode`] for why the uploader picks between them per-channel.
    ///
    /// For [`AttachMode::Edit`], `EditAttachments::new()` starts from an empty
    /// set and `.add` supplies the OGG; the embed is left untouched (only fields
    /// set on [`EditMessage`] are sent). For [`AttachMode::Reply`], referencing
    /// the embed message keeps the audio visually tied to it even when other
    /// calls' embeds land in between. Carries the longer [`DISCORD_ATTACH_TIMEOUT`]
    /// purely as a backstop.
    pub async fn attach_file(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        file_data: Vec<u8>,
        filename: &str,
        mode: AttachMode,
    ) -> Result<()> {
        let attachment = CreateAttachment::bytes(file_data, filename.to_owned());
        match mode {
            AttachMode::Edit => {
                with_timeout_for(
                    "attach_audio",
                    DISCORD_ATTACH_TIMEOUT,
                    channel_id.edit_message(
                        &self.http,
                        message_id,
                        EditMessage::new().attachments(EditAttachments::new().add(attachment)),
                    ),
                )
                .await?;
            }
            AttachMode::Reply => {
                with_timeout_for(
                    "attach_audio",
                    DISCORD_ATTACH_TIMEOUT,
                    channel_id.send_message(
                        &self.http,
                        CreateMessage::new()
                            .reference_message((channel_id, message_id))
                            .add_file(attachment),
                    ),
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Post an embed carrying an inline image attachment (the stats chart).
    /// The embed should reference the image via `attachment://{filename}`.
    /// Lives in the summary thread — a separate per-channel rate bucket from
    /// the per-call embeds — so it carries the generous attach timeout.
    pub async fn send_embed_with_image(
        &self,
        channel_id: ChannelId,
        embed: CreateEmbed,
        image: Vec<u8>,
        filename: &str,
    ) -> Result<MessageId> {
        let attachment = CreateAttachment::bytes(image, filename.to_owned());
        let msg = with_timeout_for(
            "send_embed_with_image",
            DISCORD_ATTACH_TIMEOUT,
            channel_id.send_message(
                &self.http,
                CreateMessage::new().embed(embed).add_file(attachment),
            ),
        )
        .await?;
        Ok(msg.id)
    }

    /// Edit an existing embed-with-image message, replacing the attachment with
    /// a fresh one. A new [`EditAttachments`] drops the old image; `.add`
    /// supplies the replacement (referenced by the embed via `attachment://`).
    pub async fn edit_embed_with_image(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        embed: CreateEmbed,
        image: Vec<u8>,
        filename: &str,
    ) -> Result<()> {
        let attachment = CreateAttachment::bytes(image, filename.to_owned());
        with_timeout_for(
            "edit_embed_with_image",
            DISCORD_ATTACH_TIMEOUT,
            channel_id.edit_message(
                &self.http,
                message_id,
                EditMessage::new()
                    .embed(embed)
                    .attachments(EditAttachments::new().add(attachment)),
            ),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_message(&self, channel_id: ChannelId, message_id: MessageId) -> Result<()> {
        with_timeout(
            "delete_message",
            channel_id.delete_message(&self.http, message_id),
        )
        .await?;
        Ok(())
    }

    /// Add the bot's own reaction to a message (in-flight transcription marker).
    pub async fn add_reaction(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: &str,
    ) -> Result<()> {
        let reaction = ReactionType::Unicode(emoji.to_owned());
        with_timeout(
            "add_reaction",
            self.http.create_reaction(channel_id, message_id, &reaction),
        )
        .await
    }

    /// Remove the bot's own reaction from a message.
    pub async fn remove_reaction(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        emoji: &str,
    ) -> Result<()> {
        let reaction = ReactionType::Unicode(emoji.to_owned());
        with_timeout(
            "remove_reaction",
            self.http
                .delete_reaction_me(channel_id, message_id, &reaction),
        )
        .await
    }

    pub async fn get_guild_channels(&self, guild_id: GuildId) -> Result<Vec<GuildChannel>> {
        let channels = with_timeout("get_guild_channels", guild_id.channels(&self.http)).await?;
        Ok(channels.into_values().collect())
    }

    pub async fn create_category(&self, guild_id: GuildId, name: &str) -> Result<GuildChannel> {
        let channel = with_timeout(
            "create_category",
            guild_id.create_channel(
                &self.http,
                CreateChannel::new(name).kind(ChannelType::Category),
            ),
        )
        .await?;
        Ok(channel)
    }

    pub async fn create_text_channel(
        &self,
        guild_id: GuildId,
        name: &str,
        category_id: ChannelId,
    ) -> Result<GuildChannel> {
        let channel = with_timeout(
            "create_text_channel",
            guild_id.create_channel(
                &self.http,
                CreateChannel::new(name)
                    .kind(ChannelType::Text)
                    .category(category_id),
            ),
        )
        .await?;
        Ok(channel)
    }
}
