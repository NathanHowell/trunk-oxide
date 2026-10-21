//! The Discord message the dashboard lives in.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use serenity::all::{
    CreateAttachment, CreateEmbed, CreateEmbedFooter, CreateMessage, EditAttachments, EditMessage,
    GetMessages,
};
use serenity::http::Http;
use serenity::model::id::{ChannelId, MessageId, UserId};
use tokio::sync::OnceCell;

use crate::dashboard::Board;

/// Filename the embed references via `attachment://`.
const FILENAME: &str = "stats.png";
/// What every dashboard message's embed title begins with: how a restart
/// finds the message it left.
const TITLE: &str = "📊 Monitoring";
/// How far back in the channel to look for it.
const LOOK_BACK: u8 = 50;
/// Hard cap on one call. Discord rate-limits per channel and serenity sleeps
/// on a 429 with no bound, so without one a call could hold the loop for ever.
// `from_mins` is not stable on our MSRV (1.94); keep `from_secs`.
#[allow(clippy::duration_suboptimal_units)]
const TIMEOUT: Duration = Duration::from_secs(180);

async fn bounded<T>(what: &str, call: impl Future<Output = serenity::Result<T>>) -> Result<T> {
    tokio::time::timeout(TIMEOUT, call)
        .await
        .map_err(|_| anyhow::anyhow!("discord {what} timed out after {}s", TIMEOUT.as_secs()))?
        .with_context(|| format!("discord {what}"))
}

/// The embed title.
fn title() -> String {
    format!("{TITLE} — last 6 h (UTC)")
}

/// Whether `title` is a dashboard's, as [`title`] makes it.
fn is_dashboard(title: &str) -> bool {
    title.starts_with(TITLE)
}

/// The embed wrapping the chart. Discord renders the attached PNG inline.
fn embed() -> CreateEmbed {
    let updated = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
    CreateEmbed::new()
        .title(title())
        .colour(0x0034_98db)
        .image(format!("attachment://{FILENAME}"))
        .footer(CreateEmbedFooter::new(format!("updated {updated}")))
}

/// The dashboard message in one channel.
pub struct Channel {
    http: Http,
    id: ChannelId,
    me: OnceCell<UserId>,
}

impl Channel {
    pub fn new(bot_token: &str, id: ChannelId) -> Self {
        Self {
            http: Http::new(bot_token),
            id,
            me: OnceCell::new(),
        }
    }

    async fn me(&self) -> Result<UserId> {
        self.me
            .get_or_try_init(|| async {
                Ok(bounded("get_current_user", self.http.get_current_user())
                    .await?
                    .id)
            })
            .await
            .copied()
    }
}

impl Board for Channel {
    /// The newest dashboard message this bot has left in the channel.
    async fn find_previous(&self) -> Result<Option<MessageId>> {
        let me = self.me().await?;
        let recent = bounded(
            "get_messages",
            self.id
                .messages(&self.http, GetMessages::new().limit(LOOK_BACK)),
        )
        .await?;
        // newest first
        Ok(recent
            .iter()
            .find(|m| {
                m.author.id == me
                    && m.embeds
                        .first()
                        .and_then(|e| e.title.as_deref())
                        .is_some_and(is_dashboard)
            })
            .map(|m| m.id))
    }

    async fn post(&self, png: Vec<u8>) -> Result<MessageId> {
        let attachment = CreateAttachment::bytes(png, FILENAME);
        let message = bounded(
            "send_message",
            self.id.send_message(
                &self.http,
                CreateMessage::new().embed(embed()).add_file(attachment),
            ),
        )
        .await?;
        Ok(message.id)
    }

    /// Replaces the embed and its picture: a new [`EditAttachments`] drops the
    /// old image, and `add` supplies the new one.
    async fn edit(&self, id: MessageId, png: Vec<u8>) -> Result<()> {
        let attachment = CreateAttachment::bytes(png, FILENAME);
        bounded(
            "edit_message",
            self.id.edit_message(
                &self.http,
                id,
                EditMessage::new()
                    .embed(embed())
                    .attachments(EditAttachments::new().add(attachment)),
            ),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restart_recognises_the_title_it_posted() {
        assert!(is_dashboard(&title()));
        assert!(!is_dashboard("trunk-discord is now online."));
        assert!(!is_dashboard("Recorders"));
    }
}
