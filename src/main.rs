//! Requires the "client", "standard_framework", and "voice" features be enabled
//! in your Cargo.toml, like so:
//!
//! ```toml
//! [dependencies.serenity]
//! git = "https://github.com/serenity-rs/serenity.git"
//! features = ["client", "standard_framework", "voice"]
//! ```

mod discord;
mod encode;
mod receiver;

use anyhow::Context;
use receiver::Receiver;
use serenity::all::standard::Configuration;
use serenity::model::id::{ChannelId, GuildId};
use serenity::prelude::GatewayIntents;
use serenity::{client::Client, framework::StandardFramework};
use songbird::{driver::DecodeMode, Config, SerenityInit};
use std::env;
use std::sync::Arc;

#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

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

    let framework = StandardFramework::new().group(&discord::GENERAL_GROUP);
    framework.configure(Configuration::new().prefix("!"));

    let intents = GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT;

    // Here, we need to configure Songbird to decode all incoming voice packets.
    // If you want, you can do this on a per-call basis---here, we need it to
    // read the audio data that other people are sending us!
    let songbird_config = Config::default().decode_mode(DecodeMode::Decode);

    let mut client = Client::builder(&token, intents)
        .event_handler(discord::Handler::new(
            GuildId::new(guild_id),
            ChannelId::new(voice_channel_id),
            ChannelId::new(text_channel_id),
        ))
        .framework(framework)
        .register_songbird_from_config(songbird_config)
        .await
        .expect("Err creating client");

    {
        let mut data = client.data.write().await;
        data.insert::<Receiver>(Arc::new(Receiver::new()));
    }

    client.start().await.context("Client ended: {:?}")
}
