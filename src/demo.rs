//! Offline demo: a fake library and a generated cover, so the interface can be
//! explored (and screenshotted) without a Spotify account.

use std::sync::Arc;

use image::{Rgb, RgbImage};

use crate::app::{App, LyricState};
use crate::backend::{Msg, PageUpdate};
use crate::config::Config;
use crate::engine::{Conn, Event};
use crate::model::*;
use crate::audio::Hub;

const COVER: &str = "demo://cover";

/// A synthwave sunset: gradient sky, banded sun, perspective grid.
fn cover() -> RgbImage {
    RgbImage::from_fn(300, 300, |x, y| {
        let (fx, fy) = (x as f32 / 300.0, y as f32 / 300.0);
        let lerp = |a: [f32; 3], b: [f32; 3], t: f32| {
            [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
        };
        let horizon = 0.62;
        let mut c = if fy < horizon {
            lerp([24.0, 10.0, 62.0], [240.0, 72.0, 128.0], (fy / horizon).powf(1.6))
        } else {
            let t = (fy - horizon) / (1.0 - horizon);
            let mut ground = lerp([46.0, 8.0, 74.0], [10.0, 4.0, 26.0], t);
            // Grid lines converge on the horizon.
            let depth = 1.0 / (t + 0.04);
            let across = ((fx - 0.5) * depth * 3.0).rem_euclid(1.0);
            let along = (depth * 1.4).rem_euclid(1.0);
            if across < 0.06 || along < 0.10 {
                ground = lerp(ground, [255.0, 90.0, 220.0], 0.8);
            }
            ground
        };
        let (dx, dy) = (fx - 0.5, fy - 0.42);
        if (dx * dx + dy * dy).sqrt() < 0.23 && fy < horizon {
            let banded = fy > 0.40 && ((fy - 0.40) * 34.0).rem_euclid(1.0) < 0.34;
            if !banded {
                c = lerp([255.0, 226.0, 92.0], [255.0, 84.0, 112.0], ((dy + 0.23) / 0.46).clamp(0.0, 1.0));
            }
        }
        Rgb([c[0] as u8, c[1] as u8, c[2] as u8])
    })
}

/// The other albums' covers, each drawn its own way so the squares and the
/// lists have some variety to them.
fn album_cover(album: &str) -> Option<RgbImage> {
    let lerp = |a: [f32; 3], b: [f32; 3], t: f32| -> [f32; 3] {
        std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t.clamp(0.0, 1.0))
    };
    let paint = |f: &dyn Fn(f32, f32) -> [f32; 3]| {
        RgbImage::from_fn(300, 300, |x, y| {
            let c = f(x as f32 / 300.0, y as f32 / 300.0);
            Rgb([c[0] as u8, c[1] as u8, c[2] as u8])
        })
    };
    Some(match album {
        // A wave curling over under a low sun.
        "Tidal" => paint(&|x, y| {
            let mut c = lerp([255.0, 196.0, 120.0], [255.0, 120.0, 90.0], y * 1.6);
            let (dx, dy) = (x - 0.72, y - 0.3);
            if (dx * dx + dy * dy).sqrt() < 0.11 {
                c = [255.0, 244.0, 200.0];
            }
            for (k, colour) in [[20.0, 70.0, 130.0], [10.0, 120.0, 170.0], [60.0, 190.0, 210.0]].iter().enumerate() {
                let crest = 0.48 + k as f32 * 0.12 + (x * 9.0 + k as f32 * 2.0).sin() * 0.05;
                if y > crest {
                    c = *colour;
                    if y < crest + 0.02 {
                        c = [240.0, 250.0, 255.0];
                    }
                }
            }
            c
        }),
        // Squares inside squares, in muted paint.
        "Small Rooms" => paint(&|x, y| {
            let d = (x - 0.5).abs().max((y - 0.56).abs());
            match (d * 8.0) as u32 {
                0 => [236.0, 196.0, 92.0],
                1 => [214.0, 120.0, 72.0],
                2 => [150.0, 64.0, 70.0],
                3 => [70.0, 44.0, 70.0],
                _ => [34.0, 30.0, 44.0],
            }
        }),
        // A station roundel on tiles.
        "Platform Nine" => paint(&|x, y| {
            let (dx, dy) = (x - 0.5, y - 0.5);
            let r = (dx * dx + dy * dy).sqrt();
            let tile = ((x * 12.0).fract() < 0.06 || (y * 24.0).fract() < 0.06) as u8 as f32;
            let mut c = lerp([238.0, 236.0, 226.0], [180.0, 178.0, 170.0], tile);
            if (0.22..0.33).contains(&r) {
                c = [220.0, 36.0, 48.0];
            }
            if dy.abs() < 0.05 && dx.abs() < 0.38 {
                c = [24.0, 48.0, 140.0];
            }
            c
        }),
        // Seventies stripes sweeping across cream.
        "Polaroids" => paint(&|x, y| {
            let band = ((x + y * 0.6) * 7.0) as i32;
            match band {
                3 => [232.0, 72.0, 52.0],
                4 => [246.0, 146.0, 44.0],
                5 => [250.0, 204.0, 64.0],
                6 => [74.0, 160.0, 120.0],
                _ => [244.0, 232.0, 206.0],
            }
        }),
        // Pines black against a big orange moon.
        "Hollow Pines" => paint(&|x, y| {
            let mut c = lerp([30.0, 22.0, 60.0], [120.0, 60.0, 90.0], y);
            let (dx, dy) = (x - 0.5, y - 0.42);
            if (dx * dx + dy * dy).sqrt() < 0.24 {
                c = [250.0, 150.0, 60.0];
            }
            for k in 0..6 {
                let base = k as f32 / 5.0;
                let height = 0.35 + (k * 37 % 5) as f32 * 0.06;
                if y > 1.0 - height && (x - base).abs() < (y - (1.0 - height)) * 0.35 {
                    c = [12.0, 24.0, 20.0];
                }
            }
            c
        }),
        _ => return None,
    })
}

fn album_url(album: &str) -> String {
    match album_cover(album) {
        Some(_) => format!("demo://{}", album.to_lowercase().replace(' ', "-")),
        None => COVER.into(),
    }
}

fn track(n: usize, name: &str, artist: &str, album: &str, secs: u32) -> Track {
    Track {
        uri: format!("spotify:track:demo{n:018}"),
        id: format!("demo{n:018}"),
        name: name.into(),
        artists: vec![ArtistRef { id: format!("artist{n}"), name: artist.into() }],
        album: album.into(),
        album_id: format!("album{n}"),
        duration_ms: secs * 1000,
        explicit: false,
        image: Some(album_url(album)),
        added_at: None,
        playable: true,
    }
}

const SONGS: &[(&str, &str, &str, u32)] = &[
    ("Midnight Drive", "Neon Arcade", "Afterglow", 232),
    ("Glass Horizon", "Neon Arcade", "Afterglow", 198),
    ("Paper Planes at Dawn", "The Quiet Hours", "Small Rooms", 254),
    ("Saltwater", "Juno Vale", "Tidal", 187),
    ("Northern Line", "Harbour & Co.", "Platform Nine", 221),
    ("Velvet Static", "Neon Arcade", "Afterglow", 243),
    ("Slow Motion Summer", "Marlowe", "Polaroids", 209),
    ("Kerosene Skies", "The Quiet Hours", "Small Rooms", 276),
    ("Low Tide Radio", "Juno Vale", "Tidal", 194),
    ("Second Hand Stars", "Marlowe", "Polaroids", 238),
    ("Echo Park", "Harbour & Co.", "Platform Nine", 205),
    ("Lanterns", "Ivy & the Wolves", "Hollow Pines", 262),
    ("Copper Wire", "Ivy & the Wolves", "Hollow Pines", 216),
    ("Afterglow", "Neon Arcade", "Afterglow", 301),
    ("Tangerine", "Marlowe", "Polaroids", 177),
    ("Half Light", "Juno Vale", "Tidal", 229),
    ("Ghost in the Stereo", "The Quiet Hours", "Small Rooms", 248),
    ("Wildflower Motel", "Ivy & the Wolves", "Hollow Pines", 233),
    ("Signal Fires", "Harbour & Co.", "Platform Nine", 211),
    ("Cassette Summer", "Neon Arcade", "B-Sides", 196),
    ("Blue Hour", "Marlowe", "Polaroids", 244),
    ("Undertow", "Juno Vale", "Tidal", 259),
    ("Polaroid Heart", "Marlowe", "Polaroids", 188),
    ("Satellites", "The Quiet Hours", "Small Rooms", 272),
];

const LYRICS: &[&str] = &[
    "Headlights paint the avenue in gold",
    "Radio's humming something old",
    "We don't need a map tonight",
    "Just the road and the city lights",
    "",
    "Midnight drive, windows down",
    "Every signal turning green in this town",
    "Hold on tight, don't look back",
    "We're a song on an endless track",
    "",
    "Skyline flickers like a film reel",
    "Tell me everything you feel",
    "The night is young and so are we",
    "Chasing neon to the sea",
    "",
    "Midnight drive, windows down",
    "Every signal turning green in this town",
    "Hold on tight, don't look back",
    "We're a song on an endless track",
];

/// Use `image` as the album cover.
pub fn set_cover(app: &mut App, image: RgbImage) {
    app.on_msg(Msg::Image { url: COVER.into(), image: Some(Arc::new(image)) });
}

pub fn app(cfg: Config) -> App {
    let mut app = App::new(cfg, None, Arc::new(Hub::default()));
    app.demo = true;
    app.on_msg(Msg::Image { url: COVER.into(), image: Some(Arc::new(cover())) });
    for album in ["Tidal", "Small Rooms", "Platform Nine", "Polaroids", "Hollow Pines"] {
        app.on_msg(Msg::Image { url: album_url(album), image: album_cover(album).map(Arc::new) });
    }

    let playlists = [
        ("Late Night Coding", 84, 0),
        ("Morning Coffee", 52, 0),
        ("Road Trip 2026", 131, 0),
        ("Gym", 67, 1),
        ("Run Club", 45, 1),
        ("Deep Focus", 203, 0),
        ("Indie Mix", 96, 0),
        ("Discover Weekly", 30, 0),
        ("Throwbacks", 148, 0),
    ];
    let mut side = Vec::new();
    for (i, (name, len, depth)) in playlists.iter().enumerate() {
        if i == 3 {
            side.push(SideEntry::Folder { name: "Workouts".into(), depth: 0 });
        }
        side.push(SideEntry::Playlist {
            playlist: Playlist {
                id: format!("pl{i}"),
                name: (*name).into(),
                owner: if *name == "Discover Weekly" { "spotify".into() } else { String::new() },
                description: String::new(),
                len: *len,
                image: None,
            },
            depth: *depth,
        });
    }
    app.on_msg(Msg::Sidebar(side));

    let tracks: Vec<Track> = SONGS
        .iter()
        .enumerate()
        .map(|(i, (name, artist, album, secs))| track(i, name, artist, album, *secs))
        .collect();
    let id = app.pages[0].id;
    app.pages[0].head.subtitle = format!("{} songs", tracks.len());
    app.on_msg(Msg::Page {
        id,
        update: PageUpdate::Set {
            tab: 0,
            items: tracks.iter().cloned().map(Item::Track).collect(),
            done: true,
        },
    });
    app.on_msg(Msg::Liked(tracks.iter().step_by(2).map(|t| t.uri.clone()).collect()));
    app.on_msg(Msg::Queue(tracks[1..9].to_vec()));

    app.on_msg(Msg::Engine(Event::Conn(Conn::Online)));
    app.on_msg(Msg::Engine(Event::Track(tracks[0].clone())));
    app.on_msg(Msg::Engine(Event::Playing { uri: tracks[0].uri.clone(), pos_ms: 47_000 }));
    app.pb.devices = vec![
        Device { id: "a".into(), name: "riff".into(), kind: "computer".into(), volume: 70, active: true, this: true },
        Device { id: "b".into(), name: "Jasper's iPhone".into(), kind: "smartphone".into(), volume: 55, active: false, this: false },
        Device { id: "c".into(), name: "Living Room".into(), kind: "speaker".into(), volume: 40, active: false, this: false },
    ];
    app.lyrics = LyricState::Ready(Lyrics {
        lines: LYRICS
            .iter()
            .enumerate()
            .map(|(i, l)| LyricLine { at_ms: Some(30_000 + i as u32 * 4_200), text: (*l).into() })
            .collect(),
        synced: true,
        source: "demo".into(),
    });
    app
}
