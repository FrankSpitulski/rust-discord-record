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

const COOL_FRUIT_TALK: ChannelId = ChannelId(176846624432193537);
const BOT_TEST: ChannelId = ChannelId(823808752205430794);
const FRUIT_BANANZA: GuildId = GuildId(136200944709795840);

pub struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: client::Context, ready: Ready) {
        tracing::info!("{} is connected!", ready.user.name);
        if let Err(e) = join_voice_channel(&ctx, COOL_FRUIT_TALK, FRUIT_BANANZA, BOT_TEST).await {
            tracing::error!("failed to join smooth brain channel on startup {:?}", e);
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
    let receiver;
    {
        let data_read = ctx.data.read().await;
        receiver = data_read.get::<Receiver>().unwrap().clone();
    }
    check_msg(msg.channel_id.say(&ctx.http, "taking a dump").await);
    let ogg_file = receiver.drain_buffer().await;
    check_msg(msg.channel_id.say(&ctx.http, "domped").await);
    if msg.content.contains("file") {
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
