use anyhow::Result;
use serenity::all::{AutoArchiveDuration, ChannelType, CreateMessage, CreateThread, EditMessage};
use serenity::http::Http;
use serenity::model::id::{ChannelId, MessageId};

pub struct DiscordClient {
    http: Http,
}

impl DiscordClient {
    pub fn new(config: &crate::config::DiscordConfig) -> Self {
        Self {
            http: Http::new(&config.bot_token),
        }
    }

    pub async fn create_call_thread(&self, channel_id: u64, name: &str) -> Result<ChannelId> {
        let channel = ChannelId::new(channel_id);
        let thread = channel
            .create_thread(
                &self.http,
                CreateThread::new(name)
                    .kind(ChannelType::PublicThread)
                    .auto_archive_duration(AutoArchiveDuration::OneDay),
            )
            .await?;
        Ok(thread.id)
    }

    pub async fn send_message(&self, channel_id: ChannelId, text: &str) -> Result<MessageId> {
        let msg = channel_id
            .send_message(&self.http, CreateMessage::new().content(text))
            .await?;
        Ok(msg.id)
    }

    pub async fn edit_message(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        text: &str,
    ) -> Result<()> {
        channel_id
            .edit_message(&self.http, message_id, EditMessage::new().content(text))
            .await?;
        Ok(())
    }
}
