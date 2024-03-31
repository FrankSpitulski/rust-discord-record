use crate::receiver::{user_to_ogg_file, write_ogg_to_disk, write_ogg_to_disk_named, Receiver};
use anyhow::{anyhow, Error};
use async_trait::async_trait;
use poise::CreateReply;
use serenity::all::CreateAttachment;
use serenity::{
    client,
    model::{channel::Message, gateway::Ready, id::ChannelId, id::GuildId},
    prelude::Mentionable,
    Result as SerenityResult,
};
use songbird::input::core::io::MediaSource;
use songbird::input::core::probe::Hint;
use songbird::input::{AudioStream, Input, LiveInput};
use songbird::model::id::UserId;
use songbird::{CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler};
use std::io;
use std::sync::Arc;

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

    check_msg(
        response_channel
            .say(&ctx.http, &format!("Joined {}", connect_to.mention()))
            .await,
    );
    Ok(())
}

/// Checks that a message successfully sent; if not, then logs why to stdout.
fn check_msg(result: SerenityResult<Message>) {
    if let Err(why) = result {
        tracing::error!("Error sending message: {:?}", why);
    }
}

#[poise::command(prefix_command, slash_command)]
pub async fn dump(ctx: Context<'_>, command: Option<String>) -> Result<(), Error> {
    let command = command.unwrap_or_default();
    tracing::info!("received message '{}'", command);
    ctx.say("taking a dump").await?;
    let args = command.split_whitespace();
    let mut write_to_disk = false;
    let mut drain_duration = None;
    for arg in args {
        match arg {
            "file" => {
                write_to_disk = true;
            }
            arg => {
                if drain_duration.is_none() {
                    if let Ok(duration) = humantime::parse_duration(arg) {
                        drain_duration = Some(duration);
                    }
                }
            }
        }
    }
    let receiver = ctx.data();
    let ogg_file: Vec<u8> = receiver.drain_buffer(drain_duration).await;
    ctx.say("domped").await?;
    if write_to_disk {
        write_ogg_to_disk(&ogg_file).await?;
    }
    ctx.send(
        CreateReply::default()
            .content("some audio file")
            .attachment(CreateAttachment::bytes(ogg_file, "domp.ogg")),
    )
    .await?;
    Ok(())
}

#[poise::command(prefix_command, slash_command)]
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

#[poise::command(prefix_command, slash_command)]
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
        hint.mime_type("audio/ogg").with_extension("ogg");
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
