//! Spotify login: OAuth (authorization code + PKCE), token storage and refresh.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config;

/// Client ID librespot signs in with (Spotify's own desktop client).
pub const KEYMASTER_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
pub const PORT: u16 = 8898;
pub const REDIRECT_OWN: &str = "http://127.0.0.1:8898/callback";
pub const REDIRECT_KEYMASTER: &str = "http://127.0.0.1:8898/login";

const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";

pub const SCOPES: &[&str] = &[
    "streaming",
    "user-read-playback-state",
    "user-modify-playback-state",
    "user-read-currently-playing",
    "playlist-read-private",
    "playlist-read-collaborative",
    "playlist-modify-private",
    "playlist-modify-public",
    "user-follow-read",
    "user-follow-modify",
    "user-library-read",
    "user-library-modify",
    "user-read-recently-played",
    "user-top-read",
    "user-read-private",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tokens {
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn token_path() -> PathBuf {
    config::config_dir().join("token.json")
}

impl Tokens {
    pub fn load() -> Option<Self> {
        serde_json::from_slice(&std::fs::read(token_path()).ok()?).ok()
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(config::config_dir())?;
        config::write_private(&token_path(), &serde_json::to_vec_pretty(self)?)
    }

    fn fresh(&self) -> bool {
        self.expires_at > now() + 60
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: u64,
}

fn random_url_safe(n: usize) -> String {
    let bytes: Vec<u8> = (0..n).map(|_| rand::random::<u8>()).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn open_in_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok();
}

/// Run the browser login for `client_id` and return fresh tokens.
/// `on_url` is handed the authorize URL so the caller can display it.
pub async fn login(
    client_id: &str,
    redirect_uri: &str,
    on_url: impl FnOnce(&str),
) -> Result<Tokens> {
    let verifier = random_url_safe(48);
    let challenge =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_url_safe(16);

    let url = reqwest::Url::parse_with_params(
        AUTHORIZE_URL,
        &[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("scope", &SCOPES.join(" ")),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge),
            ("state", &state),
        ],
    )?;

    // Bind before opening the browser so the redirect can never race the listener.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", PORT))
        .await
        .with_context(|| format!("port {PORT} is busy; close whatever is using it and retry"))?;

    on_url(url.as_str());
    open_in_browser(url.as_str());

    let code = tokio::time::timeout(Duration::from_secs(300), wait_for_code(&listener, &state))
        .await
        .map_err(|_| anyhow!("timed out waiting for the browser login"))??;
    drop(listener);

    let resp = reqwest::Client::new()
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier.as_str()),
        ])
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("could not reach accounts.spotify.com")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("Spotify rejected the login ({status}): {body}");
    }
    let t: TokenResponse = resp.json().await?;
    Ok(Tokens {
        client_id: client_id.to_string(),
        access_token: t.access_token,
        refresh_token: t.refresh_token,
        expires_at: now() + t.expires_in.max(60),
    })
}

/// Serve the loopback redirect. Browsers also probe for favicons and the like,
/// so anything without our `state` is answered and ignored.
async fn wait_for_code(listener: &tokio::net::TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let n = match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            _ => continue,
        };
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request.split_whitespace().nth(1).unwrap_or("");
        let params: Vec<(String, String)> =
            match reqwest::Url::parse(&format!("http://127.0.0.1{target}")) {
                Ok(u) => u.query_pairs().into_owned().collect(),
                Err(_) => Vec::new(),
            };
        let get = |k: &str| params.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());

        if let Some(err) = get("error") {
            respond(&mut stream, "Login cancelled", "You can close this tab.").await;
            bail!("Spotify login was not approved ({err})");
        }
        match (get("code"), get("state")) {
            (Some(code), Some(s)) if s == state => {
                respond(&mut stream, "You're in.", "riff is connected. You can close this tab.")
                    .await;
                return Ok(code);
            }
            _ => {
                let _ = stream
                    .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            }
        }
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, title: &str, body: &str) {
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>riff</title>\
         <body style=\"margin:0;height:100vh;display:grid;place-items:center;background:#0d0f0e;\
         color:#e8ece9;font:16px system-ui,sans-serif\"><div style=\"text-align:center\">\
         <div style=\"font-size:44px;color:#1db954;letter-spacing:.2em\">riff</div>\
         <h2 style=\"font-weight:500\">{title}</h2><p style=\"opacity:.7\">{body}</p></div>"
    );
    let _ = stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}",
                html.len()
            )
            .as_bytes(),
        )
        .await;
    let _ = stream.shutdown().await;
}

/// Holds the Web API token and renews it shortly before it expires.
pub struct TokenStore {
    http: reqwest::Client,
    inner: tokio::sync::Mutex<Tokens>,
}

impl TokenStore {
    pub fn new(tokens: Tokens) -> Self {
        Self {
            http: reqwest::Client::new(),
            inner: tokio::sync::Mutex::new(tokens),
        }
    }

    pub async fn bearer(&self) -> Result<String> {
        let mut t = self.inner.lock().await;
        if !t.fresh() {
            *t = self.refresh(&t).await?;
        }
        Ok(t.access_token.clone())
    }

    /// Called after a 401: the token was revoked early, so renew regardless of expiry.
    pub async fn force_refresh(&self, stale: &str) -> Result<String> {
        let mut t = self.inner.lock().await;
        if t.access_token == stale {
            *t = self.refresh(&t).await?;
        }
        Ok(t.access_token.clone())
    }

    async fn refresh(&self, old: &Tokens) -> Result<Tokens> {
        let refresh = old
            .refresh_token
            .as_deref()
            .ok_or_else(|| anyhow!("login expired; run `riff login`"))?;
        let resp = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("client_id", old.client_id.as_str()),
            ])
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .context("could not reach accounts.spotify.com")?;
        if resp.status().is_client_error() {
            bail!("login expired; run `riff login`");
        }
        let t: TokenResponse = resp.error_for_status()?.json().await?;
        let tokens = Tokens {
            client_id: old.client_id.clone(),
            access_token: t.access_token,
            // Spotify only sends a new refresh token when it rotates it.
            refresh_token: t.refresh_token.or_else(|| old.refresh_token.clone()),
            expires_at: now() + t.expires_in.max(60),
        };
        tokens.save().ok();
        Ok(tokens)
    }
}
