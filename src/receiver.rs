use crate::encode;
use async_trait::async_trait;
use audiopus::coder::Encoder as OpusEnc;
use audiopus::Bitrate;
use circular_queue::CircularQueue;
use serenity::prelude::TypeMapKey;
use songbird::{Event, EventContext, EventHandler as VoiceEventHandler};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

const AUDIO_FREQUENCY: u32 = 48000;
const AUDIO_CHANNELS: u8 = 2;
/// 1000 / 20 samples per second. 60 seconds in a minute. 30 minutes.
const BUFFER_SIZE: usize = (1000 / 20) * 60 * 30;
const AUDIO_PACKET_SIZE: usize = 1920; // 20ms @ 48kHz of 2ch 16 bit pcm

type RawAudioPacket = [i16; AUDIO_PACKET_SIZE];

#[ord_by_key::ord_eq_by_key_selector(|p| Reverse(&p.time))]
struct SortableAudioPacket {
    packet: RawAudioPacket,
    time: Instant,
}

pub struct Receiver {
    buf: Arc<Mutex<CircularQueue<Vec<u8>>>>,
    user_to_packet_buffer: Arc<Mutex<BinaryHeap<SortableAudioPacket>>>,
    background_tasks: Vec<JoinHandle<()>>,
    opus_encoder: Arc<Mutex<OpusEnc>>,
}

const TICK_RATE_MS: u64 = 20;

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
            user_to_packet_buffer: Default::default(),
            background_tasks: Vec::new(),
            opus_encoder: Arc::new(Mutex::new(opus_encoder)),
        };
        receiver.start_task_mix_packet_buffer();
        receiver
    }

    // TODO add each packet to the master record based on an offset within the buffer
    // TODO from the exact time instead of starting every packet exactly at 20ms intervals
    pub async fn add_sound(&self, _ssrc: u32, data: &[i16]) {
        if data.len() != AUDIO_PACKET_SIZE {
            return;
        }
        let packet = SortableAudioPacket {
            time: Instant::now(),
            packet: data.try_into().expect("wrong sized data"),
        };
        self.user_to_packet_buffer.lock().await.push(packet);
    }

    fn start_task_mix_packet_buffer(&mut self) {
        let user_to_packet_buffer = self.user_to_packet_buffer.clone();
        let output_buffer = self.buf.clone();
        let encoder = self.opus_encoder.clone();
        let mut interval = tokio::time::interval(Duration::from_millis(TICK_RATE_MS));
        let join_handle = tokio::spawn(async move {
            loop {
                interval.tick().await;
                let mix_buf = Self::create_mixed_raw_buffer(&user_to_packet_buffer).await;
                const MAX_PACKET: usize = 4000;
                let mut output = [0; MAX_PACKET];
                let result;
                {
                    result = encoder.lock().await.encode(&mix_buf, &mut output).unwrap();
                }
                let output = output[..result].to_vec();
                output_buffer.lock().await.push(output);
            }
        });
        self.background_tasks.push(join_handle);
    }

    async fn create_mixed_raw_buffer(
        user_to_packet_buffer: &Mutex<BinaryHeap<SortableAudioPacket>>,
    ) -> RawAudioPacket {
        let now = Instant::now();
        let mut mix_buf = [0i16; AUDIO_PACKET_SIZE];
        let mut packet_heap = user_to_packet_buffer.lock().await;
        let mut packet_pushback = vec![];
        while let Some(packet) = packet_heap.peek() {
            // packet must be at least 40ms old since we are buffering 2x the tick rate
            let start_of_buffer = now - Duration::from_millis(2 * TICK_RATE_MS);
            if packet.time >= now - Duration::from_millis(TICK_RATE_MS) {
                break;
            }
            const MS_TO_PACKET_INDEX_FACTOR: usize = AUDIO_PACKET_SIZE / TICK_RATE_MS as usize;

            let offset_in_buffer_time = start_of_buffer - packet.time;
            let offset_ms = offset_in_buffer_time.as_millis() as usize % TICK_RATE_MS as usize;
            let packet_index = offset_ms * MS_TO_PACKET_INDEX_FACTOR;

            if start_of_buffer >= packet.time {
                // start at beginning of packet and count forward
                let mix_buf_slice = &mut mix_buf[AUDIO_PACKET_SIZE - packet_index..];
                #[allow(clippy::needless_range_loop)]
                for i in 0..packet_index {
                    mix_buf_slice[i] = mix_buf_slice[i].saturating_add(packet.packet[i]);
                }
                packet_heap.pop();
            } else {
                // start at end of packet and count back
                let packet_index_start = AUDIO_PACKET_SIZE - packet_index;
                let packet_slice = &packet.packet[packet_index_start..];
                for i in 0..packet_index {
                    mix_buf[i] = mix_buf[i].saturating_add(packet_slice[i]);
                }
                // push the older packets back onto the heap so we can get their second "half" in the next buffer
                packet_pushback.push(packet_heap.pop().unwrap());
            }
        }
        packet_pushback
            .into_iter()
            .for_each(|packet| packet_heap.push(packet));
        mix_buf
    }

    pub async fn drain_buffer(&self) -> Vec<u8> {
        let mut packets = Vec::new();
        {
            // closure to limit lock scope
            let unlocked_reader = self.buf.lock().await;
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
                self.add_sound(data.packet.ssrc, audio).await;
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
