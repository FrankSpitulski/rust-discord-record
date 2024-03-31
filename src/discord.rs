use std::io;
use std::sync::Arc;

use anyhow::{anyhow, Error};
use async_trait::async_trait;
use poise::CreateReply;
use serenity::{
    client,
    model::{gateway::Ready, id::ChannelId, id::GuildId},
    prelude::Mentionable,
};
use serenity::all::CreateAttachment;
use songbird::{CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler};
use songbird::input::{AudioStream, Input, LiveInput};
use songbird::input::core::io::MediaSource;
use songbird::input::core::probe::Hint;
use songbird::model::id::UserId;

use crate::receiver::{Receiver, user_to_ogg_file, write_ogg_to_disk, write_ogg_to_disk_named};

type Context<'a> = poise::Context<'a, Arc<Receiver>, Error>;

pub async fn on_ready(
    ctx: &client::Context,
    ready: &Ready,
    guild: GuildId,
    voice_channel: ChannelId,
    text_channel: ChannelId,
    receiver: Arc<Receiver>,
) -> anyhow::Result<()> {
    tracing::info!(
        "{} is connected! {} v{}",
        ready.user.name,
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    join_voice_channel(ctx, voice_channel, guild, text_channel, receiver)
        .await
        .map_err(|e| {
            tracing::error!("failed to join voice channel on startup {:?}", e);
            e
        })
}

struct ArcEventHandlerInvoker<T: VoiceEventHandler> {
    delegate: Arc<T>,
}

#[async_trait]
impl<T: VoiceEventHandler> VoiceEventHandler for ArcEventHandlerInvoker<T> {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        self.delegate.act(ctx).await
    }
}

async fn join_voice_channel(
    ctx: &client::Context,
    connect_to: ChannelId,
    guild_id: GuildId,
    response_channel: ChannelId,
    receiver: Arc<Receiver>,
) -> anyhow::Result<()> {
    let manager = songbird::get(ctx)
        .await
        .ok_or_else(|| anyhow!("Songbird Voice client placed in at initialisation."))?;

    let handler_lock = manager.join(guild_id, connect_to).await?;

    let mut handler = handler_lock.lock().await;
    handler.add_global_event(
        CoreEvent::VoiceTick.into(),
        ArcEventHandlerInvoker {
            delegate: receiver.clone(),
        },
    );
    handler.add_global_event(
        CoreEvent::SpeakingStateUpdate.into(),
        ArcEventHandlerInvoker {
            delegate: receiver.clone(),
        },
    );

    response_channel
        .say(&ctx.http, &format!("Joined {}", connect_to.mention()))
        .await?;
    Ok(())
}

#[poise::command(slash_command)]
pub async fn dump(
    ctx: Context<'_>,
    duration: Option<String>,
    write_to_disk: Option<bool>,
) -> Result<(), Error> {
    let write_to_disk = write_to_disk.unwrap_or(false);
    tracing::info!("dumping to disk '{}'", write_to_disk);
    ctx.say("dumping").await?;
    let drain_duration = match duration {
        Some(duration) => humantime::parse_duration(&duration).ok(),
        _ => None,
    };

    let receiver = ctx.data();
    let ogg_file = receiver.lookback.drain_buffer(drain_duration).await;
    ctx.say("dumped").await?;
    if write_to_disk {
        write_ogg_to_disk(&ogg_file).await?;
    }
    ctx.send(
        CreateReply::default()
            .content("some audio file")
            .attachment(CreateAttachment::bytes(ogg_file, "dump.ogg")),
    )
    .await?;
    Ok(())
}

#[poise::command(slash_command)]
pub async fn clone(ctx: Context<'_>, user: poise::serenity_prelude::User) -> Result<(), Error> {
    tracing::info!("cloning last 2m of voice for user '{}'", user);
    ctx.say(format!("cloning last 2m of voice for user '{}'", user))
        .await?;
    let receiver = ctx.data();

    let user_id = UserId(user.id.get());
    let ogg_file = receiver
        .tts
        .per_user_sound_buffer
        .read()
        .await
        .get_ogg_buffer(user_id)?;

    write_ogg_to_disk_named(&ogg_file, user_to_ogg_file(user_id)).await?;
    ctx.say("finished cloning").await?;
    Ok(())
}

#[poise::command(slash_command)]
pub async fn ctts(
    ctx: Context<'_>,
    user: poise::serenity_prelude::User,
    text: String,
) -> Result<(), Error> {
    tracing::info!("tts for user '{}': {}", user, text);
    ctx.say("working on tts").await?;

    let receiver = ctx.data();
    let user_id = UserId(user.id.get());
    let ogg_output = receiver.tts.tts(user_id, text).await?;

    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or_else(|| anyhow!("Songbird Voice client placed in at initialisation."))?;

    if let Some(handler_lock) = manager.get(receiver.guild_id) {
        let mut handler = handler_lock.lock().await;
        let mut hint = Hint::default();
        hint.mime_type("audio/wav").with_extension("wav");
        let audio_stream: AudioStream<Box<dyn MediaSource>> = AudioStream {
            input: Box::new(io::Cursor::new(ogg_output)),
            hint: Some(hint),
        };
        let input = Input::Live(LiveInput::Raw(audio_stream), None);
        let _ = handler.play_input(input);
    }
    ctx.say("finished tts").await?;
    Ok(())
}
