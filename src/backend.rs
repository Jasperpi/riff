//! Loads everything the interface shows. Each loader answers from the disk
//! cache first, then revalidates over the network and sends only what changed.
//!
//! Almost everything travels over the player's own connection (`spot`). The
//! Web API (`api`) is optional: present only when the user has added their own
//! Client ID, and used for the handful of things nothing else can provide.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use image::RgbImage;
use librespot_core::Session;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::api::{self, Api, RemoteCmd, RemoteState};
use crate::cache::Cache;
use crate::engine::{self, Engine};
use crate::model::*;
use crate::{lyrics, spot};

pub enum Msg {
    Engine(engine::Event),
    Sidebar(Vec<SideEntry>),
    Page { id: u64, update: PageUpdate },
    /// URIs of every liked track, for the hearts.
    Liked(Vec<String>),
    LikeFailed { uri: String, was: bool },
    Image { url: String, image: Option<Arc<RgbImage>> },
    /// How near each part of a cover is, for the depth cover style.
    Depth { url: String, map: Option<Arc<crate::art::DepthMap>> },
    Lyrics { uri: String, lyrics: Option<Lyrics> },
    Queue(Vec<Track>),
    /// Songs that were to be queued but never made it.
    QueueFailed(Vec<String>),
    Devices(Vec<Device>),
    TrackInfo(Track),
    Remote(RemoteState),
    Toast { text: String, error: bool },
    /// A media key or desktop widget asked for something.
    Media(crate::mpris::MediaKey),
    /// The device we were remote-controlling is no longer there.
    RemoteGone,
}

pub enum PageUpdate {
    Head(Head),
    Set { tab: usize, items: Vec<Item>, done: bool },
    Append { tab: usize, items: Vec<Item>, done: bool },
    Error { tab: usize, message: String },
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Head {
    pub title: String,
    pub subtitle: String,
    pub image: Option<String>,
    /// Spotify context to play from, when the page is one.
    pub context: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    head: Head,
    tabs: Vec<Vec<Item>>,
}

pub const NEEDS_CLIENT_ID: &str = "This needs your own Spotify Client ID. Quit and run: riff setup";

#[derive(Clone)]
pub struct Backend {
    pub api: Option<Arc<Api>>,
    pub engine: Arc<Engine>,
    pub cache: Cache,
    tx: UnboundedSender<Msg>,
    http: reqwest::Client,
    /// Tracks seen this session, so queue and now-playing lookups are free.
    memo: Arc<Mutex<HashMap<String, Track>>>,
    /// How far into Spotify's results each search tab has read, by page and
    /// tab. Not the same as the rows shown: some results come back empty.
    search_at: Arc<Mutex<HashMap<(u64, usize), usize>>>,
    /// Covers have their depth worked out one at a time.
    #[cfg_attr(not(feature = "depth"), allow(dead_code))]
    depth_turn: Arc<tokio::sync::Mutex<()>>,
}

/// A list refreshed this recently is shown as-is; flipping between pages
/// shouldn't cost a request each time.
const FRESH: Duration = Duration::from_secs(45);

/// A row standing in for a song whose details never arrived.
fn unresolved(t: &Track) -> bool {
    t.id.is_empty() && t.uri.starts_with("spotify:track:")
}

const SOME_MISSING: &str = "Some songs didn't load. R to try again";

fn tracks_to_items(tracks: Vec<Track>) -> Vec<Item> {
    tracks.into_iter().map(Item::Track).collect()
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

fn head(title: &str, subtitle: String) -> Head {
    Head { title: title.to_string(), subtitle, image: None, context: None }
}

impl Backend {
    pub fn new(
        api: Option<Arc<Api>>,
        engine: Arc<Engine>,
        cache: Cache,
        tx: UnboundedSender<Msg>,
    ) -> Self {
        Self {
            api,
            engine,
            cache,
            tx,
            http: reqwest::Client::new(),
            memo: Default::default(),
            search_at: Default::default(),
            depth_turn: Default::default(),
        }
    }

    fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }

    fn page(&self, id: u64, update: PageUpdate) {
        self.send(Msg::Page { id, update });
    }

    fn toast_err(&self, e: &anyhow::Error) {
        self.send(Msg::Toast { text: e.to_string(), error: true });
    }

    /// A refresh failed: with something cached on screen that's only a notice,
    /// otherwise the page has nothing to show but the error.
    fn fail(&self, id: u64, tabs: usize, have_cache: bool, e: anyhow::Error) {
        if have_cache {
            self.toast_err(&e);
        } else {
            for tab in 0..tabs {
                self.page(id, PageUpdate::Error { tab, message: e.to_string() });
            }
        }
    }

    fn web(&self) -> Result<&Arc<Api>> {
        self.api.as_ref().ok_or_else(|| anyhow!(NEEDS_CLIENT_ID))
    }

    fn remember(&self, items: &[Item]) {
        let mut memo = self.memo.lock().unwrap();
        if memo.len() > 30_000 {
            memo.clear();
        }
        for item in items {
            if let Item::Track(t) = item {
                if !t.name.is_empty() && !unresolved(t) {
                    memo.insert(t.uri.clone(), t.clone());
                }
            }
        }
    }

    pub fn known(&self, uri: &str) -> Option<Track> {
        self.memo.lock().unwrap().get(uri).cloned()
    }

    /// Show a cached page straight away. Returns the cached tabs, their
    /// revision, and whether they are recent enough to skip the network.
    fn show_cached(&self, id: u64, key: &str) -> (Vec<Vec<Item>>, String, bool) {
        match self.cache.get::<Stored>(key) {
            Some(hit) => {
                self.page(id, PageUpdate::Head(hit.value.head));
                for (tab, items) in hit.value.tabs.iter().enumerate() {
                    self.remember(items);
                    self.page(id, PageUpdate::Set { tab, items: items.clone(), done: true });
                }
                (hit.value.tabs, hit.rev, hit.age < FRESH)
            }
            None => (Vec::new(), String::new(), false),
        }
    }

    // ---- sidebar ------------------------------------------------------

    pub fn load_sidebar(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            let cached = this.cache.get::<Vec<SideEntry>>("rootlist");
            let have_cache = cached.is_some();
            if let Some(hit) = cached {
                this.send(Msg::Sidebar(hit.value));
            }
            let Ok(session) = this.engine.session().await else { return };
            match spot::rootlist(&session).await {
                Ok(entries) => {
                    this.cache.put("rootlist", "", &entries);
                    this.send(Msg::Sidebar(entries));
                }
                Err(e) => {
                    log::warn!("rootlist: {e}");
                    // With nothing cached the sidebar would just sit empty.
                    if !have_cache {
                        this.toast_err(&anyhow!("Couldn't load your playlists: {e}"));
                    }
                }
            }
        });
    }

    // ---- saved library ------------------------------------------------

    /// Every saved track and album (URI, added-at), newest first. After the
    /// first full download only the changes since then are fetched.
    async fn library(&self, session: &Session) -> Result<Vec<(String, i64)>> {
        if let Some(hit) = self.cache.get::<Vec<(String, i64)>>("collection") {
            if hit.age < FRESH {
                return Ok(hit.value);
            }
            if !hit.rev.is_empty() {
                match spot::collection_delta(session, "collection", &hit.rev).await {
                    Ok(Some((changes, next))) => {
                        let mut items = hit.value;
                        for change in &changes {
                            items.retain(|(uri, _)| uri != &change.uri);
                            if !change.removed {
                                items.push((change.uri.clone(), change.added_at));
                            }
                        }
                        if !changes.is_empty() {
                            items.sort_by(|a, b| b.1.cmp(&a.1));
                        }
                        let rev = if next.is_empty() { hit.rev } else { next };
                        self.cache.put("collection", &rev, &items);
                        return Ok(items);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        // Offline or flaky: what we had is better than nothing.
                        log::warn!("library delta: {e}");
                        return Ok(hit.value);
                    }
                }
            }
        }
        let (items, sync) = spot::collection(session, "collection").await?;
        let items: Vec<(String, i64)> = items.into_iter().map(|s| (s.uri, s.added_at)).collect();
        self.cache.put("collection", &sync, &items);
        Ok(items)
    }

    async fn liked(&self, id: u64) {
        let (tabs, _, _) = self.show_cached(id, "liked");
        let cached = tabs.into_iter().next().unwrap_or_default();
        let have_cache = !cached.is_empty();
        if have_cache {
            self.send(Msg::Liked(cached.iter().map(Item::uri).collect()));
        }

        let library = async {
            let session = self.engine.session().await?;
            let library = self.library(&session).await?;
            anyhow::Ok((session, library))
        };
        let (session, library) = match library.await {
            Ok(v) => v,
            Err(e) => return self.fail(id, 1, have_cache, e),
        };
        let want: Vec<(String, i64)> =
            library.into_iter().filter(|(uri, _)| uri.starts_with("spotify:track:")).collect();
        self.send(Msg::Liked(want.iter().map(|(u, _)| u.clone()).collect()));
        let title = |n: usize| head("Liked Songs", plural(n, "song"));
        // An empty cache is no cache: a library with nothing liked still has
        // to be told it has finished loading.
        if have_cache && cached.len() == want.len() && cached.iter().zip(&want).all(|(c, w)| c.uri() == w.0) {
            return;
        }

        // Reuse details already on disk; only new arrivals need fetching.
        let mut have: HashMap<String, Track> = cached
            .into_iter()
            .filter_map(|i| match i {
                Item::Track(t) if !unresolved(&t) => Some((t.uri.clone(), t)),
                _ => None,
            })
            .collect();
        self.page(id, PageUpdate::Head(title(want.len())));
        let mut all: Vec<Item> = Vec::with_capacity(want.len());
        let mut complete = true;
        const STEP: usize = 400;
        for (n, chunk) in want.chunks(STEP).enumerate() {
            let missing: Vec<String> =
                chunk.iter().filter(|(u, _)| !have.contains_key(u)).map(|(u, _)| u.clone()).collect();
            if !missing.is_empty() {
                let (tracks, answered) = spot::tracks_checked(&session, &missing).await;
                complete &= answered;
                for t in tracks {
                    have.insert(t.uri.clone(), t);
                }
            }
            let items: Vec<Item> = chunk
                .iter()
                .filter_map(|(uri, at)| {
                    let mut t = have.remove(uri)?;
                    t.added_at = Some(*at);
                    Some(Item::Track(t))
                })
                .collect();
            // A first load fills in as it arrives; a refresh swaps in whole below.
            if !have_cache {
                let done = (n + 1) * STEP >= want.len();
                let update = if n == 0 {
                    PageUpdate::Set { tab: 0, items: items.clone(), done }
                } else {
                    PageUpdate::Append { tab: 0, items: items.clone(), done }
                };
                self.page(id, update);
            }
            all.extend(items);
        }
        self.remember(&all);
        if have_cache || all.is_empty() {
            self.page(id, PageUpdate::Set { tab: 0, items: all.clone(), done: true });
        }
        // Rows we never heard back about are shown, but not kept: the next
        // visit should ask again rather than find them on disk.
        if complete {
            self.cache.put("liked", "", &Stored { head: title(all.len()), tabs: vec![all] });
        } else {
            self.send(Msg::Toast { text: SOME_MISSING.into(), error: true });
        }
    }

    async fn saved_albums(&self, id: u64) {
        let (tabs, _, _) = self.show_cached(id, "albums");
        let cached = tabs.into_iter().next().unwrap_or_default();
        let have_cache = !cached.is_empty();
        let result = async {
            let session = self.engine.session().await?;
            let want: Vec<String> = self
                .library(&session)
                .await?
                .into_iter()
                .filter_map(|(uri, _)| uri.strip_prefix("spotify:album:").map(str::to_string))
                .collect();
            if have_cache
                && cached.len() == want.len()
                && cached.iter().zip(&want).all(|(c, id)| matches!(c, Item::Album(a) if &a.id == id))
            {
                return Ok(None);
            }
            let mut have: HashMap<String, Album> = cached
                .iter()
                .filter_map(|i| match i {
                    Item::Album(a) => Some((a.id.clone(), a.clone())),
                    _ => None,
                })
                .collect();
            let missing: Vec<String> = want.iter().filter(|id| !have.contains_key(*id)).cloned().collect();
            for a in spot::albums(&session, &missing).await {
                have.insert(a.id.clone(), a);
            }
            let items: Vec<Item> = want.iter().filter_map(|id| have.remove(id)).map(Item::Album).collect();
            anyhow::Ok(Some(items))
        }
        .await;
        match result {
            Ok(Some(items)) => {
                let h = head("Albums", plural(items.len(), "album"));
                self.cache.put("albums", "", &Stored { head: h.clone(), tabs: vec![items.clone()] });
                self.page(id, PageUpdate::Head(h));
                self.page(id, PageUpdate::Set { tab: 0, items, done: true });
            }
            Ok(None) => {}
            Err(e) => self.fail(id, 1, have_cache, e),
        }
    }

    async fn followed_artists(&self, id: u64) {
        let (tabs, _, fresh) = self.show_cached(id, "artists");
        let have_cache = !tabs.is_empty();
        if fresh {
            return;
        }
        let result = async {
            let session = self.engine.session().await?;
            let (saved, _) = spot::collection(&session, "artist").await?;
            let ids: Vec<String> = saved
                .iter()
                .filter_map(|s| s.uri.strip_prefix("spotify:artist:").map(str::to_string))
                .collect();
            let mut artists = spot::artists(&session, &ids).await;
            artists.sort_by_key(|a| a.name.to_lowercase());
            anyhow::Ok(artists.into_iter().map(Item::Artist).collect::<Vec<_>>())
        }
        .await;
        match result {
            Ok(items) => {
                let h = head("Artists", plural(items.len(), "artist"));
                self.cache.put("artists", "", &Stored { head: h.clone(), tabs: vec![items.clone()] });
                self.page(id, PageUpdate::Head(h));
                self.page(id, PageUpdate::Set { tab: 0, items, done: true });
            }
            Err(e) => self.fail(id, 1, have_cache, e),
        }
    }

    // ---- pages --------------------------------------------------------

    pub fn load_page(&self, id: u64, spec: PageSpec) {
        let this = self.clone();
        tokio::spawn(async move {
            match spec {
                PageSpec::Liked => this.liked(id).await,
                PageSpec::SavedAlbums => this.saved_albums(id).await,
                PageSpec::FollowedArtists => this.followed_artists(id).await,
                PageSpec::Recent => {
                    this.web_list(id, "recent", "Recently Played", async {
                        Ok(tracks_to_items(this.web()?.recent().await?))
                    })
                    .await
                }
                PageSpec::Top => {
                    this.web_list(id, "top", "On Repeat", async {
                        Ok(tracks_to_items(this.web()?.top_tracks().await?))
                    })
                    .await
                }
                PageSpec::Playlist(pid) => this.playlist(id, &pid).await,
                PageSpec::Album(aid) => this.album(id, &aid).await,
                PageSpec::Artist(aid) => this.artist(id, &aid).await,
                PageSpec::Search(q) => this.search(id, &q).await,
            }
        });
    }

    /// A single list that only the Web API can provide.
    async fn web_list(&self, id: u64, key: &str, title: &str, fetch: impl Future<Output = Result<Vec<Item>>>) {
        let (tabs, _, fresh) = self.show_cached(id, key);
        if fresh {
            return;
        }
        match fetch.await {
            Ok(items) => {
                let h = head(title, String::new());
                self.remember(&items);
                self.cache.put(key, "", &Stored { head: h.clone(), tabs: vec![items.clone()] });
                self.page(id, PageUpdate::Head(h));
                self.page(id, PageUpdate::Set { tab: 0, items, done: true });
            }
            Err(e) => self.fail(id, 1, !tabs.is_empty(), e),
        }
    }

    async fn playlist(&self, id: u64, pid: &str) {
        let key = format!("pl_{pid}");
        let (tabs, cached_rev, _) = self.show_cached(id, &key);
        let have_cache = !tabs.is_empty();
        let cached_len = tabs.first().map(Vec::len).unwrap_or(0);

        let session = match self.engine.session().await {
            Ok(s) => s,
            Err(e) => return self.fail(id, 1, have_cache, e),
        };
        let info = match spot::playlist(&session, pid).await {
            Ok(h) => h,
            Err(e) => return self.fail(id, 1, have_cache, e),
        };
        let page_head = Head {
            title: info.playlist.name.clone(),
            subtitle: {
                let n = plural(info.items.len(), "track");
                if info.playlist.owner.is_empty() { n } else { format!("{n} · by {}", info.playlist.owner) }
            },
            image: info.playlist.image.clone(),
            context: Some(format!("spotify:playlist:{pid}")),
        };
        self.page(id, PageUpdate::Head(page_head.clone()));
        // Same revision: the contents are exactly what the cache already showed.
        if have_cache && cached_rev == info.revision && cached_len == info.items.len() {
            return;
        }

        let uris: Vec<String> = info.items.iter().map(|(u, _)| u.clone()).collect();
        let mut all: Vec<Item> = Vec::with_capacity(uris.len());
        let mut complete = true;
        const STEP: usize = 400;
        for (n, chunk) in uris.chunks(STEP).enumerate() {
            let (mut tracks, answered) = spot::tracks_checked(&session, chunk).await;
            complete &= answered;
            for (i, t) in tracks.iter_mut().enumerate() {
                t.added_at = info.items[n * STEP + i].1;
            }
            let items = tracks_to_items(tracks);
            if !have_cache {
                let done = all.len() + items.len() >= uris.len();
                let update = if n == 0 {
                    PageUpdate::Set { tab: 0, items: items.clone(), done }
                } else {
                    PageUpdate::Append { tab: 0, items: items.clone(), done }
                };
                self.page(id, update);
            }
            all.extend(items);
        }
        self.remember(&all);
        if have_cache || uris.is_empty() {
            self.page(id, PageUpdate::Set { tab: 0, items: all.clone(), done: true });
        }
        if complete {
            self.cache.put(&key, &info.revision, &Stored { head: page_head, tabs: vec![all] });
        } else {
            self.send(Msg::Toast { text: SOME_MISSING.into(), error: true });
        }
    }

    async fn album(&self, id: u64, aid: &str) {
        let key = format!("al_{aid}");
        let (tabs, _, _) = self.show_cached(id, &key);
        // Released albums don't change; a cached one is as good as a fresh one.
        if !tabs.is_empty() {
            return;
        }
        let result = async {
            let session = self.engine.session().await?;
            let (album, uris) = spot::album(&session, aid).await?;
            let head = Head {
                title: album.name.clone(),
                subtitle: [album.artist_line(), album.year.clone(), plural(uris.len(), "track")]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · "),
                image: album.image.clone(),
                context: Some(format!("spotify:album:{aid}")),
            };
            self.page(id, PageUpdate::Head(head.clone()));
            let (tracks, complete) = spot::tracks_checked(&session, &uris).await;
            anyhow::Ok((head, tracks_to_items(tracks), complete))
        }
        .await;
        match result {
            Ok((head, items, complete)) => {
                self.remember(&items);
                // A cached album is never fetched again, so only keep a whole one.
                if complete {
                    self.cache.put(&key, "", &Stored { head, tabs: vec![items.clone()] });
                } else {
                    self.send(Msg::Toast { text: SOME_MISSING.into(), error: true });
                }
                self.page(id, PageUpdate::Set { tab: 0, items, done: true });
            }
            Err(e) => self.fail(id, 1, false, e),
        }
    }

    async fn artist(&self, id: u64, aid: &str) {
        let key = format!("ar_{aid}");
        let age = self.cache.get::<Stored>(&key).map(|h| h.age);
        let (tabs, _, _) = self.show_cached(id, &key);
        if age.is_some_and(|age| age < Duration::from_secs(24 * 3600)) {
            return;
        }
        let result = async {
            let session = self.engine.session().await?;
            let a = spot::artist(&session, aid).await?;
            let head = Head {
                title: a.artist.name.clone(),
                subtitle: "Artist".into(),
                image: a.artist.image.clone(),
                context: None,
            };
            self.page(id, PageUpdate::Head(head.clone()));
            let (top, albums, singles) = tokio::join!(
                spot::tracks(&session, &a.top),
                spot::albums(&session, &a.albums),
                spot::albums(&session, &a.singles),
            );
            let tabs = vec![
                tracks_to_items(top),
                albums.into_iter().map(Item::Album).collect(),
                singles.into_iter().map(Item::Album).collect::<Vec<_>>(),
            ];
            anyhow::Ok((head, tabs))
        }
        .await;
        match result {
            Ok((head, tabs)) => {
                for (tab, items) in tabs.iter().enumerate() {
                    self.remember(items);
                    self.page(id, PageUpdate::Set { tab, items: items.clone(), done: true });
                }
                self.cache.put(&key, "", &Stored { head, tabs });
            }
            Err(e) => self.fail(id, 3, !tabs.is_empty(), e),
        }
    }

    /// Search. With a Client ID the Web API returns every kind of result; without
    /// one, tracks come over the player connection and the album and artist tabs
    /// are drawn from those tracks.
    async fn search(&self, id: u64, query: &str) {
        self.page(id, PageUpdate::Head(head(&format!("“{query}”"), "Search".into())));
        let result = async {
            if let Some(api) = &self.api {
                match api.search(query, "track,album,artist,playlist", 0).await {
                    Ok(r) => {
                        let mut at = self.search_at.lock().unwrap();
                        if at.len() > 400 {
                            at.clear();
                        }
                        for (tab, seen) in r.seen.iter().enumerate() {
                            at.insert((id, tab), *seen);
                        }
                        return Ok(vec![
                            (tracks_to_items(r.tracks), r.more[0]),
                            (r.albums.into_iter().map(Item::Album).collect(), r.more[1]),
                            (r.artists.into_iter().map(Item::Artist).collect(), r.more[2]),
                            (r.playlists.into_iter().map(Item::Playlist).collect(), r.more[3]),
                        ]);
                    }
                    // The Web API being busy shouldn't take song search down
                    // with it: the player connection can still do that part.
                    Err(e) => {
                        log::warn!("web search: {e}");
                        self.send(Msg::Toast { text: "Web API is busy. Showing songs only".into(), error: false });
                    }
                }
            }
            let session = self.engine.session().await?;
            let uris = spot::search_tracks(&session, query).await?;
            let tracks: Vec<Track> =
                spot::tracks(&session, &uris).await.into_iter().filter(|t| !t.id.is_empty()).collect();
            let mut albums: Vec<Album> = Vec::new();
            let mut artists: Vec<Artist> = Vec::new();
            for t in &tracks {
                if !t.album_id.is_empty() && !albums.iter().any(|a| a.id == t.album_id) {
                    albums.push(Album {
                        id: t.album_id.clone(),
                        name: t.album.clone(),
                        artists: t.artists.clone(),
                        image: t.image.clone(),
                        ..Default::default()
                    });
                }
                for a in &t.artists {
                    if !a.id.is_empty() && !artists.iter().any(|x| x.id == a.id) {
                        artists.push(Artist { id: a.id.clone(), name: a.name.clone(), image: None });
                    }
                }
            }
            let mut tabs = vec![
                (tracks_to_items(tracks), false),
                (albums.into_iter().map(Item::Album).collect(), false),
                (artists.into_iter().map(Item::Artist).collect(), false),
            ];
            if self.api.is_some() {
                // The page has a playlists tab waiting; this route can't fill it.
                tabs.push((Vec::new(), false));
            }
            anyhow::Ok(tabs)
        }
        .await;
        match result {
            Ok(tabs) => {
                for (tab, (items, more)) in tabs.into_iter().enumerate() {
                    self.remember(&items);
                    self.page(id, PageUpdate::Set { tab, items, done: !more });
                }
            }
            Err(e) => self.fail(id, 4, false, e),
        }
    }

    /// Next page of Web API search results for one tab.
    pub fn search_more(&self, id: u64, query: String, tab: usize, offset: usize) {
        let this = self.clone();
        tokio::spawn(async move {
            let Some(api) = this.api.clone() else {
                return this.page(id, PageUpdate::Append { tab, items: vec![], done: true });
            };
            let tab = tab.min(3);
            let kind = ["track", "album", "artist", "playlist"][tab];
            // Where Spotify's own list has got to, which runs ahead of the
            // rows shown whenever it sends back empty entries.
            let offset = this.search_at.lock().unwrap().get(&(id, tab)).copied().unwrap_or(offset);
            match api.search(&query, kind, offset).await {
                Ok(r) => {
                    let next = offset + r.seen[tab];
                    this.search_at.lock().unwrap().insert((id, tab), next);
                    let items: Vec<Item> = match tab {
                        0 => tracks_to_items(r.tracks),
                        1 => r.albums.into_iter().map(Item::Album).collect(),
                        2 => r.artists.into_iter().map(Item::Artist).collect(),
                        _ => r.playlists.into_iter().map(Item::Playlist).collect(),
                    };
                    this.remember(&items);
                    let done = !r.more[tab] || r.seen[tab] == 0 || next >= 200;
                    this.page(id, PageUpdate::Append { tab, items, done });
                }
                Err(e) => {
                    this.toast_err(&e);
                    this.page(id, PageUpdate::Append { tab, items: vec![], done: true });
                }
            }
        });
    }

    // ---- now playing --------------------------------------------------

    pub fn load_image(&self, url: String) {
        let this = self.clone();
        tokio::spawn(async move {
            let (bytes, fresh) = match this.cache.image(&url) {
                Some(b) => (Some(b), false),
                None => match api::fetch_bytes(&this.http, &url).await {
                    Ok(b) => (Some(b), true),
                    Err(e) => {
                        log::warn!("image {url}: {e}");
                        (None, false)
                    }
                },
            };
            let image = match bytes {
                Some(b) => {
                    let decoded = tokio::task::spawn_blocking(move || {
                        let image = image::load_from_memory(&b).ok().map(|i| Arc::new(i.to_rgb8()));
                        (image, b)
                    })
                    .await
                    .ok();
                    match decoded {
                        // Only a picture that opens is worth keeping on disk.
                        Some((Some(image), b)) => {
                            if fresh {
                                this.cache.put_image(&url, &b);
                            }
                            Some(image)
                        }
                        _ => {
                            log::warn!("image {url}: not a picture we can read");
                            this.cache.forget_image(&url);
                            None
                        }
                    }
                }
                None => None,
            };
            this.send(Msg::Image { url, image });
        });
    }

    /// Work out how near each part of a cover is. Done once per cover and
    /// kept on disk; the model itself is fetched the first time it is needed.
    #[cfg(feature = "depth")]
    pub fn load_depth(&self, url: String, image: Arc<RgbImage>) {
        use crate::art::DepthMap;
        let this = self.clone();
        tokio::spawn(async move {
            let map = match this.cache.depth(&url).and_then(DepthMap::from_bytes) {
                Some(map) => Some(map),
                None => {
                    let made = async {
                        // One at a time: it takes a fair bit of memory while
                        // it runs, and the first cover waits on the download.
                        let _turn = this.depth_turn.lock().await;
                        if !crate::depth::model_path().exists() {
                            let text = "Fetching the depth model (27 MB), just this once…".to_string();
                            this.send(Msg::Toast { text, error: false });
                            crate::depth::fetch(&this.http).await?;
                            this.send(Msg::Toast { text: "Depth model ready".into(), error: false });
                        }
                        tokio::task::spawn_blocking(move || crate::depth::estimate(&image)).await?
                    }
                    .await;
                    match made {
                        Ok(map) => {
                            this.cache.put_depth(&url, &map.near);
                            Some(map)
                        }
                        Err(e) => {
                            log::warn!("depth for {url}: {e}");
                            this.toast_err(&anyhow!("Couldn't give the cover depth: {e}"));
                            None
                        }
                    }
                }
            };
            this.send(Msg::Depth { url, map: map.map(Arc::new) });
        });
    }

    /// Built without the depth model: there is nothing to work it out with.
    #[cfg(not(feature = "depth"))]
    pub fn load_depth(&self, url: String, _image: Arc<RgbImage>) {
        self.send(Msg::Depth { url, map: None });
    }

    pub fn load_lyrics(&self, track: Track) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut found = None;
            if !track.id.is_empty() {
                if let Ok(session) = this.engine.session().await {
                    found = spot::lyrics(&session, &track.id).await;
                }
            }
            if found.is_none() {
                found = lyrics::lrclib(&this.http, &track).await;
            }
            this.send(Msg::Lyrics { uri: track.uri, lyrics: found });
        });
    }

    /// Fill in details (album id, cover) for a track we only know by URI.
    pub fn resolve_track(&self, uri: String) {
        if let Some(t) = self.known(&uri) {
            return self.send(Msg::TrackInfo(t));
        }
        let this = self.clone();
        tokio::spawn(async move {
            let Ok(session) = this.engine.session().await else { return };
            if let Some(t) = spot::tracks(&session, &[uri]).await.into_iter().next() {
                if !t.id.is_empty() {
                    this.remember(&[Item::Track(t.clone())]);
                    this.send(Msg::TrackInfo(t));
                }
            }
        });
    }

    /// Turn a list of upcoming URIs into tracks for the "up next" view.
    pub fn resolve_queue(&self, uris: Vec<String>) {
        let this = self.clone();
        tokio::spawn(async move {
            let missing: Vec<String> = uris.iter().filter(|u| this.known(u).is_none()).cloned().collect();
            if !missing.is_empty() {
                let Ok(session) = this.engine.session().await else { return };
                this.remember(&tracks_to_items(spot::tracks(&session, &missing).await));
            }
            this.send(Msg::Queue(uris.iter().filter_map(|u| this.known(u)).collect()));
        });
    }

    /// The real queue, as Spotify sees it (Web API only).
    pub fn load_queue(&self) {
        let Some(api) = self.api.clone() else { return };
        let this = self.clone();
        tokio::spawn(async move {
            match api.queue().await {
                Ok(tracks) => this.send(Msg::Queue(tracks)),
                Err(e) => log::warn!("queue: {e}"),
            }
        });
    }

    pub fn load_devices(&self) {
        let Some(api) = self.api.clone() else { return };
        let this = self.clone();
        tokio::spawn(async move {
            match api.devices().await {
                Ok(devices) => this.send(Msg::Devices(devices)),
                Err(e) => log::warn!("devices: {e}"),
            }
        });
    }

    /// One-off read of account-wide playback, to show what another device is playing.
    pub fn seed_remote(&self) {
        let Some(api) = self.api.clone() else { return };
        let this = self.clone();
        tokio::spawn(async move {
            if let Ok(Some(state)) = api.player().await {
                this.send(Msg::Remote(state));
            }
        });
    }

    // ---- actions ------------------------------------------------------

    pub fn set_liked(&self, uri: String, liked: bool) {
        let this = self.clone();
        tokio::spawn(async move {
            // The player connection first: it isn't rate limited the way the
            // Web API is. The Web API is the fallback when there is one.
            let direct = async {
                let session = this.engine.session().await?;
                spot::collection_write(&session, "collection", &uri, liked).await
            }
            .await;
            let result = match (direct, &this.api) {
                (Err(e), Some(api)) => {
                    log::warn!("like over the player connection: {e}");
                    api.set_saved(&uri, liked).await
                }
                (direct, _) => direct,
            };
            match result {
                Ok(()) => {
                    // Keep the on-disk library in step so the heart survives a restart.
                    if let Some(hit) = this.cache.get::<Vec<(String, i64)>>("collection") {
                        let mut items = hit.value;
                        items.retain(|(u, _)| u != &uri);
                        if liked {
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0);
                            items.insert(0, (uri.clone(), now));
                        }
                        this.cache.put("collection", &hit.rev, &items);
                    }
                }
                Err(e) => {
                    this.toast_err(&e);
                    this.send(Msg::LikeFailed { uri, was: !liked });
                }
            }
        });
    }

    pub fn remote(&self, cmd: RemoteCmd) {
        let this = self.clone();
        tokio::spawn(async move {
            let result = match this.web() {
                Ok(api) => api.command(cmd).await,
                Err(e) => Err(e),
            };
            if let Err(e) = result {
                if e.to_string().contains("No active Spotify device") {
                    this.send(Msg::RemoteGone);
                } else {
                    this.toast_err(&e);
                }
            }
        });
    }

    /// Add several tracks to the queue, in order.
    pub fn queue_many(&self, tracks: Vec<Track>) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut added = 0;
            for t in &tracks {
                // Each one makes the player report its state to Spotify, so a
                // long run is spaced out rather than sent in a burst.
                if added > 0 {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                let direct = async { spot::queue_add(&this.engine.session().await?, &t.uri).await };
                let result = match &this.api {
                    Some(api) => match api.queue_add(&t.uri).await {
                        Ok(()) => Ok(()),
                        Err(e) => {
                            log::warn!("queue over the Web API: {e}");
                            direct.await
                        }
                    },
                    None => direct.await,
                };
                match result {
                    Ok(()) => added += 1,
                    Err(e) => {
                        this.toast_err(&e);
                        break;
                    }
                }
            }
            if added < tracks.len() {
                this.send(Msg::QueueFailed(tracks[added..].iter().map(|t| t.uri.clone()).collect()));
            }
            if added > 0 {
                let text = if added == 1 {
                    format!("Queued {}", tracks[0].name)
                } else {
                    format!("Queued {added} songs")
                };
                this.send(Msg::Toast { text, error: false });
            }
        });
    }

    pub fn transfer(&self, device_id: String, name: String) {
        let this = self.clone();
        tokio::spawn(async move {
            let result = match this.web() {
                Ok(api) => api.transfer(&device_id, true).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(()) => this.send(Msg::Toast { text: format!("Playing on {name}"), error: false }),
                Err(e) => this.toast_err(&e),
            }
        });
    }
}
