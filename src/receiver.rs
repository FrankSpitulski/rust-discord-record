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
/// 20ms @ 48kHz of 2ch 16 bit pcm
const AUDIO_PACKET_SIZE: usize = 1920;
const PACKET_DURATION: Duration = Duration::from_millis(20);

type RawAudioPacket = [i16; AUDIO_PACKET_SIZE];

#[ord_by_key::ord_eq_by_key_selector(| p | Reverse(& p.time))]
pub struct SortableAudioPacket {
    packet: RawAudioPacket,
    time: Instant,
}

#[derive(Default)]
pub struct BufferedPacketSource {
    packet_buffer: BinaryHeap<SortableAudioPacket>,
}

impl BufferedPacketSource {
    fn peek(&self, now: Instant) -> Option<&SortableAudioPacket> {
        match self.packet_buffer.peek() {
            // buffer the last two packets worth of data in case more come in
            Some(packet) if packet.time >= now - (PACKET_DURATION * 2) => None,
            Some(packet) => Some(packet),
            None => None,
        }
    }

    fn pop(&mut self, now: Instant) -> Option<SortableAudioPacket> {
        self.peek(now)?;
        self.packet_buffer.pop()
    }

    pub fn push(&mut self, packet: SortableAudioPacket) {
        self.packet_buffer.push(packet)
    }

    fn create_mixed_raw_buffer(&mut self) -> Option<RawAudioPacket> {
        let now = Instant::now();
        self.peek(now)?; // check for early exit if there are no voice packets
        let mut mix_buf = [0i16; AUDIO_PACKET_SIZE];
        while let Some(sortable_packet) = self.pop(now) {
            // TODO split by exact time offset
            #[allow(clippy::needless_range_loop)]
            for i in 0..AUDIO_PACKET_SIZE {
                mix_buf[i] = mix_buf[i].saturating_add(sortable_packet.packet[i]);
            }
        }
        Some(mix_buf)
    }
}

pub struct Receiver {
    buf: Arc<Mutex<CircularQueue<Vec<u8>>>>,
    user_to_packet_buffer: Arc<Mutex<BufferedPacketSource>>,
    background_tasks: Vec<JoinHandle<()>>,
}

impl Receiver {
    pub fn new() -> Self {
        let mut receiver = Self {
            buf: Arc::new(Mutex::new(CircularQueue::with_capacity(BUFFER_SIZE))),
            user_to_packet_buffer: Default::default(),
            background_tasks: Default::default(),
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
        let mut opus_encoder = OpusEnc::new(
            audiopus::SampleRate::Hz48000,
            audiopus::Channels::Stereo,
            audiopus::Application::Audio,
        ).unwrap();
        opus_encoder
            .set_bitrate(Bitrate::BitsPerSecond(24000))
            .unwrap();
        let mut interval = tokio::time::interval(PACKET_DURATION);
        let join_handle = tokio::spawn(async move {
            const MAX_PACKET: usize = 4000;
            let mut output = [0; MAX_PACKET];
            let empty_encoded = {
                let result = opus_encoder.encode(&[0i16; AUDIO_PACKET_SIZE], &mut output).unwrap();
                output[..result].to_vec()
            };

            loop {
                interval.tick().await;
                let mix_buf = { user_to_packet_buffer.lock().await.create_mixed_raw_buffer() };
                let encoded_vec = match mix_buf {
                    None => empty_encoded.clone(),
                    Some(mix_buf) => {
                        let result = opus_encoder.encode(&mix_buf, &mut output).unwrap();
                        output[..result].to_vec()
                    }
                };
                output_buffer.lock().await.push(encoded_vec);
            }
        });
        self.background_tasks.push(join_handle);
    }

    pub async fn drain_buffer(&self, duration_to_dump: Option<Duration>) -> Vec<u8> {
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

        let trimmed_packets = if let Some(duration_to_dump) = duration_to_dump {
            let packets_to_dump =
                (duration_to_dump.as_millis() / PACKET_DURATION.as_millis()) as usize;
            let packets_start_index = packets.len().saturating_sub(packets_to_dump);
            &packets[packets_start_index..]
        } else {
            &packets
        };

        let ogg_data = encode::encode::<AUDIO_FREQUENCY, AUDIO_CHANNELS>(trimmed_packets)
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
        if let Ctx::VoiceTick(data) = ctx {
            for (ssrc, data) in &data.speaking {
                if let Some(audio) = &data.decoded_voice {
                    self.add_sound(*ssrc, audio).await;
                } else {
                    tracing::warn!("RTP packet, but no audio. Driver may not be configured to decode.");
                }
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
