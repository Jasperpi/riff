//! riff: a Spotify client for the terminal.

mod api;
mod app;
mod art;
mod audio;
mod auth;
mod backend;
mod cache;
mod config;
mod demo;
#[cfg(feature = "depth")]
mod depth;
mod dj;
mod engine;
mod lyrics;
mod model;
mod mpris;
mod out;
mod setup;
mod spot;
mod ui;

use std::io::{IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers,
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
                Some(size) => snapshot(
                    &size,
                    value("--keys").as_deref().unwrap_or(""),
                    value("--frames").and_then(|n| n.parse().ok()).unwrap_or(40),
                ),
                None => run(None).await,
            },
            Some("logout") => {
                setup::logout();
                Ok(())
            }
            Some("doctor") if args.iter().any(|a| a == "--like") => like_test().await,
            Some("doctor") if args.iter().any(|a| a == "--mix") => mix_test().await,
            Some("doctor") if args.iter().any(|a| a == "--tempo") => tempo_file(&value("--tempo").unwrap_or_default()),
            #[cfg(feature = "depth")]
            Some("doctor") if args.iter().any(|a| a == "--depth") => {
                let number = |flag: &str| value(flag).and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.0);
                depth_test(&value("--depth").unwrap_or_default(), number("--at"), number("--beat")).await
            }
            Some("doctor") if args.iter().any(|a| a == "--capture") => capture(value("--capture"), value("--query")).await,
            Some("doctor") if args.iter().any(|a| a == "--dj") => dj_test(value("--dj")).await,
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

    let hub = Arc::new(audio::Hub::default());
    let engine = engine::Engine::start(cfg.clone(), ready.credentials, ev_tx, hub.clone());
    let api = ready.tokens.map(|t| Arc::new(api::Api::new(Arc::new(auth::TokenStore::new(t)))));
    let cache = cache::Cache::new(config::cache_dir());
    let backend = Backend::new(api, engine.clone(), cache, msg_tx);

    let saved_stderr = stderr_file.as_ref().and_then(redirect_stderr);
    let mut app = App::new(cfg, Some(backend), hub);
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
        let _ = execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            cursor::Show
        );
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
    // Bracketed paste: pasted text arrives as one event, not as a run of
    // keystrokes that would each be taken for a command.
    execute!(out, EnterAlternateScreen, cursor::Hide, EnableBracketedPaste)?;
    if mouse {
        execute!(out, EnableMouseCapture)?;
    }
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn leave_terminal() {
    TUI_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
    let _ = disable_raw_mode();
    let _ = execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    );
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

/// The pid of a running riff player, if there is one. Found by looking at the
/// process table rather than a pid file, so it also sees older builds.
fn running_player() -> Option<u32> {
    let me = std::process::id();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        if pid == me {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else { continue };
        let mut args = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty());
        let Some(program) = args.next() else { continue };
        let name = program.rsplit(|b| *b == b'/').next().unwrap_or(program);
        if name != b"riff" {
            continue;
        }
        // The player itself runs with no arguments (or setup/login, which lead into it).
        match args.next() {
            None | Some(b"setup") | Some(b"login") => return Some(pid),
            Some(_) => {}
        }
    }
    None
}

/// Diagnostics sign in as their own device, so they never collide with (or
/// take playback from) a riff that is running.
fn doctor_config() -> Result<Config> {
    let mut cfg = Config::load()?;
    cfg.device_id = cfg.device_id.chars().rev().collect();
    cfg.device_name = format!("{} doctor", cfg.device_name);
    Ok(cfg)
}

/// Tests that play audio take over playback; refuse while riff is in use.
fn refuse_if_playing() -> Result<()> {
    match running_player() {
        Some(pid) => Err(anyhow!(
            "riff is running (pid {pid}). This test takes over playback, so quit riff first."
        )),
        None => Ok(()),
    }
}

async fn doctor(play: bool) -> Result<()> {
    if play {
        refuse_if_playing()?;
    }
    let _ = init_logging();
    let cfg = doctor_config()?;
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
    let cfg = doctor_config()?;
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
    // Whatever goes wrong from here, the like is taken back before leaving.
    let checks = async {
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
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = &checks {
        println!("  \x1b[31mCheck interrupted: {e}\x1b[0m");
    }

    let undone = async {
        spot::collection_write(&session, "collection", &uri, false).await?;
        tokio::time::sleep(Duration::from_millis(900)).await;
        let (after, _) = spot::collection(&session, "collection").await?;
        anyhow::Ok(liked(&after))
    }
    .await;
    let removed = undone.as_ref().is_ok_and(|after| !after.contains(&uri));
    match &undone {
        Ok(after) => {
            println!("{} Unlike: it's gone again ({} liked)", ok(removed), after.len());
            println!("{} Library is back to how it started", ok(*after == before));
        }
        Err(e) => println!("  \x1b[31mCouldn't take the like back: {e}\x1b[0m"),
    }
    if !removed {
        println!("\n\x1b[31mThe test track is still liked. Remove it by hand: {name}\x1b[0m");
    }
    session.shutdown();
    Ok(())
}

/// Exercise the mixing output stage without making a sound (volume 0; the
/// analyser listens before volume is applied). Seeks near the end of a song
/// and watches the blend into the next, then checks pause and skip stay prompt.
async fn mix_test() -> Result<()> {
    use engine::{Conn, Event};
    refuse_if_playing()?;
    let _ = init_logging();
    let mut cfg = doctor_config()?;
    cfg.volume = 0;
    cfg.mix = true;
    cfg.mix_seconds = 6;
    let cache = engine::librespot_cache(&cfg)?;
    let creds = cache.credentials().ok_or_else(|| anyhow!("no saved login; run `riff`"))?;
    let ok = |b: bool| if b { "\x1b[32m✓\x1b[0m" } else { "\x1b[31m✗\x1b[0m" };

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let hub = Arc::new(audio::Hub::default());
    let player = engine::Engine::start(cfg, creds, tx, hub.clone());
    let started = Instant::now();
    let secs = |at: Instant| at.duration_since(started).as_secs_f32();

    // Use an album so the songs are known and consecutive.
    let mut tracks: Vec<model::Track> = Vec::new();
    let mut current: Option<model::Track> = None;
    let mut stage = 0;
    let (mut silent_polls, mut polls, mut beats) = (0u32, 0u32, 0u32);
    let mut second_started: Option<Instant> = None;
    let mut first_audible_end: Option<Instant> = None;
    let mut pause_sent: Option<Instant> = None;
    let mut pause_latency = None;
    let mut skip_sent: Option<Instant> = None;
    let mut skip_latency = None;
    let mut paused_went_quiet = false;
    let mut b_first_pos: Option<i64> = None;
    let mut seek_due: Option<Instant> = None;
    let mut overruns: Option<u32> = None;

    loop {
        let now = Instant::now();
        if secs(now) > 75.0 || stage == 9 {
            break;
        }
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(Event::Conn(Conn::Online)) if stage == 0 => {
                    let session = player.session().await?;
                    let uris = spot::search_tracks(&session, "abbey road beatles").await?;
                    tracks = spot::tracks(&session, &uris).await.into_iter().filter(|t| t.playable && t.duration_ms > 60_000).take(4).collect();
                    println!("{:5.1}s online; playing {} tracks, mix 6 s, volume 0", secs(now), tracks.len());
                    player.play_tracks(tracks.iter().map(|t| t.uri.clone()).collect(), 0, 0, false, &model::Repeat::Off)?;
                    stage = 1;
                }
                Some(Event::Track(t)) => {
                    println!("{:5.1}s now: {} ({})", secs(now), t.name, model::fmt_ms(t.duration_ms));
                    if stage == 3 {
                        second_started = Some(now);
                        stage = 4;
                    }
                    if stage == 7 {
                        skip_latency = skip_sent.map(|s| s.elapsed());
                        stage = 8;
                    }
                    current = Some(t);
                }
                Some(Event::Playing { pos_ms, .. }) => {
                    if stage == 1 {
                        // Give the player a moment: a seek sent the instant a song
                        // starts arrives before it knows how long the song is.
                        seek_due.get_or_insert(now + Duration::from_millis(700));
                    } else if stage == 6 {
                        stage = 7;
                        println!("{:5.1}s resumed at {}; skipping to the next song", secs(now), model::fmt_ms(pos_ms));
                        skip_sent = Some(Instant::now());
                        player.next()?;
                    }
                }
                Some(Event::Paused { .. }) if stage == 5 => {
                    pause_latency = pause_sent.map(|s| s.elapsed());
                    stage = 6;
                }
                Some(Event::Position(ms)) if stage == 2 => {
                    println!("{:5.1}s seeked to {}", secs(now), model::fmt_ms(ms));
                    stage = 3;
                }
                Some(_) => {}
                None => break,
            },
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                let snap = hub.poll();
                if stage == 1 && seek_due.is_some_and(|due| now >= due) {
                    let dur = current.as_ref().map(|t| t.duration_ms).unwrap_or(0);
                    let to = dur.saturating_sub(14_000);
                    println!("{:5.1}s playing; seeking to {} (14 s before the end)", secs(now), model::fmt_ms(to));
                    player.seek(to)?;
                    stage = 2;
                }
                if matches!(stage, 3 | 4 | 8) {
                    polls += 1;
                    match snap {
                        Some(s) => beats += s.beat as u32,
                        None => silent_polls += 1,
                    }
                }
                // Watch the first song's own clock run out while the second is already "current".
                if stage == 4 {
                    let a = &tracks[0];
                    if first_audible_end.is_none() && hub.position(&a.uri).is_none() {
                        // The clock has moved on to the second song.
                    }
                    if let Some(b) = current.as_ref() {
                        if let Some(pos) = hub.position(&b.uri) {
                            if b_first_pos.is_none() {
                                b_first_pos = Some(pos);
                                println!("{:5.1}s second song's clock starts at {pos} ms (negative = previous song still fading)", secs(now));
                            }
                            if pos >= 0 && first_audible_end.is_none() {
                                first_audible_end = Some(now);
                            }
                            // Four seconds into the six-second blend, check that pause is
                            // immediate. That is also well past the point where a blend
                            // fed too slowly gets caught by the playhead and starts over.
                            if pos > 4_000 {
                                println!("{:5.1}s pausing mid-blend (second song at {pos} ms)", secs(now));
                                // Read before the skip below, which rightly overruns the blend.
                                overruns = Some(hub.blend_overruns());
                                pause_sent = Some(Instant::now());
                                player.play_pause()?;
                                stage = 5;
                            }
                        }
                    }
                } else if stage == 6 {
                    // Paused: output must have stopped, and then we resume.
                    if pause_sent.is_some_and(|p| p.elapsed() > Duration::from_millis(900)) {
                        paused_went_quiet = hub.poll().is_none();
                        pause_sent = None;
                        player.play_pause()?;
                    }
                } else if stage == 8 && skip_sent.is_some_and(|s| s.elapsed() > Duration::from_secs(4)) {
                    stage = 9;
                }
            }
        }
    }
    let end_pos = current.as_ref().and_then(|t| hub.position(&t.uri));
    let (waiting, dry) = hub.output_health();
    player.shutdown();
    tokio::time::sleep(Duration::from_millis(400)).await;

    println!();
    println!("{} Reached the end of every stage: {}", ok(stage == 9), stage);
    println!("{} Next song was lined up {} before its first note (the blend then runs 6 s)", ok(b_first_pos.is_some_and(|p| p < 0)),
        match (second_started, first_audible_end) { (Some(a), Some(b)) => format!("{:.1} s", b.duration_since(a).as_secs_f32()), _ => "?".into() });
    println!("{} Audio kept flowing through the blend: {} of {} checks had signal, {beats} beats seen", ok(polls > 0 && silent_polls * 20 <= polls), polls - silent_polls, polls);
    println!("{} Playback never overtook the blend: {} chunks from audio the next song hadn't reached", ok(overruns == Some(0)),
        overruns.map_or("?".into(), |n| n.to_string()));
    println!(
        "{} Sound is {waiting:.0} ms from the speakers once it leaves riff; the sound card ran dry {dry} times",
        ok(waiting > 0.0 && waiting < 80.0 && dry <= 2)
    );
    println!("{} Pause mid-blend took {:?}", ok(pause_latency.is_some_and(|d| d < Duration::from_millis(400))), pause_latency);
    println!("{} Output went quiet while paused: {paused_went_quiet}", ok(paused_went_quiet));
    println!("{} Skip took {:?}", ok(skip_latency.is_some_and(|d| d < Duration::from_millis(1500))), skip_latency);
    println!("{} Clock after skip: {:?} ms into the new song (about 4000 expected)", ok(end_pos.is_some_and(|p| (2_500..6_000).contains(&p))), end_pos);
    Ok(())
}

/// Save 40 seconds of a real song (raw f32 stereo) for tuning the beat tracker.
async fn capture(path: Option<String>, query: Option<String>) -> Result<()> {
    struct Mute;
    impl librespot_playback::mixer::VolumeGetter for Mute {
        fn attenuation_factor(&self) -> f64 {
            0.0
        }
    }
    let path = path.ok_or_else(|| anyhow!("--capture needs a file path"))?;
    let _ = init_logging();
    let cfg = doctor_config()?;
    let cache = engine::librespot_cache(&cfg)?;
    let creds = cache.credentials().ok_or_else(|| anyhow!("no saved login; run `riff`"))?;
    let session = librespot_core::Session::new(engine::session_config(&cfg), Some(cache));
    session.connect(creds, true).await.map_err(|e| anyhow!("connect: {e}"))?;
    let uris = spot::search_tracks(&session, &query.unwrap_or_else(|| "daft punk".into())).await?;
    let track = spot::tracks(&session, &uris).await.into_iter().find(|t| t.playable && t.duration_ms > 120_000).ok_or_else(|| anyhow!("no song found"))?;
    println!("{} · {}", track.name, track.artist_line());
    let mut decks = dj::Dj::start(session.clone(), Box::new(Mute), Arc::new(audio::Hub::default()), librespot_playback::config::Bitrate::Bitrate320, true);
    *dj::CAPTURE.lock().unwrap() = Some(Vec::new());
    // RIFF_CAPTURE_FROM=<seconds> picks where in the song to start (default 40).
    let from = std::env::var("RIFF_CAPTURE_FROM").ok().and_then(|v| v.parse::<u32>().ok()).unwrap_or(40);
    decks.load(0, track, from * 1000, true)?;
    tokio::time::sleep(Duration::from_secs(42)).await;
    let tape = dj::CAPTURE.lock().unwrap().take().unwrap_or_default();
    let bytes: Vec<u8> = tape.iter().flat_map(|v| v.to_le_bytes()).collect();
    std::fs::write(&path, bytes)?;
    println!("saved {:.1} s to {path}", tape.len() as f32 / 88_200.0);
    drop(decks);
    session.shutdown();
    Ok(())
}

/// Run the beat tracker over a captured file and print what it concludes over time.
fn tempo_file(path: &str) -> Result<()> {
    let bytes = std::fs::read(path)?;
    let samples: Vec<f64> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64).collect();
    let mut tempo = dj::Tempo::default();
    let mut line = String::new();
    for (i, chunk) in samples.chunks(512).enumerate() {
        tempo.feed(chunk);
        if i % 172 == 171 && (i + 1) / 172 >= 4 {
            line.push_str(&format!(" {}s:{}({:.2})", (i + 1) / 172, tempo.bpm.map(|b| format!("{b:.1}")).unwrap_or_else(|| "-".into()), tempo.strength));
        }
    }
    println!("{path}:{line}");
    Ok(())
}

/// Run the two decks for real, muted: load two songs, read their tempos, do a
/// beat-matched transition and check the beats actually line up. The decks are
/// not a Spotify Connect device, so this never touches what is playing.
async fn dj_test(query: Option<String>) -> Result<()> {
    struct Mute;
    impl librespot_playback::mixer::VolumeGetter for Mute {
        fn attenuation_factor(&self) -> f64 {
            0.0
        }
    }
    let _ = init_logging();
    let cfg = doctor_config()?;
    let cache = engine::librespot_cache(&cfg)?;
    let creds = cache.credentials().ok_or_else(|| anyhow!("no saved login; run `riff`"))?;
    let session = librespot_core::Session::new(engine::session_config(&cfg), Some(cache));
    session.connect(creds, true).await.map_err(|e| anyhow!("connect: {e}"))?;
    let ok = |b: bool| if b { "\x1b[32m✓\x1b[0m" } else { "\x1b[31m✗\x1b[0m" };

    // Two songs, one per deck: "first query|second query".
    let query = query
        .filter(|q| !q.starts_with('-'))
        .unwrap_or_else(|| "daft punk digital love|rolling stones start me up".into());
    let mut tracks: Vec<model::Track> = Vec::new();
    for q in query.split('|') {
        let uris = spot::search_tracks(&session, q).await?;
        let found = spot::tracks(&session, &uris).await.into_iter().find(|t| t.playable && t.duration_ms > 120_000);
        tracks.extend(found);
    }
    if tracks.len() < 2 {
        return Err(anyhow!("need two playable songs for “{query}”"));
    }
    let hub = Arc::new(audio::Hub::default());
    let mut decks = dj::Dj::start(
        session.clone(),
        Box::new(Mute),
        hub.clone(),
        librespot_playback::config::Bitrate::Bitrate320,
        true,
    );
    let started = Instant::now();
    let t = |s: Instant| s.elapsed().as_secs_f32();
    let show = |v: &dj::View, d: usize| {
        let k = &v.decks[d];
        format!(
            "{} {} {:>5} {} {}",
            dj::deck_name(d),
            if k.playing { "▶" } else { "‖" },
            model::fmt_ms(k.pos_ms),
            k.meter.bpm.map(|b| format!("{b:6.1} BPM")).unwrap_or_else(|| "   ?   BPM".into()),
            if k.bend.abs() > 0.0005 { format!("{:+.1}%", k.bend * 100.0) } else { String::new() }
        )
    };

    println!("A: {} · {}\nB: {} · {}", tracks[0].name, tracks[0].artist_line(), tracks[1].name, tracks[1].artist_line());
    decks.load(0, tracks[0].clone(), 30_000, true)?;
    decks.load(1, tracks[1].clone(), 30_000, false)?;

    let mut signal = 0;
    let mut a_bpm = None;
    while t(started) < 9.0 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        signal += hub.poll().is_some_and(|s| s.bands.iter().any(|b| *b > 0.2)) as u32;
        let (v, _) = decks.view();
        a_bpm = v.decks[0].meter.bpm.or(a_bpm);
    }
    let (v, _) = decks.view();
    println!("{:5.1}s {} | {}", t(started), show(&v, 0), show(&v, 1));
    println!("{} Deck A plays and is heard by the analyser ({signal} of ~90 checks had signal)", ok(signal > 60));
    println!("{} Deck A position advances: {}", ok((37_000..41_000).contains(&v.decks[0].pos_ms)), model::fmt_ms(v.decks[0].pos_ms));
    println!("{} Deck A tempo: {:?}", ok(a_bpm.is_some()), a_bpm.map(|b| (b * 10.0).round() / 10.0));
    println!("{} Deck B is loaded and waiting at {}", ok(!v.decks[1].playing && !v.decks[1].loading), model::fmt_ms(v.decks[1].pos_ms));

    // Bring B in: it should start silently, be matched, then fade across.
    let mix_at = Instant::now();
    decks.mix(14.0)?;
    let mut notes = Vec::new();
    let (mut worst, mut samples, mut listened, mut faded_at) = (0.0f32, 0, false, None);
    let mut matched_at = None;
    while t(mix_at) < 25.0 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (v, new) = decks.view();
        for n in new {
            println!("{:5.1}s note: {n}", t(started));
            if n.starts_with("Beat-matched") {
                matched_at = Some(Instant::now());
            }
            notes.push(n);
        }
        if let Some((_, _, listening)) = v.auto {
            listened |= listening;
        }
        // Once matched and re-measured, the two decks' beats should coincide.
        if matched_at.is_some_and(|m| m.elapsed() > Duration::from_secs(5)) {
            if let (Some(a), Some(b)) = (v.decks[0].meter.phase, v.decks[1].meter.phase) {
                if v.decks[0].playing && v.decks[1].playing {
                    let d = (a - b).abs();
                    worst = worst.max(d.min(1.0 - d));
                    samples += 1;
                    if samples % 20 == 1 {
                        let mut signed = a - b;
                        if signed > 0.5 { signed -= 1.0 } else if signed < -0.5 { signed += 1.0 }
                        println!("{:5.1}s B is {:+.0}% of a beat behind A (A {:.1} BPM, B {:.1} BPM at {:+.1}%)", t(started), signed * 100.0, v.decks[0].meter.bpm.unwrap_or(0.0), v.decks[1].meter.bpm.unwrap_or(0.0), v.decks[1].bend * 100.0);
                    }
                }
            }
        }
        if v.auto.is_none() && v.fader > 0.99 && faded_at.is_none() && t(mix_at) > 1.0 {
            faded_at = Some(t(mix_at));
            println!("{:5.1}s {} | {}  fader {:.2}", t(started), show(&v, 0), show(&v, 1), v.fader);
        }
    }
    let (v, _) = decks.view();
    let matched = notes.iter().any(|n| n.starts_with("Beat-matched"));
    println!("{} Listened to deck B before bringing it in: {listened}", ok(listened));
    println!("{} Outcome: {}", ok(!notes.is_empty()), notes.last().cloned().unwrap_or_else(|| "nothing reported".into()));
    if matched {
        println!("{} Tempos agree after matching: A {:?}, B {:?}", ok(match (v.decks[0].meter.bpm, v.decks[1].meter.bpm) { (Some(a), Some(b)) => (a - b).abs() < 1.5 || !v.decks[0].playing, _ => true }), v.decks[0].meter.bpm, v.decks[1].meter.bpm);
        println!("{} Beats stayed together through the blend: worst gap {:.0}% of a beat over {samples} checks", ok(samples == 0 || worst < 0.12), worst * 100.0);
    }
    println!("{} Crossfader reached deck B after {:?} s; deck A stopped: {}", ok(faded_at.is_some() && !v.decks[0].playing), faded_at, !v.decks[0].playing);

    // Manual controls.
    decks.seek_by(1, 30_000);
    let rate = decks.bend(1, 0.0);
    decks.set_fader(0.5);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let (after, _) = decks.view();
    println!("{} Seek +30 s on deck B: {} → {}", ok(after.decks[1].pos_ms > v.decks[1].pos_ms + 25_000), model::fmt_ms(v.decks[1].pos_ms), model::fmt_ms(after.decks[1].pos_ms));
    println!("{} Tempo reset: {:+.1}%, fader centred: {:.2}", ok(rate == 0.0 && (after.fader - 0.5).abs() < 0.01), rate * 100.0, after.fader);
    let (waiting, dry) = hub.output_health();
    println!(
        "{} A change is heard about {:.0} ms later (6 ms to mix, {waiting:.0} ms on its way to the speakers); the sound card ran dry {dry} times",
        ok(waiting > 0.0 && waiting < 80.0 && dry <= 2),
        waiting + 5.8
    );
    decks.toggle(1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let quiet = hub.poll().is_none();
    println!("{} Pausing the last deck silences the output: {quiet}", ok(quiet));
    drop(decks);
    session.shutdown();
    Ok(())
}

/// Play a few seconds for real: checks audio output, events, seek and pause.
async fn play_test(mut cfg: Config, creds: librespot_core::authentication::Credentials, uris: Vec<String>) {
    use engine::{Conn, Event};
    cfg.volume = cfg.volume.min(40);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let hub = Arc::new(audio::Hub::default());
    let player = engine::Engine::start(cfg, creds, tx, hub.clone());
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
                    if let Err(e) = player.play_tracks(uris.clone(), 0, 0, false, &model::Repeat::Off) {
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
                if let Some(snap) = hub.poll() {
                    frames += 1;
                    if snap.bands.iter().any(|v| *v > 0.2) {
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
fn snapshot(size: &str, keys: &str, frames: u32) -> Result<()> {
    let (w, h) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u16>().ok()?, h.parse::<u16>().ok()?)))
        .ok_or_else(|| anyhow!("size looks like 120x36"))?;
    let mut app = demo::app(Config::default());
    app.fixed_step = Some(1.0 / 30.0);
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
    for _ in 0..frames {
        app.on_tick();
    }
    terminal.draw(|f| ui::draw(f, &mut app))?;

    print!("{}", frame_to_ansi(terminal.backend().buffer()));
    Ok(())
}

/// Give one picture depth and draw a frame of it: `doctor --depth cover.jpg
/// --at 2.5 --beat 1`. Prints the depth map and the frame side by side as
/// ANSI text, and how long the model took, to stderr.
#[cfg(feature = "depth")]
async fn depth_test(path: &str, at: f32, beat: f32) -> Result<()> {
    let cover = image::load_from_memory(&std::fs::read(path)?)?.to_rgb8();
    if !depth::model_path().exists() {
        eprintln!("Fetching the depth model (27 MB)…");
        depth::fetch(&reqwest::Client::new()).await?;
    }
    let started = Instant::now();
    let map = depth::estimate(&cover)?;
    eprintln!("depth worked out in {:.0} ms", started.elapsed().as_secs_f32() * 1000.0);

    let (w, h) = (48u16, 24u16);
    let thumb = image::imageops::resize(&cover, 160, 160, image::imageops::FilterType::Triangle);
    let shades = image::RgbImage::from_fn(art::DepthMap::SIDE, art::DepthMap::SIDE, |x, y| {
        let v = map.near[(y * art::DepthMap::SIDE + x) as usize];
        image::Rgb([v, v, v])
    });
    let left = art::render(&shades, w, h, art::ArtMode::Blocks);
    let right = art::render_depth(&thumb, &map, w, h, art::Fx { beat, bass: beat * 0.6, time: at });
    let mut out = String::new();
    for row in 0..h as usize {
        for side in [&left, &right] {
            for cell in &side.cells[row * w as usize..(row + 1) * w as usize] {
                let (fg, bg) = (cell.fg, cell.bg.unwrap_or((0, 0, 0)));
                out.push_str(&format!(
                    "\x1b[0;38;2;{};{};{};48;2;{};{};{}m{}",
                    fg.0, fg.1, fg.2, bg.0, bg.1, bg.2, cell.ch
                ));
            }
            out.push_str("\x1b[0m  ");
        }
        out.push_str("\x1b[0m\n");
    }
    print!("{out}");
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
