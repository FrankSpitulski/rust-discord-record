use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use circular_queue::CircularQueue;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::encode;
use crate::receiver::{
    AUDIO_CHANNELS, AUDIO_FREQUENCY, AUDIO_PACKET_SIZE, empty_raw_audio, make_opus_encoder,
    MAX_OPUS_PACKET, RawAudioPacket,
};

const BUFFER_SIZE: usize = (1000 / 20) * 60 * 30;
/// 20ms @ 48kHz of 2ch 16 bit pcm
const PACKET_DURATION: Duration = Duration::from_millis(20);

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

pub struct Lookback {
    buf: Arc<Mutex<CircularQueue<Vec<u8>>>>,
    user_to_packet_buffer: Arc<Mutex<BufferedPacketSource>>,
    background_tasks: Vec<JoinHandle<()>>,
}

impl Default for Lookback {
    fn default() -> Self {
        let mut receiver = Self {
            buf: Arc::new(Mutex::new(CircularQueue::with_capacity(BUFFER_SIZE))),
            user_to_packet_buffer: Default::default(),
            background_tasks: Default::default(),
        };
        receiver.start_task_mix_packet_buffer();
        receiver
    }
}

impl Lookback {
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
