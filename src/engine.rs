//! The embedded Spotify player (librespot). riff is its own Spotify Connect
//! device, so play/pause/seek/volume are local calls, not web requests, and
//! playback state arrives as events instead of being polled.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures::StreamExt;
use librespot_connect::{
    ConnectConfig, LoadContextOptions, LoadRequest, LoadRequestOptions, Options, PlayingTrack,
    Spirc,
};
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::DeviceType;
use librespot_core::dealer::protocol::Message;
use librespot_core::error::ErrorKind;
use librespot_core::{Session, SessionConfig};
use librespot_metadata::audio::UniqueFields;
use librespot_playback::audio_backend::{self, Sink};
use librespot_playback::config::{AudioFormat, Bitrate, PlayerConfig};
use librespot_playback::mixer::softmixer::SoftMixer;
use librespot_playback::mixer::{Mixer, MixerConfig};
use librespot_playback::player::{Player, PlayerEvent};
use librespot_protocol::connect::ClusterUpdate;
use tokio::sync::{mpsc::UnboundedSender, watch};

use crate::config::{self, Config};
use crate::model::*;
use crate::viz::{NullSink, Tap, VizSink};

/// Connection lifecycle, shown in the status line.
#[derive(Clone, Debug, PartialEq)]
pub enum Conn {
    Connecting,
    Online,
    Reconnecting,
    /// Credentials were rejected; reconnecting won't help.
    LoginRejected,
}

/// What every Connect device on the account is doing, pushed by Spotify.
#[derive(Clone, Debug, Default)]
pub struct Cluster {
    pub devices: Vec<Device>,
    pub track_uri: String,
    pub context_uri: String,
    pub playing: bool,
    pub position_ms: u32,
    /// Server clock (unix ms) at which `position_ms` was true.
    pub timestamp_ms: i64,
    pub duration_ms: u32,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// Upcoming track URIs; `true` marks ones the user queued by hand.
    pub next: Vec<(String, bool)>,
}

#[derive(Debug)]
pub enum Event {
    Conn(Conn),
    User(String),
    Track(Track),
    Playing { uri: String, pos_ms: u32 },
    Paused { uri: String, pos_ms: u32 },
    Loading { uri: String, pos_ms: u32 },
    Position(u32),
    Stopped,
    Unavailable,
    Volume(u8),
    Shuffle(bool),
    Repeat(Repeat),
    Cluster(Cluster),
    Notice(String),
}

#[derive(Clone)]
pub struct Live {
    pub session: Session,
    spirc: Arc<Spirc>,
}

pub struct Engine {
    cfg: Config,
    tx: UnboundedSender<Event>,
    live: watch::Sender<Option<Live>>,
    tap: Arc<Tap>,
    quit: AtomicBool,
}

pub fn session_dir() -> PathBuf {
    config::cache_dir().join("session")
}

pub fn librespot_cache(cfg: &Config) -> Result<Cache> {
    let dir = session_dir();
    let audio = cfg.audio_cache.then(|| config::cache_dir().join("audio"));
    Cache::new(Some(dir.clone()), Some(dir), audio, Some(2 * 1024 * 1024 * 1024))
        .map_err(|e| anyhow!("can't open cache: {e}"))
}

pub fn session_config(cfg: &Config) -> SessionConfig {
    SessionConfig { device_id: cfg.device_id.clone(), ..Default::default() }
}

pub fn pct_to_vol(pct: u8) -> u16 {
    (pct.min(100) as f64 / 100.0 * 65535.0).round() as u16
}

pub fn vol_to_pct(vol: u16) -> u8 {
    (vol as f64 / 65535.0 * 100.0).round() as u8
}

impl Engine {
    pub fn start(
        cfg: Config,
        credentials: Credentials,
        tx: UnboundedSender<Event>,
        tap: Arc<Tap>,
    ) -> Arc<Self> {
        let (live, _) = watch::channel(None);
        let engine = Arc::new(Self { cfg, tx, live, tap, quit: AtomicBool::new(false) });
        tokio::spawn(engine.clone().supervise(credentials));
        engine
    }

    /// Keep a connection alive for the life of the app, reconnecting with backoff.
    async fn supervise(self: Arc<Self>, mut credentials: Credentials) {
        let mut backoff = 1u64;
        let mut first = true;
        while !self.quit.load(Ordering::Relaxed) {
            self.emit(Event::Conn(if first { Conn::Connecting } else { Conn::Reconnecting }));
            first = false;
            match self.connect(credentials.clone()).await {
                Ok((live, task)) => {
                    let since = std::time::Instant::now();
                    // A one-time access token can't sign in twice; the session has
                    // stored reusable credentials by now, so reconnect with those.
                    if let Some(c) = live.session.cache().and_then(|c| c.credentials()) {
                        credentials = c;
                    }
                    self.emit(Event::User(live.session.username()));
                    self.emit(Event::Conn(Conn::Online));
                    self.live.send_replace(Some(live));
                    task.await;
                    self.live.send_replace(None);
                    log::warn!("connection to Spotify ended");
                    // A connection that dies straight away is not a recovery; keep
                    // backing off rather than reconnecting in a tight loop.
                    if since.elapsed() > Duration::from_secs(30) {
                        backoff = 1;
                    }
                }
                Err(e) if e.kind == ErrorKind::PermissionDenied => {
                    log::error!("login rejected: {e}");
                    self.emit(Event::Conn(Conn::LoginRejected));
                    return;
                }
                Err(e) => log::warn!("connect failed: {e}"),
            }
            if self.quit.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(20);
        }
    }

    async fn connect(
        &self,
        credentials: Credentials,
    ) -> Result<(Live, impl Future<Output = ()> + use<>), librespot_core::Error> {
        let cache = librespot_cache(&self.cfg).map_err(librespot_core::Error::unavailable)?;
        let session = Session::new(session_config(&self.cfg), Some(cache));

        // Subscribe before connecting so the very first state push isn't missed.
        let mut cluster = session
            .dealer()
            .listen_for("hm://connect-state/v1/cluster", Message::from_raw::<ClusterUpdate>)?;

        let mixer = Arc::new(SoftMixer::open(MixerConfig::default())?);
        let player_config = PlayerConfig {
            bitrate: match self.cfg.bitrate {
                96 => Bitrate::Bitrate96,
                160 => Bitrate::Bitrate160,
                _ => Bitrate::Bitrate320,
            },
            normalisation: self.cfg.normalize,
            ..Default::default()
        };

        let tap = self.tap.clone();
        let notice = self.tx.clone();
        let player = Player::new(
            player_config,
            session.clone(),
            mixer.get_soft_volume(),
            move || -> Box<dyn Sink> {
                // The audio backend panics when no output device exists. Catch that
                // so the app keeps working (silently) instead of losing its player.
                let opened = std::panic::catch_unwind(|| {
                    let backend = audio_backend::find(None)?;
                    Some(backend(None, AudioFormat::default()))
                });
                match opened {
                    Ok(Some(sink)) => Box::new(VizSink::new(sink, tap)),
                    _ => {
                        let _ = notice.send(Event::Notice(
                            "No audio output device found; playing silently".into(),
                        ));
                        Box::new(NullSink)
                    }
                }
            },
        );
        let mut events = player.get_player_event_channel();

        let connect_config = ConnectConfig {
            name: self.cfg.device_name.clone(),
            device_type: DeviceType::Computer,
            initial_volume: pct_to_vol(self.cfg.volume),
            ..Default::default()
        };
        let (spirc, task) =
            Spirc::new(connect_config, session.clone(), credentials, player, mixer).await?;

        let tx = self.tx.clone();
        let forward_player = tokio::spawn(async move {
            while let Some(ev) = events.recv().await {
                if let Some(ev) = translate(ev) {
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
            }
        });

        let tx = self.tx.clone();
        let this_device = session.device_id().to_string();
        let forward_cluster = tokio::spawn(async move {
            while let Some(update) = cluster.next().await {
                match update {
                    Ok(update) => {
                        if let Some(c) = update.cluster.as_ref() {
                            let _ = tx.send(Event::Cluster(summarise(c, &this_device)));
                        }
                    }
                    Err(e) => log::debug!("unreadable cluster update: {e}"),
                }
            }
        });

        let live = Live { session, spirc: Arc::new(spirc) };
        let done = async move {
            task.await;
            forward_player.abort();
            forward_cluster.abort();
        };
        Ok((live, done))
    }

    fn emit(&self, ev: Event) {
        let _ = self.tx.send(ev);
    }

    pub fn is_online(&self) -> bool {
        self.live.borrow().is_some()
    }

    /// The live session, waiting briefly if a (re)connect is in flight.
    pub async fn session(&self) -> Result<Session> {
        let mut rx = self.live.subscribe();
        let wait = async {
            loop {
                if let Some(live) = rx.borrow_and_update().as_ref() {
                    return Ok(live.session.clone());
                }
                if rx.changed().await.is_err() {
                    return Err(anyhow!("shutting down"));
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), wait)
            .await
            .map_err(|_| anyhow!("not connected to Spotify"))?
    }

    fn with<F: FnOnce(&Spirc) -> Result<(), librespot_core::Error>>(&self, f: F) -> Result<()> {
        let live = self.live.borrow().clone();
        match live {
            Some(live) => f(&live.spirc).map_err(|e| anyhow!("player error: {e}")),
            None => Err(anyhow!("still connecting to Spotify…")),
        }
    }

    pub fn play_pause(&self) -> Result<()> {
        self.with(|s| s.play_pause())
    }

    pub fn next(&self) -> Result<()> {
        self.with(|s| s.next())
    }

    pub fn prev(&self) -> Result<()> {
        self.with(|s| s.prev())
    }

    pub fn seek(&self, ms: u32) -> Result<()> {
        self.with(|s| s.set_position_ms(ms))
    }

    pub fn volume(&self, pct: u8) -> Result<()> {
        self.with(|s| s.set_volume(pct_to_vol(pct)))
    }

    pub fn shuffle(&self, on: bool) -> Result<()> {
        self.with(|s| s.shuffle(on))
    }

    pub fn repeat(&self, mode: &Repeat) -> Result<()> {
        self.with(|s| {
            s.repeat(*mode == Repeat::Context)?;
            s.repeat_track(*mode == Repeat::Track)
        })
    }

    fn options(&self, shuffle: bool, repeat: &Repeat, at: Option<PlayingTrack>) -> LoadRequestOptions {
        LoadRequestOptions {
            start_playing: true,
            seek_to: 0,
            context_options: Some(LoadContextOptions::Options(Options {
                shuffle,
                repeat: *repeat == Repeat::Context,
                repeat_track: *repeat == Repeat::Track,
            })),
            playing_track: at,
        }
    }

    /// Play a list of tracks, starting at `index`.
    pub fn play_tracks(
        &self,
        uris: Vec<String>,
        index: usize,
        shuffle: bool,
        repeat: &Repeat,
    ) -> Result<()> {
        let at = Some(PlayingTrack::Index(index as u32));
        let request = LoadRequest::from_tracks(uris, self.options(shuffle, repeat, at));
        self.with(|s| {
            // `load` is ignored unless this device is the active one.
            s.activate()?;
            s.load(request)
        })
    }

    /// Pull whatever is playing on another device over to this one.
    pub fn take_over(&self) -> Result<()> {
        self.with(|s| s.transfer(None))
    }

    pub fn shutdown(&self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(live) = self.live.borrow().as_ref() {
            let _ = live.spirc.shutdown();
        }
    }
}

fn translate(ev: PlayerEvent) -> Option<Event> {
    let uri = |u: &librespot_core::SpotifyUri| u.to_uri().unwrap_or_default();
    Some(match ev {
        PlayerEvent::TrackChanged { audio_item } => {
            // ~300px cover: plenty for a terminal.
            let image = audio_item
                .covers
                .iter()
                .min_by_key(|c| (c.width - 300).abs())
                .map(|c| c.url.clone());
            let (artists, album) = match &audio_item.unique_fields {
                UniqueFields::Track { artists, album, .. } => (
                    artists
                        .iter()
                        .map(|a| ArtistRef {
                            id: a.id.to_id().unwrap_or_default(),
                            name: a.name.clone(),
                        })
                        .collect(),
                    album.clone(),
                ),
                UniqueFields::Episode { show_name, .. } => (
                    vec![ArtistRef { id: String::new(), name: show_name.clone() }],
                    "Podcast".to_string(),
                ),
                UniqueFields::Local { artists, album, .. } => (
                    vec![ArtistRef {
                        id: String::new(),
                        name: artists.clone().unwrap_or_default(),
                    }],
                    album.clone().unwrap_or_default(),
                ),
            };
            Event::Track(Track {
                id: id_of(&audio_item.uri).to_string(),
                uri: audio_item.uri.clone(),
                name: audio_item.name.clone(),
                artists,
                album,
                album_id: String::new(),
                duration_ms: audio_item.duration_ms,
                explicit: audio_item.is_explicit,
                image,
                added_at: None,
                playable: true,
            })
        }
        PlayerEvent::Playing { track_id, position_ms, .. } => {
            Event::Playing { uri: uri(&track_id), pos_ms: position_ms }
        }
        PlayerEvent::Paused { track_id, position_ms, .. } => {
            Event::Paused { uri: uri(&track_id), pos_ms: position_ms }
        }
        PlayerEvent::Loading { track_id, position_ms, .. } => {
            Event::Loading { uri: uri(&track_id), pos_ms: position_ms }
        }
        PlayerEvent::Seeked { position_ms, .. }
        | PlayerEvent::PositionCorrection { position_ms, .. }
        | PlayerEvent::PositionChanged { position_ms, .. } => Event::Position(position_ms),
        PlayerEvent::Stopped { .. } => Event::Stopped,
        PlayerEvent::Unavailable { .. } => Event::Unavailable,
        PlayerEvent::VolumeChanged { volume } => Event::Volume(vol_to_pct(volume)),
        PlayerEvent::ShuffleChanged { shuffle } => Event::Shuffle(shuffle),
        PlayerEvent::RepeatChanged { context, track } => Event::Repeat(if track {
            Repeat::Track
        } else if context {
            Repeat::Context
        } else {
            Repeat::Off
        }),
        _ => return None,
    })
}

fn summarise(c: &librespot_protocol::connect::Cluster, this_device: &str) -> Cluster {
    let mut devices: Vec<Device> = c
        .device
        .iter()
        .map(|(id, d)| Device {
            id: id.clone(),
            name: d.name.clone(),
            kind: format!("{:?}", d.device_type.enum_value_or_default()).to_lowercase(),
            volume: vol_to_pct(d.volume.min(65535) as u16),
            active: *id == c.active_device_id,
            this: id == this_device,
        })
        .collect();
    devices.sort_by(|a, b| b.this.cmp(&a.this).then(a.name.cmp(&b.name)));

    let ps = c.player_state.get_or_default();
    let opts = ps.options.get_or_default();
    Cluster {
        devices,
        track_uri: ps.track.get_or_default().uri.clone(),
        context_uri: ps.context_uri.clone(),
        playing: ps.is_playing && !ps.is_paused,
        position_ms: ps.position_as_of_timestamp.max(0) as u32,
        timestamp_ms: ps.timestamp,
        duration_ms: ps.duration.max(0) as u32,
        shuffle: opts.shuffling_context,
        repeat: if opts.repeating_track {
            Repeat::Track
        } else if opts.repeating_context {
            Repeat::Context
        } else {
            Repeat::Off
        },
        next: ps
            .next_tracks
            .iter()
            .filter(|t| t.uri.starts_with("spotify:track:") || t.uri.starts_with("spotify:episode:"))
            .take(60)
            .map(|t| (t.uri.clone(), t.provider == "queue"))
            .collect(),
    }
}
