//! Disk cache. Everything the library shows is served from here first and
//! refreshed in the background, so the interface never waits on the network.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Serialize, de::DeserializeOwned};

#[derive(Clone)]
pub struct Cache {
    dir: PathBuf,
}

#[derive(Serialize, serde::Deserialize)]
struct Entry<T> {
    at: u64,
    /// Revision / snapshot marker, used to skip re-downloading unchanged lists.
    #[serde(default)]
    rev: String,
    value: T,
}

pub struct Hit<T> {
    pub value: T,
    pub age: Duration,
    pub rev: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn safe(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(120)
        .collect()
}

impl Cache {
    pub fn new(dir: PathBuf) -> Self {
        std::fs::create_dir_all(dir.join("data")).ok();
        std::fs::create_dir_all(dir.join("img")).ok();
        std::fs::create_dir_all(dir.join("depth")).ok();
        Self { dir }
    }

    fn data_path(&self, key: &str) -> PathBuf {
        self.dir.join("data").join(format!("{}.json", safe(key)))
    }

    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Option<Hit<T>> {
        let bytes = std::fs::read(self.data_path(key)).ok()?;
        let entry: Entry<T> = serde_json::from_slice(&bytes).ok()?;
        Some(Hit {
            value: entry.value,
            age: Duration::from_secs(now().saturating_sub(entry.at)),
            rev: entry.rev,
        })
    }

    pub fn put<T: Serialize>(&self, key: &str, rev: &str, value: &T) {
        let entry = Entry { at: now(), rev: rev.to_string(), value };
        let Ok(bytes) = serde_json::to_vec(&entry) else { return };
        let path = self.data_path(key);
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            std::fs::rename(tmp, path).ok();
        }
    }

    fn image_path(&self, url: &str) -> PathBuf {
        // Spotify image URLs end in a stable content hash.
        let name = url.rsplit('/').next().unwrap_or(url);
        self.dir.join("img").join(safe(name))
    }

    pub fn image(&self, url: &str) -> Option<Vec<u8>> {
        std::fs::read(self.image_path(url)).ok()
    }

    pub fn put_image(&self, url: &str, bytes: &[u8]) {
        // Written aside and moved into place, so a crash can't leave half a file.
        let path = self.image_path(url);
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            std::fs::rename(tmp, path).ok();
        }
    }

    /// The depth map worked out for a cover, kept under the cover's own name.
    #[cfg_attr(not(feature = "depth"), allow(dead_code))]
    pub fn depth(&self, url: &str) -> Option<Vec<u8>> {
        let name = self.image_path(url).file_name()?.to_owned();
        std::fs::read(self.dir.join("depth").join(name)).ok()
    }

    #[cfg_attr(not(feature = "depth"), allow(dead_code))]
    pub fn put_depth(&self, url: &str, map: &[u8]) {
        let Some(name) = self.image_path(url).file_name().map(|n| n.to_owned()) else { return };
        let path = self.dir.join("depth").join(name);
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, map).is_ok() {
            std::fs::rename(tmp, path).ok();
        }
    }

    pub fn forget_image(&self, url: &str) {
        std::fs::remove_file(self.image_path(url)).ok();
    }
}
