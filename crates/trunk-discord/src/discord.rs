use anyhow::Result;
use serenity::all::{
    AutoArchiveDuration, ChannelType, CreateChannel, CreateEmbed, CreateMessage, CreateThread,
    EditMessage, GuildChannel,
};
use serenity::http::Http;
use serenity::model::id::{ChannelId, GuildId, MessageId};

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
        let thread = channel_id
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

    pub async fn send_embed(&self, channel_id: ChannelId, embed: CreateEmbed) -> Result<MessageId> {
        let msg = channel_id
            .send_message(&self.http, CreateMessage::new().embed(embed))
            .await?;
        Ok(msg.id)
    }

    pub async fn edit_embed(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        embed: CreateEmbed,
    ) -> Result<()> {
        channel_id
            .edit_message(&self.http, message_id, EditMessage::new().embed(embed))
            .await?;
        Ok(())
    }

    pub async fn delete_message(&self, channel_id: ChannelId, message_id: MessageId) -> Result<()> {
        channel_id.delete_message(&self.http, message_id).await?;
        Ok(())
    }

    pub async fn get_guild_channels(&self, guild_id: GuildId) -> Result<Vec<GuildChannel>> {
        let channels = guild_id.channels(&self.http).await?;
        Ok(channels.into_values().collect())
    }

    pub async fn create_category(&self, guild_id: GuildId, name: &str) -> Result<GuildChannel> {
        let channel = guild_id
            .create_channel(
                &self.http,
                CreateChannel::new(name).kind(ChannelType::Category),
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
        let channel = guild_id
            .create_channel(
                &self.http,
                CreateChannel::new(name)
                    .kind(ChannelType::Text)
                    .category(category_id),
            )
            .await?;
        Ok(channel)
    }
}
