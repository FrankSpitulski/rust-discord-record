use std::time::Duration;

use audiopus::coder::Encoder;
use circular_queue::CircularQueue;
use songbird::events::context_data::VoiceTick;
use tokio::sync::Mutex;

use crate::encode;
use crate::receiver::{
    AUDIO_CHANNELS, AUDIO_FREQUENCY, AUDIO_PACKET_SIZE, empty_raw_audio, make_opus_encoder,
    MAX_OPUS_PACKET, to_raw_audio_packet,
};

const BUFFER_SIZE: usize = (1000 / 20) * 60 * 30;
/// 20ms @ 48kHz of 2ch 16 bit pcm
const PACKET_DURATION: Duration = Duration::from_millis(20);

pub struct Lookback {
    encoded_opus_buf: Mutex<CircularQueue<Vec<u8>>>,
    opus_encoder: Mutex<Encoder>, // will never actually be contested
    empty_encoded: Vec<u8>,
    output_scratch_space: Mutex<[u8; MAX_OPUS_PACKET]>,
}

impl Default for Lookback {
    fn default() -> Self {
        let opus_encoder = make_opus_encoder();
        let mut output_scratch_space = [0; MAX_OPUS_PACKET];
        let empty_encoded = {
            let empty = empty_raw_audio();
            let result = opus_encoder
                .encode(&empty, &mut output_scratch_space)
                .unwrap();
            output_scratch_space[..result].to_vec()
        };
        Self {
            encoded_opus_buf: CircularQueue::with_capacity(BUFFER_SIZE).into(),
            opus_encoder: opus_encoder.into(),
            empty_encoded,
            output_scratch_space: output_scratch_space.into(),
        }
    }
}

impl Lookback {
    pub async fn tick(&self, data: &VoiceTick) {
        let packet = if data.speaking.is_empty() {
            // early exit, empty packet
            self.empty_encoded.clone()
        } else {
            let mut mix_buf = empty_raw_audio();

            for data in data.speaking.values() {
                if let Some(audio) = &data.decoded_voice {
                    if let Some(audio) = to_raw_audio_packet(audio) {
                        for i in 0..AUDIO_PACKET_SIZE {
                            mix_buf[i] = mix_buf[i].saturating_add(audio[i]);
                        }
                    }
                }
            }

            let mut scratch_space = self.output_scratch_space.lock().await;
            self.opus_encoder
                .lock()
                .await
                .encode(&mix_buf, scratch_space.as_mut())
                .map(|written_size| scratch_space[..written_size].to_vec())
                .unwrap_or_else(|_| self.empty_encoded.clone())
        };
        self.encoded_opus_buf.lock().await.push(packet);
    }

    pub async fn drain_buffer(
        &self,
        duration_to_dump: Option<Duration>,
    ) -> anyhow::Result<Vec<u8>> {
        let mut packets = Vec::new();
        {
            // closure to limit lock scope
            let encoded_opus_buf = self.encoded_opus_buf.lock().await;
            tracing::info!("buf size before wav write {}", encoded_opus_buf.len());
            packets.reserve(encoded_opus_buf.len());
            for sample in encoded_opus_buf.asc_iter() {
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

        let ogg_data = encode::encode::<AUDIO_FREQUENCY, AUDIO_CHANNELS>(trimmed_packets)?;
        tracing::info!("done");
        Ok(ogg_data)
    }
}
