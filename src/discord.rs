use crate::receiver::{write_ogg_to_disk, Receiver};
use async_trait::async_trait;
use serenity::all::{CreateAttachment, CreateMessage};
use serenity::{
    client,
    client::EventHandler,
    framework::standard::{
        macros::{command, group},
        CommandResult,
    },
    model::{channel::Message, gateway::Ready, id::ChannelId, id::GuildId},
    prelude::Mentionable,
    Result as SerenityResult,
};
use songbird::{CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler};
use std::process::exit;
use std::sync::Arc;

pub struct Handler {
    guild: GuildId,
    voice_channel: ChannelId,
    text_channel: ChannelId,
}

impl Handler {
    pub fn new(guild: GuildId, voice_channel: ChannelId, text_channel: ChannelId) -> Self {
        Self {
            guild,
            voice_channel,
            text_channel,
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: client::Context, ready: Ready) {
        tracing::info!(
            "{} is connected! {} v{}",
            ready.user.name,
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        );
        if let Err(e) =
            join_voice_channel(&ctx, self.voice_channel, self.guild, self.text_channel).await
        {
            tracing::error!("failed to join voice channel on startup {:?}", e);
            exit(1);
        }
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

#[group]
#[commands(dump)]
struct General;

async fn join_voice_channel(
    ctx: &client::Context,
    connect_to: ChannelId,
    guild_id: GuildId,
    response_channel: ChannelId,
) -> anyhow::Result<()> {
    let manager = songbird::get(ctx)
        .await
        .expect("Songbird Voice client placed in at initialisation.")
        .clone();

    let handler_lock = manager.join(guild_id, connect_to).await?;

    let mut handler = handler_lock.lock().await;
    let data_read = ctx.data.read().await;
    let receiver = data_read.get::<Receiver>().unwrap().clone();
    handler.add_global_event(
        CoreEvent::VoiceTick.into(),
        ArcEventHandlerInvoker { delegate: receiver },
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

#[command]
async fn dump(ctx: &client::Context, msg: &Message) -> CommandResult {
    let receiver = {
        let data_read = ctx.data.read().await;
        data_read.get::<Receiver>().unwrap().clone()
    };
    tracing::info!("received message '{}'", msg.content);
    check_msg(msg.channel_id.say(&ctx.http, "taking a dump").await);
    let args = msg.content.split_whitespace();
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
    let ogg_file = receiver.drain_buffer(drain_duration).await;
    check_msg(msg.channel_id.say(&ctx.http, "domped").await);
    if write_to_disk {
        write_ogg_to_disk(&ogg_file).await;
    }

    msg.channel_id
        .send_files(
            &ctx.http,
            [CreateAttachment::bytes(ogg_file.as_slice(), "domp.ogg")],
            CreateMessage::new().content("some audio file"),
        )
        .await
        .expect("could not upload file");
    Ok(())
}
