use crate::receiver::{write_ogg_to_disk, Receiver};
use anyhow::Error;
use async_trait::async_trait;
use poise::{Context, CreateReply};
use serenity::all::CreateAttachment;
use serenity::{
    client,
    model::{channel::Message, gateway::Ready, id::ChannelId, id::GuildId},
    prelude::Mentionable,
    Result as SerenityResult,
};
use songbird::{CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler};
use std::process::exit;
use std::sync::Arc;

pub async fn on_ready(
    ctx: &client::Context,
    ready: &Ready,
    guild: GuildId,
    voice_channel: ChannelId,
    text_channel: ChannelId,
    receiver: Arc<Receiver>,
) {
    tracing::info!(
        "{} is connected! {} v{}",
        ready.user.name,
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    if let Err(e) = join_voice_channel(ctx, voice_channel, guild, text_channel, receiver).await {
        tracing::error!("failed to join voice channel on startup {:?}", e);
        exit(1);
    }
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
        .expect("Songbird Voice client placed in at initialisation.")
        .clone();

    let handler_lock = manager.join(guild_id, connect_to).await?;

    let mut handler = handler_lock.lock().await;
    handler.add_global_event(
        CoreEvent::VoiceTick.into(),
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

#[poise::command(prefix_command)]
pub async fn dump(
    ctx: Context<'_, Arc<Receiver>, Error>,
    command: Option<String>,
) -> Result<(), Error> {
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
        write_ogg_to_disk(&ogg_file).await;
    }
    ctx.send(
        CreateReply::default()
            .content("some audio file")
            .attachment(CreateAttachment::bytes(ogg_file, "domp.ogg")),
    )
    .await?;
    Ok(())
}
