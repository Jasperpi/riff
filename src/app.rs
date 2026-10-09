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
use crate::viz::{BANDS, Tap};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Sidebar,
    Main,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Browse,
    NowPlaying,
}

pub enum Overlay {
    None,
    Help,
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
    image_order: VecDeque<String>,
    image_pending: HashSet<String>,
    pub art_mode: ArtMode,
    pub art_cache: HashMap<(String, u16, u16, ArtMode), Arc<Rendered>>,
    pub palette: Palette,
    palette_target: Palette,
    palette_for: String,

    pub lyrics: LyricState,
    lyrics_for: String,
    /// Manual scroll offset in the lyrics pane; `None` follows the music.
    pub lyrics_scroll: Option<usize>,

    pub tap: Arc<Tap>,
    pub bars: [f32; BANDS],
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
    pub fn new(cfg: Config, backend: Option<Backend>, tap: Arc<Tap>) -> Self {
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
            image_order: VecDeque::new(),
            image_pending: HashSet::new(),
            art_mode,
            art_cache: HashMap::new(),
            palette: Palette::default(),
            palette_target: Palette::default(),
            palette_for: String::new(),
            lyrics: LyricState::Idle,
            lyrics_for: String::new(),
            lyrics_scroll: None,
            tap,
            bars: [0.0; BANDS],
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
        let animating = self.palette != self.palette_target || self.bars.iter().any(|b| *b > 0.01);
        let viz_live = self.cfg.visualizer && self.pb.playing && (self.pb.local || self.demo);
        if animating || viz_live {
            Duration::from_millis(33)
        } else if self.pb.playing || self.toast.is_some() {
            Duration::from_millis(200)
        } else {
            Duration::from_millis(1000)
        }
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
        let page = self.new_page(spec);
        self.pages = vec![page];
        self.dirty = true;
    }

    /// Drill into an album, artist, playlist or search; `back` returns.
    pub fn push(&mut self, spec: PageSpec) {
        if self.page().is_some_and(|p| p.spec == spec) {
            return;
        }
        let page = self.new_page(spec);
        self.pages.push(page);
        if self.pages.len() > 40 {
            self.pages.remove(1);
        }
        self.screen = Screen::Browse;
        self.focus = Focus::Main;
        self.dirty = true;
    }

    fn back(&mut self) {
        if self.screen == Screen::NowPlaying {
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
        if self.demo {
            self.pb.set_position(self.pb.position());
            self.pb.playing = !self.pb.playing;
            return;
        }
        let Some(be) = self.backend.clone() else { return };
        if self.pb.local {
            let r = be.engine.play_pause();
            self.try_engine(r);
        } else if self.remote_active() {
            let pos = self.pb.position();
            be.remote(if self.pb.playing { RemoteCmd::Pause } else { RemoteCmd::Resume });
            self.pb.playing = !self.pb.playing;
            self.pb.set_position(pos);
        } else if self.pb.track.is_some() {
            // Nothing is active anywhere: pick the last session back up on this device.
            let r = be.engine.take_over();
            self.try_engine(r);
        } else {
            self.toast("Nothing playing yet. Pick a track and press Enter", false);
        }
    }

    fn skip(&mut self, forward: bool) {
        let Some(be) = self.backend.clone() else { return };
        if self.pb.local {
            let r = if forward { be.engine.next() } else { be.engine.prev() };
            self.try_engine(r);
        } else if self.remote_active() {
            be.remote(if forward { RemoteCmd::Next } else { RemoteCmd::Prev });
        }
    }

    fn seek_by(&mut self, delta_ms: i64) {
        let Some(track) = &self.pb.track else { return };
        let dur = track.duration_ms as i64;
        let target = (self.pb.position() as i64 + delta_ms).clamp(0, (dur - 1000).max(0)) as u32;
        self.seek_to(target);
    }

    fn seek_to(&mut self, ms: u32) {
        self.pb.set_position(ms);
        self.lyrics_scroll = None;
        let Some(be) = self.backend.clone() else { return };
        if self.pb.local {
            let r = be.engine.seek(ms);
            self.try_engine(r);
        } else if self.remote_active() {
            be.remote(RemoteCmd::Seek(ms));
        }
    }

    fn volume_by(&mut self, delta: i16) {
        let v = (self.pb.volume as i16 + delta).clamp(0, 100) as u8;
        self.pb.volume = v;
        self.cfg.volume = v;
        let Some(be) = self.backend.clone() else { return };
        if self.remote_active() {
            be.remote(RemoteCmd::Volume(v));
        } else {
            let r = be.engine.volume(v);
            // Before the first connection the saved volume simply applies on connect.
            if be.engine.is_online() {
                self.try_engine(r);
            }
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
        if self.demo {
            return self.demo_play(track);
        }

        // Every track visible in this list, and where the chosen one sits in it.
        let mut uris = Vec::new();
        let mut index = 0;
        for &i in &tab.view {
            if let Item::Track(t) = &tab.items[i] {
                if t.playable {
                    if t.uri == track.uri {
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
        match be.engine.play_tracks(uris.clone(), index, shuffle, &repeat) {
            Ok(()) => {
                // Reflect the choice instantly; the player confirms within moments.
                self.pb.loading = true;
                self.pb.source = Some(source);
                self.pb.list = uris;
                // Volume changes made before this device was active were not applied.
                let _ = be.engine.volume(self.pb.volume);
                self.set_track(track, true);
                self.pb.set_position(0);
            }
            Err(e) => self.toast(e.to_string(), true),
        }
    }

    fn enqueue_selected(&mut self) {
        let Some(Item::Track(t)) = self.page().and_then(|p| p.cur().selected()).cloned() else {
            return self.toast("Only tracks can be queued", false);
        };
        if self.pb.track.is_none() {
            return self.toast("Start playing something first, then queue", false);
        }
        if let Some(be) = &self.backend {
            be.queue_add(t.uri, t.name);
        }
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
            self.pb.set_position(0);
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
        if !self.pb.local || self.pb.shuffle {
            self.queue.clear();
            return;
        }
        let current = self.pb.uri();
        let upcoming: Vec<String> = match self.pb.list.iter().position(|u| u == current) {
            Some(i) => self.pb.list.iter().skip(i + 1).take(50).cloned().collect(),
            None => Vec::new(),
        };
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
        if let Some(be) = &self.backend {
            self.image_pending.insert(url.to_string());
            be.load_image(url.to_string());
        }
    }

    fn store_image(&mut self, url: String, image: Arc<RgbImage>) {
        if self.images.insert(url.clone(), image).is_none() {
            self.image_order.push_back(url);
        }
        while self.image_order.len() > 24 {
            if let Some(old) = self.image_order.pop_front() {
                self.images.remove(&old);
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
                        if let Some(t) = page.tabs.get_mut(tab) {
                            t.items = items;
                            t.loading = !done;
                            t.done = done;
                            t.error = None;
                            t.refilter();
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
                if let Some(image) = image {
                    self.store_image(url, image);
                }
            }
            Msg::Lyrics { uri, lyrics } => {
                if uri == self.lyrics_for {
                    self.lyrics = match lyrics {
                        Some(l) => LyricState::Ready(l),
                        None => LyricState::Missing,
                    };
                }
            }
            Msg::Queue(tracks) => self.queue = tracks,
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
                        self.pb.playing = state.is_playing;
                        self.pb.set_position(state.progress_ms);
                        self.pb.shuffle = state.shuffle;
                        self.pb.repeat = state.repeat;
                        if let Some(d) = state.device.filter(|d| d.active) {
                            self.pb.remote_name = Some(d.name);
                        }
                    }
                }
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
                    if let Some(be) = &self.backend {
                        be.load_sidebar();
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
                if self.pb.uri() == uri || self.pb.track.is_none() {
                    self.pb.set_position(pos_ms);
                }
            }
            Event::Paused { uri, pos_ms } => {
                self.pb.loading = false;
                self.pb.playing = false;
                if self.pb.uri() == uri {
                    self.pb.set_position(pos_ms);
                }
            }
            Event::Loading { uri, pos_ms } => {
                self.pb.local = true;
                self.pb.loading = true;
                if self.pb.uri() == uri {
                    self.pb.set_position(pos_ms);
                }
            }
            Event::Position(ms) => self.pb.set_position(ms),
            Event::Stopped => {
                self.pb.playing = false;
                self.pb.loading = false;
            }
            Event::Unavailable => {
                self.pb.loading = false;
                self.toast("Track unavailable, skipping", true);
            }
            Event::Volume(v) => {
                if self.pb.local || self.pb.remote_name.is_none() {
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

        // Spectrum bars rise instantly and fall smoothly.
        let live = if self.demo && self.pb.playing {
            Some(demo_bands(self.frame))
        } else {
            self.tap.current()
        };
        let mut moving = false;
        for (i, bar) in self.bars.iter_mut().enumerate() {
            let target = live.map(|b| b[i]).unwrap_or(0.0);
            let next = if target > *bar { target } else { (*bar - 0.045).max(target).max(0.0) };
            if (next - *bar).abs() > 0.001 {
                moving = true;
            }
            *bar = next;
        }
        if moving && self.cfg.visualizer {
            self.dirty = true;
        }

        let sec = self.pb.position() / 1000;
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
            mpris.sync(self.pb.track.as_ref(), self.pb.playing, self.pb.position());
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
            Overlay::Help => {
                self.overlay = Overlay::None;
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

        // Keys that work everywhere.
        match key.code {
            KeyCode::Char('q') => return self.quit = true,
            KeyCode::Char('?') => return self.overlay = Overlay::Help,
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
                return self.toast(format!("Cover style: {}", self.art_mode.name()), false);
            }
            KeyCode::Char('v') => {
                self.screen = match self.screen {
                    Screen::Browse => Screen::NowPlaying,
                    Screen::NowPlaying => Screen::Browse,
                };
                self.ensure_lyrics();
                return;
            }
            KeyCode::Esc | KeyCode::Backspace => return self.back(),
            _ => {}
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
                match key.code {
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
                    KeyCode::Char('a') => self.enqueue_selected(),
                    KeyCode::Char('f') if !ctrl => self.like_selected(),
                    KeyCode::Char('f') | KeyCode::Char('\\') => self.overlay = Overlay::Filter,
                    _ => {}
                }
            }
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
                    (Overlay::None, _) if self.screen == Screen::NowPlaying => {
                        let total = match &self.lyrics {
                            LyricState::Ready(l) => l.lines.len(),
                            _ => 0,
                        };
                        let cur = self.lyrics_scroll.unwrap_or_else(|| self.lyric_line().unwrap_or(0));
                        self.lyrics_scroll =
                            Some((cur as isize + delta.signum()).clamp(0, total.saturating_sub(1) as isize) as usize);
                    }
                    (Overlay::None, _) => {
                        if let Some(page) = self.pages.last_mut() {
                            page.cur_mut().step(delta);
                        }
                    }
                    _ => {}
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
                            Screen::Browse => Screen::NowPlaying,
                            Screen::NowPlaying => Screen::Browse,
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
            LyricState::Ready(l) => crate::lyrics::current_line(l, self.pb.position()),
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

/// A plausible moving spectrum for demo mode.
fn demo_bands(frame: u64) -> [f32; BANDS] {
    let t = frame as f32 * 0.09;
    let mut out = [0.0; BANDS];
    for (i, b) in out.iter_mut().enumerate() {
        let x = i as f32 / BANDS as f32;
        let wave = (t * 1.7 + x * 9.0).sin() * 0.5 + 0.5;
        let beat = ((t * 2.6).sin() * 0.5 + 0.5).powi(3) * (1.0 - x).powi(2);
        let shimmer = ((t * 5.3 + i as f32 * 1.9).sin() * 0.5 + 0.5) * 0.35;
        *b = ((wave * 0.45 + beat * 0.8 + shimmer) * (1.0 - x * 0.55)).clamp(0.02, 1.0);
    }
    out
}
