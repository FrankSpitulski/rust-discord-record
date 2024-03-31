use crate::{encode, tts};
use async_trait::async_trait;
use audiopus::coder::{Encoder as OpusEnc, Encoder};
use audiopus::Bitrate;
use circular_queue::CircularQueue;
use dashmap::DashMap;
use songbird::model::id::UserId;
use songbird::{Event, EventContext, EventHandler as VoiceEventHandler};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

pub const AUDIO_FREQUENCY: u32 = 48000;
pub const AUDIO_CHANNELS: u8 = 2;
/// 1000 / 20 samples per second. 60 seconds in a minute. 30 minutes.
const BUFFER_SIZE: usize = (1000 / 20) * 60 * 30;
/// 20ms @ 48kHz of 2ch 16 bit pcm
pub(crate) const AUDIO_PACKET_SIZE: usize = 1920;
const PACKET_DURATION: Duration = Duration::from_millis(20);

pub(crate) const MAX_OPUS_PACKET: usize = 4000;

pub(crate) type RawAudioPacket = [i16; AUDIO_PACKET_SIZE];

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
        let mut mix_buf = empty_raw_audio();
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
    pub tts: tts::Tts,
    ssrc_to_user: DashMap<u32, UserId>,
}

impl Receiver {
    pub fn new() -> Self {
        let mut receiver = Self {
            buf: Arc::new(Mutex::new(CircularQueue::with_capacity(BUFFER_SIZE))),
            user_to_packet_buffer: Default::default(),
            background_tasks: Default::default(),
            tts: Default::default(),
            ssrc_to_user: Default::default(),
        };
        receiver.start_task_mix_packet_buffer();
        receiver
    }

    // TODO add each packet to the master record based on an offset within the buffer
    // TODO from the exact time instead of starting every packet exactly at 20ms intervals
    pub async fn add_sound_mux(&self, _ssrc: u32, packet: RawAudioPacket) {
        let packet = SortableAudioPacket {
            time: Instant::now(),
            packet,
        };
        self.user_to_packet_buffer.lock().await.push(packet);
    }

    fn start_task_mix_packet_buffer(&mut self) {
        let user_to_packet_buffer = self.user_to_packet_buffer.clone();
        let output_buffer = self.buf.clone();
        let opus_encoder = make_opus_encoder();
        let mut interval = tokio::time::interval(PACKET_DURATION);
        let join_handle = tokio::spawn(async move {
            let mut output = [0; MAX_OPUS_PACKET];
            let empty_encoded = {
                let empty = empty_raw_audio();
                let result = opus_encoder.encode(&empty, &mut output).unwrap();
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

#[async_trait]
impl VoiceEventHandler for Receiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        use songbird::EventContext as Ctx;
        match ctx {
            Ctx::VoiceTick(data) => {
                let mut tts = self.tts.per_user_sound_buffer.write().await;
                for (ssrc, data) in &data.speaking {
                    let user = self.ssrc_to_user.get(ssrc);
                    if let Some(user) = user {
                        if let Some(audio) = &data.decoded_voice {
                            let packet = to_raw_audio_packet(audio);
                            if let Some(packet) = packet {
                                self.add_sound_mux(*ssrc, packet).await;
                            }
                            tts.push(*user, packet).await;
                        } else {
                            tracing::warn!(
                                "RTP packet, but no audio. Driver may not be configured to decode."
                            );
                            tts.push(*user, None).await;
                        }
                    }
                }
                for ssrc in &data.silent {
                    if let Some(user) = self.ssrc_to_user.get(ssrc) {
                        tts.push(*user, None).await;
                    }
                }
            }
            Ctx::SpeakingStateUpdate(speaking) => {
                if let Some(user) = speaking.user_id {
                    tracing::info!(
                        "recording ssrc mapping uid {} -> ssrc {}",
                        user,
                        speaking.ssrc
                    );
                    self.ssrc_to_user.insert(speaking.ssrc, user);
                }
            }
            _ => {}
        }
        None
    }
}

pub async fn write_ogg_to_disk(ogg_data: &[u8]) {
    let date = chrono::prelude::Local::now()
        .format("%Y-%m-%d_%H-%M-%S.ogg")
        .to_string();
    write_ogg_to_disk_named(ogg_data, date.into()).await
}

pub async fn write_ogg_to_disk_named(ogg_data: &[u8], file_name: PathBuf) {
    let root_dir = env::var("DISCORD_AUDIO_DIR").unwrap_or_else(|_| ".".to_string());
    let ogg_path = PathBuf::from(root_dir).join(file_name);
    tracing::info!("writing {}", ogg_path.display());
    tokio::fs::write(&ogg_path, &ogg_data)
        .await
        .expect("unable to write ogg file");
    tracing::info!("done writing {}", ogg_path.display());
}

pub fn to_raw_audio_packet(data: impl AsRef<[i16]>) -> Option<RawAudioPacket> {
    data.as_ref().try_into().ok()
}

pub fn make_opus_encoder() -> Encoder {
    let mut opus_encoder = OpusEnc::new(
        audiopus::SampleRate::Hz48000,
        audiopus::Channels::Stereo,
        audiopus::Application::Audio,
    )
    .expect("failed to create opus encoder");
    opus_encoder
        .set_bitrate(Bitrate::BitsPerSecond(24000))
        .expect("failed to set opus encoder bitrate");
    opus_encoder
}

pub fn empty_raw_audio() -> RawAudioPacket {
    [0i16; AUDIO_PACKET_SIZE]
}
