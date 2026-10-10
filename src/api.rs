//! Spotify Web API client.
//!
//! Two rules keep it well-behaved: a 429 pauses *every* request until Spotify's
//! Retry-After has passed (no retry storms), and responses are read as loose JSON
//! so a missing or null field never fails a whole page.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

use crate::auth::TokenStore;
use crate::model::*;

const BASE: &str = "https://api.spotify.com/v1";
/// Longest rate-limit pause a background read will sit through before giving up.
const MAX_WAIT: Duration = Duration::from_secs(12);

#[derive(Debug)]
pub struct RateLimited(pub u64);

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Spotify is rate limiting requests; retry in {}s", self.0)
    }
}

impl std::error::Error for RateLimited {}

pub struct Api {
    base: String,
    http: reqwest::Client,
    tokens: Arc<TokenStore>,
    gate: tokio::sync::Semaphore,
    blocked_until: Mutex<Option<Instant>>,
}

#[derive(Default)]
pub struct SearchResults {
    pub tracks: Vec<Track>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
    pub playlists: Vec<Playlist>,
    /// Per kind (tracks, albums, artists, playlists): how many entries
    /// Spotify sent, usable or not, and whether it has more after them.
    pub seen: [usize; 4],
    pub more: [bool; 4],
}

#[derive(Default, Debug, Clone)]
pub struct RemoteState {
    pub track: Option<Track>,
    pub is_playing: bool,
    pub progress_ms: u32,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub device: Option<Device>,
}

impl Api {
    pub fn new(tokens: Arc<TokenStore>) -> Self {
        Self {
            base: BASE.to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(12))
                .connect_timeout(Duration::from_secs(6))
                .build()
                .unwrap_or_default(),
            tokens,
            gate: tokio::sync::Semaphore::new(4),
            blocked_until: Mutex::new(None),
        }
    }

    /// Seconds left on a rate-limit pause, if one is in force.
    pub fn blocked_for(&self) -> Option<u64> {
        let until = (*self.blocked_until.lock().unwrap())?;
        let left = until.checked_duration_since(Instant::now())?;
        Some(left.as_secs() + 1)
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let patient = method == Method::GET;
        let mut attempts = 0;
        let mut refreshed = false;
        loop {
            attempts += 1;

            let wait = {
                let until = *self.blocked_until.lock().unwrap();
                until.and_then(|u| u.checked_duration_since(Instant::now()))
            };
            if let Some(wait) = wait {
                if !patient || wait > MAX_WAIT {
                    return Err(RateLimited(wait.as_secs() + 1).into());
                }
                tokio::time::sleep(wait + Duration::from_millis(150)).await;
            }

            let _permit = self.gate.acquire().await?;
            let bearer = self.tokens.bearer().await?;
            let mut req = self
                .http
                .request(method.clone(), format!("{}{path}", self.base))
                .bearer_auth(&bearer)
                .query(query);
            req = match &body {
                Some(b) => req.json(b),
                // Spotify answers 411 to body-less PUT/POST without a length.
                None if method != Method::GET => req.header("content-length", "0"),
                None => req,
            };

            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    if patient && attempts < 3 {
                        tokio::time::sleep(Duration::from_millis(400 * attempts)).await;
                        continue;
                    }
                    bail!("network error: {}", short_reqwest(&e));
                }
            };

            let status = resp.status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                let secs = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(5);
                *self.blocked_until.lock().unwrap() =
                    Some(Instant::now() + Duration::from_secs(secs));
                log::warn!("429 on {method} {path}: retry after {secs}s");
                if patient && attempts < 3 && Duration::from_secs(secs) <= MAX_WAIT {
                    continue;
                }
                return Err(RateLimited(secs).into());
            }
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                self.tokens.force_refresh(&bearer).await?;
                continue;
            }
            if status.is_server_error() && patient && attempts < 3 {
                tokio::time::sleep(Duration::from_millis(500 * attempts)).await;
                continue;
            }

            let text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                // Player endpoints reply 200/204 with empty or non-JSON bodies.
                return Ok(serde_json::from_str(&text).unwrap_or(Value::Null));
            }
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_default();
            log::warn!("{status} on {method} {path}: {detail}");
            bail!(match status {
                StatusCode::UNAUTHORIZED => "Spotify login expired; run `riff login`".to_string(),
                StatusCode::FORBIDDEN if detail.to_lowercase().contains("premium") =>
                    "Spotify Premium is required for that".to_string(),
                StatusCode::FORBIDDEN if detail.to_lowercase().contains("not registered") =>
                    "Your account isn't listed under User Management in your Spotify app".to_string(),
                StatusCode::FORBIDDEN => format!("Spotify refused: {}", or(&detail, "forbidden")),
                StatusCode::NOT_FOUND if path.starts_with("/me/player") =>
                    "No active Spotify device".to_string(),
                StatusCode::NOT_FOUND => "Not found on Spotify".to_string(),
                _ => format!("Spotify error {}: {}", status.as_u16(), or(&detail, "unknown")),
            });
        }
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.send(Method::GET, path, query, None).await
    }

    // ---- library ------------------------------------------------------

    pub async fn me(&self) -> Result<(String, String)> {
        let v = self.get("/me", &[]).await?;
        Ok((s(&v["id"]), s(&v["display_name"])))
    }

    /// One page of Liked Songs plus the total count.
    pub async fn liked(&self, offset: usize) -> Result<(Vec<Track>, usize)> {
        let v = self
            .get("/me/tracks", &[("limit", "50".into()), ("offset", offset.to_string())])
            .await?;
        let tracks = arr(&v["items"])
            .iter()
            .filter_map(|it| {
                let mut t = track(&it["track"])?;
                t.added_at = parse_date(it["added_at"].as_str());
                Some(t)
            })
            .collect();
        Ok((tracks, v["total"].as_u64().unwrap_or(0) as usize))
    }

    pub async fn recent(&self) -> Result<Vec<Track>> {
        let v = self.get("/me/player/recently-played", &[("limit", "50".into())]).await?;
        let mut out: Vec<Track> = Vec::new();
        for it in arr(&v["items"]) {
            if let Some(mut t) = track(&it["track"]) {
                if out.iter().any(|o| o.uri == t.uri) {
                    continue;
                }
                t.added_at = parse_date(it["played_at"].as_str());
                out.push(t);
            }
        }
        Ok(out)
    }

    pub async fn top_tracks(&self) -> Result<Vec<Track>> {
        let v = self
            .get("/me/top/tracks", &[("limit", "50".into()), ("time_range", "short_term".into())])
            .await?;
        Ok(arr(&v["items"]).iter().filter_map(track).collect())
    }

    /// `kinds` is a comma list of track,album,artist,playlist.
    pub async fn search(&self, query: &str, kinds: &str, offset: usize) -> Result<SearchResults> {
        let v = self
            .get(
                "/search",
                &[
                    ("q", query.to_string()),
                    ("type", kinds.to_string()),
                    ("limit", "10".into()),
                    ("offset", offset.to_string()),
                ],
            )
            .await?;
        // Spotify sends `null` in place of results it won't show (playlists
        // above all), so paging goes by what it sent, not by what was usable.
        let kinds = ["tracks", "albums", "artists", "playlists"];
        Ok(SearchResults {
            tracks: arr(&v["tracks"]["items"]).iter().filter_map(track).collect(),
            albums: arr(&v["albums"]["items"]).iter().filter_map(album).collect(),
            artists: arr(&v["artists"]["items"]).iter().filter_map(artist).collect(),
            playlists: arr(&v["playlists"]["items"]).iter().filter_map(playlist).collect(),
            seen: kinds.map(|k| arr(&v[k]["items"]).len()),
            more: kinds.map(|k| v[k]["next"].is_string()),
        })
    }

    pub async fn set_saved(&self, uri: &str, saved: bool) -> Result<()> {
        let method = if saved { Method::PUT } else { Method::DELETE };
        let new = self
            .send(method.clone(), "/me/library", &[("uris", uri.to_string())], None)
            .await;
        match new {
            Ok(_) => Ok(()),
            Err(e) if e.is::<RateLimited>() => Err(e),
            // Apps that predate the 2026 library endpoints still use the per-type ones.
            Err(first) => {
                let Some(id) = uri.strip_prefix("spotify:track:") else { return Err(first) };
                self.send(method, "/me/tracks", &[("ids", id.to_string())], None)
                    .await
                    .map(|_| ())
                    .map_err(|_| first)
            }
        }
    }

    // ---- player -------------------------------------------------------

    pub async fn player(&self) -> Result<Option<RemoteState>> {
        let v = self.get("/me/player", &[]).await?;
        if v.is_null() {
            return Ok(None);
        }
        Ok(Some(RemoteState {
            track: track(&v["item"]),
            is_playing: v["is_playing"].as_bool().unwrap_or(false),
            progress_ms: v["progress_ms"].as_u64().unwrap_or(0) as u32,
            shuffle: v["shuffle_state"].as_bool().unwrap_or(false),
            repeat: match v["repeat_state"].as_str() {
                Some("context") => Repeat::Context,
                Some("track") => Repeat::Track,
                _ => Repeat::Off,
            },
            device: device(&v["device"]),
        }))
    }

    pub async fn devices(&self) -> Result<Vec<Device>> {
        let v = self.get("/me/player/devices", &[]).await?;
        Ok(arr(&v["devices"]).iter().filter_map(device).collect())
    }

    /// Upcoming tracks on the active device.
    pub async fn queue(&self) -> Result<Vec<Track>> {
        let v = self.get("/me/player/queue", &[]).await?;
        Ok(arr(&v["queue"]).iter().filter_map(track).collect())
    }

    pub async fn queue_add(&self, uri: &str) -> Result<()> {
        self.send(Method::POST, "/me/player/queue", &[("uri", uri.to_string())], None)
            .await
            .map(|_| ())
    }

    pub async fn transfer(&self, device_id: &str, play: bool) -> Result<()> {
        self.send(
            Method::PUT,
            "/me/player",
            &[],
            Some(json!({ "device_ids": [device_id], "play": play })),
        )
        .await
        .map(|_| ())
    }

    /// Remote-control verbs, used only while another device is the active one.
    pub async fn command(&self, cmd: RemoteCmd) -> Result<()> {
        let (method, path, query, body): (Method, &str, Vec<(&str, String)>, Option<Value>) =
            match cmd {
                RemoteCmd::Resume => (Method::PUT, "/me/player/play", vec![], None),
                RemoteCmd::Pause => (Method::PUT, "/me/player/pause", vec![], None),
                RemoteCmd::Next => (Method::POST, "/me/player/next", vec![], None),
                RemoteCmd::Prev => (Method::POST, "/me/player/previous", vec![], None),
                RemoteCmd::Seek(ms) => {
                    (Method::PUT, "/me/player/seek", vec![("position_ms", ms.to_string())], None)
                }
                RemoteCmd::Volume(pct) => (
                    Method::PUT,
                    "/me/player/volume",
                    vec![("volume_percent", pct.to_string())],
                    None,
                ),
                RemoteCmd::Shuffle(on) => {
                    (Method::PUT, "/me/player/shuffle", vec![("state", on.to_string())], None)
                }
                RemoteCmd::Repeat(mode) => {
                    let state = match mode {
                        Repeat::Off => "off",
                        Repeat::Context => "context",
                        Repeat::Track => "track",
                    };
                    (Method::PUT, "/me/player/repeat", vec![("state", state.to_string())], None)
                }
                RemoteCmd::PlayContext { context_uri, track_uri } => {
                    let mut body = json!({ "context_uri": context_uri });
                    if let Some(t) = track_uri {
                        body["offset"] = json!({ "uri": t });
                    }
                    (Method::PUT, "/me/player/play", vec![], Some(body))
                }
                RemoteCmd::PlayTracks { uris, index } => (
                    Method::PUT,
                    "/me/player/play",
                    vec![],
                    Some(json!({ "uris": uris, "offset": { "position": index } })),
                ),
            };
        self.send(method, path, &query, body).await.map(|_| ())
    }
}

pub enum RemoteCmd {
    Resume,
    Pause,
    Next,
    Prev,
    Seek(u32),
    Volume(u8),
    Shuffle(bool),
    Repeat(Repeat),
    PlayContext { context_uri: String, track_uri: Option<String> },
    PlayTracks { uris: Vec<String>, index: usize },
}

pub async fn fetch_bytes(http: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let resp = http
        .get(url)
        .timeout(Duration::from_secs(12))
        .send()
        .await
        .map_err(|e| anyhow!("network error: {}", short_reqwest(&e)))?
        .error_for_status()?;
    Ok(resp.bytes().await?.to_vec())
}

fn short_reqwest(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timed out"
    } else if e.is_connect() {
        "can't reach Spotify"
    } else {
        "request failed"
    }
}

fn or<'a>(a: &'a str, b: &'a str) -> &'a str {
    if a.is_empty() { b } else { a }
}

// ---- loose JSON → model ----------------------------------------------

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn arr(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// Prefer the ~300px rendition: plenty for a terminal, quick to fetch and decode.
fn image(v: &Value) -> Option<String> {
    let imgs = arr(v);
    imgs.iter()
        .find(|i| (200..=400).contains(&i["width"].as_u64().unwrap_or(0)))
        .or_else(|| imgs.first())
        .and_then(|i| i["url"].as_str())
        .map(str::to_string)
}

fn artist_refs(v: &Value) -> Vec<ArtistRef> {
    arr(v)
        .iter()
        .filter(|a| a["name"].is_string())
        .map(|a| ArtistRef { id: s(&a["id"]), name: s(&a["name"]) })
        .collect()
}

pub fn track(v: &Value) -> Option<Track> {
    let uri = v["uri"].as_str()?;
    let name = v["name"].as_str()?;
    if v["type"].as_str() == Some("episode") {
        return Some(Track {
            uri: uri.to_string(),
            id: s(&v["id"]),
            name: name.to_string(),
            artists: vec![ArtistRef { id: String::new(), name: s(&v["show"]["name"]) }],
            album: "Podcast".into(),
            duration_ms: v["duration_ms"].as_u64().unwrap_or(0) as u32,
            image: image(&v["images"]),
            playable: true,
            ..Default::default()
        });
    }
    Some(Track {
        uri: uri.to_string(),
        id: s(&v["id"]),
        name: name.to_string(),
        artists: artist_refs(&v["artists"]),
        album: s(&v["album"]["name"]),
        album_id: s(&v["album"]["id"]),
        duration_ms: v["duration_ms"].as_u64().unwrap_or(0) as u32,
        explicit: v["explicit"].as_bool().unwrap_or(false),
        image: image(&v["album"]["images"]),
        added_at: None,
        playable: v["is_playable"].as_bool().unwrap_or(true),
    })
}

pub fn album(v: &Value) -> Option<Album> {
    Some(Album {
        id: v["id"].as_str()?.to_string(),
        name: v["name"].as_str()?.to_string(),
        artists: artist_refs(&v["artists"]),
        year: v["release_date"].as_str().unwrap_or("").chars().take(4).collect(),
        kind: s(&v["album_type"]),
        image: image(&v["images"]),
        total_tracks: v["total_tracks"].as_u64().unwrap_or(0) as u32,
    })
}

pub fn artist(v: &Value) -> Option<Artist> {
    Some(Artist {
        id: v["id"].as_str()?.to_string(),
        name: v["name"].as_str()?.to_string(),
        image: image(&v["images"]),
    })
}

pub fn playlist(v: &Value) -> Option<Playlist> {
    let count = &v["items"]["total"];
    let count = if count.is_null() { &v["tracks"]["total"] } else { count };
    Some(Playlist {
        id: v["id"].as_str()?.to_string(),
        name: v["name"].as_str()?.to_string(),
        owner: v["owner"]["display_name"]
            .as_str()
            .or(v["owner"]["id"].as_str())
            .unwrap_or("")
            .to_string(),
        description: s(&v["description"]),
        len: count.as_u64().unwrap_or(0) as u32,
        image: image(&v["images"]),
    })
}

fn device(v: &Value) -> Option<Device> {
    Some(Device {
        id: v["id"].as_str()?.to_string(),
        name: s(&v["name"]),
        kind: s(&v["type"]),
        volume: v["volume_percent"].as_u64().unwrap_or(0).min(100) as u8,
        active: v["is_active"].as_bool().unwrap_or(false),
        this: false,
    })
}

/// `2024-03-09T18:22:10Z` → unix seconds, without pulling in a date crate.
pub fn parse_date(text: Option<&str>) -> Option<i64> {
    let t = text?;
    let num = |range: std::ops::Range<usize>| t.get(range)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13).unwrap_or(0), num(14..16).unwrap_or(0), num(17..19).unwrap_or(0));
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(parse_date(Some("1970-01-01T00:00:00Z")), Some(0));
        assert_eq!(parse_date(Some("2024-03-09T18:22:10Z")), Some(1710008530));
        assert_eq!(parse_date(Some("nope")), None);
    }

    #[test]
    fn tolerant_track() {
        // Dev-mode responses drop fields and null others; none of that may fail a row.
        let v = json!({
            "uri": "spotify:track:abc", "id": "abc", "name": "Song",
            "artists": [{ "id": null, "name": "A" }, { "name": null }],
            "album": { "name": "Alb", "images": null },
            "duration_ms": null
        });
        let t = track(&v).unwrap();
        assert_eq!(t.artists.len(), 1);
        assert_eq!(t.album, "Alb");
        assert!(t.image.is_none());
        assert!(track(&Value::Null).is_none());
    }

    #[test]
    fn playlist_count_either_shape() {
        let new = json!({ "id": "p", "name": "P", "items": { "total": 7 } });
        let old = json!({ "id": "p", "name": "P", "tracks": { "total": 9 } });
        assert_eq!(playlist(&new).unwrap().len, 7);
        assert_eq!(playlist(&old).unwrap().len, 9);
    }

    /// A throwaway HTTP server that answers each request with the next canned
    /// response and counts how many requests it saw.
    async fn serve(responses: Vec<&'static str>) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else { return };
                let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let reply = responses[n.min(responses.len() - 1)];
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream.write_all(reply.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (base, hits)
    }

    fn client(base: String) -> Api {
        let tokens = crate::auth::Tokens {
            client_id: "test".into(),
            access_token: "token".into(),
            refresh_token: None,
            expires_at: u64::MAX / 2,
        };
        let mut api = Api::new(Arc::new(TokenStore::new(tokens)));
        api.base = base;
        api
    }

    const OK: &str = "HTTP/1.1 200 OK\r\ncontent-length: 15\r\nconnection: close\r\n\r\n{\"id\":\"jasper\"}";
    const LIMIT_1: &str = "HTTP/1.1 429 Too Many Requests\r\nretry-after: 1\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
    const LIMIT_30: &str = "HTTP/1.1 429 Too Many Requests\r\nretry-after: 30\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
    const BROKEN: &str = "HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

    #[tokio::test]
    async fn short_rate_limit_is_waited_out_once() {
        let (base, hits) = serve(vec![LIMIT_1, OK]).await;
        let api = client(base);
        let started = Instant::now();
        assert_eq!(api.me().await.unwrap().0, "jasper");
        assert!(started.elapsed() >= Duration::from_secs(1), "must honour Retry-After");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2, "exactly one retry");
    }

    #[tokio::test]
    async fn long_rate_limit_fails_fast_and_silences_everything() {
        let (base, hits) = serve(vec![LIMIT_30]).await;
        let api = client(base);
        let err = api.me().await.unwrap_err();
        assert_eq!(err.downcast_ref::<RateLimited>().map(|r| r.0), Some(30));
        assert!(api.blocked_for().is_some());
        // While Spotify has us paused, nothing else may reach the network.
        for _ in 0..5 {
            assert!(api.me().await.unwrap_err().is::<RateLimited>());
            assert!(api.command(RemoteCmd::Pause).await.unwrap_err().is::<RateLimited>());
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1, "no retry storm");
    }

    #[tokio::test]
    async fn server_errors_retry_reads_but_never_writes() {
        let (base, hits) = serve(vec![BROKEN, OK]).await;
        let api = client(base);
        assert!(api.me().await.is_ok());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);

        let (base, hits) = serve(vec![BROKEN, OK]).await;
        let api = client(base);
        assert!(api.command(RemoteCmd::Next).await.is_err(), "a skip must not be sent twice");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
