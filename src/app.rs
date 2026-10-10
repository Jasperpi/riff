//! Application state and behaviour: what every key, click and background
//! message does. Drawing lives in `ui`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use image::RgbImage;
use ratatui::crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::Rect;

use crate::api::RemoteCmd;
use crate::art::{ArtMode, Palette, Rendered};
use crate::backend::{Backend, Head, Msg, PageUpdate};
use crate::config::Config;
use crate::engine::{Conn, Event};
use crate::model::*;
use crate::audio::{BANDS, Hub};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Sidebar,
    Main,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Browse,
    NowPlaying,
    /// The two DJ decks.
    Decks,
}

/// A run of rows being selected with V, vim-visual style.
struct Range {
    page: u64,
    tab: usize,
    anchor: usize,
    /// Songs this run has added, so shrinking it takes them back out.
    added: Vec<String>,
}

pub enum Overlay {
    None,
    /// The key list, which takes more than one page in a short window.
    Help { page: usize },
    Search { input: String },
    Filter,
    Queue { sel: usize },
    Devices { sel: usize },
}

/// Something on screen that reacts to the mouse.
#[derive(Clone, Debug)]
pub enum Hit {
    Side(usize),
    Row(usize),
    Tab(usize),
    Seek,
    PlayPause,
    MainPane,
    SidePane,
    OverlayRow(usize),
    NowPlayingArt,
}

pub struct Tab {
    pub label: &'static str,
    pub items: Vec<Item>,
    /// Indices into `items` that pass the filter, in display order.
    pub view: Vec<usize>,
    pub filter: String,
    pub sel: usize,
    pub top: usize,
    pub loading: bool,
    /// Nothing more to fetch.
    pub done: bool,
    pub error: Option<String>,
}

impl Tab {
    fn new(label: &'static str) -> Self {
        Self {
            label,
            items: Vec::new(),
            view: Vec::new(),
            filter: String::new(),
            sel: 0,
            top: 0,
            loading: true,
            done: false,
            error: None,
        }
    }

    pub fn refilter(&mut self) {
        let needle = self.filter.to_lowercase();
        let words: Vec<&str> = needle.split_whitespace().collect();
        self.view = if words.is_empty() {
            (0..self.items.len()).collect()
        } else {
            self.items
                .iter()
                .enumerate()
                .filter(|(_, it)| {
                    let hay = it.haystack().to_lowercase();
                    words.iter().all(|w| hay.contains(w))
                })
                .map(|(i, _)| i)
                .collect()
        };
        self.sel = self.sel.min(self.view.len().saturating_sub(1));
    }

    pub fn selected(&self) -> Option<&Item> {
        self.items.get(*self.view.get(self.sel)?)
    }

    fn step(&mut self, delta: isize) {
        let last = self.view.len().saturating_sub(1) as isize;
        self.sel = (self.sel as isize + delta).clamp(0, last.max(0)) as usize;
    }
}

pub struct Page {
    pub id: u64,
    pub spec: PageSpec,
    pub head: Head,
    pub tabs: Vec<Tab>,
    pub tab: usize,
}

impl Page {
    pub fn cur(&self) -> &Tab {
        &self.tabs[self.tab]
    }

    pub fn cur_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.tab]
    }
}

#[derive(Default)]
pub struct Playback {
    pub track: Option<Track>,
    pub playing: bool,
    pub loading: bool,
    pos_ms: u32,
    pos_at: Option<Instant>,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub volume: u8,
    pub context_uri: String,
    /// The page local playback was started from.
    pub source: Option<PageSpec>,
    /// The tracks handed to the player, in order, for working out what's next.
    pub list: Vec<String>,
    /// This app is the device making sound.
    pub local: bool,
    /// Name of another device that is playing, if any.
    pub remote_name: Option<String>,
    pub devices: Vec<Device>,
}

impl Playback {
    pub fn position(&self) -> u32 {
        let dur = self.track.as_ref().map(|t| t.duration_ms).unwrap_or(0);
        let mut pos = self.pos_ms;
        if self.playing && !self.loading {
            if let Some(at) = self.pos_at {
                pos = pos.saturating_add(at.elapsed().as_millis().min(u32::MAX as u128) as u32);
            }
        }
        if dur > 0 { pos.min(dur) } else { pos }
    }

    pub fn set_position(&mut self, ms: u32) {
        self.pos_ms = ms;
        self.pos_at = Some(Instant::now());
    }

    pub fn uri(&self) -> &str {
        self.track.as_ref().map(|t| t.uri.as_str()).unwrap_or("")
    }
}

pub enum LyricState {
    Idle,
    Loading,
    Missing,
    Ready(Lyrics),
}

pub struct Toast {
    pub text: String,
    pub error: bool,
    until: Instant,
}

pub struct App {
    pub cfg: Config,
    pub backend: Option<Backend>,
    pub quit: bool,
    pub dirty: bool,
    pub screen: Screen,
    pub focus: Focus,
    pub overlay: Overlay,

    pub sidebar: Vec<SideEntry>,
    pub side_sel: usize,
    pub side_top: usize,
    /// Keep the sidebar selection in view (off while the wheel scrolls freely).
    pub side_follow: bool,
    pub pages: Vec<Page>,
    next_page: u64,

    pub pb: Playback,
    pub liked: HashSet<String>,
    pub queue: Vec<Track>,
    queue_uris: Vec<String>,
    /// Device the user explicitly sent playback to; `None` means play here.
    target_remote: Option<String>,

    pub images: HashMap<String, Arc<RgbImage>>,
    pub thumbs: HashMap<String, Arc<RgbImage>>,
    image_order: VecDeque<String>,
    image_pending: HashSet<String>,
    /// Covers that wouldn't load, and when, so they are retried later
    /// rather than on every redraw.
    image_failed: HashMap<String, Instant>,
    /// How near each part of a cover is, for the depth cover style, and the
    /// covers being worked on or that couldn't be done.
    pub depths: HashMap<String, Arc<crate::art::DepthMap>>,
    depth_pending: HashSet<String>,
    depth_failed: HashMap<String, Instant>,
    pub art_mode: ArtMode,
    pub art_cache: HashMap<(String, u16, u16, ArtMode), Arc<Rendered>>,
    pub palette: Palette,
    palette_target: Palette,
    palette_for: String,

    pub lyrics: LyricState,
    lyrics_for: String,
    /// Manual scroll offset in the lyrics pane; `None` follows the music.
    pub lyrics_scroll: Option<usize>,

    pub hub: Arc<Hub>,
    pub bars: [f32; BANDS],
    /// 1.0 at the instant of a beat, decaying towards 0.
    pub beat: f32,
    /// Beats heard so far; drives colour changes in party mode.
    pub beats: u64,
    /// Smoothed low-end energy, 0..1.
    pub bass: f32,
    /// Animation time in seconds; runs faster when the music is louder.
    pub clock: f32,
    last_tick: Instant,
    pub fixed_step: Option<f32>,
    /// The whole interface dances to the music.
    pub party: bool,

    /// Songs marked with x / ctrl-click. Kept across pages and searches so a
    /// queue can be assembled from anywhere.
    pub basket: Vec<Track>,
    /// Songs added to the queue by hand, in order, until each one plays.
    queued: VecDeque<String>,
    range: Option<Range>,
    /// The DJ decks, when they are switched on.
    pub dj: Option<crate::dj::Dj>,
    /// What the decks looked like at the last tick.
    pub decks: crate::dj::View,
    /// The deck that seek, tempo and space act on.
    pub deck_focus: usize,
    deck_demo: bool,
    /// The last key that costs Spotify a request, and when, to tell a held
    /// key's repeats from separate presses.
    last_command: Option<(KeyCode, Instant)>,
    /// A seek waiting for the key to be released, so scrubbing sends one request.
    pending_seek: Option<(u32, Instant)>,
    /// Likewise for volume: heard at once, reported to Spotify when it settles.
    pending_volume: Option<(u8, Instant)>,
    /// The volume changed while this device wasn't the active one; Spotify
    /// still needs telling when it next becomes active.
    volume_unsent: bool,
    /// Trust the optimistic position until the player has caught up with a seek.
    seek_hold: Option<Instant>,
    /// Where cover art was drawn this frame.
    pub art_rects: Vec<Rect>,
    pub toast: Option<Toast>,
    pub conn: Conn,
    pub user: String,

    pub hits: Vec<(Rect, Hit)>,
    last_click: Option<(Instant, u16, u16)>,
    last_shown_sec: u32,
    pub frame: u64,
    /// Demo mode fakes playback so the interface can be explored offline.
    pub demo: bool,
    pub mpris: Option<crate::mpris::Mpris>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn tabs_for(spec: &PageSpec, web_api: bool) -> Vec<Tab> {
    match spec {
        PageSpec::Artist(_) => {
            vec![Tab::new("Top Tracks"), Tab::new("Albums"), Tab::new("Singles & EPs")]
        }
        PageSpec::Search(_) => {
            let mut tabs = vec![Tab::new("Tracks"), Tab::new("Albums"), Tab::new("Artists")];
            // Playlist search exists only on the Web API.
            if web_api {
                tabs.push(Tab::new("Playlists"));
            }
            tabs
        }
        _ => vec![Tab::new("")],
    }
}

impl App {
    pub fn new(cfg: Config, backend: Option<Backend>, hub: Arc<Hub>) -> Self {
        let art_mode = ArtMode::parse(&cfg.art);
        let volume = cfg.volume;
        let mut app = Self {
            cfg,
            backend,
            quit: false,
            dirty: true,
            screen: Screen::Browse,
            focus: Focus::Main,
            overlay: Overlay::None,
            sidebar: Vec::new(),
            side_sel: 1,
            side_top: 0,
            side_follow: true,
            pages: Vec::new(),
            next_page: 1,
            pb: Playback { volume, ..Default::default() },
            liked: HashSet::new(),
            queue: Vec::new(),
            queue_uris: Vec::new(),
            target_remote: None,
            images: HashMap::new(),
            thumbs: HashMap::new(),
            image_order: VecDeque::new(),
            image_pending: HashSet::new(),
            image_failed: HashMap::new(),
            depths: HashMap::new(),
            depth_pending: HashSet::new(),
            depth_failed: HashMap::new(),
            art_mode,
            art_cache: HashMap::new(),
            palette: Palette::default(),
            palette_target: Palette::default(),
            palette_for: String::new(),
            lyrics: LyricState::Idle,
            lyrics_for: String::new(),
            lyrics_scroll: None,
            hub,
            bars: [0.0; BANDS],
            beat: 0.0,
            beats: 0,
            bass: 0.0,
            clock: 0.0,
            last_tick: Instant::now(),
            fixed_step: None,
            party: false,
            basket: Vec::new(),
            queued: VecDeque::new(),
            range: None,
            dj: None,
            decks: Default::default(),
            deck_focus: 0,
            deck_demo: false,
            last_command: None,
            pending_seek: None,
            pending_volume: None,
            volume_unsent: false,
            seek_hold: None,
            art_rects: Vec::new(),
            toast: None,
            conn: Conn::Connecting,
            user: String::new(),
            hits: Vec::new(),
            last_click: None,
            last_shown_sec: u32::MAX,
            frame: 0,
            demo: false,
            mpris: None,
        };
        app.sidebar = app.base_sidebar();
        if let Some(be) = app.backend.clone() {
            be.load_sidebar();
            be.seed_remote();
        }
        app.open_root(PageSpec::Liked);
        app
    }

    // ---- small helpers ------------------------------------------------

    pub fn toast(&mut self, text: impl Into<String>, error: bool) {
        let secs = if error { 6 } else { 3 };
        self.toast = Some(Toast {
            text: text.into(),
            error,
            until: Instant::now() + Duration::from_secs(secs),
        });
        self.dirty = true;
    }

    fn try_engine(&mut self, result: anyhow::Result<()>) {
        if let Err(e) = result {
            self.toast(e.to_string(), true);
        }
    }

    /// Is the Web API available (the user added their own Client ID)?
    pub fn web_api(&self) -> bool {
        self.demo || self.backend.as_ref().is_some_and(|b| b.api.is_some())
    }

    /// The fixed top of the sidebar. Lists only the Web API can fill are left
    /// out when it isn't configured, rather than shown and broken.
    fn base_sidebar(&self) -> Vec<SideEntry> {
        let mut entries = vec![
            SideEntry::Header("Library".into()),
            SideEntry::Liked,
            SideEntry::Albums,
            SideEntry::Artists,
        ];
        if self.backend.is_none() || self.web_api() {
            entries.push(SideEntry::Recent);
            entries.push(SideEntry::Top);
        }
        entries.push(SideEntry::Header("Playlists".into()));
        entries
    }

    pub fn page(&self) -> Option<&Page> {
        self.pages.last()
    }

    fn page_mut(&mut self) -> Option<&mut Page> {
        self.pages.last_mut()
    }

    pub fn is_liked(&self, uri: &str) -> bool {
        self.liked.contains(uri)
    }

    /// How long the event loop may sleep before the screen needs attention.
    pub fn tick_interval(&self) -> Duration {
        let animating = self.palette != self.palette_target
            || self.bars.iter().any(|b| *b > 0.01)
            || self.beat > 0.01;
        let decks_playing = self.decks.decks.iter().any(|d| d.playing);
        let sounding = (self.pb.playing && (self.pb.local || self.demo)) || decks_playing;
        let reactive =
            self.cfg.visualizer || self.party || matches!(self.art_mode, ArtMode::Pulse | ArtMode::Depth);
        if decks_playing && self.screen == Screen::Decks {
            Duration::from_millis(33)
        } else if self.party && sounding {
            // Party mode repaints the whole screen; 25 frames a second is plenty.
            Duration::from_millis(40)
        } else if animating || (sounding && reactive) {
            Duration::from_millis(33)
        } else if self.pending_seek.is_some() || self.pending_volume.is_some() {
            Duration::from_millis(50)
        } else if self.pb.playing || self.toast.is_some() {
            Duration::from_millis(200)
        } else {
            Duration::from_millis(1000)
        }
    }

    /// Playback position in milliseconds. While this app is the one playing,
    /// that is read from the audio output itself, so it is exact through
    /// seeks, pauses and blends between songs.
    pub fn position(&self) -> u32 {
        let dur = self.pb.track.as_ref().map(|t| t.duration_ms).unwrap_or(0);
        let held = self.seek_hold.is_some_and(|until| Instant::now() < until);
        if self.pb.local && !self.demo && !held {
            if let Some(ms) = self.hub.position(self.pb.uri()) {
                let ms = ms.max(0) as u32;
                return if dur > 0 { ms.min(dur) } else { ms };
            }
        }
        self.pb.position()
    }

    // ---- navigation ---------------------------------------------------

    fn new_page(&mut self, spec: PageSpec) -> Page {
        let id = self.next_page;
        self.next_page += 1;
        let title = match &spec {
            PageSpec::Liked => "Liked Songs",
            PageSpec::SavedAlbums => "Albums",
            PageSpec::FollowedArtists => "Artists",
            PageSpec::Recent => "Recently Played",
            PageSpec::Top => "On Repeat",
            _ => "",
        };
        if let Some(be) = &self.backend {
            be.load_page(id, spec.clone());
        }
        Page {
            id,
            tabs: tabs_for(&spec, self.web_api()),
            spec,
            head: Head { title: title.to_string(), ..Default::default() },
            tab: 0,
        }
    }

    /// Open a top-level page from the sidebar, replacing the history.
    pub fn open_root(&mut self, spec: PageSpec) {
        if self.pages.len() == 1 && self.pages[0].spec == spec {
            return;
        }
        self.range = None;
        let page = self.new_page(spec);
        self.pages = vec![page];
        self.dirty = true;
    }

    /// Drill into an album, artist, playlist or search; `back` returns.
    pub fn push(&mut self, spec: PageSpec) {
        // Asked for from the now-playing or decks screen, the page should be
        // shown even when it is the one already open underneath.
        self.screen = Screen::Browse;
        self.focus = Focus::Main;
        self.dirty = true;
        if self.page().is_some_and(|p| p.spec == spec) {
            return;
        }
        // A run being selected belongs to the page it was started on.
        self.range = None;
        let page = self.new_page(spec);
        self.pages.push(page);
        if self.pages.len() > 40 {
            self.pages.remove(1);
        }
    }

    fn back(&mut self) {
        if self.range.take().is_some() {
            // Esc ends a run being selected; the songs stay selected.
        } else if self.screen != Screen::Browse {
            self.screen = Screen::Browse;
        } else if self.page().is_some_and(|p| !p.cur().filter.is_empty()) {
            let tab = self.page_mut().unwrap().cur_mut();
            tab.filter.clear();
            tab.refilter();
        } else if self.pages.len() > 1 {
            self.pages.pop();
        } else {
            self.focus = Focus::Sidebar;
        }
    }

    fn refresh(&mut self) {
        let Some(page) = self.pages.pop() else { return };
        let fresh = self.new_page(page.spec);
        self.pages.push(fresh);
        if let Some(be) = &self.backend {
            be.load_sidebar();
        }
        self.toast("Refreshing…", false);
    }

    fn side_activate(&mut self) {
        let spec = match self.sidebar.get(self.side_sel) {
            Some(SideEntry::Liked) => PageSpec::Liked,
            Some(SideEntry::Albums) => PageSpec::SavedAlbums,
            Some(SideEntry::Artists) => PageSpec::FollowedArtists,
            Some(SideEntry::Recent) => PageSpec::Recent,
            Some(SideEntry::Top) => PageSpec::Top,
            Some(SideEntry::Playlist { playlist, .. }) => PageSpec::Playlist(playlist.id.clone()),
            _ => return,
        };
        // Show the name immediately; the loader fills in the rest.
        let title = match self.sidebar.get(self.side_sel) {
            Some(SideEntry::Playlist { playlist, .. }) => Some(playlist.name.clone()),
            _ => None,
        };
        self.open_root(spec);
        if let (Some(t), Some(p)) = (title, self.page_mut()) {
            if p.head.title.is_empty() {
                p.head.title = t;
            }
        }
        self.screen = Screen::Browse;
        self.focus = Focus::Main;
    }

    fn side_step(&mut self, delta: isize) {
        self.side_follow = true;
        let n = self.sidebar.len() as isize;
        let mut i = self.side_sel as isize;
        loop {
            i += delta.signum();
            if i < 0 || i >= n {
                return;
            }
            if self.sidebar[i as usize].selectable() {
                break;
            }
        }
        // Larger jumps (page up/down) repeat single steps so they never land on a header.
        self.side_sel = i as usize;
        if delta.abs() > 1 {
            self.side_step(delta - delta.signum());
        }
    }

    // ---- playback -----------------------------------------------------

    /// Should commands go to another device over the Web API?
    fn remote_active(&self) -> bool {
        !self.pb.local && self.pb.remote_name.is_some()
    }

    fn play_pause(&mut self) {
        if self.decks_on() {
            return self.deck_toggle(self.deck_focus);
        }
        if self.demo {
            self.pb.set_position(self.pb.position());
            self.pb.playing = !self.pb.playing;
            return;
        }
        let Some(be) = self.backend.clone() else { return };
        if self.pb.local {
            let r = be.engine.play_pause();
            self.try_engine(r);
        } else if self.remote_active() && self.pb.playing {
            let pos = self.pb.position();
            be.remote(RemoteCmd::Pause);
            self.pb.playing = false;
            self.pb.set_position(pos);
        } else if self.remote_active() {
            let pos = self.pb.position();
            be.remote(RemoteCmd::Resume);
            self.pb.playing = true;
            self.pb.set_position(pos);
        } else if self.pb.track.is_some() {
            self.resume_here();
        } else {
            self.toast("Nothing playing yet. Pick a track and press Enter", false);
        }
    }

    /// Nothing is playing anywhere, but we know the last song: carry on with it
    /// on this device, from where it left off.
    fn resume_here(&mut self) {
        let (Some(be), Some(track)) = (self.backend.clone(), self.pb.track.clone()) else { return };
        if !track.uri.starts_with("spotify:track:") && !track.uri.starts_with("spotify:episode:") {
            return;
        }
        let at = self.pb.position();
        let at = if at + 5_000 >= track.duration_ms { 0 } else { at };
        self.target_remote = None;
        self.pb.remote_name = None;
        match be.engine.play_tracks(vec![track.uri.clone()], 0, at, false, &self.pb.repeat) {
            Ok(()) => {
                self.sync_volume(&be);
                self.pb.loading = true;
                self.pb.local = true;
                self.pb.list = vec![track.uri];
                self.pb.source = None;
                self.pb.set_position(at);
                self.seek_hold = Some(Instant::now() + Duration::from_millis(1500));
            }
            Err(e) => self.toast(e.to_string(), true),
        }
    }

    fn skip(&mut self, forward: bool) {
        if self.decks_on() {
            return self.toast("The decks are on: m mixes in the other deck. D on the decks screen switches them off", false);
        }
        let Some(be) = self.backend.clone() else { return };
        if self.pb.local {
            let r = if forward { be.engine.next() } else { be.engine.prev() };
            self.try_engine(r);
        } else if self.remote_active() {
            be.remote(if forward { RemoteCmd::Next } else { RemoteCmd::Prev });
        }
    }

    fn seek_by(&mut self, delta_ms: i64) {
        if self.decks_on() {
            if let Some(dj) = self.dj.as_mut() {
                dj.seek_by(self.deck_focus, delta_ms);
            }
            return;
        }
        let Some(track) = &self.pb.track else { return };
        let dur = track.duration_ms as i64;
        let target = (self.position() as i64 + delta_ms).clamp(0, (dur - 1000).max(0)) as u32;
        self.seek_to(target);
    }

    /// Move the playhead on screen now; tell the player once the key is let go.
    /// Holding a seek key then costs one request instead of thirty a second,
    /// which Spotify answers with rate limiting.
    fn seek_to(&mut self, ms: u32) {
        self.pb.set_position(ms);
        self.lyrics_scroll = None;
        self.seek_hold = Some(Instant::now() + Duration::from_millis(900));
        self.pending_seek = Some((ms, Instant::now()));
    }

    fn flush_seek(&mut self) {
        let Some((ms, at)) = self.pending_seek else { return };
        if at.elapsed() < Duration::from_millis(200) {
            return;
        }
        self.pending_seek = None;
        self.seek_hold = Some(Instant::now() + Duration::from_millis(700));
        let Some(be) = self.backend.clone() else { return };
        if self.pb.local {
            let r = be.engine.seek(ms);
            self.try_engine(r);
        } else if self.remote_active() {
            be.remote(RemoteCmd::Seek(ms));
        }
    }

    /// Volume is changed in two steps: the sound changes at once, locally,
    /// and Spotify is told once the level has stopped moving. Reporting every
    /// step of a held key is what got the player rate limited.
    fn volume_by(&mut self, delta: i16) {
        let v = (self.pb.volume as i16 + delta).clamp(0, 100) as u8;
        self.pb.volume = v;
        // Only our own level is saved; another device's is its own business.
        if !self.remote_active() {
            self.cfg.volume = v;
        }
        self.pending_volume = Some((v, Instant::now()));
        if let Some(be) = &self.backend {
            if !self.remote_active() {
                be.engine.volume_local(v);
            }
        }
    }

    fn flush_volume(&mut self) {
        let Some((v, at)) = self.pending_volume else { return };
        if at.elapsed() < Duration::from_millis(350) {
            return;
        }
        self.pending_volume = None;
        let Some(be) = self.backend.clone() else { return };
        if self.remote_active() {
            be.remote(RemoteCmd::Volume(v));
        } else if self.pb.local {
            let _ = be.engine.volume(v);
        } else {
            self.volume_unsent = true;
        }
    }

    /// Called when starting playback: make sure the level set while idle is
    /// both what is heard and what Spotify believes.
    fn sync_volume(&mut self, be: &Backend) {
        // While another device had the music the level shown was that
        // device's. Playing here again goes back to ours.
        self.pb.volume = self.cfg.volume;
        be.engine.volume_local(self.pb.volume);
        if std::mem::take(&mut self.volume_unsent) {
            let _ = be.engine.volume(self.pb.volume);
        }
    }

    fn toggle_shuffle(&mut self) {
        let on = !self.pb.shuffle;
        self.pb.shuffle = on;
        if let Some(be) = self.backend.clone() {
            if self.pb.local {
                let r = be.engine.shuffle(on);
                self.try_engine(r);
            } else if self.remote_active() {
                be.remote(RemoteCmd::Shuffle(on));
            }
        }
        self.toast(if on { "Shuffle on" } else { "Shuffle off" }, false);
    }

    fn cycle_repeat(&mut self) {
        let mode = match self.pb.repeat {
            Repeat::Off => Repeat::Context,
            Repeat::Context => Repeat::Track,
            Repeat::Track => Repeat::Off,
        };
        self.pb.repeat = mode.clone();
        if let Some(be) = self.backend.clone() {
            if self.pb.local {
                let r = be.engine.repeat(&mode);
                self.try_engine(r);
            } else if self.remote_active() {
                be.remote(RemoteCmd::Repeat(mode.clone()));
            }
        }
        self.toast(
            match mode {
                Repeat::Off => "Repeat off",
                Repeat::Context => "Repeat all",
                Repeat::Track => "Repeat one",
            },
            false,
        );
    }

    /// Enter on the selected row: play a track, or open whatever else it is.
    fn activate(&mut self) {
        let Some(page) = self.page() else { return };
        let tab = page.cur();
        let Some(item) = tab.selected().cloned() else { return };
        let track = match item {
            Item::Album(a) => return self.push(PageSpec::Album(a.id)),
            Item::Artist(a) => return self.push(PageSpec::Artist(a.id)),
            Item::Playlist(p) => return self.push(PageSpec::Playlist(p.id)),
            Item::Track(t) => t,
        };
        if !track.playable {
            return self.toast("That track isn't available", true);
        }
        if self.decks_on() {
            return self.deck_load(None, track);
        }
        if !self.basket.is_empty() {
            self.range = None;
            return self.play_basket();
        }
        if self.demo {
            return self.demo_play(track);
        }

        // Every track visible in this list, and where the chosen one sits in it.
        let mut uris = Vec::new();
        let mut index = 0;
        for (row, &i) in tab.view.iter().enumerate() {
            if let Item::Track(t) = &tab.items[i] {
                if t.playable {
                    // By row, not by song: a playlist can hold the same one twice.
                    if row == tab.sel {
                        index = uris.len();
                    }
                    uris.push(t.uri.clone());
                }
            }
        }
        // Playback always starts from the list on screen, by position. That is exact
        // even deep inside a huge playlist, where resolving a track by URI can miss.
        let context = match (&page.spec, tab.filter.is_empty()) {
            (PageSpec::Playlist(_) | PageSpec::Album(_), true) => page.head.context.clone(),
            _ => None,
        };
        let source = page.spec.clone();
        const WINDOW: usize = 1500;
        if uris.len() > WINDOW {
            let start = index.saturating_sub(100).min(uris.len() - WINDOW);
            uris = uris[start..start + WINDOW].to_vec();
            index -= start;
        }

        let Some(be) = self.backend.clone() else { return };
        let (shuffle, repeat) = (self.pb.shuffle, self.pb.repeat.clone());
        if self.target_remote.is_some() && self.remote_active() {
            // The Web API resolves contexts server-side, so they're safe to use here.
            be.remote(match context {
                Some(context_uri) => {
                    RemoteCmd::PlayContext { context_uri, track_uri: Some(track.uri.clone()) }
                }
                None => {
                    let start = index.saturating_sub(50).min(uris.len().saturating_sub(300));
                    let end = (start + 300).min(uris.len());
                    RemoteCmd::PlayTracks { uris: uris[start..end].to_vec(), index: index - start }
                }
            });
            return;
        }
        self.target_remote = None;
        match be.engine.play_tracks(uris.clone(), index, 0, shuffle, &repeat) {
            Ok(()) => {
                // Reflect the choice instantly; the player confirms within moments.
                self.pb.loading = true;
                self.pb.source = Some(source);
                self.pb.list = uris;
                self.queued.clear();
                // Volume changes made before this device was active were not applied.
                self.sync_volume(&be);
                self.set_track(track, true);
                self.pb.set_position(0);
            }
            Err(e) => self.toast(e.to_string(), true),
        }
    }

    // ---- selection and queue ------------------------------------------

    pub fn is_marked(&self, uri: &str) -> bool {
        self.basket.iter().any(|t| t.uri == uri)
    }

    fn set_mark(&mut self, track: Track, on: bool) {
        if !on {
            self.basket.retain(|t| t.uri != track.uri);
        } else if track.playable && !self.is_marked(&track.uri) {
            // One already picked keeps its place in the order.
            self.basket.push(track);
        }
    }

    /// x: mark or unmark the row under the cursor and move on, so a run of
    /// songs is `x x x`. With `extend` (shift-arrows) rows are only ever added.
    fn mark_selected(&mut self, step: isize, extend: bool) {
        let Some(page) = self.pages.last_mut() else { return };
        let tab = page.cur_mut();
        let picked = match tab.selected() {
            Some(Item::Track(t)) => Some(t.clone()),
            _ => None,
        };
        tab.step(step);
        match picked {
            Some(t) => {
                let on = extend || !self.is_marked(&t.uri);
                self.set_mark(t, on);
            }
            None => self.toast("Only songs can be selected", false),
        }
    }

    /// V: start (or finish) selecting a run. While it is on, every row the
    /// cursor passes over between the starting row and here is selected.
    fn toggle_range(&mut self) {
        if self.range.take().is_some() {
            let n = self.basket.len();
            return self.toast(format!("{n} selected. a queues them, enter plays them"), false);
        }
        let Some(page) = self.pages.last() else { return };
        self.range = Some(Range { page: page.id, tab: page.tab, anchor: page.cur().sel, added: Vec::new() });
        self.update_range();
    }

    /// Bring the selection in line with the run between the anchor and the cursor.
    fn update_range(&mut self) {
        let Some(range) = self.range.take() else { return };
        let Some(page) = self.pages.last() else { return };
        if page.id != range.page || page.tab != range.tab {
            // Moved somewhere else: the run is finished, its songs stay selected.
            return;
        }
        let tab = page.cur();
        let (lo, hi) = (range.anchor.min(tab.sel), range.anchor.max(tab.sel));
        let wanted: Vec<Track> = (lo..=hi)
            .filter_map(|row| match tab.items.get(*tab.view.get(row)?) {
                Some(Item::Track(t)) if t.playable => Some(t.clone()),
                _ => None,
            })
            .collect();
        // Rows the run has backed off from are unselected again.
        for uri in &range.added {
            if !wanted.iter().any(|t| &t.uri == uri) {
                self.basket.retain(|t| &t.uri != uri);
            }
        }
        let mut added = Vec::new();
        for t in wanted {
            let ours = range.added.contains(&t.uri);
            if ours || !self.is_marked(&t.uri) {
                added.push(t.uri.clone());
            }
            if !self.is_marked(&t.uri) {
                self.basket.push(t);
            }
        }
        self.range = Some(Range { added, ..range });
    }

    pub fn selecting_range(&self) -> bool {
        self.range.is_some()
    }

    // ---- DJ decks -----------------------------------------------------

    pub fn decks_on(&self) -> bool {
        self.dj.is_some() || self.deck_demo
    }

    /// Switch the decks on. Whatever is playing carries on from deck A.
    fn decks_start(&mut self) -> bool {
        if self.decks_on() {
            return true;
        }
        if self.demo {
            self.deck_demo = true;
            return true;
        }
        let Some(be) = self.backend.clone() else { return false };
        let Some((session, volume)) = be.engine.deck_parts() else {
            self.toast("Still connecting to Spotify…", true);
            return false;
        };
        let mut dj = crate::dj::Dj::start(session, volume, self.hub.clone(), be.engine.bitrate(), be.engine.normalize());
        if self.pb.local && self.pb.playing {
            if let Some(track) = self.pb.track.clone() {
                let at = self.position();
                let _ = be.engine.play_pause();
                let _ = dj.load(0, track, at, true);
            }
        }
        self.dj = Some(dj);
        self.deck_focus = 0;
        true
    }

    fn decks_stop(&mut self) {
        self.dj = None;
        self.deck_demo = false;
        self.decks = Default::default();
        if self.screen == Screen::Decks {
            self.screen = Screen::Browse;
        }
        self.toast("Decks off. Space carries on where you were", false);
    }

    /// D: open the decks (switching them on if need be); on the decks screen, switch them off.
    fn decks_key(&mut self) {
        if self.screen == Screen::Decks {
            return self.decks_stop();
        }
        if self.decks_start() {
            self.screen = Screen::Decks;
            if self.decks.decks.iter().all(|d| d.track.is_none()) && self.dj.as_ref().is_none_or(|d| !d.is_loaded(0)) {
                self.toast("Decks on. Go back (esc) and press 1 or 2 on a song to load deck A or B", false);
            }
        }
    }

    /// Put a song on a deck. With `deck` unset it goes to whichever deck is
    /// free, or failing that the one not being heard.
    fn deck_load(&mut self, deck: Option<usize>, track: Track) {
        if !track.uri.starts_with("spotify:track:") {
            return self.toast("Only songs can go on a deck", false);
        }
        if !self.decks_start() {
            return;
        }
        if let Some(url) = track.image.clone() {
            self.want_image(&url);
        }
        let name = track.name.clone();
        if self.deck_demo {
            let d = deck.unwrap_or(if self.decks.decks[0].track.is_none() { 0 } else { 1 });
            self.decks.decks[d].track = Some(track);
            return self.toast(format!("Deck {}: {name}", crate::dj::deck_name(d)), false);
        }
        let Some(dj) = self.dj.as_mut() else { return };
        let d = deck.unwrap_or_else(|| {
            if !dj.is_loaded(0) {
                0
            } else if !dj.is_loaded(1) {
                1
            } else {
                1 - dj.live_deck()
            }
        });
        // A song waits, cued, while the other deck is being heard. Otherwise
        // it starts straight away, so loading the deck that is playing swaps
        // the song rather than stopping the music.
        let play = !dj.is_playing(1 - d);
        let result = dj.load(d, track, 0, play);
        let which = crate::dj::deck_name(d);
        match result {
            Ok(()) if play => self.toast(format!("Deck {which}: {name}"), false),
            Ok(()) => self.toast(format!("Deck {which}: {name}, cued. m mixes it in"), false),
            Err(e) => self.toast(e.to_string(), true),
        }
    }

    /// Turn one band of a deck's EQ to `step`.
    fn deck_eq(&mut self, deck: usize, band: usize, step: i8, say: bool) {
        let step = step.clamp(crate::dj::EQ_KILL, crate::dj::EQ_MAX);
        if let Some(dj) = &self.dj {
            dj.set_eq(deck, band, step);
        }
        // Shown at once, and all there is for the pretend decks of demo mode.
        self.decks.decks[deck].eq[band] = step;
        if say {
            let (name, band) = (crate::dj::deck_name(deck), crate::dj::BAND_NAMES[band]);
            self.toast(format!("Deck {name} {band}: {}", crate::dj::eq_label(step)), false);
        }
    }

    /// m with the decks on: bring in the deck that isn't being heard.
    fn deck_mix(&mut self) {
        let seconds = self.cfg.mix_seconds.max(3) as f32;
        match self.dj.as_mut().map(|dj| dj.mix(seconds)) {
            Some(Ok(to)) => self.toast(format!("Mixing in deck {} over {seconds:.0} s", crate::dj::deck_name(to)), false),
            Some(Err(e)) => self.toast(e.to_string(), true),
            None => {}
        }
    }

    fn deck_toggle(&mut self, deck: usize) {
        self.deck_focus = deck;
        if self.deck_demo {
            let d = &mut self.decks.decks[deck];
            d.playing = d.track.is_some() && !d.playing;
            return;
        }
        let Some(dj) = self.dj.as_mut() else { return };
        if dj.is_loaded(deck) {
            dj.toggle(deck);
        } else {
            let which = crate::dj::deck_name(deck);
            self.toast(format!("Deck {which} is empty: press {} on a song to load it", deck + 1), false);
        }
    }

    /// Keys on the decks screen. Returns whether the key was one of them.
    fn on_decks_key(&mut self, key: KeyEvent) -> bool {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let focus = self.deck_focus;
        match key.code {
            KeyCode::Char('a') => self.deck_toggle(0),
            KeyCode::Char('b') => self.deck_toggle(1),
            KeyCode::Tab | KeyCode::BackTab => self.deck_focus = 1 - focus,
            // EQ: three columns of keys for three knobs. The upper key turns
            // a band up, the one under it turns it down, and with shift they
            // go straight to flat and to a kill.
            KeyCode::Char(c @ ('u' | 'i' | 'o' | 'j' | 'k' | 'l' | 'U' | 'I' | 'O' | 'J' | 'K' | 'L')) => {
                let band = match c.to_ascii_lowercase() {
                    'u' | 'j' => 0,
                    'i' | 'k' => 1,
                    _ => 2,
                };
                let now = self.decks.decks[focus].eq[band];
                let to = match c {
                    'u' | 'i' | 'o' => now + 1,
                    'j' | 'k' | 'l' => now - 1,
                    'U' | 'I' | 'O' => 0,
                    _ if now == crate::dj::EQ_KILL => 0,
                    _ => crate::dj::EQ_KILL,
                };
                self.deck_eq(focus, band, to, true);
            }
            // The bass swap: this deck's lows come in as the other's go out.
            KeyCode::Char('x') => {
                self.deck_eq(1 - focus, 0, crate::dj::EQ_KILL, false);
                self.deck_eq(focus, 0, 0, false);
                self.toast(format!("Bass is with deck {}", crate::dj::deck_name(focus)), false);
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Down => {
                let step = if shift { 0.25 } else { 0.05 };
                let to = match key.code {
                    KeyCode::Left => self.decks.fader - step,
                    KeyCode::Right => self.decks.fader + step,
                    _ => 0.5,
                }
                .clamp(0.0, 1.0);
                self.decks.fader = to;
                self.decks.auto = None;
                if let Some(dj) = &self.dj {
                    dj.set_fader(to);
                }
            }
            KeyCode::Char('m') => self.deck_mix(),
            KeyCode::Char('s') => {
                if let Some(dj) = &self.dj {
                    dj.sync(focus);
                }
            }
            KeyCode::Char('[') | KeyCode::Char(']') | KeyCode::Char('0') => {
                let delta = match key.code {
                    KeyCode::Char('[') => -0.005,
                    KeyCode::Char(']') => 0.005,
                    _ => 0.0,
                };
                if let Some(dj) = &self.dj {
                    let bend = dj.bend(focus, delta);
                    self.toast(format!("Deck {} tempo {:+.1}%", crate::dj::deck_name(focus), bend * 100.0), false);
                }
            }
            KeyCode::Char('{') | KeyCode::Char('}') => {
                if let Some(dj) = &self.dj {
                    dj.nudge(focus, if key.code == KeyCode::Char('{') { -15.0 } else { 15.0 });
                }
            }
            _ => return false,
        }
        true
    }

    fn clear_marks(&mut self) {
        if !self.basket.is_empty() {
            self.basket.clear();
            self.toast("Selection cleared", false);
        }
    }

    /// Play the selected songs now, in the order they were picked.
    fn play_basket(&mut self) {
        let tracks = std::mem::take(&mut self.basket);
        let Some(first) = tracks.first().cloned() else { return };
        let count = tracks.len();
        if self.demo {
            self.queue = tracks[1..].to_vec();
            self.demo_play(first);
            return self.toast(format!("Playing {count} selected"), false);
        }
        let Some(be) = self.backend.clone() else { return };
        let uris: Vec<String> = tracks.iter().map(|t| t.uri.clone()).collect();
        self.target_remote = None;
        match be.engine.play_tracks(uris.clone(), 0, 0, false, &self.pb.repeat) {
            Ok(()) => {
                self.sync_volume(&be);
                self.pb.loading = true;
                self.pb.source = None;
                self.pb.list = uris;
                self.pb.shuffle = false;
                self.queued.clear();
                self.set_track(first, true);
                self.pb.set_position(0);
                self.toast(format!("Playing {count} selected"), false);
            }
            Err(e) => {
                self.basket = tracks;
                self.toast(e.to_string(), true);
            }
        }
    }

    /// a: put the selection (or, with nothing selected, the row under the
    /// cursor) at the front of what plays next.
    fn enqueue(&mut self) {
        self.range = None;
        if self.decks_on() {
            return self.toast("The decks are on: 1 or 2 loads a song onto deck A or B", false);
        }
        let tracks: Vec<Track> = if !self.basket.is_empty() {
            std::mem::take(&mut self.basket)
        } else {
            match self.page().and_then(|p| p.cur().selected()) {
                Some(Item::Track(t)) if t.playable => vec![t.clone()],
                Some(Item::Track(_)) => return self.toast("That track isn't available", true),
                _ => return self.toast("Only songs can be queued", false),
            }
        };
        // With nothing playing there is no queue to add to: start one.
        let idle = self.pb.track.is_none() || (!self.pb.local && !self.remote_active());
        if idle {
            self.basket = tracks;
            return self.play_basket();
        }
        if self.demo {
            let n = tracks.len();
            self.queue.splice(0..0, tracks);
            return self.toast(format!("Queued {n} song{}", if n == 1 { "" } else { "s" }), false);
        }
        self.queued.extend(tracks.iter().map(|t| t.uri.clone()));
        if let Some(be) = &self.backend {
            be.queue_many(tracks);
        }
        self.refresh_queue();
    }

    fn toggle_like(&mut self, uri: String, name: &str) {
        if !uri.starts_with("spotify:track:") {
            return;
        }
        let liked = !self.liked.contains(&uri);
        if liked {
            self.liked.insert(uri.clone());
        } else {
            self.liked.remove(&uri);
        }
        self.toast(
            format!("{} {name}", if liked { "♥ Saved" } else { "Removed" }),
            false,
        );
        if let Some(be) = &self.backend {
            be.set_liked(uri, liked);
        }
    }

    fn like_selected(&mut self) {
        if let Some(Item::Track(t)) = self.page().and_then(|p| p.cur().selected()).cloned() {
            self.toggle_like(t.uri, &t.name);
        }
    }

    fn like_playing(&mut self) {
        if let Some(t) = self.pb.track.clone() {
            self.toggle_like(t.uri, &t.name);
        }
    }

    /// The track an "open album / artist" key refers to.
    fn subject(&self) -> Option<Track> {
        if self.screen == Screen::NowPlaying || self.focus == Focus::Sidebar {
            return self.pb.track.clone();
        }
        match self.page().and_then(|p| p.cur().selected()) {
            Some(Item::Track(t)) => Some(t.clone()),
            _ => self.pb.track.clone(),
        }
    }

    fn open_album(&mut self) {
        match self.subject() {
            Some(t) if !t.album_id.is_empty() => self.push(PageSpec::Album(t.album_id)),
            Some(_) => self.toast("Album details are still loading", false),
            None => {}
        }
    }

    fn open_artist(&mut self) {
        if let Some(Item::Album(a)) = self.page().and_then(|p| p.cur().selected()) {
            if let Some(first) = a.artists.first().filter(|a| !a.id.is_empty()) {
                return self.push(PageSpec::Artist(first.id.clone()));
            }
        }
        if let Some(t) = self.subject() {
            if let Some(a) = t.artists.iter().find(|a| !a.id.is_empty()) {
                self.push(PageSpec::Artist(a.id.clone()));
            }
        }
    }

    // ---- now-playing bookkeeping --------------------------------------

    /// Adopt `track` as the current one and fetch what the display needs.
    fn set_track(&mut self, mut track: Track, local: bool) {
        let changed = self.pb.uri() != track.uri;
        if let Some(be) = &self.backend {
            // Player events omit some details; reuse what a list already taught us.
            if track.album_id.is_empty() || track.image.is_none() {
                if let Some(known) = be.known(&track.uri) {
                    track.album_id = known.album_id;
                    track.image = track.image.or(known.image);
                    if track.artists.is_empty() {
                        track.artists = known.artists;
                    }
                } else if track.album_id.is_empty() && track.uri.starts_with("spotify:track:") {
                    be.resolve_track(track.uri.clone());
                }
            }
        }
        if let Some(url) = track.image.clone() {
            self.want_image(&url);
        }
        self.pb.local = local;
        if local {
            self.pb.remote_name = None;
        }
        if changed {
            if let Some(i) = self.queued.iter().position(|u| *u == track.uri) {
                self.queued.drain(..=i);
            }
            self.pb.set_position(0);
            // A seek meant for the last song must not land on this one.
            self.pending_seek = None;
            self.lyrics = LyricState::Idle;
            self.lyrics_for.clear();
            self.lyrics_scroll = None;
        }
        self.pb.track = Some(track);
        self.ensure_lyrics();
        if changed {
            self.refresh_queue();
        }
        self.dirty = true;
    }

    /// Work out what plays next. With the Web API that's Spotify's own answer;
    /// otherwise it is read off the list this app handed to the player.
    fn refresh_queue(&mut self) {
        let Some(be) = self.backend.clone() else { return };
        if be.api.is_some() {
            return be.load_queue();
        }
        if !self.pb.local {
            self.queue.clear();
            return;
        }
        // Hand-queued songs come first; then the rest of the list, unless
        // shuffle is on and the order is the player's to decide.
        let mut upcoming: Vec<String> = self.queued.iter().cloned().collect();
        if !self.pb.shuffle {
            let current = self.pb.uri();
            if let Some(i) = self.pb.list.iter().position(|u| u == current) {
                upcoming.extend(self.pb.list.iter().skip(i + 1).take(50).cloned());
            }
        }
        if upcoming.is_empty() {
            self.queue.clear();
        } else {
            be.resolve_queue(upcoming);
        }
    }

    fn ensure_lyrics(&mut self) {
        if self.screen != Screen::NowPlaying {
            return;
        }
        let Some(track) = self.pb.track.clone() else { return };
        if self.lyrics_for == track.uri {
            return;
        }
        self.lyrics_for = track.uri.clone();
        match &self.backend {
            Some(be) => {
                self.lyrics = LyricState::Loading;
                be.load_lyrics(track);
            }
            None if self.demo => {}
            None => self.lyrics = LyricState::Missing,
        }
    }

    pub fn want_image(&mut self, url: &str) {
        if self.images.contains_key(url) || self.image_pending.contains(url) {
            return;
        }
        if self.image_failed.get(url).is_some_and(|at| at.elapsed() < Duration::from_secs(30)) {
            return;
        }
        if let Some(be) = &self.backend {
            self.image_pending.insert(url.to_string());
            be.load_image(url.to_string());
        }
    }

    /// Ask for a cover's depth to be worked out, once its picture is here.
    pub fn want_depth(&mut self, url: &str) {
        if self.depths.contains_key(url) || self.depth_pending.contains(url) {
            return;
        }
        if self.depth_failed.get(url).is_some_and(|at| at.elapsed() < Duration::from_secs(300)) {
            return;
        }
        if let (Some(be), Some(image)) = (&self.backend, self.images.get(url)) {
            self.depth_pending.insert(url.to_string());
            be.load_depth(url.to_string(), image.clone());
        }
    }

    fn store_image(&mut self, url: String, image: Arc<RgbImage>) {
        // A small copy for the animated cover, which is redrawn every frame.
        let thumb = image::imageops::resize(&*image, 160, 160, image::imageops::FilterType::Triangle);
        self.thumbs.insert(url.clone(), Arc::new(thumb));
        if self.images.insert(url.clone(), image).is_none() {
            self.image_order.push_back(url);
        }
        while self.image_order.len() > 24 {
            if let Some(old) = self.image_order.pop_front() {
                self.images.remove(&old);
                self.thumbs.remove(&old);
                self.depths.remove(&old);
                self.art_cache.retain(|k, _| k.0 != old);
            }
        }
    }

    /// Recompute the colour scheme when the artwork on display changes.
    fn sync_palette(&mut self) {
        let url = self.pb.track.as_ref().and_then(|t| t.image.clone()).unwrap_or_default();
        if url == self.palette_for {
            return;
        }
        if url.is_empty() || !self.cfg.dynamic_color {
            self.palette_for = url;
            self.palette_target = Palette::default();
        } else if let Some(img) = self.images.get(&url) {
            self.palette_target = crate::art::palette(img);
            self.palette_for = url;
        }
    }

    // ---- background messages ------------------------------------------

    pub fn on_msg(&mut self, msg: Msg) {
        self.dirty = true;
        match msg {
            Msg::Engine(ev) => self.on_engine(ev),
            Msg::Sidebar(playlists) => {
                let keep = match self.sidebar.get(self.side_sel) {
                    Some(SideEntry::Playlist { playlist, .. }) => Some(playlist.id.clone()),
                    _ => None,
                };
                let mut entries = self.base_sidebar();
                entries.extend(playlists);
                self.sidebar = entries;
                if let Some(id) = keep {
                    if let Some(i) = self.sidebar.iter().position(
                        |e| matches!(e, SideEntry::Playlist { playlist, .. } if playlist.id == id),
                    ) {
                        self.side_sel = i;
                    }
                }
                self.side_sel = self.side_sel.min(self.sidebar.len().saturating_sub(1));
            }
            Msg::Page { id, update } => {
                let Some(page) = self.pages.iter_mut().find(|p| p.id == id) else { return };
                match update {
                    PageUpdate::Head(head) => {
                        if !head.title.is_empty() {
                            page.head = head;
                        }
                    }
                    PageUpdate::Set { tab, items, done } => {
                        let (page_id, shown) = (page.id, page.tab);
                        if let Some(t) = page.tabs.get_mut(tab) {
                            // A refresh can add or drop rows above the cursor.
                            // Stay on the same song, at the same height on screen.
                            let was = t.selected().map(|item| (item.uri(), t.sel));
                            t.items = items;
                            t.loading = !done;
                            t.done = done;
                            t.error = None;
                            t.refilter();
                            if let Some((uri, old)) = was {
                                let at = |row: usize| t.view.get(row).is_some_and(|&i| t.items[i].uri() == uri);
                                let now = if at(old) { Some(old) } else { (0..t.view.len()).find(|&row| at(row)) };
                                if let Some(now) = now.filter(|&now| now != old) {
                                    t.sel = now;
                                    t.top = (t.top + now).saturating_sub(old);
                                    // A run being selected moves with its rows.
                                    if let Some(range) = &mut self.range {
                                        if range.page == page_id && range.tab == tab && shown == tab {
                                            range.anchor = (range.anchor + now).saturating_sub(old);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    PageUpdate::Append { tab, items, done } => {
                        if let Some(t) = page.tabs.get_mut(tab) {
                            t.items.extend(items);
                            t.loading = !done && !matches!(page.spec, PageSpec::Search(_));
                            t.done = done;
                            t.refilter();
                        }
                    }
                    PageUpdate::Error { tab, message } => {
                        if let Some(t) = page.tabs.get_mut(tab) {
                            t.loading = false;
                            t.done = true;
                            t.error = Some(message);
                        }
                    }
                }
            }
            Msg::Liked(uris) => self.liked = uris.into_iter().collect(),
            Msg::LikeFailed { uri, was } => {
                if was {
                    self.liked.insert(uri);
                } else {
                    self.liked.remove(&uri);
                }
            }
            Msg::Image { url, image } => {
                self.image_pending.remove(&url);
                match image {
                    Some(image) => {
                        self.image_failed.remove(&url);
                        self.store_image(url, image);
                    }
                    None => {
                        self.image_failed.insert(url, Instant::now());
                    }
                }
            }
            Msg::Depth { url, map } => {
                self.depth_pending.remove(&url);
                match map {
                    // Only worth keeping while the cover itself is.
                    Some(map) if self.images.contains_key(&url) => {
                        self.depths.insert(url, map);
                    }
                    Some(_) => {}
                    None => {
                        self.depth_failed.insert(url, Instant::now());
                    }
                }
            }
            Msg::Lyrics { uri, lyrics } => {
                if uri == self.lyrics_for {
                    self.lyrics = match lyrics {
                        Some(l) => LyricState::Ready(l),
                        None => LyricState::Missing,
                    };
                    // Scrolling done before they arrived was scrolling nothing.
                    self.lyrics_scroll = None;
                }
            }
            Msg::Queue(tracks) => {
                self.queue = tracks;
                if let Overlay::Queue { sel } = &mut self.overlay {
                    *sel = (*sel).min(self.queue.len().saturating_sub(1));
                }
            }
            Msg::QueueFailed(uris) => {
                // They were listed as up next the moment `a` was pressed.
                for uri in uris {
                    if let Some(i) = self.queued.iter().rposition(|u| *u == uri) {
                        self.queued.remove(i);
                    }
                }
                self.refresh_queue();
            }
            Msg::Devices(mut devices) => {
                // Spotify lists every device but doesn't say which one is us.
                let me = self.cfg.device_id.clone();
                for d in &mut devices {
                    d.this = d.id == me;
                }
                if !devices.iter().any(|d| d.this) {
                    devices.insert(0, self.this_device());
                }
                devices.sort_by(|a, b| b.this.cmp(&a.this).then(a.name.cmp(&b.name)));
                self.pb.devices = devices;
            }
            Msg::TrackInfo(info) => {
                if let Some(t) = self.pb.track.as_ref().filter(|t| t.uri == info.uri) {
                    let local = self.pb.local;
                    let merged = Track {
                        // Keep the player's own duration: it reflects the file being played.
                        duration_ms: if t.duration_ms > 0 { t.duration_ms } else { info.duration_ms },
                        ..info
                    };
                    self.set_track(merged, local);
                }
            }
            Msg::Remote(state) => {
                // Only a starting point; live events and cluster pushes take over.
                if self.pb.track.is_none() && !self.pb.local {
                    if let Some(track) = state.track {
                        self.set_track(track, false);
                        self.pb.set_position(state.progress_ms);
                        self.pb.shuffle = state.shuffle;
                        self.pb.repeat = state.repeat;
                        // Spotify remembers the last device as "active" long after it
                        // has gone (often this app's own previous run). Only a device
                        // that is someone else and actually playing counts as remote.
                        let me = self.cfg.device_id.clone();
                        let other = state.device.filter(|d| d.active && d.id != me);
                        match other {
                            Some(d) if state.is_playing => {
                                self.pb.playing = true;
                                self.pb.remote_name = Some(d.name);
                            }
                            _ => {
                                self.pb.playing = false;
                                self.pb.remote_name = None;
                            }
                        }
                    }
                }
            }
            Msg::RemoteGone => {
                self.pb.remote_name = None;
                self.pb.playing = false;
                self.target_remote = None;
                self.toast("That device is gone. Press space to play here", false);
            }
            Msg::Toast { text, error } => self.toast(text, error),
            Msg::Media(key) => {
                use crate::mpris::MediaKey;
                match key {
                    MediaKey::Toggle => self.play_pause(),
                    MediaKey::Play if !self.pb.playing => self.play_pause(),
                    MediaKey::Pause if self.pb.playing => self.play_pause(),
                    MediaKey::Play | MediaKey::Pause => {}
                    MediaKey::Next => self.skip(true),
                    MediaKey::Prev => self.skip(false),
                    MediaKey::SeekBy(ms) => self.seek_by(ms),
                    MediaKey::SeekTo(ms) => self.seek_to(ms),
                }
            }
        }
    }

    fn on_engine(&mut self, ev: Event) {
        match ev {
            Event::Conn(c) => {
                if c == Conn::Online && self.conn != Conn::Online {
                    // Back online: covers that failed are worth another go.
                    self.image_failed.clear();
                    if let Some(be) = &self.backend {
                        be.load_sidebar();
                        if let (Some(dj), Some((session, _))) = (&self.dj, be.engine.deck_parts()) {
                            dj.set_session(&session);
                        }
                    }
                    // Pages opened while offline were left waiting; load them now.
                    let stuck: Vec<(u64, PageSpec)> = self
                        .pages
                        .iter()
                        .filter(|p| p.tabs.iter().any(|t| t.error.is_some()))
                        .map(|p| (p.id, p.spec.clone()))
                        .collect();
                    for (id, spec) in stuck {
                        if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
                            for t in &mut p.tabs {
                                t.error = None;
                                t.loading = true;
                            }
                        }
                        if let Some(be) = &self.backend {
                            be.load_page(id, spec);
                        }
                    }
                }
                if c == Conn::Online && self.pb.devices.is_empty() {
                    let me = self.this_device();
                    self.pb.devices.push(me);
                }
                if c != Conn::Online {
                    self.pb.local = false;
                    if self.pb.remote_name.is_none() {
                        self.pb.playing = false;
                    }
                }
                self.conn = c;
            }
            Event::User(name) => self.user = name,
            Event::Track(track) => self.set_track(track, true),
            Event::Playing { uri, pos_ms } => {
                self.pb.local = true;
                self.pb.remote_name = None;
                self.pb.loading = false;
                self.pb.playing = true;
                if (self.pb.uri() == uri || self.pb.track.is_none()) && self.pending_seek.is_none() {
                    self.pb.set_position(pos_ms);
                }
            }
            Event::Paused { uri, pos_ms } => {
                self.pb.loading = false;
                self.pb.playing = false;
                if self.pb.uri() == uri && self.pending_seek.is_none() {
                    self.pb.set_position(pos_ms);
                }
            }
            Event::Loading { uri, pos_ms } => {
                self.pb.local = true;
                self.pb.loading = true;
                if self.pb.uri() == uri && self.pending_seek.is_none() {
                    self.pb.set_position(pos_ms);
                }
            }
            // While a seek is waiting to be sent, the playhead is where the
            // user put it, not where the player last was.
            Event::Position(ms) => {
                if self.pending_seek.is_none() {
                    self.pb.set_position(ms);
                }
            }
            Event::Stopped => {
                self.pb.playing = false;
                self.pb.loading = false;
            }
            Event::Unavailable => {
                self.pb.loading = false;
                self.toast("Track unavailable, skipping", true);
            }
            Event::Volume(v) => {
                // While a local change is still settling, ours is the truth.
                let ours = self.pending_volume.is_some() || self.volume_unsent;
                if !ours && (self.pb.local || self.pb.remote_name.is_none()) {
                    self.pb.volume = v;
                    self.cfg.volume = v;
                }
            }
            Event::Shuffle(on) => {
                self.pb.shuffle = on;
                self.refresh_queue();
            }
            Event::Repeat(mode) => self.pb.repeat = mode,
            Event::Notice(text) => self.toast(text, true),
            Event::Cluster(c) => self.on_cluster(c),
        }
    }

    fn this_device(&self) -> Device {
        Device {
            id: self.cfg.device_id.clone(),
            name: self.cfg.device_name.clone(),
            kind: "computer".into(),
            volume: self.pb.volume,
            active: self.pb.local,
            this: true,
        }
    }

    fn on_cluster(&mut self, c: crate::engine::Cluster) {
        self.pb.devices = c.devices.clone();
        if !self.pb.devices.iter().any(|d| d.this) {
            let me = self.this_device();
            self.pb.devices.insert(0, me);
        }
        let active = c.devices.iter().find(|d| d.active);
        let here = active.is_some_and(|d| d.this);
        self.pb.context_uri = c.context_uri.clone();

        if here {
            self.pb.local = true;
            self.pb.remote_name = None;
        } else if let Some(dev) = active {
            // Another device has the music: mirror it.
            self.pb.local = false;
            self.pb.remote_name = Some(dev.name.clone());
            self.pb.volume = dev.volume;
            self.pb.shuffle = c.shuffle;
            self.pb.repeat = c.repeat.clone();
            self.pb.loading = false;
            if !c.track_uri.is_empty() && self.pb.uri() != c.track_uri {
                let known = self.backend.as_ref().and_then(|b| b.known(&c.track_uri));
                let track = known.unwrap_or_else(|| Track {
                    uri: c.track_uri.clone(),
                    id: id_of(&c.track_uri).to_string(),
                    name: "…".into(),
                    duration_ms: c.duration_ms,
                    playable: true,
                    ..Default::default()
                });
                self.set_track(track, false);
            }
            self.pb.playing = c.playing;
            let lag = if c.playing { (now_ms() - c.timestamp_ms).clamp(0, 600_000) as u32 } else { 0 };
            self.pb.set_position(c.position_ms.saturating_add(lag));
        } else {
            self.pb.local = false;
            self.pb.remote_name = None;
            self.pb.playing = false;
        }
        if self.target_remote.as_ref().is_some_and(|t| !c.devices.iter().any(|d| &d.id == t)) {
            self.target_remote = None;
        }

        let uris: Vec<String> = c.next.into_iter().map(|(u, _)| u).collect();
        if uris != self.queue_uris {
            self.queue_uris = uris.clone();
            if uris.is_empty() {
                self.queue.clear();
            } else if let Some(be) = &self.backend {
                be.resolve_queue(uris);
            }
        }
    }

    // ---- time ---------------------------------------------------------

    pub fn on_tick(&mut self) {
        self.frame += 1;
        if self.toast.as_ref().is_some_and(|t| Instant::now() >= t.until) {
            self.toast = None;
            self.dirty = true;
        }

        self.sync_palette();
        if self.palette != self.palette_target {
            let step = |a: (u8, u8, u8), b: (u8, u8, u8)| {
                let m = crate::art::mix(a, b, 0.18);
                // Integer rounding can stall one short of the target; snap when close.
                if m.0.abs_diff(b.0) <= 2 && m.1.abs_diff(b.1) <= 2 && m.2.abs_diff(b.2) <= 2 { b } else { m }
            };
            self.palette.accent = step(self.palette.accent, self.palette_target.accent);
            self.palette.shade = step(self.palette.shade, self.palette_target.shade);
            self.dirty = true;
        }

        // Snapshots step time by a fixed amount so frames are reproducible.
        let dt = self.fixed_step.unwrap_or_else(|| self.last_tick.elapsed().as_secs_f32().min(0.25));
        self.last_tick = Instant::now();
        self.flush_seek();
        self.flush_volume();

        // Spectrum bars rise instantly and fall smoothly.
        let live = if self.demo && self.pb.playing {
            Some(demo_snapshot(self.frame))
        } else {
            self.hub.poll()
        };
        let mut moving = false;
        for (i, bar) in self.bars.iter_mut().enumerate() {
            let target = live.map(|s| s.bands[i]).unwrap_or(0.0);
            let next = if target > *bar { target } else { (*bar - 1.4 * dt).max(target).max(0.0) };
            if (next - *bar).abs() > 0.001 {
                moving = true;
            }
            *bar = next;
        }
        // The beat envelope snaps to full on a hit and falls away in ~0.3 s.
        let was = self.beat;
        self.beat = (self.beat - dt * 3.4).max(0.0);
        if live.is_some_and(|s| s.beat) {
            self.beat = 1.0;
            self.beats += 1;
        }
        let bass = live.map(|s| s.bass).unwrap_or(0.0);
        self.bass += (bass - self.bass) * (dt * 10.0).min(1.0);
        self.clock += dt * (0.6 + self.bass);
        let reactive = self.party || self.art_mode == ArtMode::Pulse;
        if (moving && (self.cfg.visualizer || reactive)) || (reactive && (self.beat > 0.0 || was > 0.0)) {
            self.dirty = true;
        }
        if self.party && self.pb.playing {
            self.dirty = true;
        }
        // The depth cover drifts for as long as there is music.
        if self.art_mode == ArtMode::Depth && (self.pb.playing || self.decks.decks.iter().any(|d| d.playing)) {
            self.dirty = true;
        }

        if let Some(dj) = self.dj.as_mut() {
            let (view, notes) = dj.view();
            if view.decks.iter().any(|d| d.playing) || view.auto.is_some() {
                self.dirty = true;
            }
            self.decks = view;
            for note in notes {
                self.toast(note, false);
            }
        } else if self.deck_demo {
            self.demo_decks();
        }

        let sec = self.position() / 1000;
        if sec != self.last_shown_sec {
            self.last_shown_sec = sec;
            self.dirty = true;
        }
        // Synced lyrics move between seconds, so keep that screen fresh while playing.
        if self.screen == Screen::NowPlaying && self.pb.playing {
            self.dirty = true;
        }

        if self.demo {
            self.demo_tick();
        }
        if let Some(mpris) = &mut self.mpris {
            let at = if self.pb.local && !self.demo { self.hub.position(self.pb.uri()).map(|ms| ms.max(0) as u32) } else { None };
            mpris.sync(self.pb.track.as_ref(), self.pb.playing, at.unwrap_or_else(|| self.pb.position()));
        }
        self.load_more_if_needed();
    }

    /// Search results arrive ten at a time; fetch the next ten as the cursor nears the end.
    fn load_more_if_needed(&mut self) {
        let Some(be) = self.backend.clone() else { return };
        let Some(page) = self.pages.last_mut() else { return };
        let PageSpec::Search(query) = page.spec.clone() else { return };
        let (id, tab_index) = (page.id, page.tab);
        let tab = page.cur_mut();
        if tab.done || tab.loading || !tab.filter.is_empty() || tab.items.is_empty() {
            return;
        }
        if tab.sel + 4 >= tab.items.len() {
            tab.loading = true;
            be.search_more(id, query, tab_index, tab.items.len());
        }
    }

    // ---- input --------------------------------------------------------

    pub fn on_term(&mut self, ev: TermEvent) {
        match ev {
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => {
                self.dirty = true;
                self.on_key(key);
            }
            TermEvent::Mouse(m) => self.on_mouse(m),
            TermEvent::Resize(..) => {
                self.art_cache.clear();
                self.dirty = true;
            }
            TermEvent::Paste(text) => {
                if let Overlay::Search { input } = &mut self.overlay {
                    input.push_str(text.trim());
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }

        // Text entry and pop-ups swallow keys first.
        match &mut self.overlay {
            Overlay::Search { input } => {
                match key.code {
                    KeyCode::Esc => self.overlay = Overlay::None,
                    KeyCode::Enter => {
                        let q = input.trim().to_string();
                        self.overlay = Overlay::None;
                        if !q.is_empty() {
                            self.push(PageSpec::Search(q));
                        }
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char('u') if ctrl => input.clear(),
                    KeyCode::Char('w') if ctrl => {
                        let trimmed = input.trim_end();
                        let cut = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
                        input.truncate(cut);
                    }
                    KeyCode::Char(c) if !ctrl => input.push(c),
                    _ => {}
                }
                return;
            }
            Overlay::Filter => {
                let Some(page) = self.pages.last_mut() else {
                    self.overlay = Overlay::None;
                    return;
                };
                let tab = page.cur_mut();
                match key.code {
                    KeyCode::Esc => {
                        tab.filter.clear();
                        tab.refilter();
                        self.overlay = Overlay::None;
                    }
                    KeyCode::Enter => self.overlay = Overlay::None,
                    KeyCode::Backspace => {
                        tab.filter.pop();
                        tab.refilter();
                    }
                    KeyCode::Down => tab.step(1),
                    KeyCode::Up => tab.step(-1),
                    KeyCode::Char(c) if !ctrl => {
                        tab.filter.push(c);
                        tab.sel = 0;
                        tab.refilter();
                    }
                    _ => {}
                }
                return;
            }
            Overlay::Help { page } => {
                match key.code {
                    KeyCode::Down | KeyCode::Right | KeyCode::PageDown | KeyCode::Char('j' | ' ') => *page += 1,
                    KeyCode::Up | KeyCode::Left | KeyCode::PageUp | KeyCode::Char('k') => *page = page.saturating_sub(1),
                    _ => self.overlay = Overlay::None,
                }
                return;
            }
            Overlay::Queue { sel } => {
                let n = self.queue.len();
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(n.saturating_sub(1)),
                    KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
                    KeyCode::Char(' ') => self.play_pause(),
                    KeyCode::Char('n') => self.skip(true),
                    _ => self.overlay = Overlay::None,
                }
                return;
            }
            Overlay::Devices { sel } => {
                let n = self.pb.devices.len();
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(n.saturating_sub(1)),
                    KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
                    KeyCode::Enter => {
                        let chosen = self.pb.devices.get(*sel).cloned();
                        self.overlay = Overlay::None;
                        if let Some(d) = chosen {
                            self.pick_device(d);
                        }
                    }
                    _ => self.overlay = Overlay::None,
                }
                return;
            }
            Overlay::None => {}
        }

        if self.screen == Screen::Decks && self.on_decks_key(key) {
            return;
        }

        // A held key repeats some thirty times a second. For keys that each
        // send Spotify a command, only the first press of a run counts: a
        // burst of them is what gets the player rate limited.
        if matches!(key.code, KeyCode::Char(' ' | 'n' | 'p' | 's' | 'r' | 'f' | 'F' | 'a')) {
            let now = Instant::now();
            let held = key.kind == KeyEventKind::Repeat
                || self.last_command.is_some_and(|(code, at)| {
                    code == key.code && now.duration_since(at) < Duration::from_millis(90)
                });
            // Scripted keys (snapshots) arrive all at once and are all meant.
            if self.fixed_step.is_none() {
                self.last_command = Some((key.code, now));
                if held {
                    return;
                }
            }
        }

        // Keys that work everywhere.
        match key.code {
            KeyCode::Char('D') => return self.decks_key(),
            KeyCode::Char('q') => return self.quit = true,
            KeyCode::Char('?') => return self.overlay = Overlay::Help { page: 0 },
            KeyCode::Char('/') => return self.overlay = Overlay::Search { input: String::new() },
            KeyCode::Char(' ') => return self.play_pause(),
            KeyCode::Char('n') => return self.skip(true),
            KeyCode::Char('p') => return self.skip(false),
            KeyCode::Char('.') => return self.seek_by(5_000),
            KeyCode::Char(',') => return self.seek_by(-5_000),
            KeyCode::Char('>') => return self.seek_by(30_000),
            KeyCode::Char('<') => return self.seek_by(-30_000),
            KeyCode::Char('+') | KeyCode::Char('=') => return self.volume_by(5),
            KeyCode::Char('-') | KeyCode::Char('_') => return self.volume_by(-5),
            KeyCode::Char('s') => return self.toggle_shuffle(),
            KeyCode::Char('r') => return self.cycle_repeat(),
            KeyCode::Char('R') => return self.refresh(),
            KeyCode::Char('F') => return self.like_playing(),
            KeyCode::Char('o') => return self.open_album(),
            KeyCode::Char('A') => return self.open_artist(),
            KeyCode::Char('u') if !ctrl => {
                self.refresh_queue();
                return self.overlay = Overlay::Queue { sel: 0 };
            }
            KeyCode::Char('d') if !ctrl => {
                if let Some(be) = &self.backend {
                    be.load_devices();
                }
                let sel = self.pb.devices.iter().position(|d| d.active).unwrap_or(0);
                return self.overlay = Overlay::Devices { sel };
            }
            KeyCode::Char('c') => {
                self.art_mode = self.art_mode.next();
                self.cfg.art = self.art_mode.name().to_string();
                let what = match self.art_mode {
                    ArtMode::Pulse => "pulse (moves with the beat)",
                    ArtMode::Depth => "depth (the cover in 3-D, moving with the music)",
                    other => other.name(),
                };
                return self.toast(format!("Cover style: {what}"), false);
            }
            KeyCode::Char('z') => {
                self.party = !self.party;
                return self.toast(if self.party { "Party mode. z to calm down" } else { "Party's over" }, false);
            }
            KeyCode::Char('m') if self.decks_on() => return self.deck_mix(),
            KeyCode::Char('m') => return self.set_mix(!self.cfg.mix, self.cfg.mix_seconds),
            KeyCode::Char('M') => {
                let next = match self.cfg.mix_seconds {
                    0..=3 => 6,
                    4..=6 => 9,
                    7..=9 => 12,
                    _ => 3,
                };
                if self.decks_on() {
                    self.cfg.mix_seconds = next;
                    return self.toast(format!("Deck transitions now take {next} s"), false);
                }
                return self.set_mix(true, next);
            }
            KeyCode::Char('X') => return self.clear_marks(),
            KeyCode::Char('v') => {
                self.screen = match self.screen {
                    Screen::NowPlaying => Screen::Browse,
                    _ => Screen::NowPlaying,
                };
                self.ensure_lyrics();
                return;
            }
            KeyCode::Esc | KeyCode::Backspace => return self.back(),
            _ => {}
        }
        if self.screen == Screen::Decks {
            return;
        }

        if self.screen == Screen::NowPlaying {
            // Up/down scroll the lyrics by hand; any seek snaps back to following.
            let total = match &self.lyrics {
                LyricState::Ready(l) => l.lines.len(),
                _ => 0,
            };
            let cur = self.lyrics_scroll.unwrap_or_else(|| self.lyric_line().unwrap_or(0));
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.lyrics_scroll = Some((cur + 1).min(total.saturating_sub(1)))
                }
                KeyCode::Up | KeyCode::Char('k') => self.lyrics_scroll = Some(cur.saturating_sub(1)),
                KeyCode::Char('f') => self.like_playing(),
                KeyCode::Enter => self.lyrics_scroll = None,
                _ => {}
            }
            return;
        }

        match self.focus {
            Focus::Sidebar => match key.code {
                KeyCode::Down | KeyCode::Char('j') => self.side_step(1),
                KeyCode::Up | KeyCode::Char('k') => self.side_step(-1),
                KeyCode::PageDown => self.side_step(10),
                KeyCode::PageUp => self.side_step(-10),
                KeyCode::Char('d') if ctrl => self.side_step(10),
                KeyCode::Char('u') if ctrl => self.side_step(-10),
                KeyCode::Home | KeyCode::Char('g') => {
                    self.side_sel = 0;
                    self.side_step(1);
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.side_follow = true;
                    self.side_sel = self.sidebar.len().saturating_sub(1);
                    if !self.sidebar[self.side_sel].selectable() {
                        self.side_step(-1);
                    }
                }
                KeyCode::Enter => self.side_activate(),
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => self.focus = Focus::Main,
                _ => {}
            },
            Focus::Main => {
                let page_rows = 10isize;
                let Some(page) = self.pages.last_mut() else { return };
                let n_tabs = page.tabs.len();
                let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                match key.code {
                    KeyCode::Down if shift => self.mark_selected(1, true),
                    KeyCode::Up if shift => self.mark_selected(-1, true),
                    KeyCode::Down | KeyCode::Char('j') => page.cur_mut().step(1),
                    KeyCode::Up | KeyCode::Char('k') => page.cur_mut().step(-1),
                    KeyCode::PageDown => page.cur_mut().step(page_rows),
                    KeyCode::PageUp => page.cur_mut().step(-page_rows),
                    KeyCode::Char('d') if ctrl => page.cur_mut().step(page_rows),
                    KeyCode::Char('u') if ctrl => page.cur_mut().step(-page_rows),
                    KeyCode::Home | KeyCode::Char('g') => page.cur_mut().sel = 0,
                    KeyCode::End | KeyCode::Char('G') => {
                        let tab = page.cur_mut();
                        tab.sel = tab.view.len().saturating_sub(1);
                    }
                    KeyCode::Tab | KeyCode::Char(']') if n_tabs > 1 => {
                        page.tab = (page.tab + 1) % n_tabs
                    }
                    KeyCode::BackTab | KeyCode::Char('[') if n_tabs > 1 => {
                        page.tab = (page.tab + n_tabs - 1) % n_tabs
                    }
                    KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Sidebar,
                    KeyCode::Enter => self.activate(),
                    KeyCode::Char('a') => self.enqueue(),
                    KeyCode::Char('V') => self.toggle_range(),
                    KeyCode::Char('1') | KeyCode::Char('2') => {
                        let deck = if key.code == KeyCode::Char('1') { 0 } else { 1 };
                        match page.cur().selected() {
                            Some(Item::Track(t)) => {
                                let t = t.clone();
                                self.deck_load(Some(deck), t);
                            }
                            _ => self.toast("Put the cursor on a song to load it onto a deck", false),
                        }
                    }
                    KeyCode::Char('x') => self.mark_selected(1, false),
                    KeyCode::Char('J') => self.mark_selected(1, true),
                    KeyCode::Char('K') => self.mark_selected(-1, true),
                    KeyCode::Char('f') if !ctrl => self.like_selected(),
                    KeyCode::Char('f') | KeyCode::Char('\\') => self.overlay = Overlay::Filter,
                    _ => {}
                }
                self.update_range();
            }
        }
    }

    fn set_mix(&mut self, on: bool, seconds: u8) {
        self.cfg.mix = on;
        self.cfg.mix_seconds = seconds.clamp(1, 12);
        if let Some(be) = &self.backend {
            be.engine.set_mix(if on { self.cfg.mix_seconds as u32 } else { 0 });
        }
        if on {
            self.toast(format!("Crossfade on: songs blend over {} s (M changes the length)", self.cfg.mix_seconds), false);
        } else {
            self.toast("Crossfade off: songs play back to back", false);
        }
    }

    fn pick_device(&mut self, d: Device) {
        let Some(be) = self.backend.clone() else { return };
        if d.this {
            self.target_remote = None;
            let r = be.engine.take_over();
            self.try_engine(r);
            self.toast("Playing here", false);
        } else {
            self.target_remote = Some(d.id.clone());
            be.transfer(d.id, d.name);
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        if !self.cfg.mouse {
            return;
        }
        let hit = self
            .hits
            .iter()
            .rev()
            .find(|(r, _)| m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height)
            .map(|(r, h)| (*r, h.clone()));

        match m.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let delta = if m.kind == MouseEventKind::ScrollDown { 3 } else { -3 };
                self.dirty = true;
                match (&mut self.overlay, hit.map(|h| h.1)) {
                    (Overlay::Queue { sel }, _) => {
                        *sel = (*sel as isize + delta).clamp(0, self.queue.len().saturating_sub(1) as isize) as usize
                    }
                    (Overlay::None, Some(Hit::SidePane | Hit::Side(_))) => {
                        // Scroll the view, not the selection, like any other list.
                        let max = self.sidebar.len().saturating_sub(1) as isize;
                        self.side_follow = false;
                        self.side_top = (self.side_top as isize + delta).clamp(0, max) as usize;
                    }
                    (Overlay::None, _) if self.screen == Screen::Decks => {}
                    (Overlay::None, _) if self.screen == Screen::NowPlaying => {
                        let total = match &self.lyrics {
                            LyricState::Ready(l) => l.lines.len(),
                            _ => 0,
                        };
                        let cur = self.lyrics_scroll.unwrap_or_else(|| self.lyric_line().unwrap_or(0));
                        self.lyrics_scroll =
                            Some((cur as isize + delta.signum()).clamp(0, total.saturating_sub(1) as isize) as usize);
                    }
                    (Overlay::None | Overlay::Filter, _) => {
                        if let Some(page) = self.pages.last_mut() {
                            page.cur_mut().step(delta);
                        }
                        self.update_range();
                    }
                    _ => {}
                }
            }
            // Ctrl-click, right-click or middle-click marks a song, like picking
            // several files. (Some terminals keep ctrl-click for themselves.)
            MouseEventKind::Down(button)
                if matches!(self.overlay, Overlay::None)
                    && (button != MouseButton::Left || m.modifiers.contains(KeyModifiers::CONTROL)) =>
            {
                self.dirty = true;
                if let Some((_, Hit::Row(i))) = hit {
                    self.focus = Focus::Main;
                    let picked = self.pages.last_mut().and_then(|page| {
                        let tab = page.cur_mut();
                        tab.sel = i.min(tab.view.len().saturating_sub(1));
                        match tab.selected() {
                            Some(Item::Track(t)) => Some(t.clone()),
                            _ => None,
                        }
                    });
                    if let Some(t) = picked {
                        let on = !self.is_marked(&t.uri);
                        self.set_mark(t, on);
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.dirty = true;
                let double = self.last_click.is_some_and(|(at, x, y)| {
                    at.elapsed() < Duration::from_millis(450) && x == m.column && y == m.row
                });
                self.last_click = Some((Instant::now(), m.column, m.row));

                if !matches!(self.overlay, Overlay::None) {
                    match (&mut self.overlay, hit) {
                        (Overlay::Devices { sel }, Some((_, Hit::OverlayRow(i)))) => {
                            *sel = i;
                            if let Some(d) = self.pb.devices.get(i).cloned() {
                                self.overlay = Overlay::None;
                                self.pick_device(d);
                            }
                        }
                        (Overlay::Queue { sel }, Some((_, Hit::OverlayRow(i)))) => *sel = i,
                        _ => self.overlay = Overlay::None,
                    }
                    return;
                }
                match hit {
                    Some((_, Hit::Side(i))) => {
                        self.focus = Focus::Sidebar;
                        if self.sidebar.get(i).is_some_and(SideEntry::selectable) {
                            self.side_sel = i;
                            self.side_activate();
                        }
                    }
                    Some((_, Hit::Row(i))) => {
                        self.focus = Focus::Main;
                        if let Some(page) = self.pages.last_mut() {
                            let tab = page.cur_mut();
                            let already = tab.sel == i;
                            tab.sel = i.min(tab.view.len().saturating_sub(1));
                            if double && already {
                                self.activate();
                            }
                        }
                    }
                    Some((_, Hit::Tab(i))) => {
                        if let Some(page) = self.pages.last_mut() {
                            page.tab = i.min(page.tabs.len() - 1);
                        }
                        self.focus = Focus::Main;
                    }
                    Some((rect, Hit::Seek)) => {
                        if let Some(dur) = self.pb.track.as_ref().map(|t| t.duration_ms) {
                            let frac = (m.column - rect.x) as f64 / rect.width.max(1) as f64;
                            self.seek_to((dur as f64 * frac) as u32);
                        }
                    }
                    Some((_, Hit::PlayPause)) => self.play_pause(),
                    Some((_, Hit::NowPlayingArt)) => {
                        self.screen = match self.screen {
                            Screen::NowPlaying => Screen::Browse,
                            _ => Screen::NowPlaying,
                        };
                        self.ensure_lyrics();
                    }
                    Some((_, Hit::MainPane)) => self.focus = Focus::Main,
                    Some((_, Hit::SidePane)) => self.focus = Focus::Sidebar,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    pub fn lyric_line(&self) -> Option<usize> {
        match &self.lyrics {
            LyricState::Ready(l) => crate::lyrics::current_line(l, self.position()),
            _ => None,
        }
    }

    // ---- demo ---------------------------------------------------------

    fn demo_play(&mut self, track: Track) {
        self.pb.local = true;
        self.pb.playing = true;
        self.pb.loading = false;
        self.pb.track = Some(track);
        self.pb.set_position(0);
    }

    /// Animate two pretend decks so the decks screen can be shown offline.
    fn demo_decks(&mut self) {
        use crate::dj::DeckMeter;
        let songs: Vec<Track> = self
            .pages
            .first()
            .map(|p| p.tabs[0].items.iter().filter_map(|i| match i { Item::Track(t) => Some(t.clone()), _ => None }).collect())
            .unwrap_or_default();
        let beat = (self.frame % 15) as f32 / 15.0;
        for (d, deck) in self.decks.decks.iter_mut().enumerate() {
            if deck.track.is_none() && d == 0 {
                deck.track = songs.first().cloned();
                deck.playing = true;
                deck.pos_ms = 47_000;
            }
            if deck.playing {
                deck.pos_ms += 33;
            }
            deck.meter = DeckMeter {
                bpm: deck.track.as_ref().map(|_| if d == 0 { 124.0 } else { 126.5 }),
                phase: deck.playing.then_some(beat),
                level: if deck.playing { 0.5 + 0.3 * (1.0 - beat) } else { 0.0 },
                bands: if deck.playing {
                    let shape = [0.75 * (1.0 - beat).powi(2) + 0.1, 0.45, 0.25 + 0.2 * beat];
                    std::array::from_fn(|band| shape[band] * crate::dj::eq_gain(deck.eq[band]).min(1.3))
                } else {
                    [0.0; 3]
                },
            };
        }
        self.dirty = true;
    }

    fn demo_tick(&mut self) {
        let Some(track) = &self.pb.track else { return };
        if self.pb.playing && self.pb.position() >= track.duration_ms {
            self.pb.set_position(0);
        }
    }

    /// Persist the settings changed at runtime (volume, cover style).
    pub fn save_settings(&self) {
        if !self.demo {
            self.cfg.save().ok();
        }
    }
}

/// A plausible moving spectrum with a steady beat, for demo mode.
fn demo_snapshot(frame: u64) -> crate::audio::Snapshot {
    let t = frame as f32 * 0.09;
    let mut bands = [0.0; BANDS];
    // 120 bpm at 30 frames a second: a hit every 15 frames.
    let since_hit = (frame % 15) as f32 / 15.0;
    let kick = (1.0 - since_hit).powi(3);
    for (i, b) in bands.iter_mut().enumerate() {
        let x = i as f32 / BANDS as f32;
        let wave = (t * 1.7 + x * 9.0).sin() * 0.5 + 0.5;
        let shimmer = ((t * 5.3 + i as f32 * 1.9).sin() * 0.5 + 0.5) * 0.35;
        *b = ((wave * 0.4 + kick * (1.0 - x).powi(2) * 0.9 + shimmer) * (1.0 - x * 0.55)).clamp(0.02, 1.0);
    }
    crate::audio::Snapshot { bands, beat: frame % 15 == 0, bass: 0.3 + 0.6 * kick }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo;

    fn key(app: &mut App, code: KeyCode) {
        app.on_term(TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            key(app, KeyCode::Char(c));
        }
    }

    fn click(app: &mut App, column: u16, row: u16, button: MouseButton, modifiers: KeyModifiers) {
        app.on_term(TermEvent::Mouse(MouseEvent { kind: MouseEventKind::Down(button), column, row, modifiers }));
    }

    fn names(tracks: &[Track]) -> Vec<&str> {
        tracks.iter().map(|t| t.name.as_str()).collect()
    }

    #[test]
    fn x_selects_a_run_and_a_queues_it_in_order() {
        let mut app = demo::app(Config::default());
        typed(&mut app, "xx");
        key(&mut app, KeyCode::Down);
        typed(&mut app, "x");
        assert_eq!(names(&app.basket), ["Midnight Drive", "Glass Horizon", "Saltwater"]);

        typed(&mut app, "a");
        assert!(app.basket.is_empty(), "queueing uses up the selection");
        assert_eq!(names(&app.queue[..3]), ["Midnight Drive", "Glass Horizon", "Saltwater"]);
    }

    #[test]
    fn x_again_unselects_and_shift_arrows_only_add() {
        let mut app = demo::app(Config::default());
        typed(&mut app, "x");
        key(&mut app, KeyCode::Up);
        typed(&mut app, "x");
        assert!(app.basket.is_empty());

        for _ in 0..3 {
            app.on_term(TermEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT)));
        }
        assert_eq!(app.basket.len(), 3);
        // Going back up over them with shift held must not unselect anything.
        for _ in 0..3 {
            app.on_term(TermEvent::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)));
        }
        assert_eq!(app.basket.len(), 4);
        typed(&mut app, "X");
        assert!(app.basket.is_empty());
    }

    #[test]
    fn going_back_over_selected_songs_keeps_the_picked_order() {
        let mut app = demo::app(Config::default());
        for _ in 0..3 {
            app.on_term(TermEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT)));
        }
        let picked: Vec<String> = app.basket.iter().map(|t| t.name.clone()).collect();
        for _ in 0..2 {
            app.on_term(TermEvent::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT)));
        }
        assert_eq!(names(&app.basket)[..3], picked[..]);
    }

    #[test]
    fn a_refreshed_list_keeps_the_cursor_on_its_song() {
        let mut app = demo::app(Config::default());
        for _ in 0..4 {
            key(&mut app, KeyCode::Down);
        }
        let page = app.pages.last().unwrap();
        let (id, tab) = (page.id, page.tab);
        let here = page.cur().selected().unwrap().uri();
        // The same list comes back with a new song on top, as it does after liking one.
        let mut items = page.cur().items.clone();
        items.insert(0, Item::Track(Track { uri: "spotify:track:new".into(), name: "New".into(), ..Default::default() }));
        app.on_msg(Msg::Page { id, update: PageUpdate::Set { tab, items, done: true } });
        let tab = app.pages.last().unwrap().cur();
        assert_eq!(tab.selected().unwrap().uri(), here);
        assert_eq!(tab.sel, 5);
    }

    #[test]
    fn a_held_key_sends_its_command_once() {
        let mut app = demo::app(Config::default());
        let before = app.pb.shuffle;
        // Auto-repeat: the same key again within a few milliseconds.
        typed(&mut app, "sss");
        assert_eq!(app.pb.shuffle, !before);
    }

    #[test]
    fn selection_survives_search_and_going_back() {
        let mut app = demo::app(Config::default());
        typed(&mut app, "xx");
        typed(&mut app, "/neon");
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.page().unwrap().spec, PageSpec::Search(_)));
        assert_eq!(app.basket.len(), 2, "opening a search keeps the selection");
        key(&mut app, KeyCode::Esc);
        assert!(matches!(app.page().unwrap().spec, PageSpec::Liked));
        assert_eq!(app.basket.len(), 2, "so does going back");
        // And it can keep growing from wherever we are.
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        typed(&mut app, "x");
        assert_eq!(app.basket.len(), 3);
    }

    #[test]
    fn enter_plays_the_selection_in_picked_order() {
        let mut app = demo::app(Config::default());
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        typed(&mut app, "x"); // Paper Planes at Dawn
        key(&mut app, KeyCode::Char('g'));
        typed(&mut app, "x"); // Midnight Drive
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.pb.track.as_ref().unwrap().name, "Paper Planes at Dawn");
        assert_eq!(names(&app.queue), ["Midnight Drive"]);
        assert!(app.basket.is_empty());
    }

    #[test]
    fn ctrl_click_and_right_click_select_plain_click_does_not() {
        let mut app = demo::app(Config::default());
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
        terminal.draw(|f| crate::ui::draw(f, &mut app)).unwrap();
        let row_of = |app: &App, index: usize| {
            app.hits.iter().find_map(|(r, h)| matches!(h, Hit::Row(i) if *i == index).then_some((r.x + 10, r.y))).unwrap()
        };
        let (x, y) = row_of(&app, 3);
        click(&mut app, x, y, MouseButton::Left, KeyModifiers::CONTROL);
        let (x, y) = row_of(&app, 5);
        click(&mut app, x, y, MouseButton::Right, KeyModifiers::NONE);
        assert_eq!(names(&app.basket), ["Saltwater", "Velvet Static"]);

        // An ordinary click moves the cursor and leaves the selection alone.
        let (x, y) = row_of(&app, 8);
        click(&mut app, x, y, MouseButton::Left, KeyModifiers::NONE);
        assert_eq!(app.basket.len(), 2);
        assert_eq!(app.page().unwrap().cur().sel, 8);

        // Ctrl-click on a selected song takes it back out.
        let (x, y) = row_of(&app, 3);
        click(&mut app, x, y, MouseButton::Left, KeyModifiers::CONTROL);
        assert_eq!(names(&app.basket), ["Velvet Static"]);
    }

    #[test]
    fn v_selects_a_run_that_grows_and_shrinks_with_the_cursor() {
        let mut app = demo::app(Config::default());
        key(&mut app, KeyCode::Down);
        typed(&mut app, "V");
        assert_eq!(names(&app.basket), ["Glass Horizon"], "the starting row is in the run");
        for _ in 0..3 {
            key(&mut app, KeyCode::Down);
        }
        assert_eq!(app.basket.len(), 4);
        // Backing up shrinks the run again…
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Up);
        assert_eq!(names(&app.basket), ["Glass Horizon", "Paper Planes at Dawn"]);
        // …and crossing the starting row extends it the other way.
        for _ in 0..3 {
            key(&mut app, KeyCode::Up);
        }
        assert_eq!(names(&app.basket), ["Glass Horizon", "Midnight Drive"]);
        typed(&mut app, "V");
        assert!(!app.selecting_range());
        // Finished: moving no longer changes the selection.
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.basket.len(), 2);
    }

    #[test]
    fn a_run_keeps_songs_that_were_already_selected() {
        let mut app = demo::app(Config::default());
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        typed(&mut app, "x"); // Paper Planes at Dawn, selected by hand; cursor moves to row 3
        key(&mut app, KeyCode::Char('g'));
        typed(&mut app, "V");
        for _ in 0..4 {
            key(&mut app, KeyCode::Down);
        }
        assert_eq!(app.basket.len(), 5);
        // Shrinking the run back above it must not take the hand-picked song out.
        for _ in 0..4 {
            key(&mut app, KeyCode::Up);
        }
        assert_eq!(names(&app.basket), ["Paper Planes at Dawn", "Midnight Drive"]);
        // Esc ends the run without going anywhere or losing the selection.
        key(&mut app, KeyCode::Esc);
        assert!(!app.selecting_range());
        assert_eq!(app.basket.len(), 2);
        assert_eq!(app.focus, Focus::Main);
    }

    #[test]
    fn decks_load_from_the_list_and_take_over_the_transport() {
        let mut app = demo::app(Config::default());
        key(&mut app, KeyCode::Down);
        typed(&mut app, "2");
        assert!(app.decks_on(), "loading a deck switches the decks on");
        assert_eq!(app.decks.decks[1].track.as_ref().unwrap().name, "Glass Horizon");
        typed(&mut app, "1");
        assert_eq!(app.decks.decks[0].track.as_ref().unwrap().name, "Glass Horizon");

        typed(&mut app, "D");
        assert_eq!(app.screen, Screen::Decks);
        // Crossfader: right moves towards B, down re-centres, and it stops at the ends.
        for _ in 0..30 {
            key(&mut app, KeyCode::Right);
        }
        assert_eq!(app.decks.fader, 1.0);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.decks.fader, 0.5);
        // a and b are the decks' own play buttons.
        typed(&mut app, "b");
        assert!(app.decks.decks[1].playing);
        assert_eq!(app.deck_focus, 1);
        typed(&mut app, "b");
        assert!(!app.decks.decks[1].playing);
        // Esc goes back to the songs with the decks still on; D on the decks screen ends them.
        key(&mut app, KeyCode::Esc);
        assert!(app.screen == Screen::Browse && app.decks_on());
        typed(&mut app, "D");
        typed(&mut app, "D");
        assert!(!app.decks_on());
        assert_eq!(app.screen, Screen::Browse);
    }

    #[test]
    fn cover_cycle_has_pulse_and_no_off() {
        let mut app = demo::app(Config::default());
        // The depth style is only in the cycle when its model was built in.
        let mut expected = vec![ArtMode::Blocks, ArtMode::Ascii, ArtMode::Braille, ArtMode::Pulse];
        if cfg!(feature = "depth") {
            expected.push(ArtMode::Depth);
        }
        expected.push(ArtMode::Blocks);
        let mut seen = vec![app.art_mode];
        for _ in 1..expected.len() {
            typed(&mut app, "c");
            seen.push(app.art_mode);
        }
        assert_eq!(seen, expected);
    }

    #[test]
    fn party_and_mix_toggle_on_and_off() {
        let mut app = demo::app(Config::default());
        typed(&mut app, "z");
        assert!(app.party);
        typed(&mut app, "z");
        assert!(!app.party);
        assert!(!app.cfg.mix);
        typed(&mut app, "m");
        assert!(app.cfg.mix);
        typed(&mut app, "M");
        assert!(app.cfg.mix && app.cfg.mix_seconds == 9);
        typed(&mut app, "m");
        assert!(!app.cfg.mix);
    }

    #[test]
    fn holding_seek_moves_the_playhead_but_sends_one_seek() {
        let mut app = demo::app(Config::default());
        app.demo = false; // exercise the real seek path (there is no backend to call)
        app.pb.local = false;
        let before = app.position();
        for _ in 0..20 {
            typed(&mut app, ".");
        }
        assert!(app.position() >= before + 95_000, "the display follows every press");
        assert!(app.pending_seek.is_some(), "but the request waits for the key to be released");
        std::thread::sleep(Duration::from_millis(240));
        app.on_tick();
        assert!(app.pending_seek.is_none());
    }

    #[test]
    fn holding_volume_changes_the_level_but_reports_it_once() {
        let mut app = demo::app(Config::default());
        let start = app.pb.volume;
        for _ in 0..30 {
            typed(&mut app, "-");
            app.on_tick();
        }
        assert_eq!(app.pb.volume, start.saturating_sub(150).max(0));
        assert!(app.pending_volume.is_some(), "nothing is reported while the key is held");
        // A stale level echoed back by the player must not undo the change.
        app.on_msg(Msg::Engine(Event::Volume(start)));
        assert_eq!(app.pb.volume, 0);
        std::thread::sleep(Duration::from_millis(380));
        app.on_tick();
        assert!(app.pending_volume.is_none());
    }

    #[test]
    fn a_stale_remote_device_does_not_capture_the_space_bar() {
        // Spotify reports this app's own previous run as the "active" device.
        let mut app = demo::app(Config::default());
        app.demo = false;
        app.pb = Playback::default();
        let me = app.cfg.device_id.clone();
        let track = Track { uri: "spotify:track:x".into(), name: "Song".into(), duration_ms: 200_000, playable: true, ..Default::default() };
        let stale = |id: &str, playing: bool| crate::api::RemoteState {
            track: Some(track.clone()),
            is_playing: playing,
            progress_ms: 30_000,
            shuffle: false,
            repeat: Repeat::Off,
            device: Some(Device { id: id.into(), name: "riff".into(), kind: "computer".into(), volume: 50, active: true, this: false }),
        };
        app.on_msg(Msg::Remote(stale(&me, true)));
        assert!(app.pb.remote_name.is_none(), "our own old session is not a remote device");
        assert!(!app.pb.playing);

        app.pb = Playback::default();
        app.on_msg(Msg::Remote(stale("someone-else", false)));
        assert!(app.pb.remote_name.is_none(), "a paused device elsewhere is not in control");

        app.pb = Playback::default();
        app.on_msg(Msg::Remote(stale("someone-else", true)));
        assert_eq!(app.pb.remote_name.as_deref(), Some("riff"));
    }
}
