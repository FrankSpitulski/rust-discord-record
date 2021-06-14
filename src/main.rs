//! Requires the "client", "standard_framework", and "voice" features be enabled
//! in your Cargo.toml, like so:
//!
//! ```toml
//! [dependencies.serenity]
//! git = "https://github.com/serenity-rs/serenity.git"
//! features = ["client", "standard_framework", "voice"]
//! ```

mod encode;

use audiopus::{coder::Encoder as OpusEnc, Bitrate};
use chrono;
use circular_queue::CircularQueue;
use lockfree::map::Map;
use lockfree::queue::Queue;
use serenity::model::id::GuildId;
use serenity::prelude::TypeMapKey;
use serenity::{
    async_trait,
    client::{Client, Context, EventHandler},
    framework::{
        standard::{
            macros::{command, group},
            Args, CommandResult,
        },
        StandardFramework,
    },
    model::{channel::Message, gateway::Ready, id::ChannelId, misc::Mentionable},
    Result as SerenityResult,
};
use songbird::{
    driver::{Config as DriverConfig, DecodeMode},
    CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler, SerenityInit, Songbird,
};
use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

struct Handler;

const SMOOTH_BRAIN_CENTRAL: ChannelId = ChannelId(176846624432193537);
const BOT_TEST: ChannelId = ChannelId(823808752205430794);
const MEME: GuildId = GuildId(136200944709795840);

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
        join_voice_channel(&ctx, SMOOTH_BRAIN_CENTRAL, MEME, BOT_TEST).await;
    }
}

const AUDIO_FREQUENCY: u32 = 48000;
const AUDIO_CHANNELS: u8 = 2;
/// 1000 / 20 samples per second. 60 seconds in a minute. 30 minutes.
const BUFFER_SIZE: usize = (1000 / 20) * 60 * 30;
const AUDIO_PACKET_SIZE: usize = 1920; // 20ms @ 48kHz of 2ch 16 bit pcm

struct Receiver {
    buf: Arc<Mutex<CircularQueue<Vec<u8>>>>,
    user_to_packet_buffer: Arc<Map<u32, Queue<[i16; AUDIO_PACKET_SIZE]>>>,
    background_tasks: Vec<JoinHandle<()>>,
    opus_encoder: Arc<Mutex<OpusEnc>>,
}

impl Receiver {
    pub fn new() -> Self {
        let mut opus_encoder = OpusEnc::new(
            audiopus::SampleRate::Hz48000,
            audiopus::Channels::Stereo,
            audiopus::Application::Audio,
        )
        .unwrap();
        opus_encoder
            .set_bitrate(Bitrate::BitsPerSecond(24000))
            .unwrap();
        let mut receiver = Self {
            buf: Arc::new(Mutex::new(CircularQueue::with_capacity(BUFFER_SIZE))),
            user_to_packet_buffer: Arc::new(Map::new()),
            background_tasks: Vec::new(),
            opus_encoder: Arc::new(Mutex::new(opus_encoder)),
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
        self.user_to_packet_buffer
            .get(&ssrc)
            .unwrap_or_else(|| {
                self.user_to_packet_buffer.insert(ssrc, Default::default());
                self.user_to_packet_buffer.get(&ssrc).unwrap()
            })
            .val()
            .push(buf);
    }

    fn start_task_mix_packet_buffer(&mut self) {
        let user_to_packet_buffer = self.user_to_packet_buffer.clone();
        let output_buffer = self.buf.clone();
        let encoder = self.opus_encoder.clone();
        let mut interval = tokio::time::interval(Duration::from_millis(20));
        let join_handle = tokio::spawn(async move {
            loop {
                interval.tick().await;
                let mut mix_buf = [0i16; AUDIO_PACKET_SIZE];
                for packet_buffer_entry in user_to_packet_buffer.iter() {
                    for user_packet in packet_buffer_entry.val().pop_iter() {
                        if user_packet.len() != AUDIO_PACKET_SIZE {
                            println!(
                                "incorrect buffer size packet received, size {}",
                                user_packet.len()
                            );
                            continue;
                        }
                        for i in 0..AUDIO_PACKET_SIZE {
                            mix_buf[i] = mix_buf[i].saturating_add(user_packet[i]);
                        }
                    }
                }
                const MAX_PACKET: usize = 4000;
                let mut output: Vec<u8> = vec![0; MAX_PACKET];
                let result;
                {
                    result = encoder
                        .lock()
                        .unwrap()
                        .encode(&mix_buf, output.as_mut_slice())
                        .unwrap();
                }
                output.truncate(result);
                {
                    output_buffer.lock().unwrap().push(output);
                }
            }
        });
        self.background_tasks.push(join_handle);
    }

    pub async fn drain_buffer(&self) -> Vec<u8> {
        let mut packets = Vec::new();
        {
            // closure to limit lock scope
            let unlocked_reader = self.buf.lock().unwrap();
            println!("buf size before wav write {}", unlocked_reader.len());
            packets.reserve(unlocked_reader.len());
            for sample in unlocked_reader.asc_iter() {
                packets.push(sample.clone());
            }
        }
        println!("dumped circ buff");
        let ogg_data = encode::encode::<AUDIO_FREQUENCY, AUDIO_CHANNELS>(&packets)
            .expect("unable to encode pcm as ogg");
        println!("done");
        ogg_data
    }

    async fn write_ogg_to_disk(ogg_data: &[u8]) {
        let date = chrono::prelude::Local::now()
            .format("%Y-%m-%d_%H-%M-%S.ogg")
            .to_string();
        println!("writing {}", date);
        tokio::fs::write(date, &ogg_data)
            .await
            .expect("unable to write ogg file");
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
            Ctx::VoicePacket {
                audio,
                packet,
                payload_offset,
                payload_end_pad,
            } => {
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
    let token = env::var("DISCORD_TOKEN").expect("Expected a token in the environment");

    let framework = StandardFramework::new()
        .configure(|c| c.prefix("!"))
        .group(&GENERAL_GROUP);

    // Here, we need to configure Songbird to decode all incoming voice packets.
    // If you want, you can do this on a per-call basis---here, we need it to
    // read the audio data that other people are sending us!
    let songbird = Songbird::serenity();
    songbird.set_config(DriverConfig::default().decode_mode(DecodeMode::Decode));

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

    let _ = client
        .start()
        .await
        .map_err(|why| println!("Client ended: {:?}", why));
}

#[command]
#[only_in(guilds)]
async fn join(ctx: &Context, msg: &Message, mut args: Args) -> CommandResult {
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

    let guild = msg.guild(&ctx.cache).await.unwrap();
    let guild_id = guild.id;
    let response_channel = msg.channel_id;

    join_voice_channel(ctx, connect_to, guild_id, response_channel).await;

    Ok(())
}

async fn join_voice_channel(
    ctx: &Context,
    connect_to: ChannelId,
    guild_id: GuildId,
    response_channel: ChannelId,
) {
    let manager = songbird::get(ctx)
        .await
        .expect("Songbird Voice client placed in at initialisation.")
        .clone();

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
        check_msg(
            response_channel
                .say(&ctx.http, &format!("Joined {}", connect_to.mention()))
                .await,
        );
    } else {
        check_msg(
            response_channel
                .say(&ctx.http, "Error joining the channel")
                .await,
        );
    }
}

#[command]
#[only_in(guilds)]
async fn leave(ctx: &Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).await.unwrap();
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
    let ogg_file = receiver.drain_buffer().await;
    check_msg(msg.channel_id.say(&ctx.http, "domped").await);
    if msg.content.contains("file") {
        Receiver::write_ogg_to_disk(&ogg_file).await;
    }
    msg.channel_id
        .send_files(&ctx.http, vec![(ogg_file.as_slice(), "domp.ogg")], |m| {
            m.content("some audio file")
        })
        .await
        .expect("could not upload file");
    Ok(())
}
