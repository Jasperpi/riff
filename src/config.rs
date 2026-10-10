//! On-disk settings and the directories everything else lives in.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Client ID of your own Spotify app. Optional: enables the Web API extras.
    pub client_id: String,
    pub device_name: String,
    pub device_id: String,
    /// 0-100
    pub volume: u8,
    /// 96, 160 or 320
    pub bitrate: u16,
    pub normalize: bool,
    pub audio_cache: bool,
    /// blocks | ascii | braille | pulse | depth
    pub art: String,
    /// Tint the interface with the colours of the current album.
    pub dynamic_color: bool,
    pub visualizer: bool,
    pub mouse: bool,
    /// Blend the end of each song into the start of the next.
    pub mix: bool,
    /// Length of that blend, 1-12.
    pub mix_seconds: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            device_name: "riff".into(),
            device_id: String::new(),
            volume: 70,
            bitrate: 320,
            normalize: true,
            audio_cache: true,
            art: "blocks".into(),
            dynamic_color: true,
            visualizer: true,
            mouse: true,
            mix: false,
            mix_seconds: 6,
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("riff")
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("riff")
}

fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        let mut cfg = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("{} is not valid TOML", path.display()))?,
            Err(_) => Config::default(),
        };
        if cfg.device_id.is_empty() {
            // Stable across launches so Spotify sees one "riff" device, not a new one each run.
            let bytes: [u8; 20] = rand::random();
            cfg.device_id = bytes.iter().map(|b| format!("{b:02x}")).collect();
            cfg.save().ok();
        }
        cfg.volume = cfg.volume.min(100);
        cfg.mix_seconds = cfg.mix_seconds.clamp(1, 12);
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(config_dir())?;
        write_private(&config_path(), toml::to_string_pretty(self)?.as_bytes())
    }
}

/// Write a file only the user can read; tokens and settings live here.
pub fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
