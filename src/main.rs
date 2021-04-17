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
use std::thread::sleep;
use std::time::Duration;

use circular_queue::CircularQueue;
use cpal;
use cpal::{BufferSize, Sample, SampleRate, StreamConfig};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rodio::buffer::SamplesBuffer;
use rodio::dynamic_mixer::DynamicMixerController;
use rodio::Source;
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
    model::payload::{ClientConnect, ClientDisconnect, Speaking},
    SerenityInit,
    Songbird,
};

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
    }
}

struct Receiver {
    input: Arc<DynamicMixerController<i16>>,
    buf: Arc<Mutex<CircularQueue<i16>>>,
}

impl Receiver {
    pub fn new() -> Self {
        // You can manage state here, such as a buffer of audio packet bytes so
        // you can later store them in intervals.
        const SIZE: usize = 2 * 48000 * 60; // 60 seconds
        let buf = Arc::new(Mutex::new(CircularQueue::with_capacity(SIZE)));
        let (input, mut output) = rodio::dynamic_mixer::mixer(2, 48000);

        let buf_writer_ref = buf.clone();
        tokio::spawn(async move {
            let device = cpal::default_host()
                .default_output_device().unwrap();
            let config = StreamConfig {
                channels: output.channels(),
                sample_rate: SampleRate(output.sample_rate()),
                buffer_size: BufferSize::Default,
            };
            let supported_config = device.default_output_config().unwrap();
            if supported_config.channels() != config.channels || supported_config.sample_rate() != config.sample_rate {
                panic!("supported config doesn't match")
            }
            let stream = device.build_output_stream(&supported_config.config(), move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mut unlocked_buf_writer = buf_writer_ref.lock().unwrap();
                // react to stream events and read or write stream data here.
                data.iter_mut()
                    .for_each(|x| {
                        let sample = output.next().unwrap_or(0);
                        unlocked_buf_writer.push(sample);
                        // *x=sample.to_f32(); // uncomment to play sound immediately
                    })
            }, move |err| {
                // react to errors here.
                println!("some error occurred {:?}", err);
            }).unwrap();
            stream.play().unwrap();


            // let (_stream, stream_handle) = rodio::OutputStream::try_default().unwrap();
            // stream_handle.play_raw(output.convert_samples()).expect("something broke");
            sleep(Duration::new(u64::MAX, 1_000_000_000 - 1)); // TODO replace with cond wait until Receiver Drop
        });

        Self { input, buf }
    }

    pub fn add_sound(&self, data: Vec<i16>) {
        // Receiver::play_sound(&data);
        let samples_buffer = SamplesBuffer::new(2, 48000, data);
        self.input.add(samples_buffer);
    }

    fn play_sound(data: &Vec<i16>) {
        let cloned_data = data.clone();
        tokio::spawn(async move {
            let samples_buffer = SamplesBuffer::new(2, 48000, cloned_data);
            let duration = samples_buffer.total_duration().unwrap();
            let (_stream, stream_handle) = rodio::OutputStream::try_default().unwrap();
            stream_handle.play_raw(samples_buffer.convert_samples()).expect("something broke");
            sleep(duration * 2)
        });
    }

    pub fn drain_buffer(&self) {
        let path: &Path = "test.wav".as_ref();
        let mut writer = hound::WavWriter::create(path, hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        }).unwrap();
        {
            let unlocked_reader = self.buf.lock().unwrap();
            println!("buf size before wav write {}", unlocked_reader.len());
            for sample in unlocked_reader.asc_iter() {
                writer.write_sample(*sample).unwrap();
            }
        }
        writer.finalize().unwrap();
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
            Ctx::SpeakingStateUpdate(
                Speaking { speaking, ssrc, user_id, .. }
            ) => {
                // Discord voice calls use RTP, where every sender uses a randomly allocated
                // *Synchronisation Source* (SSRC) to allow receivers to tell which audio
                // stream a received packet belongs to. As this number is not derived from
                // the sender's user_id, only Discord Voice Gateway messages like this one
                // inform us about which random SSRC a user has been allocated. Future voice
                // packets will contain *only* the SSRC.
                //
                // You can implement logic here so that you can differentiate users'
                // SSRCs and map the SSRC to the User ID and maintain this state.
                // Using this map, you can map the `ssrc` in `voice_packet`
                // to the user ID and handle their audio packets separately.
                println!(
                    "Speaking state update: user {:?} has SSRC {:?}, using {:?}",
                    user_id,
                    ssrc,
                    speaking,
                );
            }
            Ctx::SpeakingUpdate { ssrc, speaking } => {
                // You can implement logic here which reacts to a user starting
                // or stopping speaking.
                println!(
                    "Source {} has {} speaking.",
                    ssrc,
                    if *speaking { "started" } else { "stopped" },
                );
            }
            Ctx::VoicePacket { audio, packet, payload_offset, payload_end_pad } => {
                // An event which fires for every received audio packet,
                // containing the decoded data.
                if let Some(audio) = audio {
                    self.add_sound(audio.clone());
                } else {
                    println!("RTP packet, but no audio. Driver may not be configured to decode.");
                }
            }
            Ctx::RtcpPacket { packet, payload_offset, payload_end_pad } => {
                // An event which fires for every received rtcp packet,
                // containing the call statistics and reporting information.
                println!("RTCP packet received: {:?}", packet);
            }
            Ctx::ClientConnect(
                ClientConnect { audio_ssrc, video_ssrc, user_id, .. }
            ) => {
                // You can implement your own logic here to handle a user who has joined the
                // voice channel e.g., allocate structures, map their SSRC to User ID.

                println!(
                    "Client connected: user {:?} has audio SSRC {:?}, video SSRC {:?}",
                    user_id,
                    audio_ssrc,
                    video_ssrc,
                );
            }
            Ctx::ClientDisconnect(
                ClientDisconnect { user_id, .. }
            ) => {
                // You can implement your own logic here to handle a user who has left the
                // voice channel e.g., finalise processing of statistics etc.
                // You will typically need to map the User ID to their SSRC; observed when
                // speaking or connecting.

                println!("Client disconnected: user {:?}", user_id);
            }
            _ => {
                // We won't be registering this struct for any more event classes.
                unimplemented!()
            }
        }

        None
    }
}

struct ArcEventHandlerInvoker<T: VoiceEventHandler> {
    delegate: Arc<T>
}

#[async_trait]
impl<T: VoiceEventHandler> VoiceEventHandler for ArcEventHandlerInvoker<T> {
    #[allow(unused_variables)]
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        self.delegate.act(ctx).await
    }
}

#[group]
#[commands(join, leave, ping, dump)]
struct General;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

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

        // handler.add_global_event(
        //     CoreEvent::SpeakingStateUpdate.into(),
        //     Receiver::new(),
        // );

        // handler.add_global_event(
        //     CoreEvent::SpeakingUpdate.into(),
        //     Receiver::new(),
        // );
        let data_read = ctx.data.read().await;
        let receiver = data_read.get::<Receiver>().unwrap().clone();
        handler.add_global_event(
            CoreEvent::VoicePacket.into(),
            ArcEventHandlerInvoker { delegate: receiver },
        );


        // handler.add_global_event(
        //     CoreEvent::RtcpPacket.into(),
        //     Receiver::new(),
        // );

        // handler.add_global_event(
        //     CoreEvent::ClientConnect.into(),
        //     Receiver::new(),
        // );
        //
        // handler.add_global_event(
        //     CoreEvent::ClientDisconnect.into(),
        //     Receiver::new(),
        // );

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

#[command]
async fn ping(ctx: &Context, msg: &Message) -> CommandResult {
    check_msg(msg.channel_id.say(&ctx.http, "Pong!").await);

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
    receiver.drain_buffer();
    check_msg(msg.channel_id.say(&ctx.http, "domped").await);

    Ok(())
}