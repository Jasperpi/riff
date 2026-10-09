//! Desktop media integration (MPRIS): keyboard media keys, the GNOME/KDE media
//! widget and lock-screen controls all drive riff, and show what's playing.

use std::time::Duration;

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::backend::Msg;
use crate::model::Track;

#[derive(Debug)]
pub enum MediaKey {
    Toggle,
    Play,
    Pause,
    Next,
    Prev,
    SeekBy(i64),
    SeekTo(u32),
}

pub struct Mpris {
    controls: MediaControls,
    shown: (String, bool, u32),
}

impl Mpris {
    /// Returns `None` when there is no session bus (a bare TTY or SSH); riff
    /// simply runs without desktop integration then.
    pub fn start(tx: UnboundedSender<Msg>) -> Option<Self> {
        let config = PlatformConfig { display_name: "riff", dbus_name: "riff", hwnd: None };
        let mut controls = MediaControls::new(config).ok()?;
        controls
            .attach(move |event| {
                let signed = |dir: SeekDirection, ms: i64| match dir {
                    SeekDirection::Forward => ms,
                    SeekDirection::Backward => -ms,
                };
                let key = match event {
                    MediaControlEvent::Toggle => MediaKey::Toggle,
                    MediaControlEvent::Play => MediaKey::Play,
                    MediaControlEvent::Pause | MediaControlEvent::Stop => MediaKey::Pause,
                    MediaControlEvent::Next => MediaKey::Next,
                    MediaControlEvent::Previous => MediaKey::Prev,
                    MediaControlEvent::Seek(dir) => MediaKey::SeekBy(signed(dir, 5_000)),
                    MediaControlEvent::SeekBy(dir, by) => {
                        MediaKey::SeekBy(signed(dir, by.as_millis() as i64))
                    }
                    MediaControlEvent::SetPosition(MediaPosition(at)) => {
                        MediaKey::SeekTo(at.as_millis() as u32)
                    }
                    _ => return,
                };
                let _ = tx.send(Msg::Media(key));
            })
            .ok()?;
        Some(Self { controls, shown: (String::new(), false, u32::MAX) })
    }

    /// Publish the current state; cheap to call often, only changes are sent.
    pub fn sync(&mut self, track: Option<&Track>, playing: bool, pos_ms: u32) {
        let uri = track.map(|t| t.uri.as_str()).unwrap_or("");
        // Position is re-announced every few seconds so widgets' progress bars stay honest.
        let state = (uri.to_string(), playing, pos_ms / 4000);
        if state == self.shown {
            return;
        }
        if state.0 != self.shown.0 {
            let artist = track.map(|t| t.artist_line()).unwrap_or_default();
            let _ = self.controls.set_metadata(MediaMetadata {
                title: track.map(|t| t.name.as_str()),
                album: track.map(|t| t.album.as_str()),
                artist: Some(artist.as_str()).filter(|a| !a.is_empty()),
                cover_url: track.and_then(|t| t.image.as_deref()).filter(|u| u.starts_with("http")),
                duration: track.map(|t| Duration::from_millis(t.duration_ms as u64)),
            });
        }
        let progress = Some(MediaPosition(Duration::from_millis(pos_ms as u64)));
        let _ = self.controls.set_playback(match (track, playing) {
            (None, _) => MediaPlayback::Stopped,
            (Some(_), true) => MediaPlayback::Playing { progress },
            (Some(_), false) => MediaPlayback::Paused { progress },
        });
        self.shown = state;
    }
}
