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

use anyhow::{Context, Result};
use receiver::Receiver;
use serenity::prelude::GatewayIntents;
use serenity::{client::Client, framework::StandardFramework};
use songbird::{driver::DecodeMode, Config, SerenityInit, Songbird};
use std::env;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    // Configure the client with your Discord bot token in the environment.
    let token = env::var("DISCORD_TOKEN").expect("Expected a token in the environment");

    let framework = StandardFramework::new()
        .configure(|c| c.prefix("!"))
        .group(&discord::GENERAL_GROUP);

    // Here, we need to configure Songbird to decode all incoming voice packets.
    // If you want, you can do this on a per-call basis---here, we need it to
    // read the audio data that other people are sending us!
    let songbird = Songbird::serenity();
    songbird.set_config(Config::default().decode_mode(DecodeMode::Decode));

    let mut client = Client::builder(&token, GatewayIntents::default())
        .event_handler(discord::Handler)
        .framework(framework)
        .register_songbird_with(songbird)
        .await
        .expect("Err creating client");

    {
        let mut data = client.data.write().await;
        data.insert::<Receiver>(Arc::new(Receiver::new()));
    }

    client.start().await.context("Client ended: {:?}")?;
    Ok(())
}
