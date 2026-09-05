# Dukebox

Dukebox is a deliberately small prototype Discord music bot written in Rust.

## Features

- Slash commands
- Join / leave voice
- Play YouTube URLs
- Play SoundCloud URLs through `yt-dlp`
- Search YouTube when `/play` receives plain text
- Spotify track, album, and playlist URL resolution
- Scores multiple YouTube candidates using title, artist, album, and duration metadata
- Queue
- Pause / resume / skip / stop
- Uses Songbird for Discord voice
- Recovers stale voice connections on the next play request
- Disconnects after 5 minutes with an empty queue

## Configure

Create a `.env` file with:

```env
DISCORD_TOKEN=your_discord_bot_token
SPOTIFY_CLIENT_ID=your_spotify_client_id
SPOTIFY_CLIENT_SECRET=your_spotify_client_secret
# Required for playlist URLs under Spotify's current playlist API:
SPOTIFY_REFRESH_TOKEN=your_spotify_refresh_token
RUST_LOG=info
```

## Run

```bash
cargo run --release
```

The bot registers slash commands globally. Discord can sometimes take a little while to surface newly registered global commands.

## Nix

Dukebox includes a Nix flake.

Enter the development environment:

```bash
nix develop
```

This provides Rust, Cargo, Clippy, rustfmt, Opus, FFmpeg, `pkg-config`, and `yt-dlp`.

Run Dukebox from the development shell:

```bash
cargo run --release
```

Or run it directly through the flake:

```bash
nix run
```

The first Cargo run will generate `Cargo.lock` if one does not already exist. Once the dependency set is stable, `Cargo.lock` should be committed so Dukebox can also be packaged reproducibly with `rustPlatform.buildRustPackage`.

## Commands

| Command | Description |
|---|---|
| `/join` | Joins the voice channel you are currently in. |
| `/leave` | Disconnects Dukebox from the voice channel. |
| `/play <query-or-url>` | Queues YouTube/SoundCloud URLs, Spotify tracks/albums/playlists, or searches YouTube from plain text. |
| `/pause` | Pauses the current track. |
| `/resume` | Resumes the paused track. |
| `/skip` | Skips the current track and moves to the next queued item. |
| `/stop` | Stops playback and clears the queue. |
| `/queue` | Shows how many tracks are currently queued. |
| `/ping` | Checks whether the bot is responding. |

## Architecture

```text
Discord
   |
   v
Poise / Serenity
   |
   v
Songbird
   ^
   |
48 kHz stereo PCM
   ^
   |
 FFmpeg
   ^
   |
 yt-dlp URL resolution
   |
   +--> YouTube
   |
   +--> SoundCloud
   |
   +--> Spotify Web API --> metadata --> YouTube search
```

For a large production bot, the next architectural decision would be whether to keep direct Songbird playback or move media playback behind Lavalink/audio workers.

## TODO

1. Store rich queue metadata.
2. Add `/nowplaying`.
3. Add per-guild volume.
4. Add structured errors and health metrics.
5. Load tests with many guild queues.
6. Consider Lavalink nodes once concurrent playback becomes large.