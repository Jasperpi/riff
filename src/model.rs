//! Plain data types shared by every layer. Nothing here talks to the network.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ArtistRef {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Track {
    pub uri: String,
    /// Base62 id; empty for local files.
    pub id: String,
    pub name: String,
    pub artists: Vec<ArtistRef>,
    pub album: String,
    pub album_id: String,
    pub duration_ms: u32,
    pub explicit: bool,
    pub image: Option<String>,
    /// Unix seconds.
    pub added_at: Option<i64>,
    #[serde(default = "yes")]
    pub playable: bool,
}

fn yes() -> bool {
    true
}

impl Track {
    pub fn artist_line(&self) -> String {
        self.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub artists: Vec<ArtistRef>,
    pub year: String,
    pub kind: String,
    pub image: Option<String>,
    pub total_tracks: u32,
}

impl Album {
    pub fn artist_line(&self) -> String {
        self.artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub image: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub description: String,
    pub len: u32,
    pub image: Option<String>,
}

/// Anything that can sit in a list in the main pane.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Item {
    Track(Track),
    Album(Album),
    Artist(Artist),
    Playlist(Playlist),
}

impl Item {
    pub fn uri(&self) -> String {
        match self {
            Item::Track(t) => t.uri.clone(),
            Item::Album(a) => format!("spotify:album:{}", a.id),
            Item::Artist(a) => format!("spotify:artist:{}", a.id),
            Item::Playlist(p) => format!("spotify:playlist:{}", p.id),
        }
    }

    /// Text the in-list filter matches against.
    pub fn haystack(&self) -> String {
        match self {
            Item::Track(t) => format!("{} {} {}", t.name, t.artist_line(), t.album),
            Item::Album(a) => format!("{} {}", a.name, a.artist_line()),
            Item::Artist(a) => a.name.clone(),
            Item::Playlist(p) => format!("{} {}", p.name, p.owner),
        }
    }
}

/// One row of the library sidebar.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SideEntry {
    Liked,
    Albums,
    Artists,
    Recent,
    Top,
    Header(String),
    Folder { name: String, depth: u8 },
    Playlist { playlist: Playlist, depth: u8 },
}

impl SideEntry {
    pub fn selectable(&self) -> bool {
        !matches!(self, SideEntry::Header(_) | SideEntry::Folder { .. })
    }
}

/// What a page shows; also the key its data is cached under.
#[derive(Clone, Debug, PartialEq)]
pub enum PageSpec {
    Liked,
    SavedAlbums,
    FollowedArtists,
    Recent,
    Top,
    Playlist(String),
    Album(String),
    Artist(String),
    Search(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Repeat {
    #[default]
    Off,
    Context,
    Track,
}

#[derive(Clone, Debug, Default)]
pub struct LyricLine {
    pub at_ms: Option<u32>,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct Lyrics {
    pub lines: Vec<LyricLine>,
    pub synced: bool,
    pub source: String,
}

#[derive(Clone, Debug, Default)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub volume: u8,
    pub active: bool,
    pub this: bool,
}

pub fn fmt_ms(ms: u32) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

pub fn id_of(uri: &str) -> &str {
    uri.rsplit(':').next().unwrap_or(uri)
}
