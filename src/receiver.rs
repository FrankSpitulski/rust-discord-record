use std::env;
use std::path::PathBuf;

use async_trait::async_trait;
use audiopus::Bitrate;
use audiopus::coder::Encoder;
use dashmap::DashMap;
use serenity::all::GuildId;
use songbird::{Event, EventContext, EventHandler as VoiceEventHandler};
use songbird::model::id::UserId;

use crate::{lookback, tts};

pub(crate) const AUDIO_FREQUENCY: u32 = 48000;
pub(crate) const AUDIO_CHANNELS: u8 = 2;

/// 20ms @ 48kHz of 2ch 16 bit pcm
pub(crate) const AUDIO_PACKET_SIZE: usize = 1920;
pub(crate) const MAX_OPUS_PACKET: usize = 4000;

pub(crate) type RawAudioPacket = [i16; AUDIO_PACKET_SIZE];

pub struct Receiver {
    ssrc_to_user: DashMap<u32, UserId>,
    user_to_ssrc: DashMap<UserId, u32>,
    pub tts: tts::Tts,
    pub guild_id: GuildId,
    pub lookback: lookback::Lookback,
}

impl Receiver {
    pub fn new(guild_id: GuildId) -> Self {
        Self {
            tts: Default::default(),
            lookback: Default::default(),
            ssrc_to_user: Default::default(),
            user_to_ssrc: Default::default(),
            guild_id,
        }
    }
}

#[async_trait]
impl VoiceEventHandler for Receiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        use songbird::EventContext as Ctx;
        match ctx {
            Ctx::VoiceTick(data) => {
                self.lookback.tick(data);

                let mut tts = self.tts.per_user_sound_buffer.write().await;
                for (ssrc, data) in &data.speaking {
                    let user = self.ssrc_to_user.get(ssrc);
                    if let Some(user) = user {
                        if let Some(audio) = &data.decoded_voice {
                            let packet = to_raw_audio_packet(audio);
                            tts.push(*user, packet);
                        } else {
                            tracing::warn!(
                                "RTP packet, but no audio. Driver may not be configured to decode."
                            );
                            tts.push(*user, None);
                        }
                    }
                }
                for ssrc in &data.silent {
                    if let Some(user) = self.ssrc_to_user.get(ssrc) {
                        tts.push(*user, None);
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
                    match self.user_to_ssrc.insert(user, speaking.ssrc) {
                        Some(prev_ssrc) if prev_ssrc != speaking.ssrc => {
                            self.ssrc_to_user.remove(&prev_ssrc);
                        }
                        _ => {}
                    }
                    self.ssrc_to_user.insert(speaking.ssrc, user);
                }
            }
            _ => {}
        }
        None
    }
}

pub async fn write_ogg_to_disk(ogg_data: &[u8]) -> anyhow::Result<()> {
    let date = chrono::prelude::Local::now()
        .format("%Y-%m-%d_%H-%M-%S.ogg")
        .to_string();
    write_ogg_to_disk_named(ogg_data, date.into()).await
}

pub async fn write_ogg_to_disk_named(ogg_data: &[u8], file_name: PathBuf) -> anyhow::Result<()> {
    let root_dir = env::var("DISCORD_AUDIO_DIR").unwrap_or_else(|_| ".".to_string());
    let ogg_path = PathBuf::from(root_dir).join(file_name);
    tracing::info!("writing {}", ogg_path.display());
    tokio::fs::write(&ogg_path, &ogg_data).await?;
    tracing::info!("done writing {}", ogg_path.display());
    Ok(())
}

pub async fn read_ogg_file(file_name: PathBuf) -> anyhow::Result<Vec<u8>> {
    let root_dir = env::var("DISCORD_AUDIO_DIR").unwrap_or_else(|_| ".".to_string());
    let ogg_path = PathBuf::from(root_dir).join(file_name);
    Ok(tokio::fs::read(ogg_path).await?)
}

pub fn user_to_ogg_file(user_id: UserId) -> PathBuf {
    format!("{}.ogg", user_id).into()
}

pub(crate) fn to_raw_audio_packet(data: impl AsRef<[i16]>) -> Option<RawAudioPacket> {
    data.as_ref().try_into().ok()
}

pub fn make_opus_encoder() -> Encoder {
    let mut opus_encoder = Encoder::new(
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
