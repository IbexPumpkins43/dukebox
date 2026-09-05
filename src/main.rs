mod spotify;

use anyhow::{anyhow, Context as _, Result};
use poise::serenity_prelude as serenity;
use reqwest::Client as HttpClient;
use songbird::{
    input::{
        core::io::ReadOnlySource,
        ChildContainer,
        Input,
        RawAdapter,
    },
    SerenityInit,
};
use std::{
    collections::HashSet,
    process::{Command, Stdio},
    sync::Arc,
};
use tokio::{
    sync::Mutex,
    time::{sleep, Duration, Instant},
};
use spotify::{SpotifyResolver, SpotifyTrackInfo};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Context<'a> = poise::Context<'a, Data, Error>;

const IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Clone)]
struct Data {
    spotify: SpotifyResolver,
    idle_monitors: Arc<Mutex<HashSet<serenity::GuildId>>>,
}

#[derive(Debug, serde::Deserialize)]
struct SearchCandidate {
    #[serde(default)]
    title: String,
    #[serde(default)]
    uploader: String,
    duration: Option<f64>,
    webpage_url: Option<String>,
    url: Option<String>,
}

fn voice_channel(ctx: Context<'_>) -> Result<serenity::ChannelId> {
    let guild = ctx
        .guild()
        .ok_or_else(|| anyhow!("This command only works in a server."))?;

    guild
        .voice_states
        .get(&ctx.author().id)
        .and_then(|state| state.channel_id)
        .ok_or_else(|| anyhow!("Join a voice channel first."))
}

async fn ensure_joined(ctx: Context<'_>) -> Result<serenity::GuildId> {
    let guild_id = ctx
        .guild_id()
        .ok_or_else(|| anyhow!("This command only works in a server."))?;

    let channel_id = voice_channel(ctx)?;
    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or_else(|| anyhow!("Songbird was not initialised."))?
        .clone();

    let needs_join = match manager.get(guild_id) {
        Some(call) => {
            let call = call.lock().await;
            call.current_channel().is_none()
        }
        None => true,
    };

    if needs_join {
        tracing::info!(%guild_id, %channel_id, "Joining or recovering voice connection");
        manager.join(guild_id, channel_id).await?;
    }

    start_idle_monitor(ctx.data(), manager, guild_id).await;

    Ok(guild_id)
}

async fn start_idle_monitor(
    data: &Data,
    manager: Arc<songbird::Songbird>,
    guild_id: serenity::GuildId,
) {
    {
        let mut monitors = data.idle_monitors.lock().await;
        if !monitors.insert(guild_id) {
            return;
        }
    }

    let monitors = Arc::clone(&data.idle_monitors);

    tokio::spawn(async move {
        let mut idle_since: Option<Instant> = None;

        loop {
            sleep(IDLE_POLL_INTERVAL).await;

            let Some(call) = manager.get(guild_id) else {
                break;
            };

            let queue_is_empty = {
                let call = call.lock().await;
                call.queue().len() == 0
            };

            if queue_is_empty {
                let since = idle_since.get_or_insert_with(Instant::now);

                if since.elapsed() >= IDLE_TIMEOUT {
                    tracing::info!(%guild_id, "Disconnecting after voice inactivity");

                    if let Err(error) = manager.remove(guild_id).await {
                        tracing::warn!(%guild_id, %error, "Failed to disconnect idle voice call");
                    }

                    break;
                }
            } else {
                idle_since = None;
            }
        }

        monitors.lock().await.remove(&guild_id);
    });
}

fn normalise(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn overlap_score(needle: &str, haystack: &str) -> i64 {
    let needle = normalise(needle);
    let haystack = normalise(haystack);

    needle
        .split_whitespace()
        .filter(|word| word.len() > 1 && haystack.contains(word))
        .count() as i64
}

fn best_spotify_match(track: &SpotifyTrackInfo) -> Result<String> {
    let query = track.search_query();
    let search = format!("ytsearch5:{query}");

    let output = Command::new("yt-dlp")
        .args([
            "--no-warnings",
            "--dump-json",
            "--skip-download",
            &search,
        ])
        .output()
        .context("Failed to search YouTube with yt-dlp")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("yt-dlp search failed: {stderr}"));
    }

    let target_seconds = track.duration_ms as f64 / 1000.0;
    let artist_text = track.artists.join(" ");

    let mut best: Option<(i64, String)> = None;

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let candidate: SearchCandidate = match serde_json::from_str(line) {
            Ok(candidate) => candidate,
            Err(_) => continue,
        };

        let candidate_text = format!("{} {}", candidate.title, candidate.uploader);
        let mut score = overlap_score(&track.title, &candidate_text) * 8;
        score += overlap_score(&artist_text, &candidate_text) * 5;

        if let Some(album) = &track.album {
            score += overlap_score(album, &candidate_text) * 2;
        }

        if normalise(&candidate.title).contains(&normalise(&track.title)) {
            score += 12;
        }

        if let Some(duration) = candidate.duration {
            let delta = (duration - target_seconds).abs();
            score += if delta <= 2.0 {
                20
            } else if delta <= 5.0 {
                12
            } else if delta <= 10.0 {
                5
            } else if delta >= 30.0 {
                -15
            } else {
                0
            };
        }

        let lower = candidate_text.to_ascii_lowercase();
        if lower.contains("official audio") || lower.contains("provided to youtube") {
            score += 5;
        }
        if lower.contains("live") && !track.title.to_ascii_lowercase().contains("live") {
            score -= 8;
        }
        if lower.contains("remix") && !track.title.to_ascii_lowercase().contains("remix") {
            score -= 8;
        }

        let url = candidate.webpage_url.or_else(|| {
            candidate.url.map(|id| {
                if id.starts_with("http://") || id.starts_with("https://") {
                    id
                } else {
                    format!("https://www.youtube.com/watch?v={id}")
                }
            })
        });

        let Some(url) = url else {
            continue;
        };

        if best.as_ref().map_or(true, |(best_score, _)| score > *best_score) {
            best = Some((score, url));
        }
    }

    best.map(|(_, url)| url)
        .ok_or_else(|| anyhow!("No playable YouTube match found for {}", track.display_name()))
}

fn ffmpeg_input(target: &str, search: bool) -> Result<Input> {
    let target = if search {
        format!("ytsearch1:{target}")
    } else {
        target.to_string()
    };

    // Resolve the site URL first. This avoids piping yt-dlp's media output into
    // FFmpeg, which can produce "Broken pipe" when FFmpeg exits before yt-dlp.
    let output = Command::new("yt-dlp")
        .args([
            "--no-playlist",
            "--no-warnings",
            "-f",
            "bestaudio",
            "--get-url",
            &target,
        ])
        .output()
        .context("Failed to start yt-dlp")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("yt-dlp failed to resolve media URL: {stderr}"));
    }

    let media_url = String::from_utf8(output.stdout)
        .context("yt-dlp returned a non-UTF-8 media URL")?
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .ok_or_else(|| anyhow!("yt-dlp did not return a playable media URL"))?
        .to_string();

    let ffmpeg = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-loglevel",
            "warning",
            "-reconnect",
            "1",
            "-reconnect_streamed",
            "1",
            "-reconnect_delay_max",
            "5",
            "-i",
            &media_url,
            "-vn",
            "-f",
            "f32le",
            "-ar",
            "48000",
            "-ac",
            "2",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .context("Failed to start ffmpeg")?;

    let source = RawAdapter::new(
        ReadOnlySource::new(ChildContainer::new(vec![ffmpeg])),
        48_000,
        2,
    );

    Ok(source.into())
}

#[poise::command(slash_command, guild_only)]
async fn join(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().unwrap();
    let channel_id = voice_channel(ctx)?;

    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or("Songbird was not initialised")?
        .clone();

    manager.join(guild_id, channel_id).await?;
    start_idle_monitor(ctx.data(), manager, guild_id).await;
    ctx.say("Joined your voice channel.").await?;
    Ok(())
}

#[poise::command(slash_command, guild_only)]
async fn leave(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().unwrap();
    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or("Songbird was not initialised")?
        .clone();

    if manager.get(guild_id).is_some() {
        manager.remove(guild_id).await?;
        ctx.say("Disconnected.").await?;
    } else {
        ctx.say("I'm not connected to a voice channel.").await?;
    }

    Ok(())
}

#[poise::command(slash_command, guild_only)]
async fn play(
    ctx: Context<'_>,
    #[description = "YouTube/SoundCloud URL, Spotify track/album/playlist URL, or search text"]
    #[rest]
    query: String,
) -> Result<(), Error> {
    ctx.defer().await?;

    let guild_id = ensure_joined(ctx).await?;

    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or("Songbird was not initialised")?
        .clone();

    let handler_lock = manager
        .get(guild_id)
        .ok_or("Voice connection disappeared")?;

    let trimmed = query.trim();
    let is_spotify = trimmed.starts_with("https://open.spotify.com/");

    if is_spotify {
        let resource = ctx
            .data()
            .spotify
            .resolve(trimmed)
            .await
            .context("Could not resolve Spotify URL")?;

        let label = resource.label();
        let tracks = resource.into_tracks();

        if tracks.is_empty() {
            return Err(anyhow!("Spotify returned no playable tracks.").into());
        }

        let mut queued = 0usize;
        for track in tracks {
            let matched_url = best_spotify_match(&track)
                .with_context(|| format!("Could not match {}", track.display_name()))?;
            let input = ffmpeg_input(&matched_url, false)?;

            let mut handler = handler_lock.lock().await;
            handler.enqueue_input(input).await;
            queued += 1;
        }

        ctx.say(format!("Queued {queued} track(s) from {label}.")).await?;
        return Ok(());
    }

    let (input, label) = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        (
            ffmpeg_input(trimmed, false)?,
            format!("URL: <{trimmed}>"),
        )
    } else {
        (
            ffmpeg_input(trimmed, true)?,
            format!("Search: `{trimmed}`"),
        )
    };

    {
        let mut handler = handler_lock.lock().await;
        handler.enqueue_input(input).await;
    }

    ctx.say(format!("Queued {label}")).await?;
    Ok(())
}

#[poise::command(slash_command, guild_only)]
async fn skip(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().unwrap();
    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or("Songbird was not initialised")?
        .clone();

    let Some(handler_lock) = manager.get(guild_id) else {
        ctx.say("I'm not in a voice channel.").await?;
        return Ok(());
    };

    let handler = handler_lock.lock().await;
    match handler.queue().skip() {
        Ok(_) => ctx.say("Skipped.").await?,
        Err(_) => ctx.say("Nothing is playing.").await?,
    };

    Ok(())
}

#[poise::command(slash_command, guild_only)]
async fn pause(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().unwrap();
    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or("Songbird was not initialised")?
        .clone();

    let Some(handler_lock) = manager.get(guild_id) else {
        ctx.say("I'm not in a voice channel.").await?;
        return Ok(());
    };

    let handler = handler_lock.lock().await;
    match handler.queue().pause() {
        Ok(_) => ctx.say("Paused.").await?,
        Err(_) => ctx.say("Nothing is playing.").await?,
    };

    Ok(())
}

#[poise::command(slash_command, guild_only)]
async fn resume(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().unwrap();
    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or("Songbird was not initialised")?
        .clone();

    let Some(handler_lock) = manager.get(guild_id) else {
        ctx.say("I'm not in a voice channel.").await?;
        return Ok(());
    };

    let handler = handler_lock.lock().await;
    match handler.queue().resume() {
        Ok(_) => ctx.say("Resumed.").await?,
        Err(_) => ctx.say("Nothing is paused.").await?,
    };

    Ok(())
}

#[poise::command(slash_command, guild_only)]
async fn stop(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().unwrap();
    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or("Songbird was not initialised")?
        .clone();

    let Some(handler_lock) = manager.get(guild_id) else {
        ctx.say("I'm not in a voice channel.").await?;
        return Ok(());
    };

    let handler = handler_lock.lock().await;
    handler.queue().stop();
    ctx.say("Stopped playback and cleared the queue.").await?;
    Ok(())
}

#[poise::command(slash_command, guild_only)]
async fn queue(ctx: Context<'_>) -> Result<(), Error> {
    let guild_id = ctx.guild_id().unwrap();
    let manager = songbird::get(ctx.serenity_context())
        .await
        .ok_or("Songbird was not initialised")?
        .clone();

    let Some(handler_lock) = manager.get(guild_id) else {
        ctx.say("Queue is empty.").await?;
        return Ok(());
    };

    let handler = handler_lock.lock().await;
    let count = handler.queue().len();

    ctx.say(format!("{count} track(s) currently in the Songbird queue."))
        .await?;
    Ok(())
}

#[poise::command(slash_command)]
async fn ping(ctx: Context<'_>) -> Result<(), Error> {
    ctx.say("Pong.").await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let token = std::env::var("DISCORD_TOKEN")
        .context("DISCORD_TOKEN is missing. Set it in the environment or a local .env file.")?;

    let http = HttpClient::builder()
        .user_agent("dukebox/0.1")
        .build()?;

    let spotify = SpotifyResolver::from_env(http.clone());

    if !spotify.is_configured() {
        tracing::warn!(
            "Spotify credentials are not set. YouTube/SoundCloud/search will work; Spotify track URLs will not."
        );
    }

    let intents =
        serenity::GatewayIntents::GUILDS | serenity::GatewayIntents::GUILD_VOICE_STATES;

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                join(),
                leave(),
                play(),
                skip(),
                pause(),
                resume(),
                stop(),
                queue(),
                ping(),
            ],
            on_error: |error| {
                Box::pin(async move {
                    if let poise::FrameworkError::Command { error, ctx, .. } = error {
                        let _ = ctx.say(format!("Error: {error}")).await;
                    } else {
                        eprintln!("A non-command framework error occurred.");
                    }
                })
            },
            ..Default::default()
        })
        .setup(|ctx, ready, framework| {
            Box::pin(async move {
                tracing::info!("Logged in as {}", ready.user.name);
                poise::builtins::register_globally(ctx, &framework.options().commands).await?;

                Ok(Data {
                    spotify: spotify.clone(),
                    idle_monitors: Arc::new(Mutex::new(HashSet::new())),
                })
            })
        })
        .build();

    let mut client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .register_songbird()
        .await
        .context("Failed to create Discord client")?;

    client.start().await.context("Discord client exited")?;
    Ok(())
}
