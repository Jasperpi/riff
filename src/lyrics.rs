//! Lyrics fallback: LRCLIB, a free community database of synced lyrics.

use std::time::Duration;

use crate::model::{LyricLine, Lyrics, Track};

pub async fn lrclib(http: &reqwest::Client, track: &Track) -> Option<Lyrics> {
    let artist = track.artists.first()?.name.clone();
    let resp = http
        .get("https://lrclib.net/api/get")
        .query(&[
            ("track_name", track.name.clone()),
            ("artist_name", artist),
            ("album_name", track.album.clone()),
            ("duration", (track.duration_ms / 1000).to_string()),
        ])
        .header("user-agent", "riff (terminal Spotify client)")
        .timeout(Duration::from_secs(8))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    if let Some(synced) = v["syncedLyrics"].as_str().filter(|s| !s.trim().is_empty()) {
        let lines = parse_lrc(synced);
        if !lines.is_empty() {
            return Some(Lyrics { lines, synced: true, source: "LRCLIB".into() });
        }
    }
    let plain = v["plainLyrics"].as_str().filter(|s| !s.trim().is_empty())?;
    Some(Lyrics {
        lines: plain.lines().map(|l| LyricLine { at_ms: None, text: l.to_string() }).collect(),
        synced: false,
        source: "LRCLIB".into(),
    })
}

/// Parse `[mm:ss.xx] text` lines; anything without a timestamp is skipped.
pub fn parse_lrc(text: &str) -> Vec<LyricLine> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix('[') else { continue };
        let Some((stamp, words)) = rest.split_once(']') else { continue };
        let Some((min, sec)) = stamp.split_once(':') else { continue };
        let (Ok(min), Ok(sec)) = (min.parse::<u32>(), sec.parse::<f64>()) else { continue };
        out.push(LyricLine {
            at_ms: Some(min * 60_000 + (sec * 1000.0).round() as u32),
            text: words.trim().to_string(),
        });
    }
    out
}

/// Index of the line being sung at `pos_ms`.
pub fn current_line(lyrics: &Lyrics, pos_ms: u32) -> Option<usize> {
    if !lyrics.synced {
        return None;
    }
    lyrics
        .lines
        .iter()
        .rposition(|l| l.at_ms.is_some_and(|t| t <= pos_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lrc() {
        let l = parse_lrc("[00:12.50] hello\n[ar: someone]\n[01:02.00]world\nplain");
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].at_ms, Some(12_500));
        assert_eq!(l[1].at_ms, Some(62_000));
        assert_eq!(l[1].text, "world");
        let lyrics = Lyrics { lines: l, synced: true, source: String::new() };
        assert_eq!(current_line(&lyrics, 0), None);
        assert_eq!(current_line(&lyrics, 13_000), Some(0));
        assert_eq!(current_line(&lyrics, 99_000), Some(1));
    }
}
