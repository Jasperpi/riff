//! Catalogue and library reads over the player's own connection (the same
//! service the official apps use). These don't touch the public Web API, so they
//! aren't subject to its rate limits and work for playlists you merely follow.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use anyhow::{Result, anyhow};
use librespot_core::error::ErrorKind;
use librespot_core::{Session, SpotifyId, SpotifyUri};
use librespot_protocol::extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery};
use librespot_protocol::extension_kind::ExtensionKind;
use librespot_protocol::metadata as pb;
use librespot_protocol::playlist4_external::SelectedListContent;
use protobuf::{EnumOrUnknown, Message};
use reqwest::Method;

use crate::model::*;

fn err(e: librespot_core::Error) -> anyhow::Error {
    anyhow!("Spotify: {e}")
}

/// The player connection has no timeout of its own, so a stalled one would
/// leave a page loading for ever. Give up on a request after this long.
const PATIENCE: Duration = Duration::from_secs(20);

async fn within<T>(
    request: impl Future<Output = Result<T, librespot_core::Error>>,
) -> Result<T, librespot_core::Error> {
    match tokio::time::timeout(PATIENCE, request).await {
        Ok(answer) => answer,
        Err(_) => Err(librespot_core::Error::deadline_exceeded("no answer from Spotify")),
    }
}

fn b62(gid: &[u8]) -> Option<String> {
    SpotifyId::from_raw(gid).ok()?.to_base62().ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Cover URL closest to 300px wide.
fn cover(group: &pb::ImageGroup, loose: &[pb::Image]) -> Option<String> {
    group
        .image
        .iter()
        .chain(loose)
        .filter(|i| !i.file_id().is_empty())
        .min_by_key(|i| (i.width() - 300).abs())
        .map(|i| format!("https://i.scdn.co/image/{}", hex(i.file_id())))
}

fn artist_refs(list: &[pb::Artist]) -> Vec<ArtistRef> {
    list.iter()
        .map(|a| ArtistRef { id: b62(a.gid()).unwrap_or_default(), name: a.name().to_string() })
        .collect()
}

fn track_from(uri: &str, t: &pb::Track) -> Track {
    let album = t.album.get_or_default();
    Track {
        uri: uri.to_string(),
        id: id_of(uri).to_string(),
        name: t.name().to_string(),
        artists: artist_refs(&t.artist),
        album: album.name().to_string(),
        album_id: b62(album.gid()).unwrap_or_default(),
        duration_ms: t.duration().max(0) as u32,
        explicit: t.explicit(),
        image: cover(album.cover_group.get_or_default(), &album.cover),
        added_at: None,
        playable: !t.file.is_empty() || !t.alternative.is_empty(),
    }
}

fn album_from(a: &pb::Album) -> Option<Album> {
    Some(Album {
        id: b62(a.gid())?,
        name: a.name().to_string(),
        artists: artist_refs(&a.artist),
        year: match a.date.get_or_default().year() {
            0 => String::new(),
            y => y.to_string(),
        },
        kind: format!("{:?}", a.type_()).to_lowercase(),
        image: cover(a.cover_group.get_or_default(), &a.cover),
        total_tracks: a.disc.iter().map(|d| d.track.len() as u32).sum(),
    })
}

/// Fetch one kind of metadata for many URIs: a few requests of 100 in flight
/// at a time, so even thousands of tracks arrive in a second or two. The flag
/// says whether every request was answered; when it is false, entries missing
/// from the result may simply not have been heard back about.
async fn batch(session: &Session, kind: ExtensionKind, uris: &[String]) -> (Vec<(String, Vec<u8>)>, bool) {
    let chunks: Vec<&[String]> = uris.chunks(100).collect();
    let mut out = Vec::with_capacity(uris.len());
    let mut complete = true;
    for wave in chunks.chunks(4) {
        let mut requests = Vec::with_capacity(wave.len());
        for chunk in wave {
            requests.push(batch_chunk(session, kind, chunk));
        }
        for (part, answered) in futures::future::join_all(requests).await {
            out.extend(part);
            complete &= answered;
        }
    }
    (out, complete)
}

/// One request's worth. If the service rejects it, the chunk is split and
/// retried so a single bad entry can't sink the rest.
async fn batch_chunk(session: &Session, kind: ExtensionKind, uris: &[String]) -> (Vec<(String, Vec<u8>)>, bool) {
    let mut out = Vec::with_capacity(uris.len());
    let mut answered = true;
    let mut work: Vec<&[String]> = vec![uris];
    while let Some(chunk) = work.pop() {
        let request = BatchedEntityRequest {
            entity_request: chunk
                .iter()
                .map(|uri| EntityRequest {
                    entity_uri: uri.clone(),
                    query: vec![ExtensionQuery {
                        extension_kind: EnumOrUnknown::new(kind),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        match within(session.spclient().get_extended_metadata(request)).await {
            Ok(response) => {
                for array in response.extended_metadata {
                    for entry in array.extension_data {
                        if let Some(data) = entry.extension_data.into_option() {
                            out.push((entry.entity_uri, data.value));
                        }
                    }
                }
            }
            // Only a rejected request points at a bad entry. A rate limit or
            // a dead connection would fail both halves as well, and splitting
            // turns one failed request into two hundred.
            Err(e) if matches!(e.kind, ErrorKind::InvalidArgument | ErrorKind::NotFound) => {
                if chunk.len() > 1 {
                    log::warn!("metadata batch of {} rejected ({e}); splitting", chunk.len());
                    let (a, b) = chunk.split_at(chunk.len() / 2);
                    work.push(a);
                    work.push(b);
                } else {
                    log::warn!("no metadata for {}: {e}", chunk[0]);
                }
            }
            Err(e) => {
                log::warn!("metadata batch of {} failed: {e}", chunk.len());
                answered = false;
            }
        }
    }
    (out, answered)
}

/// Full details for track URIs, returned in the order asked. Episodes and local
/// files are given placeholder rows so list positions stay intact.
pub async fn tracks(session: &Session, uris: &[String]) -> Vec<Track> {
    tracks_checked(session, uris).await.0
}

/// As `tracks`, and whether every lookup was answered. When it wasn't, some of
/// the "Unavailable" rows are songs we never heard back about, so the result
/// is fine to show but not to keep.
pub async fn tracks_checked(session: &Session, uris: &[String]) -> (Vec<Track>, bool) {
    let wanted: Vec<String> =
        uris.iter().filter(|u| u.starts_with("spotify:track:")).cloned().collect();
    let (found, complete) = batch(session, ExtensionKind::TRACK_V4, &wanted).await;
    let found: HashMap<String, Track> = found
        .into_iter()
        .filter_map(|(uri, bytes)| {
            let t = pb::Track::parse_from_bytes(&bytes).ok()?;
            Some((uri.clone(), track_from(&uri, &t)))
        })
        .collect();

    let tracks = uris.iter().map(|uri| found.get(uri).cloned().unwrap_or_else(|| placeholder(uri))).collect();
    (tracks, complete)
}

fn placeholder(uri: &str) -> Track {
    let mut t = Track { uri: uri.to_string(), playable: false, ..Default::default() };
    let parts: Vec<&str> = uri.split(':').collect();
    if parts.get(1) == Some(&"local") {
        // spotify:local:artist:album:title:seconds
        let field = |i: usize| parts.get(i).map(|p| p.replace('+', " ")).unwrap_or_default();
        t.name = field(4);
        t.artists = vec![ArtistRef { id: String::new(), name: field(2) }];
        t.album = field(3);
        t.duration_ms = field(5).parse::<u32>().unwrap_or(0) * 1000;
    } else if parts.get(1) == Some(&"episode") {
        t.id = id_of(uri).to_string();
        t.name = "Podcast episode".into();
        t.playable = true;
    } else {
        t.name = "Unavailable".into();
    }
    t
}

pub async fn albums(session: &Session, ids: &[String]) -> Vec<Album> {
    let uris: Vec<String> = ids.iter().map(|id| format!("spotify:album:{id}")).collect();
    let found: HashMap<String, Album> = batch(session, ExtensionKind::ALBUM_V4, &uris)
        .await
        .0
        .into_iter()
        .filter_map(|(uri, bytes)| {
            Some((uri, album_from(&pb::Album::parse_from_bytes(&bytes).ok()?)?))
        })
        .collect();
    uris.iter().filter_map(|u| found.get(u).cloned()).collect()
}

/// Names and portraits for artist ids, in the order asked.
pub async fn artists(session: &Session, ids: &[String]) -> Vec<Artist> {
    let uris: Vec<String> = ids.iter().map(|id| format!("spotify:artist:{id}")).collect();
    let found: HashMap<String, Artist> = batch(session, ExtensionKind::ARTIST_V4, &uris)
        .await
        .0
        .into_iter()
        .filter_map(|(uri, bytes)| {
            let a = pb::Artist::parse_from_bytes(&bytes).ok()?;
            let artist = Artist {
                id: id_of(&uri).to_string(),
                name: a.name().to_string(),
                image: cover(a.portrait_group.get_or_default(), &a.portrait),
            };
            Some((uri, artist))
        })
        .collect();
    uris.iter().filter_map(|u| found.get(u).cloned()).collect()
}

/// The user's playlists in sidebar order, folders included.
pub async fn rootlist(session: &Session) -> Result<Vec<SideEntry>> {
    let me = session.username();
    let mut out = Vec::new();
    let mut depth = 0u8;
    let mut from = 0usize;
    const PAGE: usize = 500;
    for _ in 0..20 {
        let bytes = within(session.spclient().get_rootlist(from, Some(PAGE))).await.map_err(err)?;
        let list = SelectedListContent::parse_from_bytes(&bytes)?;
        let contents = list.contents.get_or_default();
        for (i, item) in contents.items.iter().enumerate() {
            let uri = item.uri();
            let parts: Vec<&str> = uri.split(':').collect();
            match parts.get(1).copied() {
                Some("start-group") => {
                    let name = parts.get(3).map(|n| decode(n)).unwrap_or_default();
                    out.push(SideEntry::Folder { name, depth });
                    depth = depth.saturating_add(1);
                }
                Some("end-group") => depth = depth.saturating_sub(1),
                Some("playlist") => {
                    let meta = contents.meta_items.get(i);
                    let attrs = meta.map(|m| m.attributes.get_or_default());
                    let name = attrs.map(|a| a.name().to_string()).unwrap_or_default();
                    if name.is_empty() {
                        // Deleted or inaccessible playlists come back without metadata.
                        continue;
                    }
                    let owner = meta.map(|m| m.owner_username().to_string()).unwrap_or_default();
                    out.push(SideEntry::Playlist {
                        playlist: Playlist {
                            id: id_of(uri).to_string(),
                            name,
                            owner: if owner == me { String::new() } else { owner },
                            description: String::new(),
                            len: meta.map(|m| m.length().max(0) as u32).unwrap_or(0),
                            image: None,
                        },
                        depth,
                    });
                }
                _ => {}
            }
        }
        let got = contents.items.len();
        from += got;
        if got < PAGE {
            break;
        }
    }
    Ok(out)
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let byte = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match byte {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub struct PlaylistHead {
    pub playlist: Playlist,
    /// Changes whenever the playlist's contents do.
    pub revision: String,
    /// Item URIs with the time each was added (unix seconds).
    pub items: Vec<(String, Option<i64>)>,
}

/// A playlist's details and item URIs. Track details are fetched separately so
/// an unchanged playlist (same revision) costs a single request.
pub async fn playlist(session: &Session, id: &str) -> Result<PlaylistHead> {
    let sid = SpotifyId::from_base62(id).map_err(|_| anyhow!("bad playlist id"))?;
    let bytes = within(session.spclient().get_playlist(&sid)).await.map_err(err)?;
    let list = SelectedListContent::parse_from_bytes(&bytes)?;
    let attrs = list.attributes.get_or_default();
    let mut items: Vec<(String, Option<i64>)> = Vec::new();
    let take = |list: &SelectedListContent, items: &mut Vec<(String, Option<i64>)>| {
        for item in &list.contents.get_or_default().items {
            let at = item.attributes.get_or_default().timestamp();
            items.push((item.uri().to_string(), (at > 0).then_some(at / 1000)));
        }
    };
    take(&list, &mut items);

    // Very large playlists arrive in pieces.
    let total = list.length().max(0) as usize;
    let mut guard = 0;
    while list.contents.get_or_default().truncated() && items.len() < total && guard < 50 {
        guard += 1;
        let endpoint =
            format!("/playlist/v2/playlist/{id}?from={}&length=1000", items.len());
        let more = within(session.spclient().request(&Method::GET, &endpoint, None, None))
            .await
            .map_err(err)?;
        let more = SelectedListContent::parse_from_bytes(&more)?;
        let before = items.len();
        take(&more, &mut items);
        if items.len() == before {
            break;
        }
    }

    let image = attrs
        .picture_size
        .iter()
        .find(|p| p.target_name() == "default")
        .or(attrs.picture_size.first())
        .map(|p| p.url().to_string());
    let owner = list.owner_username().to_string();
    Ok(PlaylistHead {
        playlist: Playlist {
            id: id.to_string(),
            name: attrs.name().to_string(),
            owner: if owner == session.username() { String::new() } else { owner },
            description: attrs.description().to_string(),
            len: items.len() as u32,
            image,
        },
        revision: hex(list.revision()),
        items,
    })
}

pub async fn album(session: &Session, id: &str) -> Result<(Album, Vec<String>)> {
    let sid = SpotifyId::from_base62(id).map_err(|_| anyhow!("bad album id"))?;
    let bytes = within(session.spclient().get_album_metadata(&SpotifyUri::Album { id: sid }))
        .await
        .map_err(err)?;
    let a = pb::Album::parse_from_bytes(&bytes)?;
    let uris = a
        .disc
        .iter()
        .flat_map(|d| &d.track)
        .filter_map(|t| Some(format!("spotify:track:{}", b62(t.gid())?)))
        .collect();
    let album = album_from(&a).ok_or_else(|| anyhow!("album has no id"))?;
    Ok((album, uris))
}

pub struct ArtistHead {
    pub artist: Artist,
    pub top: Vec<String>,
    pub albums: Vec<String>,
    pub singles: Vec<String>,
    pub related: Vec<Artist>,
}

pub async fn artist(session: &Session, id: &str) -> Result<ArtistHead> {
    let sid = SpotifyId::from_base62(id).map_err(|_| anyhow!("bad artist id"))?;
    let bytes = within(session.spclient().get_artist_metadata(&SpotifyUri::Artist { id: sid }))
        .await
        .map_err(err)?;
    let a = pb::Artist::parse_from_bytes(&bytes)?;

    let country = session.country();
    let top = a
        .top_track
        .iter()
        .find(|t| t.country() == country)
        .or(a.top_track.first())
        .map(|t| {
            t.track
                .iter()
                .filter_map(|t| Some(format!("spotify:track:{}", b62(t.gid())?)))
                .collect()
        })
        .unwrap_or_default();
    // Each group is one release in several editions; the first is the canonical one.
    let first_of = |groups: &[pb::AlbumGroup]| -> Vec<String> {
        groups
            .iter()
            .filter_map(|g| b62(g.album.first()?.gid()))
            .take(40)
            .collect()
    };
    Ok(ArtistHead {
        artist: Artist {
            id: id.to_string(),
            name: a.name().to_string(),
            image: cover(a.portrait_group.get_or_default(), &a.portrait),
        },
        top,
        albums: first_of(&a.album_group),
        singles: first_of(&a.single_group),
        related: a
            .related
            .iter()
            .filter_map(|r| {
                Some(Artist { id: b62(r.gid())?, name: r.name().to_string(), image: None })
            })
            .filter(|r| !r.name.is_empty())
            .collect(),
    })
}

/// Spotify's own time-synced lyrics, when the track has them.
pub async fn lyrics(session: &Session, track_id: &str) -> Option<Lyrics> {
    let sid = SpotifyId::from_base62(track_id).ok()?;
    let bytes = within(session.spclient().get_lyrics(&sid)).await.ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let inner = &v["lyrics"];
    let synced = inner["syncType"].as_str() == Some("LINE_SYNCED");
    let lines: Vec<LyricLine> = inner["lines"]
        .as_array()?
        .iter()
        .map(|l| LyricLine {
            at_ms: synced
                .then(|| l["startTimeMs"].as_str().and_then(|s| s.parse().ok()))
                .flatten(),
            text: l["words"].as_str().unwrap_or("").to_string(),
        })
        .collect();
    (!lines.is_empty()).then(|| Lyrics {
        lines,
        synced,
        source: inner["providerDisplayName"].as_str().unwrap_or("Spotify").to_string(),
    })
}

// ---- saved library ("collection") ------------------------------------------
//
// Liked Songs, saved albums and followed artists live in the collection
// service. Its messages are tiny, so they are encoded by hand here rather than
// generated.

mod wire {
    pub fn varint(buf: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        buf.push(v as u8);
    }

    pub fn string(buf: &mut Vec<u8>, field: u32, s: &str) {
        bytes(buf, field, s.as_bytes());
    }

    pub fn bytes(buf: &mut Vec<u8>, field: u32, b: &[u8]) {
        varint(buf, ((field << 3) | 2) as u64);
        varint(buf, b.len() as u64);
        buf.extend_from_slice(b);
    }

    pub fn number(buf: &mut Vec<u8>, field: u32, v: u64) {
        varint(buf, (field << 3) as u64);
        varint(buf, v);
    }

    pub enum Value<'a> {
        Number(u64),
        Bytes(&'a [u8]),
    }

    pub struct Reader<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl<'a> Reader<'a> {
        pub fn new(data: &'a [u8]) -> Self {
            Self { data, pos: 0 }
        }

        fn varint(&mut self) -> Option<u64> {
            let mut out = 0u64;
            for shift in (0..64).step_by(7) {
                let b = *self.data.get(self.pos)?;
                self.pos += 1;
                out |= ((b & 0x7f) as u64) << shift;
                if b & 0x80 == 0 {
                    return Some(out);
                }
            }
            None
        }
    }

    impl<'a> Iterator for Reader<'a> {
        type Item = (u32, Value<'a>);

        /// Yields each field in turn; stops at the end or at anything malformed.
        fn next(&mut self) -> Option<Self::Item> {
            loop {
                let tag = self.varint()?;
                let field = (tag >> 3) as u32;
                match tag & 7 {
                    0 => return Some((field, Value::Number(self.varint()?))),
                    2 => {
                        let len = self.varint()? as usize;
                        let end = self.pos.checked_add(len)?;
                        let slice = self.data.get(self.pos..end)?;
                        self.pos = end;
                        return Some((field, Value::Bytes(slice)));
                    }
                    1 => self.pos = self.pos.checked_add(8)?,
                    5 => self.pos = self.pos.checked_add(4)?,
                    _ => return None,
                }
            }
        }
    }
}

use wire::Value;

#[derive(Clone, Debug)]
pub struct Saved {
    pub uri: String,
    /// Unix seconds.
    pub added_at: i64,
    pub removed: bool,
}

fn saved_from(bytes: &[u8]) -> Saved {
    let mut item = Saved { uri: String::new(), added_at: 0, removed: false };
    for (field, value) in wire::Reader::new(bytes) {
        match (field, value) {
            (1, Value::Bytes(b)) => item.uri = String::from_utf8_lossy(b).into_owned(),
            (2, Value::Number(n)) => item.added_at = n as i64,
            (3, Value::Number(n)) => item.removed = n != 0,
            _ => {}
        }
    }
    item
}

async fn collection_call(session: &Session, verb: &str, body: Vec<u8>) -> Result<Vec<u8>> {
    use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};
    const KIND: &str = "application/vnd.collection-v2.spotify.proto";
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(KIND));
    headers.insert(ACCEPT, HeaderValue::from_static(KIND));
    let endpoint = format!("/collection/v2/{verb}");
    let bytes = within(session.spclient().request(&Method::POST, &endpoint, Some(headers), Some(&body)))
        .await
        .map_err(err)?;
    Ok(bytes.to_vec())
}

/// Everything in one saved set, newest first, plus a token for later deltas.
/// Sets: `collection` (liked tracks and saved albums), `artist` (followed artists).
pub async fn collection(session: &Session, set: &str) -> Result<(Vec<Saved>, String)> {
    let user = session.username();
    let mut items = Vec::new();
    let mut token = String::new();
    let mut sync = String::new();
    for _ in 0..400 {
        let mut body = Vec::new();
        wire::string(&mut body, 1, &user);
        wire::string(&mut body, 2, set);
        if !token.is_empty() {
            wire::string(&mut body, 3, &token);
        }
        wire::number(&mut body, 4, 300);
        let reply = collection_call(session, "paging", body).await?;
        token.clear();
        for (field, value) in wire::Reader::new(&reply) {
            match (field, value) {
                (1, Value::Bytes(b)) => items.push(saved_from(b)),
                (2, Value::Bytes(b)) => token = String::from_utf8_lossy(b).into_owned(),
                (3, Value::Bytes(b)) => sync = String::from_utf8_lossy(b).into_owned(),
                _ => {}
            }
        }
        if token.is_empty() {
            break;
        }
    }
    items.retain(|i| !i.removed && !i.uri.is_empty());
    items.sort_by(|a, b| b.added_at.cmp(&a.added_at));
    Ok((items, sync))
}

/// Changes to a set since `sync`. `None` means the service wants a full reload.
pub async fn collection_delta(
    session: &Session,
    set: &str,
    sync: &str,
) -> Result<Option<(Vec<Saved>, String)>> {
    let mut body = Vec::new();
    wire::string(&mut body, 1, &session.username());
    wire::string(&mut body, 2, set);
    wire::string(&mut body, 3, sync);
    let reply = collection_call(session, "delta", body).await?;
    let (mut possible, mut items, mut next) = (false, Vec::new(), String::new());
    for (field, value) in wire::Reader::new(&reply) {
        match (field, value) {
            (1, Value::Number(n)) => possible = n != 0,
            (2, Value::Bytes(b)) => items.push(saved_from(b)),
            (3, Value::Bytes(b)) => next = String::from_utf8_lossy(b).into_owned(),
            _ => {}
        }
    }
    Ok(possible.then_some((items, next)))
}

/// Save or remove one item (like/unlike a track, save an album, follow an artist).
pub async fn collection_write(session: &Session, set: &str, uri: &str, saved: bool) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut item = Vec::new();
    wire::string(&mut item, 1, uri);
    wire::number(&mut item, 2, now);
    if !saved {
        wire::number(&mut item, 3, 1);
    }
    let mut body = Vec::new();
    wire::string(&mut body, 1, &session.username());
    wire::string(&mut body, 2, set);
    wire::bytes(&mut body, 3, &item);
    let id: [u8; 8] = rand::random();
    wire::string(&mut body, 4, &hex(&id));
    collection_call(session, "write", body).await.map(|_| ())
}

/// Add a track to this device's queue by sending it the same Connect command
/// the official apps send.
pub async fn queue_add(session: &Session, uri: &str) -> Result<()> {
    use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
    let device = session.device_id().to_string();
    let id: [u8; 16] = rand::random();
    let body = serde_json::json!({
        "command": {
            "endpoint": "add_to_queue",
            "track": { "uri": uri, "metadata": { "is_queued": "true" }, "provider": "queue" },
            "logging_params": { "command_id": hex(&id) },
        }
    })
    .to_string();
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let endpoint = format!("/connect-state/v1/player/command/from/{device}/to/{device}");
    within(session.spclient().request(&Method::POST, &endpoint, Some(headers), Some(body.as_bytes())))
        .await
        .map(|_| ())
        .map_err(err)
}

/// Track search over the player connection: no Web API involved. Returns
/// track URIs in ranked order.
pub async fn search_tracks(session: &Session, query: &str) -> Result<Vec<String>> {
    let q: String = query.split_whitespace().collect::<Vec<_>>().join("+");
    let uri = format!("spotify:search:{}", urlencode(&q));
    let ctx = within(session.spclient().get_context(&uri)).await.map_err(err)?;
    Ok(ctx
        .pages
        .iter()
        .flat_map(|p| &p.tracks)
        .filter_map(|t| t.uri.clone())
        .filter(|u| u.starts_with("spotify:track:"))
        .collect())
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'+' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_round_trip() {
        let mut item = Vec::new();
        wire::string(&mut item, 1, "spotify:track:abc");
        wire::number(&mut item, 2, 1_700_000_000);
        wire::number(&mut item, 3, 1);
        let mut page = Vec::new();
        wire::bytes(&mut page, 1, &item);
        wire::string(&mut page, 2, "next");
        // An unknown fixed64 field in the middle must be skipped, not fatal.
        page.extend_from_slice(&[(9 << 3) | 1, 0, 0, 0, 0, 0, 0, 0, 0]);
        wire::string(&mut page, 3, "sync");

        let fields: Vec<(u32, Vec<u8>)> = wire::Reader::new(&page)
            .filter_map(|(f, v)| match v {
                Value::Bytes(b) => Some((f, b.to_vec())),
                Value::Number(_) => None,
            })
            .collect();
        assert_eq!(fields.len(), 3);
        let saved = saved_from(&fields[0].1);
        assert_eq!(saved.uri, "spotify:track:abc");
        assert_eq!(saved.added_at, 1_700_000_000);
        assert!(saved.removed);
        assert_eq!(fields[2].1, b"sync");
    }

    #[test]
    fn truncated_input_stops_cleanly() {
        let mut page = Vec::new();
        wire::string(&mut page, 1, "hello world");
        page.truncate(page.len() - 4);
        assert_eq!(wire::Reader::new(&page).count(), 0);
    }

    #[test]
    fn folder_names_decode() {
        assert_eq!(decode("Road+Trip%21"), "Road Trip!");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("caf%C3%A9"), "café");
    }
}
