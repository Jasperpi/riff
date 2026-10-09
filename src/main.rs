//! riff: a Spotify client for the terminal.

mod api;
mod app;
mod art;
mod auth;
mod backend;
mod cache;
mod config;
mod demo;
mod engine;
mod lyrics;
mod model;
mod mpris;
mod setup;
mod spot;
mod ui;
mod viz;

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as TermEvent, EventStream, KeyCode, KeyEvent,
    KeyModifiers,
};
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::crossterm::{cursor, execute};
use ratatui::style::{Color, Modifier};

use crate::app::App;
use crate::backend::{Backend, Msg};
use crate::config::Config;

const HELP: &str = "\
riff: Spotify in your terminal

USAGE
    riff              start the player
    riff setup        optional: add your own Spotify Client ID for extras
    riff login        sign in again
    riff logout       forget the saved login
    riff doctor       check the connection and report what works
    riff --demo       explore the interface offline, no account needed

Press ? inside riff for the keys. Settings: ~/.config/riff/config.toml
Log: ~/.cache/riff/riff.log";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();

    let first = args.first().map(String::as_str);
    if matches!(first, Some("-h" | "--help" | "help")) {
        println!("{HELP}");
        return;
    }
    if matches!(first, Some("-V" | "--version")) {
        println!("riff {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("riff: can't start: {e}");
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(async {
        match first {
            Some("--demo") => match value("--snapshot") {
                Some(size) => snapshot(&size, value("--keys").as_deref().unwrap_or("")),
                None => run(None).await,
            },
            Some("logout") => {
                setup::logout();
                Ok(())
            }
            Some("doctor") if args.iter().any(|a| a == "--like") => like_test().await,
            Some("doctor") => doctor(args.iter().any(|a| a == "--play")).await,
            Some("setup") => start(true, true).await,
            Some("login") => start(false, true).await,
            None => start(false, false).await,
            Some(other) => Err(anyhow!("unknown command `{other}`. Try `riff --help`")),
        }
    });
    let code = match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("riff: {e:#}");
            1
        }
    };
    // Everything that needs saving is saved by now. Exiting directly returns the
    // prompt at once instead of waiting for audio buffers and idle connections.
    std::process::exit(code)
}

// ---- logging --------------------------------------------------------------

struct FileLog(Mutex<std::fs::File>);

impl log::Log for FileLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Ok(mut f) = self.0.lock() {
            let _ = writeln!(
                f,
                "{:02}:{:02}:{:02} {:5} {} {}",
                secs / 3600 % 24,
                secs / 60 % 60,
                secs % 60,
                record.level(),
                record.target(),
                record.args()
            );
        }
    }

    fn flush(&self) {}
}

/// Send logs, and anything libraries print to stderr, to a file. Stray writes
/// to the terminal would otherwise scribble over the interface.
fn init_logging() -> Option<std::fs::File> {
    let dir = config::cache_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("riff.log");
    if std::fs::metadata(&path).map(|m| m.len() > 2_000_000).unwrap_or(false) {
        std::fs::rename(&path, dir.join("riff.log.old")).ok();
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
    let for_stderr = file.try_clone().ok();
    if log::set_boxed_logger(Box::new(FileLog(Mutex::new(file)))).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    for_stderr
}

/// Point fd 2 at `file`; returns the original stderr so it can be restored.
fn redirect_stderr(file: &std::fs::File) -> Option<i32> {
    use std::os::fd::AsRawFd;
    // SAFETY: plain fd duplication on descriptors we own; no memory is touched.
    unsafe {
        let saved = libc::dup(2);
        if saved < 0 || libc::dup2(file.as_raw_fd(), 2) < 0 {
            return None;
        }
        Some(saved)
    }
}

fn restore_stderr(saved: Option<i32>) {
    if let Some(fd) = saved {
        // SAFETY: `fd` came from `dup` above and is closed exactly once here.
        unsafe {
            libc::dup2(fd, 2);
            libc::close(fd);
        }
    }
}

// ---- startup --------------------------------------------------------------

async fn start(redo_setup: bool, redo_login: bool) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        return Err(anyhow!("riff needs an interactive terminal"));
    }
    let stderr_file = init_logging();
    let mut cfg = Config::load()?;
    let ready = setup::ensure(&mut cfg, redo_setup, redo_login).await?;

    let (msg_tx, msg_rx) = tokio::sync::mpsc::unbounded_channel::<Msg>();
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let forward = msg_tx.clone();
    let media_tx = msg_tx.clone();
    tokio::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            if forward.send(Msg::Engine(ev)).is_err() {
                break;
            }
        }
    });

    let tap = Arc::new(viz::Tap::default());
    let engine = engine::Engine::start(cfg.clone(), ready.credentials, ev_tx, tap.clone());
    let api = ready.tokens.map(|t| Arc::new(api::Api::new(Arc::new(auth::TokenStore::new(t)))));
    let cache = cache::Cache::new(config::cache_dir());
    let backend = Backend::new(api, engine.clone(), cache, msg_tx);

    let saved_stderr = stderr_file.as_ref().and_then(redirect_stderr);
    let mut app = App::new(cfg, Some(backend), tap);
    app.mpris = mpris::Mpris::start(media_tx);
    let result = tui(app, msg_rx).await;
    engine.shutdown();
    // Give the player a moment to tell Spotify this device is gone.
    tokio::time::sleep(Duration::from_millis(250)).await;
    restore_stderr(saved_stderr);
    result
}

/// Run the interface with no backend (demo mode).
async fn run(_: Option<()>) -> Result<()> {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<Msg>();
    tui(demo::app(Config::default()), rx).await
}

static TUI_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// librespot calls `exit` in a few unrecoverable cases (a non-Premium account,
/// for one). Hand the terminal back in a usable state when that happens.
extern "C" fn on_exit() {
    if TUI_ACTIVE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen, cursor::Show);
        println!("riff stopped unexpectedly. Spotify Premium is required; details in ~/.cache/riff/riff.log");
    }
}

fn enter_terminal(mouse: bool) -> Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    static REGISTER: std::sync::Once = std::sync::Once::new();
    // SAFETY: registers a plain `extern "C"` function with no captured state.
    REGISTER.call_once(|| unsafe {
        libc::atexit(on_exit);
    });
    TUI_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen, cursor::Hide)?;
    if mouse {
        execute!(out, EnableMouseCapture)?;
    }
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn leave_terminal() {
    TUI_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen, cursor::Show);
}

async fn tui(mut app: App, mut rx: tokio::sync::mpsc::UnboundedReceiver<Msg>) -> Result<()> {
    // A panic must never leave the terminal in raw mode. Only the interface thread
    // owns the screen; a crash in a background thread is logged, and the app lives on.
    let ui_thread = std::thread::current().id();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == ui_thread {
            leave_terminal();
            previous(info);
        } else {
            log::error!("background thread panicked: {info}");
        }
    }));

    let mut terminal = enter_terminal(app.cfg.mouse)?;
    let mut events = EventStream::new();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut usr1 = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
    let mut next_tick = Instant::now();

    let result: Result<()> = loop {
        if app.dirty {
            app.dirty = false;
            if let Err(e) = terminal.draw(|f| ui::draw(f, &mut app)) {
                break Err(e.into());
            }
        }
        // State changes can shorten the interval (playback started), never past due.
        next_tick = next_tick.min(Instant::now() + app.tick_interval());
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => app.on_term(ev),
                Some(Err(e)) => break Err(e.into()),
                None => break Ok(()),
            },
            msg = rx.recv() => {
                if let Some(msg) = msg {
                    app.on_msg(msg);
                    // Drain bursts (a long list arriving) before redrawing once.
                    while let Ok(more) = rx.try_recv() {
                        app.on_msg(more);
                    }
                }
            }
            _ = tokio::time::sleep_until(next_tick.into()) => {
                app.on_tick();
                next_tick = Instant::now() + app.tick_interval();
            }
            _ = usr1.recv() => {
                // `kill -USR1` saves the current screen to a file, for bug reports.
                if let Ok(size) = terminal.size() {
                    let Ok(mut off) = Terminal::new(TestBackend::new(size.width, size.height));
                    if off.draw(|f| ui::draw(f, &mut app)).is_ok() {
                        let path = config::cache_dir().join("frame.ans");
                        std::fs::write(path, frame_to_ansi(off.backend().buffer())).ok();
                    }
                }
                app.dirty = true;
            }
            _ = term.recv() => break Ok(()),
            _ = hup.recv() => break Ok(()),
        }
        if app.quit {
            break Ok(());
        }
    };

    leave_terminal();
    app.save_settings();
    // Tearing down the D-Bus service blocks for up to a second; the process is
    // about to exit, which releases the bus name anyway.
    std::mem::forget(app.mpris.take());
    result
}

// ---- diagnostics ----------------------------------------------------------

async fn doctor(play: bool) -> Result<()> {
    let _ = init_logging();
    let cfg = Config::load()?;
    let ok = |b: bool| if b { "\x1b[32m✓\x1b[0m" } else { "\x1b[31m✗\x1b[0m" };
    println!("riff {}\n", env!("CARGO_PKG_VERSION"));
    println!(
        "{} Client ID: {}",
        if cfg.client_id.is_empty() { "·" } else { ok(true) },
        if cfg.client_id.is_empty() { "none (optional; `riff setup` adds Web API extras)" } else { "your own app" }
    );

    match auth::Tokens::load() {
        None if cfg.client_id.is_empty() => {}
        None => println!("{} Web API: not signed in yet (run `riff`)", ok(false)),
        Some(tokens) => {
            let api = api::Api::new(Arc::new(auth::TokenStore::new(tokens)));
            match api.me().await {
                Ok((id, name)) => println!("{} Web API: signed in as {} ({id})", ok(true), if name.is_empty() { &id } else { &name }),
                Err(e) => println!("{} Web API: {e}", ok(false)),
            }
            match api.liked(0).await {
                Ok((_, total)) => println!("{} Liked Songs: {total}", ok(true)),
                Err(e) => println!("{} Liked Songs: {e}", ok(false)),
            }
            match api.search("daft punk", "track,album,artist,playlist", 0).await {
                Ok(r) => println!(
                    "{} Search: {} tracks, {} albums, {} artists, {} playlists",
                    ok(!r.tracks.is_empty()),
                    r.tracks.len(),
                    r.albums.len(),
                    r.artists.len(),
                    r.playlists.len()
                ),
                Err(e) => println!("{} Search: {e}", ok(false)),
            }
            match api.player().await {
                Ok(Some(st)) => println!("{} Now playing elsewhere: {}", ok(true), st.track.map(|t| t.name).unwrap_or_else(|| "nothing".into())),
                Ok(None) => println!("{} Player state: nothing active", ok(true)),
                Err(e) => println!("{} Player state: {e}", ok(false)),
            }
        }
    }

    let cache = engine::librespot_cache(&cfg)?;
    let Some(creds) = cache.credentials() else {
        println!("{} Player: no saved login. Run `riff login`.", ok(false));
        return Ok(());
    };
    let session = librespot_core::Session::new(engine::session_config(&cfg), Some(cache));
    let started = Instant::now();
    match session.connect(creds.clone(), true).await {
        Ok(()) => println!(
            "{} Player: connected as {} ({}) in {} ms",
            ok(true),
            session.username(),
            session.country(),
            started.elapsed().as_millis()
        ),
        Err(e) => {
            println!("{} Player: {e}", ok(false));
            return Ok(());
        }
    }
    let t0 = Instant::now();
    match spot::collection(&session, "collection").await {
        Ok((items, sync)) => {
            let tracks = items.iter().filter(|i| i.uri.starts_with("spotify:track:")).count();
            let albums = items.iter().filter(|i| i.uri.starts_with("spotify:album:")).count();
            println!(
                "{} Saved library: {tracks} liked songs, {albums} albums, {} other, {} ms, sync token {}",
                ok(!items.is_empty()),
                items.len() - tracks - albums,
                t0.elapsed().as_millis(),
                !sync.is_empty()
            );
            if let Some(first) = items.first() {
                println!("    newest: {} (added {})", first.uri, first.added_at);
            }
            if !sync.is_empty() {
                match spot::collection_delta(&session, "collection", &sync).await {
                    Ok(Some((changes, _))) => println!("{} Library delta: {} changes since a moment ago", ok(true), changes.len()),
                    Ok(None) => println!("{} Library delta: service asked for a full reload", ok(false)),
                    Err(e) => println!("{} Library delta: {e}", ok(false)),
                }
            }
        }
        Err(e) => println!("{} Saved library: {e}", ok(false)),
    }
    match spot::collection(&session, "artist").await {
        Ok((items, _)) => println!("{} Followed artists: {}", ok(true), items.len()),
        Err(e) => println!("{} Followed artists: {e}", ok(false)),
    }
    match spot::search_tracks(&session, "daft punk").await {
        Ok(uris) => {
            let found = spot::tracks(&session, &uris.iter().take(5).cloned().collect::<Vec<_>>()).await;
            println!(
                "{} Search (player connection): {} tracks; top: {}",
                ok(!uris.is_empty()),
                uris.len(),
                found.iter().map(|t| format!("{} · {}", t.name, t.artist_line())).collect::<Vec<_>>().join(" | ")
            );
        }
        Err(e) => println!("{} Search (player connection): {e}", ok(false)),
    }

    let mut sample: Vec<String> = Vec::new();
    match spot::rootlist(&session).await {
        Ok(entries) => {
            let playlists: Vec<&Playlist> = entries
                .iter()
                .filter_map(|e| match e {
                    model::SideEntry::Playlist { playlist, .. } => Some(playlist),
                    _ => None,
                })
                .collect();
            let folders = entries.iter().filter(|e| matches!(e, model::SideEntry::Folder { .. })).count();
            println!("{} Playlists: {} ({folders} folders)", ok(true), playlists.len());
            if let Some(p) = playlists.iter().find(|p| p.len > 0) {
                let t0 = Instant::now();
                match spot::playlist(&session, &p.id).await {
                    Ok(head) => {
                        let uris: Vec<String> = head.items.iter().map(|(u, _)| u.clone()).collect();
                        let tracks = spot::tracks(&session, &uris).await;
                        let named = tracks.iter().filter(|t| !t.name.is_empty() && !t.artists.is_empty()).count();
                        let covers = tracks.iter().filter(|t| t.image.is_some()).count();
                        println!(
                            "{} Playlist “{}”: {} items, details for {named}, covers for {covers}, {} ms",
                            ok(named > 0 && named + 2 >= uris.len()),
                            head.playlist.name,
                            uris.len(),
                            t0.elapsed().as_millis()
                        );
                        sample = tracks.iter().filter(|t| t.playable && !t.id.is_empty()).map(|t| t.uri.clone()).collect();
                        if let Some(t) = tracks.iter().find(|t| !t.id.is_empty()) {
                            println!("    e.g. {} · {} · {} · {}", t.name, t.artist_line(), t.album, model::fmt_ms(t.duration_ms));
                            let lyr = spot::lyrics(&session, &t.id).await;
                            println!(
                                "{} Lyrics for “{}”: {}",
                                ok(true),
                                t.name,
                                match &lyr {
                                    Some(l) => format!("{} lines, synced: {}, via {}", l.lines.len(), l.synced, l.source),
                                    None => "none on Spotify".into(),
                                }
                            );
                            if !t.album_id.is_empty() {
                                match spot::album(&session, &t.album_id).await {
                                    Ok((a, uris)) => println!("{} Album “{}” ({}): {} tracks", ok(!uris.is_empty()), a.name, a.year, uris.len()),
                                    Err(e) => println!("{} Album: {e}", ok(false)),
                                }
                            }
                            if let Some(ar) = t.artists.first().filter(|a| !a.id.is_empty()) {
                                match spot::artist(&session, &ar.id).await {
                                    Ok(a) => {
                                        let albums = spot::albums(&session, &a.albums).await;
                                        println!(
                                            "{} Artist “{}”: {} top tracks, {} albums ({} with details), {} singles, {} related",
                                            ok(!a.artist.name.is_empty()),
                                            a.artist.name,
                                            a.top.len(),
                                            a.albums.len(),
                                            albums.len(),
                                            a.singles.len(),
                                            a.related.len()
                                        );
                                    }
                                    Err(e) => println!("{} Artist: {e}", ok(false)),
                                }
                            }
                        }
                    }
                    Err(e) => println!("{} Playlist tracks: {e}", ok(false)),
                }
            }
        }
        Err(e) => println!("{} Playlists: {e}", ok(false)),
    }
    session.shutdown();

    if play && !sample.is_empty() {
        tokio::time::sleep(Duration::from_millis(500)).await;
        play_test(cfg, creds, sample).await;
    }
    Ok(())
}

/// Like a track that isn't liked, confirm Spotify recorded it, unlike it, and
/// confirm it's gone. Leaves the library exactly as it was.
async fn like_test() -> Result<()> {
    let _ = init_logging();
    let cfg = Config::load()?;
    let ok = |b: bool| if b { "\x1b[32m✓\x1b[0m" } else { "\x1b[31m✗\x1b[0m" };
    let cache = engine::librespot_cache(&cfg)?;
    let creds = cache.credentials().ok_or_else(|| anyhow!("no saved login; run `riff`"))?;
    let session = librespot_core::Session::new(engine::session_config(&cfg), Some(cache));
    session.connect(creds, true).await.map_err(|e| anyhow!("connect: {e}"))?;

    let liked = |items: &[spot::Saved]| -> Vec<String> {
        items.iter().filter(|i| i.uri.starts_with("spotify:track:")).map(|i| i.uri.clone()).collect()
    };
    let (before, sync) = spot::collection(&session, "collection").await?;
    let before = liked(&before);
    let candidates = spot::search_tracks(&session, "daft punk").await?;
    let uri = candidates
        .into_iter()
        .find(|u| !before.contains(u))
        .ok_or_else(|| anyhow!("every candidate is already liked"))?;
    let name = spot::tracks(&session, std::slice::from_ref(&uri)).await.first().map(|t| format!("{} · {}", t.name, t.artist_line())).unwrap_or_default();
    println!("Test track (not currently liked): {name}  [{uri}]");
    println!("Liked songs before: {}", before.len());

    spot::collection_write(&session, "collection", &uri, true).await?;
    tokio::time::sleep(Duration::from_millis(900)).await;
    let (mid, _) = spot::collection(&session, "collection").await?;
    let mid = liked(&mid);
    let added = mid.contains(&uri);
    println!("{} Like: Spotify now lists it ({} liked)", ok(added), mid.len());
    let delta = spot::collection_delta(&session, "collection", &sync).await?;
    match &delta {
        Some((changes, _)) => println!(
            "{} Change feed reports it: {}",
            ok(changes.iter().any(|c| c.uri == uri && !c.removed)),
            changes.iter().map(|c| format!("{}{}", if c.removed { "-" } else { "+" }, c.uri)).collect::<Vec<_>>().join(" ")
        ),
        None => println!("  Change feed asked for a full reload"),
    }

    spot::collection_write(&session, "collection", &uri, false).await?;
    tokio::time::sleep(Duration::from_millis(900)).await;
    let (after, _) = spot::collection(&session, "collection").await?;
    let after = liked(&after);
    let removed = !after.contains(&uri);
    println!("{} Unlike: it's gone again ({} liked)", ok(removed), after.len());
    println!("{} Library is back to how it started", ok(after == before));
    if !removed {
        println!("\n\x1b[31mThe test track is still liked. Remove it by hand: {name}\x1b[0m");
    }
    session.shutdown();
    Ok(())
}

/// Play a few seconds for real: checks audio output, events, seek and pause.
async fn play_test(mut cfg: Config, creds: librespot_core::authentication::Credentials, uris: Vec<String>) {
    use engine::{Conn, Event};
    cfg.volume = cfg.volume.min(40);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let tap = Arc::new(viz::Tap::default());
    let player = engine::Engine::start(cfg, creds, tx, tap.clone());
    println!("\nPlayback test (about 12 seconds)");
    let started = Instant::now();
    let mut stage = 0;
    let (mut frames, mut loud, mut clusters) = (0u32, 0u32, 0u32);
    let mut queued_played = false;
    let queue_uri = uris.get(2).cloned();
    loop {
        let t = started.elapsed().as_secs_f32();
        if t > 40.0 || stage == 7 {
            break;
        }
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(Event::Cluster(c)) => {
                    clusters += 1;
                    if clusters <= 2 {
                        let names: Vec<String> = c.devices.iter().map(|d| format!("{}{}", d.name, if d.active { "*" } else { "" })).collect();
                        println!("  {t:5.1}s devices: {} · next up: {}", names.join(", "), c.next.len());
                    }
                }
                Some(Event::Conn(Conn::Online)) if stage == 0 => {
                    println!("  {t:5.1}s online; starting a track");
                    if let Err(e) = player.play_tracks(uris.clone(), 0, false, &model::Repeat::Off) {
                        println!("  play failed: {e}");
                        break;
                    }
                    stage = 1;
                }
                Some(Event::Track(tr)) => {
                    println!("  {t:5.1}s track: {} · {} · cover {}", tr.name, tr.artist_line(), tr.image.is_some());
                    if stage == 5 && Some(&tr.uri) == queue_uri.as_ref() {
                        queued_played = true;
                        stage = 6;
                        frames = 0;
                    }
                }
                Some(ev) => println!("  {t:5.1}s {ev:?}"),
                None => break,
            },
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if let Some(b) = tap.current() {
                    frames += 1;
                    if b.iter().any(|v| *v > 0.2) {
                        loud += 1;
                    }
                }
                // Timeline once audio is flowing: seek, pause, resume, stop.
                if stage == 1 && frames > 30 {
                    println!("  {t:5.1}s seeking to 1:00");
                    let _ = player.seek(60_000);
                    stage = 2;
                    frames = 31;
                } else if stage == 2 && frames > 60 {
                    println!("  {t:5.1}s pausing");
                    let _ = player.play_pause();
                    stage = 3;
                } else if stage == 3 && frames > 60 && started.elapsed().as_secs_f32() > 0.0 {
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                    println!("  {:5.1}s resuming", started.elapsed().as_secs_f32());
                    let _ = player.play_pause();
                    stage = 4;
                    frames = 61;
                } else if stage == 4 && frames > 85 {
                    // Queue a specific track, skip, and expect exactly that track next.
                    match (&queue_uri, player.session().await) {
                        (Some(uri), Ok(session)) => {
                            println!("  {t:5.1}s queueing track 3, then skipping");
                            if let Err(e) = spot::queue_add(&session, uri).await {
                                println!("  queue failed: {e}");
                            }
                            tokio::time::sleep(Duration::from_millis(700)).await;
                            let _ = player.next();
                            stage = 5;
                            frames = 0;
                        }
                        _ => stage = 7,
                    }
                } else if (stage == 5 && started.elapsed().as_secs_f32() > 30.0) || (stage == 6 && frames > 15) {
                    stage = 7;
                }
            }
        }
    }
    player.shutdown();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let ok = |b: bool| if b { "\x1b[32m✓\x1b[0m" } else { "\x1b[31m✗\x1b[0m" };
    println!("{} Audio reached the output: {frames} spectrum frames, {loud} with signal", ok(loud > 10));
    println!("{} Queued track played next: {queued_played}", ok(queued_played));
    println!("  Device state pushes received: {clusters}");
}

use crate::model::Playlist;

// ---- snapshots ------------------------------------------------------------

/// Render one demo frame to stdout as ANSI text: `--demo --snapshot 120x36 --keys "v"`.
/// Keys are literal characters plus `<enter> <esc> <tab> <up> <down> <left> <right>`.
fn snapshot(size: &str, keys: &str) -> Result<()> {
    let (w, h) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u16>().ok()?, h.parse::<u16>().ok()?)))
        .ok_or_else(|| anyhow!("size looks like 120x36"))?;
    let mut app = demo::app(Config::default());
    let mut terminal = Terminal::new(TestBackend::new(w, h))?;

    let mut rest = keys;
    while !rest.is_empty() {
        let (code, used) = if let Some(end) = rest.strip_prefix('<').and_then(|r| r.find('>')) {
            let code = match &rest[1..=end] {
                "enter" => KeyCode::Enter,
                "esc" => KeyCode::Esc,
                "tab" => KeyCode::Tab,
                "up" => KeyCode::Up,
                "down" => KeyCode::Down,
                "left" => KeyCode::Left,
                "right" => KeyCode::Right,
                other => return Err(anyhow!("unknown key <{other}>")),
            };
            (code, end + 2)
        } else {
            let c = rest.chars().next().unwrap();
            (KeyCode::Char(c), c.len_utf8())
        };
        rest = &rest[used..];
        app.on_term(TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
        // Lay out between keys so scroll positions settle as they would live.
        terminal.draw(|f| ui::draw(f, &mut app))?;
    }
    for _ in 0..40 {
        app.on_tick();
    }
    terminal.draw(|f| ui::draw(f, &mut app))?;

    print!("{}", frame_to_ansi(terminal.backend().buffer()));
    Ok(())
}

/// Serialise a rendered frame as ANSI text.
fn frame_to_ansi(buffer: &ratatui::buffer::Buffer) -> String {
    let (w, h) = (buffer.area.width, buffer.area.height);
    let mut out = String::new();
    let color = |c: Color, base: u8| match c {
        Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
        _ => format!("{}", base + 9),
    };
    for y in 0..h {
        for x in 0..w {
            let cell = &buffer[(x, y)];
            let mut sgr = format!("0;{};{}", color(cell.fg, 30), color(cell.bg, 40));
            if cell.modifier.contains(Modifier::BOLD) {
                sgr.push_str(";1");
            }
            if cell.modifier.contains(Modifier::UNDERLINED) {
                sgr.push_str(";4");
            }
            out.push_str(&format!("\x1b[{sgr}m{}", cell.symbol()));
        }
        out.push_str("\x1b[0m\n");
    }
    out
}
