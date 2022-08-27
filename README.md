# rust-discord-record

env vars

- DISCORD_TOKEN
- DISCORD_AUDIO_DIR
- GUILD_ID
- VOICE_CHANNEL_ID
- TEXT_CHANNEL_ID

commands

- !dump
  - writes entire buffer to a file and uploads it to discord
- !dump file
  - writes to a file on the bot's local filesystem
- !dump 5s
  - only dumps the last 5 seconds
