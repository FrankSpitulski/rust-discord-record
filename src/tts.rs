use std::collections::HashMap;
use std::env;
use std::hash::BuildHasherDefault;
use std::sync::Mutex;

use audiopus::coder::Encoder;
use circular_queue::CircularQueue;
use nohash_hasher::NoHashHasher;
use songbird::model::id::UserId;
use tokio::sync::RwLock;

use crate::encode;
use crate::receiver::{
    AUDIO_CHANNELS, AUDIO_FREQUENCY, empty_raw_audio, make_opus_encoder, MAX_OPUS_PACKET,
    RawAudioPacket, read_ogg_file, user_to_ogg_file,
};

/// 1000 / 20 samples per second. 60 seconds in a minute. 2 minutes.
const BUFFER_SIZE: usize = (1000 / 20) * 60 * 2;

#[derive(Default)]
pub struct Tts {
    pub per_user_sound_buffer: RwLock<PerUserSoundBuffer>,
    client: reqwest::Client,
}

impl Tts {
    pub async fn tts(&self, user: UserId, text: String) -> anyhow::Result<bytes::Bytes> {
        let tts_host = env::var("TTS_HOST")?;
        let ogg_file = read_ogg_file(user_to_ogg_file(user)).await?;
        let file_part = reqwest::multipart::Part::bytes(ogg_file)
            .file_name("speaker.ogg")
            .mime_str("audio/ogg")?;
        let form = reqwest::multipart::Form::new()
            .part("speaker", file_part)
            .text("text", text);
        let response = self
            .client
            .post(format!("{}/tts", tts_host))
            .multipart(form)
            .send()
            .await?
            .error_for_status()?;
        Ok(response.bytes().await?)
    }
}

pub struct PerUserSoundBuffer {
    user_to_sound_packets:
        HashMap<UserId, CircularQueue<bytes::Bytes>, BuildHasherDefault<NoHashHasher<u64>>>,
    opus_encoder: Mutex<Encoder>, // will never actually be contested
    empty_encoded: bytes::Bytes,
    output_scratch_space: [u8; MAX_OPUS_PACKET],
}

impl Default for PerUserSoundBuffer {
    fn default() -> Self {
        let opus_encoder = make_opus_encoder();
        let mut output_scratch_space = [0; MAX_OPUS_PACKET];
        let empty_encoded = {
            let empty = empty_raw_audio();
            let result = opus_encoder
                .encode(&empty, &mut output_scratch_space)
                .unwrap();
            bytes::Bytes::copy_from_slice(&output_scratch_space[..result])
        };

        Self {
            user_to_sound_packets: Default::default(),
            opus_encoder: opus_encoder.into(),
            empty_encoded,
            output_scratch_space,
        }
    }
}

impl PerUserSoundBuffer {
    pub fn push(&mut self, user: UserId, data: Option<RawAudioPacket>) {
        let encoded_packet = self.encode_opus_packet(data);
        let buf = self
            .user_to_sound_packets
            .entry(user)
            .or_insert_with(|| CircularQueue::with_capacity(BUFFER_SIZE));
        buf.push(encoded_packet);
    }

    fn encode_opus_packet(&mut self, data: Option<RawAudioPacket>) -> bytes::Bytes {
        if let Some(data) = data {
            let encoded_size = self
                .opus_encoder
                .lock()
                .expect("encoded opus buf lock panicked")
                .encode(&data, &mut self.output_scratch_space);
            if let Ok(encoded_size) = encoded_size {
                return bytes::Bytes::copy_from_slice(&self.output_scratch_space[..encoded_size]);
            }
        }
        self.empty_encoded.clone()
    }

    pub fn get_ogg_buffer(&self, user: UserId) -> anyhow::Result<Vec<u8>> {
        let circular_queue = self
            .user_to_sound_packets
            .get(&user)
            .ok_or_else(|| anyhow::anyhow!("missing user registration"))?;
        let mut packets = Vec::with_capacity(circular_queue.len());
        for sample in circular_queue.asc_iter() {
            packets.push(sample.clone());
        }
        encode::encode::<AUDIO_FREQUENCY, AUDIO_CHANNELS>(&packets)
    }
}
