//! First-run setup and sign-in. Runs in the plain terminal, before the
//! interface starts, so links can be read and a Client ID pasted.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use librespot_core::Session;
use librespot_core::authentication::Credentials;

use crate::auth::{self, Tokens};
use crate::config::Config;
use crate::engine;

const G: &str = "\x1b[38;2;30;215;96m";
const B: &str = "\x1b[1m";
const D: &str = "\x1b[2m";
const R: &str = "\x1b[0m";

pub struct Ready {
    /// Present only when the user has their own Client ID.
    pub tokens: Option<Tokens>,
    pub credentials: Credentials,
}

fn ask(prompt: &str) -> String {
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok();
    line.trim().to_string()
}

fn yes(prompt: &str) -> bool {
    !matches!(ask(&format!("{prompt} {D}[Y/n]{R} ")).to_lowercase().as_str(), "n" | "no")
}

fn spotify_player_credentials() -> Option<PathBuf> {
    let path = dirs::cache_dir()?.join("spotify-player").join("credentials.json");
    path.exists().then_some(path)
}

/// Optional extra: collect the Client ID of the user's own Spotify app.
fn wizard(cfg: &mut Config) -> Result<()> {
    println!();
    println!("  {G}{B}riff{R}  {D}optional: your own Spotify Client ID{R}");
    println!();
    println!("  riff plays music and shows your library without this. Adding a Client ID");
    println!("  switches on the extras that need Spotify's Web API:");
    println!("    · playlist results in search, and more results per search");
    println!("    · Recently Played and On Repeat");
    println!("    · controlling other devices (phone, speakers) from riff");
    println!();
    println!("  It's your own app, so its request quota is yours alone.");
    println!();
    println!("  {B}1.{R} Open  {G}https://developer.spotify.com/dashboard{R}  and choose {B}Create app{R}");
    println!("  {B}2.{R} Name and description: anything you like");
    println!("     Redirect URI:  {G}{}{R}", auth::REDIRECT_OWN);
    println!("     APIs used:     {B}Web API{R}");
    println!("  {B}3.{R} Save, open the app's {B}Settings{R}, and copy the {B}Client ID{R}");
    println!();
    if yes("  Open the dashboard in your browser now?") {
        auth::open_in_browser("https://developer.spotify.com/dashboard");
    }
    println!();
    loop {
        let id = ask(&format!("  Paste the Client ID {D}(or press Enter to go without){R}: "));
        if id.is_empty() {
            cfg.client_id.clear();
            println!("  {D}No Client ID. Run `riff setup` again any time.{R}");
            break;
        }
        if id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit()) {
            cfg.client_id = id.to_lowercase();
            break;
        }
        println!("  {D}That doesn't look like a Client ID (32 letters and digits). Try again.{R}");
    }
    cfg.save()?;
    Ok(())
}

async fn browser_login(client_id: &str, own: bool) -> Result<Tokens> {
    let redirect = if own { auth::REDIRECT_OWN } else { auth::REDIRECT_KEYMASTER };
    println!();
    println!("  Opening Spotify in your browser to sign in…");
    let tokens = auth::login(client_id, redirect, |url| {
        println!("  {D}If nothing opens, visit:{R}");
        println!("  {D}{url}{R}");
        if own {
            println!();
            println!("  {D}Seeing \"INVALID_CLIENT: Invalid redirect URI\"? Add exactly");
            println!("  {} to your app's Redirect URIs and run riff again.{R}", auth::REDIRECT_OWN);
        }
    })
    .await?;
    println!("  {G}✓{R} Signed in");
    Ok(tokens)
}

/// Sign the player in once so it stores reusable credentials.
async fn player_login(cfg: &Config, credentials: Credentials) -> Result<Credentials, librespot_core::Error> {
    let cache = engine::librespot_cache(cfg).map_err(librespot_core::Error::unavailable)?;
    let session = Session::new(engine::session_config(cfg), Some(cache.clone()));
    session.connect(credentials, true).await?;
    session.shutdown();
    cache
        .credentials()
        .ok_or_else(|| librespot_core::Error::unavailable("Spotify returned no reusable credentials"))
}

/// Make sure everything needed to start is in place, asking only for what's missing.
pub async fn ensure(cfg: &mut Config, redo_setup: bool, redo_login: bool) -> Result<Ready> {
    if redo_setup {
        wizard(cfg)?;
    }
    if redo_login {
        std::fs::remove_file(auth::token_path()).ok();
        std::fs::remove_file(engine::session_dir().join("credentials.json")).ok();
    }

    // Web API sign-in, only for people who added their own Client ID.
    let tokens = if cfg.client_id.is_empty() {
        None
    } else {
        Some(match Tokens::load().filter(|t| t.client_id == cfg.client_id) {
            Some(t) => t,
            None => {
                let t = browser_login(&cfg.client_id, true).await?;
                t.save()?;
                t
            }
        })
    };

    // Player sign-in.
    let cache = engine::librespot_cache(cfg)?;
    if let Some(credentials) = cache.credentials() {
        return Ok(Ready { tokens, credentials });
    }
    println!();
    println!("  {G}{B}riff{R}  {D}signing in{R}");

    if let Some(path) = spotify_player_credentials() {
        println!();
        if yes("  Found a spotify_player login on this machine. Use it for riff too?") {
            let imported = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Credentials>(&b).ok());
            match imported {
                Some(creds) => match player_login(cfg, creds).await {
                    Ok(credentials) => {
                        println!("  {G}✓{R} Signed in");
                        return Ok(Ready { tokens, credentials });
                    }
                    Err(e) => println!("  {D}That login no longer works ({e}); signing in fresh.{R}"),
                },
                None => println!("  {D}Couldn't read it; signing in fresh.{R}"),
            }
        }
    }

    let player_tokens = browser_login(auth::KEYMASTER_CLIENT_ID, false).await?;
    let credentials = player_login(cfg, Credentials::with_access_token(player_tokens.access_token))
        .await
        .map_err(|e| anyhow!("Spotify wouldn't start a playback session: {e}"))?;
    Ok(Ready { tokens, credentials })
}

pub fn logout() {
    std::fs::remove_file(auth::token_path()).ok();
    std::fs::remove_file(engine::session_dir().join("credentials.json")).ok();
    // The saved library belongs to the account that just left.
    std::fs::remove_dir_all(crate::config::cache_dir().join("data")).ok();
    println!("Signed out. Run `riff` to sign in again.");
}
