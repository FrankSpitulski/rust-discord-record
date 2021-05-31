//! Requires the "client", "standard_framework", and "voice" features be enabled
//! in your Cargo.toml, like so:
//!
//! ```toml
//! [dependencies.serenity]
//! git = "https://github.com/serenity-rs/serenity.git"
//! features = ["client", "standard_framework", "voice"]
//! ```

use std::env;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use circular_queue::CircularQueue;
use lockfree::map::Map;
use lockfree::queue::Queue;
use serenity::{
    async_trait,
    client::{Client, Context, EventHandler},
    framework::{
        standard::{
            Args,
            CommandResult, macros::{command, group},
        },
        StandardFramework,
    },
    model::{
        channel::Message,
        gateway::Ready,
        id::ChannelId,
        misc::Mentionable,
    },
    Result as SerenityResult,
};
use serenity::prelude::TypeMapKey;
use songbird::{
    CoreEvent,
    driver::{Config as DriverConfig, DecodeMode},
    Event,
    EventContext,
    EventHandler as VoiceEventHandler,
    SerenityInit,
    Songbird,
};
use tokio::task::JoinHandle;

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
    }
}

/// 1 hour
const BUFFER_SIZE: usize = 2 * 48000 * 60 * 60;
const AUDIO_PACKET_SIZE: usize = 1920; // 20ms @ 48kHz of 2ch 16 bit pcm

struct Receiver {
    buf: Arc<Mutex<CircularQueue<i16>>>,
    user_to_packet_buffer: Arc<Map<u32, Queue<[i16; AUDIO_PACKET_SIZE]>>>,
    background_tasks: Vec<JoinHandle<()>>,
}

impl Receiver {
    pub fn new() -> Self {
        let mut receiver = Self {
            buf: Arc::new(Mutex::new(CircularQueue::with_capacity(BUFFER_SIZE))),
            user_to_packet_buffer: Arc::new(Map::new()),
            background_tasks: Vec::new(),
        };
        receiver.start_task_mix_packet_buffer();
        return receiver;
    }

    pub fn add_sound(&self, ssrc: u32, data: Vec<i16>) {
        if data.len() != AUDIO_PACKET_SIZE {
            return;
        }
        let mut buf = [0i16; AUDIO_PACKET_SIZE];
        for i in 0..AUDIO_PACKET_SIZE {
            buf[i] = data[i];
        }
        // the lock free map doesn't have an insert if not present.
        // it's okay to lose a few packets at the start, and even that is unlikely for one user
        // to trigger multiple packets at the same time.
        self.user_to_packet_buffer.get(&ssrc).unwrap_or_else(|| {
            self.user_to_packet_buffer.insert(ssrc, Default::default());
            self.user_to_packet_buffer.get(&ssrc).unwrap()
        })
            .val()
            .push(buf);
    }

    fn start_task_mix_packet_buffer(&mut self) {
        let user_to_packet_buffer = self.user_to_packet_buffer.clone();
        let output_buffer = self.buf.clone();
        let mut interval = tokio::time::interval(Duration::from_millis(20));
        let join_handle = tokio::spawn(async move {
            loop {
                interval.tick().await;
                let mut mix_buf = [0i16; AUDIO_PACKET_SIZE];
                for packet_buffer_entry in user_to_packet_buffer.iter() {
                    for user_packet in packet_buffer_entry.val().pop_iter() {
                        if user_packet.len() != AUDIO_PACKET_SIZE {
                            println!("incorrect buffer size packet received, size {}", user_packet.len());
                            continue;
                        }
                        for i in 0..AUDIO_PACKET_SIZE {
                            mix_buf[i] = mix_buf[i].saturating_add(user_packet[i]);
                        }
                    }
                }

                let mut circ_buf = output_buffer.lock().unwrap();
                mix_buf.iter().for_each(|sample| { circ_buf.push(*sample); });
            }
        });
        self.background_tasks.push(join_handle);
    }

    pub async fn drain_buffer(&self) {
        let mut pcm = Vec::new();
        { // closure to limit lock scope
            let unlocked_reader = self.buf.lock().unwrap();
            println!("buf size before wav write {}", unlocked_reader.len());
            pcm.reserve(unlocked_reader.len());
            for sample in unlocked_reader.asc_iter() {
                pcm.push(*sample);
            }
        }
        let ogg_data = ogg_opus::encode::<48000, 2>(&pcm).expect("unable to encode pcm as ogg");
        let path: &Path = "test.ogg".as_ref();
        tokio::fs::write(path, ogg_data).await.expect("unable to write ogg file");
        println!("done");
    }
}

impl TypeMapKey for Receiver {
    type Value = Arc<Receiver>;
}

#[async_trait]
impl VoiceEventHandler for Receiver {
    #[allow(unused_variables)]
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        use EventContext as Ctx;
        match ctx {
            Ctx::VoicePacket { audio, packet, payload_offset, payload_end_pad } => {
                if let Some(audio) = audio {
                    self.add_sound(packet.ssrc, audio.clone());
                } else {
                    println!("RTP packet, but no audio. Driver may not be configured to decode.");
                }
            }
            _ => {}
        }
        None
    }
}

struct ArcEventHandlerInvoker<T: VoiceEventHandler> {
    delegate: Arc<T>,
}

#[async_trait]
impl<T: VoiceEventHandler> VoiceEventHandler for ArcEventHandlerInvoker<T> {
    #[allow(unused_variables)]
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        self.delegate.act(ctx).await
    }
}

#[group]
#[commands(join, leave, dump)]
struct General;

#[tokio::main]
async fn main() {
    // Configure the client with your Discord bot token in the environment.
    let token = env::var("DISCORD_TOKEN")
        .expect("Expected a token in the environment");

    let framework = StandardFramework::new()
        .configure(|c| c
            .prefix("!"))
        .group(&GENERAL_GROUP);

    // Here, we need to configure Songbird to decode all incoming voice packets.
    // If you want, you can do this on a per-call basis---here, we need it to
    // read the audio data that other people are sending us!
    let songbird = Songbird::serenity();
    songbird.set_config(
        DriverConfig::default()
            .decode_mode(DecodeMode::Decode)
    );

    let mut client = Client::builder(&token)
        .event_handler(Handler)
        .framework(framework)
        .register_songbird_with(songbird.into())
        .await
        .expect("Err creating client");

    {
        let mut data = client.data.write().await;
        data.insert::<Receiver>(Arc::new(Receiver::new()));
    }

    let _ = client.start().await.map_err(|why| println!("Client ended: {:?}", why));
}

#[command]
#[only_in(guilds)]
async fn join(ctx: &Context, msg: &Message, mut args: Args) -> CommandResult {
    let connect_to = match args.single::<u64>() {
        Ok(id) => ChannelId(id),
        Err(_) => {
            check_msg(msg.reply(ctx, "Requires a valid voice channel ID be given").await);

            return Ok(());
        }
    };

    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;

    let manager = songbird::get(ctx).await
        .expect("Songbird Voice client placed in at initialisation.").clone();

    let (handler_lock, conn_result) = manager.join(guild_id, connect_to).await;

    if let Ok(_) = conn_result {
        // NOTE: this skips listening for the actual connection result.
        let mut handler = handler_lock.lock().await;
        let data_read = ctx.data.read().await;
        let receiver = data_read.get::<Receiver>().unwrap().clone();
        handler.add_global_event(
            CoreEvent::VoicePacket.into(),
            ArcEventHandlerInvoker { delegate: receiver },
        );
        check_msg(msg.channel_id.say(&ctx.http, &format!("Joined {}", connect_to.mention())).await);
    } else {
        check_msg(msg.channel_id.say(&ctx.http, "Error joining the channel").await);
    }

    Ok(())
}

#[command]
#[only_in(guilds)]
async fn leave(ctx: &Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;

    let manager = songbird::get(ctx).await
        .expect("Songbird Voice client placed in at initialisation.").clone();
    let has_handler = manager.get(guild_id).is_some();

    if has_handler {
        if let Err(e) = manager.remove(guild_id).await {
            check_msg(msg.channel_id.say(&ctx.http, format!("Failed: {:?}", e)).await);
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
        println!("Error sending message: {:?}", why);
    }
}

#[command]
async fn dump(ctx: &Context, msg: &Message) -> CommandResult {
    let receiver;
    {
        let data_read = ctx.data.read().await;
        receiver = data_read.get::<Receiver>().unwrap().clone();
    }
    check_msg(msg.channel_id.say(&ctx.http, "taking a dump").await);
    receiver.drain_buffer().await;
    check_msg(msg.channel_id.say(&ctx.http, "domped").await);

    Ok(())
}