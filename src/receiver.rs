use crate::encode;
use async_trait::async_trait;
use audiopus::coder::Encoder as OpusEnc;
use audiopus::Bitrate;
use circular_queue::CircularQueue;
use lockfree::map::Map;
use lockfree::prelude::Queue;
use serenity::prelude::TypeMapKey;
use songbird::{Event, EventContext, EventHandler as VoiceEventHandler};
use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

const AUDIO_FREQUENCY: u32 = 48000;
const AUDIO_CHANNELS: u8 = 2;
/// 1000 / 20 samples per second. 60 seconds in a minute. 30 minutes.
const BUFFER_SIZE: usize = (1000 / 20) * 60 * 30;
const AUDIO_PACKET_SIZE: usize = 1920; // 20ms @ 48kHz of 2ch 16 bit pcm

pub struct Receiver {
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
        receiver
    }

    // TODO add each packet to the master record based on an offset within the buffer
    // TODO from the exact time instead of starting every packet exactly at 20ms intervals
    pub fn add_sound(&self, ssrc: u32, data: Vec<i16>) {
        if data.len() != AUDIO_PACKET_SIZE {
            return;
        }
        let mut buf = [0i16; AUDIO_PACKET_SIZE];
        buf[..AUDIO_PACKET_SIZE].copy_from_slice(&data[..AUDIO_PACKET_SIZE]);
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
                            tracing::warn!(
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
            tracing::info!("buf size before wav write {}", unlocked_reader.len());
            packets.reserve(unlocked_reader.len());
            for sample in unlocked_reader.asc_iter() {
                packets.push(sample.clone());
            }
        }
        tracing::info!("dumped circ buff");
        let ogg_data = encode::encode::<AUDIO_FREQUENCY, AUDIO_CHANNELS>(&packets)
            .expect("unable to encode pcm as ogg");
        tracing::info!("done");
        ogg_data
    }
}

impl TypeMapKey for Receiver {
    type Value = Arc<Receiver>;
}

#[async_trait]
impl VoiceEventHandler for Receiver {
    #[allow(unused_variables)]
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        use songbird::EventContext as Ctx;
        if let Ctx::VoicePacket(data) = ctx {
            if let Some(audio) = data.audio {
                self.add_sound(data.packet.ssrc, audio.clone());
            } else {
                tracing::warn!("RTP packet, but no audio. Driver may not be configured to decode.");
            }
        }
        None
    }
}

pub async fn write_ogg_to_disk(ogg_data: &[u8]) {
    let date = chrono::prelude::Local::now()
        .format("%Y-%m-%d_%H-%M-%S.ogg")
        .to_string();
    let root_dir = env::var("DISCORD_AUDIO_DIR").unwrap_or_else(|_| ".".to_string());
    let ogg_path = PathBuf::from(root_dir).join(date);
    tracing::info!("writing {}", ogg_path.display());
    tokio::fs::write(&ogg_path, &ogg_data)
        .await
        .expect("unable to write ogg file");
    tracing::info!("done writing {}", ogg_path.display());
}
