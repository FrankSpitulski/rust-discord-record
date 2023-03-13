use crate::receiver::{write_ogg_to_disk, Receiver};
use anyhow::Context;
use async_trait::async_trait;
use serenity::framework::standard::CommandError;
use serenity::{
    client,
    client::EventHandler,
    framework::standard::{
        macros::{command, group},
        Args, CommandResult,
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
        tracing::info!("{} is connected! {} v{}", ready.user.name, env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
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
#[commands(join, leave, dump)]
struct General;

#[command]
#[only_in(guilds)]
async fn join(ctx: &client::Context, msg: &Message, mut args: Args) -> CommandResult {
    let connect_to = match args.single::<u64>() {
        Ok(id) => ChannelId(id),
        Err(_) => {
            check_msg(
                msg.reply(ctx, "Requires a valid voice channel ID be given")
                    .await,
            );

            return Ok(());
        }
    };

    let guild = msg.guild(&ctx.cache).unwrap();
    let guild_id = guild.id;
    let response_channel = msg.channel_id;

    join_voice_channel(ctx, connect_to, guild_id, response_channel)
        .await
        .map_err(CommandError::from)
}

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

    let (handler_lock, conn_result) = manager.join(guild_id, connect_to).await;

    match conn_result {
        Ok(()) => {
            // NOTE: this skips listening for the actual connection result.
            let mut handler = handler_lock.lock().await;
            let data_read = ctx.data.read().await;
            let receiver = data_read.get::<Receiver>().unwrap().clone();
            handler.add_global_event(
                CoreEvent::VoicePacket.into(),
                ArcEventHandlerInvoker { delegate: receiver },
            );
            check_msg(
                response_channel
                    .say(&ctx.http, &format!("Joined {}", connect_to.mention()))
                    .await,
            );
            Ok(())
        }
        Err(e) => {
            check_msg(
                response_channel
                    .say(&ctx.http, "Error joining the channel")
                    .await,
            );
            Err(e).context("Error joining the channel")
        }
    }
}

#[command]
#[only_in(guilds)]
async fn leave(ctx: &client::Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).unwrap();
    let guild_id = guild.id;

    let manager = songbird::get(ctx)
        .await
        .expect("Songbird Voice client placed in at initialisation.")
        .clone();
    let has_handler = manager.get(guild_id).is_some();

    if has_handler {
        if let Err(e) = manager.remove(guild_id).await {
            check_msg(
                msg.channel_id
                    .say(&ctx.http, format!("Failed: {:?}", e))
                    .await,
            );
        }

        check_msg(msg.channel_id.say(&ctx.http, "Left voice channel").await);
    } else {
        check_msg(msg.reply(ctx, "Not in a voice channel").await);
    }

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
        .send_files(&ctx.http, vec![(ogg_file.as_slice(), "domp.ogg")], |m| {
            m.content("some audio file")
        })
        .await
        .expect("could not upload file");
    Ok(())
}
