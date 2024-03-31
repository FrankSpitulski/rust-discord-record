#![warn(clippy::all)]
#![deny(warnings)]

use std::env;
use std::sync::Arc;

use anyhow::Context;
use serenity::client::Client;
use serenity::model::id::{ChannelId, GuildId};
use serenity::prelude::GatewayIntents;
use songbird::{Config, driver::DecodeMode, SerenityInit};
#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

use receiver::Receiver;

mod discord;
mod encode;
mod receiver;
mod tts;
mod lookback;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Configure the client with your Discord bot token in the environment.
    let token = env::var("DISCORD_TOKEN").expect("Expected a token in the environment");
    let guild_id = env::var("GUILD_ID")
        .expect("Expected a guild id in the environment")
        .parse()?;
    let voice_channel_id = env::var("VOICE_CHANNEL_ID")
        .expect("Expected a voice channel id in the environment")
        .parse()?;
    let text_channel_id = env::var("TEXT_CHANNEL_ID")
        .expect("Expected a text channel id in the environment")
        .parse()?;

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![discord::dump(), discord::clone(), discord::ctts()],
            prefix_options: poise::PrefixFrameworkOptions {
                prefix: Some("!".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .setup(move |ctx, ready, framework| {
            Box::pin(async move {
                let receiver = Arc::new(Receiver::new(GuildId::new(guild_id)));
                discord::on_ready(
                    ctx,
                    ready,
                    GuildId::new(guild_id),
                    ChannelId::new(voice_channel_id),
                    ChannelId::new(text_channel_id),
                    receiver.clone(),
                )
                .await?;
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;
                Ok(receiver)
            })
        })
        .build();

    let intents = GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT;

    // Here, we need to configure Songbird to decode all incoming voice packets.
    // If you want, you can do this on a per-call basis---here, we need it to
    // read the audio data that other people are sending us!
    let songbird_config = Config::default().decode_mode(DecodeMode::Decode);

    let mut client = Client::builder(&token, intents)
        .framework(framework)
        .register_songbird_from_config(songbird_config)
        .await
        .expect("Err creating client");

    client.start().await.context("Client ended: {:?}")
}
