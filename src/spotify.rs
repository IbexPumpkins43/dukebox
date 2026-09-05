use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::Deserialize;

#[derive(Clone)]
pub struct SpotifyResolver {
    client: Client,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SpotifyTrackInfo {
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: u64,
    pub isrc: Option<String>,
}

impl SpotifyTrackInfo {
    pub fn display_name(&self) -> String {
        format!("{} — {}", self.artists.join(", "), self.title)
    }

    pub fn search_query(&self) -> String {
        let artists = self.artists.join(" ");
        match &self.album {
            Some(album) if !album.is_empty() => {
                format!("{artists} {} {album} official audio", self.title)
            }
            _ => format!("{artists} {} official audio", self.title),
        }
    }
}

#[derive(Debug)]
pub enum SpotifyResource {
    Track(SpotifyTrackInfo),
    Collection {
        kind: &'static str,
        name: String,
        tracks: Vec<SpotifyTrackInfo>,
    },
}

impl SpotifyResource {
    pub fn into_tracks(self) -> Vec<SpotifyTrackInfo> {
        match self {
            Self::Track(track) => vec![track],
            Self::Collection { tracks, .. } => tracks,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Track(track) => track.display_name(),
            Self::Collection { kind, name, tracks } => {
                format!("Spotify {kind} “{name}” ({} tracks)", tracks.len())
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct SpotifyArtist {
    name: String,
}

#[derive(Debug, Deserialize)]
struct SpotifyExternalIds {
    isrc: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SpotifyAlbumSummary {
    name: String,
}

#[derive(Debug, Deserialize)]
struct SpotifyTrack {
    name: String,
    artists: Vec<SpotifyArtist>,
    duration_ms: u64,
    #[serde(default)]
    album: Option<SpotifyAlbumSummary>,
    #[serde(default)]
    external_ids: Option<SpotifyExternalIds>,
}

#[derive(Debug, Deserialize)]
struct SpotifyAlbum {
    name: String,
}

#[derive(Debug, Deserialize)]
struct SimplifiedTrack {
    name: String,
    artists: Vec<SpotifyArtist>,
    duration_ms: u64,
}

#[derive(Debug, Deserialize)]
struct AlbumTracksPage {
    items: Vec<SimplifiedTrack>,
    next: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Playlist {
    name: String,
}

#[derive(Debug, Deserialize)]
struct PlaylistItem {
    #[serde(default)]
    item: Option<SpotifyTrack>,
    // Spotify's response examples currently expose both item and track.
    #[serde(default)]
    track: Option<SpotifyTrack>,
}

impl PlaylistItem {
    fn into_track(self) -> Option<SpotifyTrack> {
        self.item.or(self.track)
    }
}

#[derive(Debug, Deserialize)]
struct PlaylistItemsPage {
    items: Vec<PlaylistItem>,
    next: Option<String>,
}

impl SpotifyResolver {
    pub fn from_env(client: Client) -> Self {
        Self {
            client,
            client_id: std::env::var("SPOTIFY_CLIENT_ID").ok().filter(|s| !s.is_empty()),
            client_secret: std::env::var("SPOTIFY_CLIENT_SECRET").ok().filter(|s| !s.is_empty()),
            refresh_token: std::env::var("SPOTIFY_REFRESH_TOKEN").ok().filter(|s| !s.is_empty()),
        }
    }

    pub fn is_configured(&self) -> bool {
        self.client_id.is_some() && self.client_secret.is_some()
    }

    pub async fn resolve(&self, spotify_url: &str) -> Result<SpotifyResource> {
        let (kind, id) = extract_resource(spotify_url)
            .ok_or_else(|| anyhow!("Unsupported Spotify URL. Use a track, album, or playlist URL."))?;

        match kind {
            "track" => self.resolve_track(&id).await,
            "album" => self.resolve_album(&id).await,
            "playlist" => self.resolve_playlist(&id).await,
            _ => Err(anyhow!("Unsupported Spotify resource type.")),
        }
    }

    async fn resolve_track(&self, track_id: &str) -> Result<SpotifyResource> {
        let token = self.client_credentials_token().await?;
        let track = self
            .client
            .get(format!("https://api.spotify.com/v1/tracks/{track_id}"))
            .bearer_auth(token)
            .send()
            .await
            .context("Spotify track request failed")?
            .error_for_status()
            .context("Spotify track request returned an error")?
            .json::<SpotifyTrack>()
            .await
            .context("Could not decode Spotify track response")?;

        Ok(SpotifyResource::Track(track.into()))
    }

    async fn resolve_album(&self, album_id: &str) -> Result<SpotifyResource> {
        let token = self.client_credentials_token().await?;

        let album = self
            .client
            .get(format!("https://api.spotify.com/v1/albums/{album_id}"))
            .bearer_auth(&token)
            .send()
            .await
            .context("Spotify album request failed")?
            .error_for_status()
            .context("Spotify album request returned an error")?
            .json::<SpotifyAlbum>()
            .await
            .context("Could not decode Spotify album response")?;

        let mut tracks = Vec::new();
        let mut next = Some(format!(
            "https://api.spotify.com/v1/albums/{album_id}/tracks?limit=50"
        ));

        while let Some(url) = next {
            let page = self
                .client
                .get(url)
                .bearer_auth(&token)
                .send()
                .await
                .context("Spotify album tracks request failed")?
                .error_for_status()
                .context("Spotify album tracks request returned an error")?
                .json::<AlbumTracksPage>()
                .await
                .context("Could not decode Spotify album tracks response")?;

            tracks.extend(page.items.into_iter().map(|track| SpotifyTrackInfo {
                title: track.name,
                artists: track.artists.into_iter().map(|artist| artist.name).collect(),
                album: Some(album.name.clone()),
                duration_ms: track.duration_ms,
                isrc: None,
            }));

            next = page.next;
        }

        Ok(SpotifyResource::Collection {
            kind: "album",
            name: album.name,
            tracks,
        })
    }

    async fn resolve_playlist(&self, playlist_id: &str) -> Result<SpotifyResource> {
        let token = self
            .user_token()
            .await
            .context(
                "Spotify playlists require SPOTIFY_REFRESH_TOKEN because Spotify's current \
                 playlist-items API only allows playlists owned by or collaborative with the \
                 authenticated user.",
            )?;

        let playlist = self
            .client
            .get(format!("https://api.spotify.com/v1/playlists/{playlist_id}"))
            .bearer_auth(&token)
            .send()
            .await
            .context("Spotify playlist request failed")?
            .error_for_status()
            .context("Spotify playlist request returned an error")?
            .json::<Playlist>()
            .await
            .context("Could not decode Spotify playlist response")?;

        let mut tracks = Vec::new();
        let mut next = Some(format!(
            "https://api.spotify.com/v1/playlists/{playlist_id}/items?limit=50"
        ));

        while let Some(url) = next {
            let response = self
                .client
                .get(url)
                .bearer_auth(&token)
                .send()
                .await
                .context("Spotify playlist items request failed")?;

            if response.status() == reqwest::StatusCode::FORBIDDEN {
                return Err(anyhow!(
                    "Spotify refused access to this playlist. Under the current API, the \
                     authenticated user must own the playlist or be a collaborator."
                ));
            }

            let page = response
                .error_for_status()
                .context("Spotify playlist items request returned an error")?
                .json::<PlaylistItemsPage>()
                .await
                .context("Could not decode Spotify playlist items response")?;

            tracks.extend(
                page.items
                    .into_iter()
                    .filter_map(PlaylistItem::into_track)
                    .map(SpotifyTrackInfo::from),
            );

            next = page.next;
        }

        Ok(SpotifyResource::Collection {
            kind: "playlist",
            name: playlist.name,
            tracks,
        })
    }

    async fn client_credentials_token(&self) -> Result<String> {
        let client_id = self
            .client_id
            .as_deref()
            .ok_or_else(|| anyhow!("SPOTIFY_CLIENT_ID is not configured."))?;
        let client_secret = self
            .client_secret
            .as_deref()
            .ok_or_else(|| anyhow!("SPOTIFY_CLIENT_SECRET is not configured."))?;

        let token = self
            .client
            .post("https://accounts.spotify.com/api/token")
            .basic_auth(client_id, Some(client_secret))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await
            .context("Spotify token request failed")?
            .error_for_status()
            .context("Spotify token request returned an error")?
            .json::<TokenResponse>()
            .await
            .context("Could not decode Spotify token response")?;

        Ok(token.access_token)
    }

    async fn user_token(&self) -> Result<String> {
        let client_id = self
            .client_id
            .as_deref()
            .ok_or_else(|| anyhow!("SPOTIFY_CLIENT_ID is not configured."))?;
        let client_secret = self
            .client_secret
            .as_deref()
            .ok_or_else(|| anyhow!("SPOTIFY_CLIENT_SECRET is not configured."))?;
        let refresh_token = self
            .refresh_token
            .as_deref()
            .ok_or_else(|| anyhow!("SPOTIFY_REFRESH_TOKEN is not configured."))?;

        let token = self
            .client
            .post("https://accounts.spotify.com/api/token")
            .basic_auth(client_id, Some(client_secret))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
            ])
            .send()
            .await
            .context("Spotify refresh-token request failed")?
            .error_for_status()
            .context("Spotify refresh-token request returned an error")?
            .json::<TokenResponse>()
            .await
            .context("Could not decode Spotify refresh-token response")?;

        Ok(token.access_token)
    }
}

impl From<SpotifyTrack> for SpotifyTrackInfo {
    fn from(track: SpotifyTrack) -> Self {
        Self {
            title: track.name,
            artists: track.artists.into_iter().map(|artist| artist.name).collect(),
            album: track.album.map(|album| album.name),
            duration_ms: track.duration_ms,
            isrc: track.external_ids.and_then(|ids| ids.isrc),
        }
    }
}

fn extract_resource(input: &str) -> Option<(&'static str, String)> {
    let url = url::Url::parse(input).ok()?;
    if url.host_str()? != "open.spotify.com" {
        return None;
    }

    let mut segments = url.path_segments()?;
    let kind = segments.next()?;

    let kind = match kind {
        "track" => "track",
        "album" => "album",
        "playlist" => "playlist",
        _ => return None,
    };

    let id = segments.next()?.trim();
    if id.is_empty() {
        None
    } else {
        Some((kind, id.to_string()))
    }
}
