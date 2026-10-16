//! Four songs at once, one of them heard: the four-square mode.
//!
//! All four songs play the whole time. The one being heard leads; the other
//! three run silently underneath it, each bent to its tempo and slid so its
//! beats, and where they can be found its bars, fall on the leader's. So
//! whichever square is picked next is already in time, and the switch can
//! land on the one.
//!
//! Picking a square never switches on the spot. The switch waits for the
//! next bar (or, for quick flicking, the next beat) and goes over in a way
//! that suits how it was asked for: a long blend with the bass lines
//! swapping half-way, a stutter into a drop, a straight cut, or, for a song
//! whose tempo can't be matched, a sweep that clears the floor for it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use librespot_core::{Session, SpotifyUri};
use librespot_playback::config::Bitrate;
use librespot_playback::mixer::VolumeGetter;
use librespot_playback::player::Player;

use crate::audio::{Hub, Monitor, RATE};
use crate::dj::{
    Biquad, DeckMix, DeckShared, EQ_KILL, HOP, HOPS_PER_SEC, Lock, OUT, Tempo, Wake, beatmatch, deck_player, load_f32,
    pair, pull,
};
use crate::model::Track;
use crate::out::Output;

pub const SQUARES: usize = 4;

/// How one square takes over from another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    /// Over a bar: the new song comes up with its bass held back, the two
    /// bass lines swap on the third beat, and the old song bows out.
    Blend,
    /// The old song's last beat stutters, faster and faster, and the new one
    /// drops on the one.
    Roll,
    /// Straight over, on the beat.
    Cut,
    /// The old song is swept up and away over a beat, and the new one drops
    /// in its place. For a song that couldn't be put in time with the other.
    Sweep,
}

impl Move {
    pub fn name(self) -> &'static str {
        match self {
            Self::Blend => "blend",
            Self::Roll => "roll",
            Self::Cut => "cut",
            Self::Sweep => "sweep",
        }
    }

    /// How long before the switch the old song starts doing something about it.
    fn lead_in(self) -> f64 {
        match self {
            Self::Roll | Self::Sweep => 1.0,
            Self::Blend | Self::Cut => 0.0,
        }
    }
}

/// The move that suits a switch: the longer the song being left has had,
/// the more of a transition it gets. Flicking between squares chops on the
/// beat; settling on one blends. `slam` asks for a drop whatever.
pub fn choose(stayed_beats: f64, matched: bool, slam: bool) -> Move {
    if !matched {
        Move::Sweep
    } else if slam {
        Move::Roll
    } else if stayed_beats < 4.0 {
        Move::Cut
    } else if stayed_beats < 16.0 {
        Move::Roll
    } else {
        Move::Blend
    }
}

/// The leader's beats and bars, on the mixer's clock (in analysis steps).
#[derive(Clone, Copy, Debug)]
pub struct Grid {
    pub period: f64,
    pub beat_at: f64,
    pub bar_at: Option<f64>,
}

impl Grid {
    fn of(t: &Tempo) -> Option<Self> {
        t.bpm.map(|_| Self { period: t.period as f64, beat_at: t.beat_at, bar_at: t.bar_at })
    }
}

/// The first point at or after `from` that is `origin` plus a whole number of `spacing`.
fn next_on(from: f64, origin: f64, spacing: f64) -> f64 {
    origin + ((from - origin) / spacing).ceil() * spacing
}

/// When a switch asked for at step `now` should land. A cut goes on the
/// next beat; anything else on the next bar, leaving the old song a beat
/// to roll or sweep in if the move needs one. Without a beat to go by, in
/// a tenth of a second.
pub fn when(now: f64, grid: Option<Grid>, mv: Move) -> f64 {
    let soon = now + HOPS_PER_SEC as f64 * 0.1;
    let Some(g) = grid.filter(|g| g.period > 1.0) else { return soon };
    // A switch needs a moment's notice so it never lands mid-block.
    let earliest = now + HOPS_PER_SEC as f64 * 0.12 + mv.lead_in() * g.period * 0.5;
    match mv {
        Move::Cut => next_on(earliest, g.beat_at, g.period),
        _ => next_on(earliest, g.bar_at.unwrap_or(g.beat_at), g.period * 4.0),
    }
}

/// How loud the outgoing and incoming songs are `p` of the way through a
/// move, and whether each has its lows: the blend holds the new song's
/// bass back until half-way, then hands it over.
pub fn blend_at(mv: Move, p: f32) -> ((f32, bool), (f32, bool)) {
    use std::f32::consts::FRAC_PI_2;
    match mv {
        Move::Blend => {
            let up = (p * 2.0).clamp(0.0, 1.0);
            let down = (p * 2.0 - 1.0).clamp(0.0, 1.0);
            let half = p >= 0.5;
            // Exactly silent at the end, so a finished song costs nothing to mix.
            let out = if down >= 1.0 { 0.0 } else { (down * FRAC_PI_2).cos() };
            ((out, !half), ((up * FRAC_PI_2).sin(), half))
        }
        _ if p <= 0.0 => ((1.0, true), (0.0, true)),
        _ => ((0.0, true), (1.0, true)),
    }
}

/// A beat repeated: the audio from the start is let through once, then
/// played again and again in slices that get shorter half-way.
#[derive(Default)]
struct Stutter {
    /// Frames since it began.
    at: usize,
    /// Its whole length, in frames.
    length: usize,
    /// The slice repeated, in frames.
    slice: usize,
    /// What has been heard since it began, interleaved stereo.
    tape: Vec<f64>,
}

impl Stutter {
    fn new(beat_frames: usize) -> Self {
        let slice = (beat_frames / 4).max(OUT);
        Self { at: 0, length: beat_frames, slice, tape: Vec::with_capacity(beat_frames * 2) }
    }

    fn done(&self) -> bool {
        self.at >= self.length
    }

    /// Replace a block of the deck's audio with the stutter.
    fn run(&mut self, block: &mut [f64]) {
        for pair in block.chunks_exact_mut(2) {
            if self.tape.len() < self.length * 2 {
                self.tape.extend_from_slice(pair);
            }
            // Quarter-beat slices, then eighths: the roll speeds up into the drop.
            let slice = if self.at * 2 >= self.length { self.slice / 2 } else { self.slice }.max(64);
            if self.at >= slice {
                let q = self.at % slice;
                // Each repeat fades in and out over a millisecond and a half, so it never clicks.
                let edge = (q.min(slice - q) as f64 / 64.0).min(1.0);
                for c in 0..2 {
                    pair[c] = self.tape[2 * q + c] * edge;
                }
            }
            self.at += 1;
        }
    }
}

/// The old song being swept away: its lows thinned out by a rising
/// high-pass, as the new one waits to drop.
struct Sweep {
    filters: [[Biquad; 2]; 2],
}

impl Sweep {
    fn new() -> Self {
        Self { filters: [[Biquad::new(30.0, true); 2]; 2] }
    }

    /// `t` runs 0..1 over the sweep.
    fn run(&mut self, block: &mut [f64], t: f32) {
        let to = 30.0 * 100f64.powf(t.clamp(0.0, 1.0) as f64);
        for channel in &mut self.filters {
            for f in channel.iter_mut() {
                f.retune(to, true);
            }
        }
        let fade = 1.0 - 0.35 * t as f64;
        for pair in block.chunks_exact_mut(2) {
            for (c, sample) in pair.iter_mut().enumerate() {
                let v = self.filters[c][0].run(*sample);
                *sample = self.filters[c][1].run(v) * fade;
            }
        }
    }
}

/// How a silent square stands with the one being heard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sync {
    /// Still finding its beat.
    #[default]
    Listening,
    /// Running in time with the leader.
    Locked,
    /// Too far apart in tempo to put in time.
    Apart,
}

enum Follow {
    Listening,
    Locked(Lock),
    Apart { retry_at: f64 },
}

impl Follow {
    fn sync(&self) -> Sync {
        match self {
            Self::Listening => Sync::Listening,
            Self::Locked(_) => Sync::Locked,
            Self::Apart { .. } => Sync::Apart,
        }
    }
}

enum Phase {
    Steady,
    /// A switch is coming, at step `at`.
    Waiting { to: usize, mv: Move, at: f64 },
    /// Going over, from step `start` for `length` steps.
    Moving { from: usize, to: usize, mv: Move, start: f64, length: f64 },
}

enum Request {
    Go { to: usize, slam: bool },
    Stutter,
}

/// One square as the interface draws it.
#[derive(Clone, Copy, Default)]
pub struct Meter {
    pub bpm: Option<f32>,
    /// 0..1 through the current beat, when the tempo is known.
    pub phase: Option<f32>,
    /// 0..1 through the current bar, when the bar is known.
    pub bar: Option<f32>,
    /// How much of it is being heard, 0..1.
    pub heard: f32,
    pub sync: Sync,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Next {
    pub to: usize,
    pub mv: Move,
    /// Beats until it lands, while waiting; then how far through it is.
    pub beats_left: f32,
    pub progress: Option<f32>,
}

#[derive(Clone, Default)]
struct Info {
    meters: [Meter; SQUARES],
    live: usize,
    next: Option<Next>,
    stuttering: bool,
    notes: Vec<String>,
    finished: Vec<usize>,
}

struct Shared {
    decks: [Arc<DeckShared>; SQUARES],
    requests: Mutex<Vec<Request>>,
    info: Mutex<Info>,
    quit: AtomicBool,
    wake: Arc<Wake>,
    hub: Arc<Hub>,
}

#[derive(Clone, Default)]
pub struct SquareView {
    pub track: Option<Track>,
    pub playing: bool,
    pub loading: bool,
    pub ended: bool,
    pub failed: bool,
    pub pos_ms: u32,
    pub meter: Meter,
}

#[derive(Clone, Default)]
pub struct View {
    pub squares: [SquareView; SQUARES],
    /// The square being heard.
    pub live: usize,
    pub next: Option<Next>,
    pub stuttering: bool,
    pub paused: bool,
}

impl View {
    /// The square the music is heading for: the one coming in, or the one heard.
    pub fn target(&self) -> usize {
        self.next.map_or(self.live, |n| n.to)
    }
}

/// The square found by moving from `from` in a direction on the two-by-two
/// grid (0 1 / 2 3). There are only two of anything, so either way across
/// is the other one.
pub fn neighbour(from: usize, across: bool) -> usize {
    if across { from ^ 1 } else { from ^ 2 }
}

/// Clockwise round the grid: 0 → 1 → 3 → 2.
pub fn clockwise(from: usize) -> usize {
    [1, 3, 0, 2][from % SQUARES]
}

pub struct Quad {
    shared: Arc<Shared>,
    players: [Arc<Player>; SQUARES],
    tracks: [Option<Track>; SQUARES],
    paused: bool,
}

impl Quad {
    pub fn start(session: Session, volume: Box<dyn VolumeGetter + Send>, hub: Arc<Hub>, bitrate: Bitrate, normalise: bool) -> Self {
        let shared = Arc::new(Shared {
            decks: Default::default(),
            requests: Mutex::default(),
            info: Mutex::default(),
            quit: AtomicBool::new(false),
            wake: Arc::new((Mutex::new(()), Condvar::new())),
            hub,
        });
        let players = std::array::from_fn(|d| deck_player(&session, bitrate, normalise, shared.decks[d].clone(), shared.wake.clone()));
        let mixer = shared.clone();
        std::thread::Builder::new().name("riff-quad".into()).spawn(move || mixer_loop(mixer, volume)).ok();
        Self { shared, players, tracks: Default::default(), paused: false }
    }

    /// After a reconnect the squares must fetch audio through the new session.
    pub fn set_session(&self, session: &Session) {
        for p in &self.players {
            p.set_session(session.clone());
        }
    }

    /// Put a song in a square, starting at `start_ms`. It plays at once,
    /// silently unless it is the square being heard.
    pub fn load(&mut self, square: usize, track: Track, start_ms: u32) -> Result<()> {
        let uri = SpotifyUri::from_uri(&track.uri).map_err(|_| anyhow!("Only songs can go in a square"))?;
        let deck = &self.shared.decks[square];
        deck.prepare(start_ms, track.duration_ms);
        deck.playing.store(!self.paused, Ordering::Relaxed);
        self.players[square].load(uri, !self.paused, start_ms);
        self.tracks[square] = Some(track);
        self.shared.wake.1.notify_all();
        Ok(())
    }

    fn request(&self, r: Request) {
        self.shared.requests.lock().unwrap().push(r);
        self.shared.wake.1.notify_all();
    }

    /// Head for square `to`. It comes in when the music says so, not now.
    pub fn go(&self, to: usize, slam: bool) {
        self.request(Request::Go { to: to % SQUARES, slam });
    }

    /// Stutter the song being heard for a beat, starting on the next one.
    pub fn stutter(&self) {
        self.request(Request::Stutter);
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Stop or start all four together, so they stay in time with each other.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        for d in 0..SQUARES {
            if self.tracks[d].is_none() {
                continue;
            }
            let deck = &self.shared.decks[d];
            if paused {
                deck.playing.store(false, Ordering::Relaxed);
                self.players[d].pause();
            } else if !deck.feed.lock().unwrap().ended {
                deck.playing.store(true, Ordering::Relaxed);
                self.players[d].play();
            }
        }
        self.shared.wake.1.notify_all();
    }

    /// Everything the interface needs to draw the squares, and notes for the listener.
    pub fn view(&mut self) -> (View, Vec<String>) {
        let (info, notes, finished) = {
            let mut info = self.shared.info.lock().unwrap();
            let notes = std::mem::take(&mut info.notes);
            let finished = std::mem::take(&mut info.finished);
            (info.clone(), notes, finished)
        };
        for d in finished {
            self.players[d].pause();
        }
        let mut view = View { live: info.live, next: info.next, stuttering: info.stuttering, paused: self.paused, ..Default::default() };
        for d in 0..SQUARES {
            let deck = &self.shared.decks[d];
            let (loading, ended, failed) = deck.status();
            view.squares[d] = SquareView {
                track: self.tracks[d].clone(),
                playing: deck.playing.load(Ordering::Relaxed),
                loading,
                ended,
                failed,
                pos_ms: deck.heard_ms(),
                meter: info.meters[d],
            };
        }
        (view, notes)
    }
}

impl Drop for Quad {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
        for p in &self.players {
            p.stop();
        }
        self.shared.wake.1.notify_all();
    }
}

/// Square names as the listener sees them, counted from one.
pub fn square_name(d: usize) -> usize {
    d + 1
}

// ---- the mixer --------------------------------------------------------------

struct Mixer {
    decks: [DeckMix; SQUARES],
    follow: [Follow; SQUARES],
    /// The generation of each deck's feed last seen: a new song means its
    /// place against the leader has to be found again.
    seen: [u64; SQUARES],
    live: usize,
    /// The step at which the square being heard took over.
    since: f64,
    phase: Phase,
    /// A square picked while a move was under way, to go to after it.
    queued: Option<(usize, bool)>,
    /// A stutter on a deck, from a step on.
    stutter: Option<(usize, f64, Stutter)>,
    stutter_asked: bool,
    sweep: Option<Sweep>,
    gains: [f32; SQUARES],
    /// Whether each deck's lows are let through.
    lows: [bool; SQUARES],
    notes: Vec<String>,
}

impl Mixer {
    fn new() -> Self {
        Self {
            decks: Default::default(),
            follow: std::array::from_fn(|_| Follow::Listening),
            seen: [0; SQUARES],
            live: 0,
            since: 0.0,
            phase: Phase::Steady,
            queued: None,
            stutter: None,
            stutter_asked: false,
            sweep: None,
            gains: [1.0, 0.0, 0.0, 0.0],
            lows: [true; SQUARES],
            notes: Vec::new(),
        }
    }

    /// The analysis clock: every deck's tempo tracker counts the same steps.
    fn now(&self) -> f64 {
        self.decks[self.live].tempo.now as f64
    }

    fn grid(&self) -> Option<Grid> {
        Grid::of(&self.decks[self.live].tempo)
    }

    fn go(&mut self, shared: &Shared, to: usize, slam: bool) {
        let playing = |d: usize| shared.decks[d].playing.load(Ordering::Relaxed);
        match self.phase {
            Phase::Moving { .. } => {
                self.queued = Some((to, slam));
                return;
            }
            Phase::Waiting { to: already, .. } if already == to => return,
            Phase::Waiting { .. } if to == self.live => {
                self.phase = Phase::Steady;
                self.sweep = None;
                self.notes.push(format!("Staying on square {}", square_name(to)));
                return;
            }
            Phase::Steady if to == self.live => return,
            _ => {}
        }
        if !playing(to) {
            self.notes.push(format!("Square {} has nothing playing", square_name(to)));
            return;
        }
        let now = self.now();
        let grid = self.grid();
        let stayed = match (grid, &self.phase) {
            // Changing its mind while a switch waits: that is flicking, so chop.
            (_, Phase::Waiting { .. }) => 0.0,
            (Some(g), _) => (now - self.since) / g.period,
            (None, _) => 99.0,
        };
        let matched = matches!(self.follow[to], Follow::Locked(_));
        let mv = choose(stayed, matched, slam);
        self.phase = Phase::Waiting { to, mv, at: when(now, grid, mv) };
    }

    /// Start a move that has come due, or finish one that is done.
    fn advance(&mut self, shared: &Shared) {
        let now = self.now();
        match self.phase {
            Phase::Waiting { to, mv, at } if now >= at => {
                let length = match (mv, self.grid()) {
                    (Move::Blend, Some(g)) => g.period * 4.0,
                    (Move::Blend, None) => HOPS_PER_SEC as f64 * 4.0,
                    // One block, so the gains glide over rather than click.
                    _ => (OUT / HOP) as f64,
                };
                self.phase = Phase::Moving { from: self.live, to, mv, start: at, length };
            }
            Phase::Moving { from, to, start, length, .. } if now >= start + length => {
                self.phase = Phase::Steady;
                self.sweep = None;
                self.take_over(shared, from, to);
            }
            _ => {}
        }
    }

    /// `to` is now the square being heard: the others follow it.
    fn take_over(&mut self, shared: &Shared, from: usize, to: usize) {
        let was_locked = matches!(self.follow[to], Follow::Locked(_));
        self.live = to;
        self.since = self.now();
        // A stutter belongs to the song it was cutting up.
        if self.stutter.as_ref().is_some_and(|(deck, ..)| *deck != to) {
            self.stutter = None;
        }
        self.follow[to] = Follow::Listening;
        for d in 0..SQUARES {
            if d == to {
                continue;
            }
            self.follow[d] = if was_locked && (d == from || matches!(self.follow[d], Follow::Locked(_))) {
                // Already on the same grid as the new leader, through the old
                // one: held there from the tempo each runs at now.
                Follow::Locked(Lock::new(d, load_f32(&shared.decks[d].rate), frames_at(&self.decks[d].tempo)))
            } else {
                Follow::Listening
            };
        }
    }

    /// Keep each silent square in time with the leader: match the ones that
    /// are ready, steer the ones that are matched.
    fn follow(&mut self, shared: &Shared) {
        let live = self.live;
        let now = self.now();
        // The square coming in: heard, or about to be.
        let coming = match self.phase {
            Phase::Moving { to, .. } | Phase::Waiting { to, .. } => Some(to),
            Phase::Steady => None,
        };
        for d in 0..SQUARES {
            if d == live || !shared.decks[d].playing.load(Ordering::Relaxed) {
                continue;
            }
            if self.decks[d].generation != self.seen[d] {
                self.seen[d] = self.decks[d].generation;
                self.follow[d] = Follow::Listening;
            }
            let rate = &shared.decks[d].rate;
            match &mut self.follow[d] {
                // While it is being brought in it is heard: leave it be.
                Follow::Locked(_) if coming == Some(d) && matches!(self.phase, Phase::Moving { .. }) => {}
                Follow::Locked(lock) => {
                    let (lead, follow) = pair(&mut self.decks, live, d);
                    lock.steer(&lead.tempo, &follow.tempo, rate, frames_at(&follow.tempo));
                }
                Follow::Apart { retry_at } if now >= *retry_at => self.follow[d] = Follow::Listening,
                Follow::Apart { .. } => {}
                // Matching may hold it back as much as a bar: never just
                // before it is to be heard.
                Follow::Listening if coming == Some(d) => {}
                Follow::Listening => {
                    let lead = &self.decks[live].tempo;
                    let follow = &self.decks[d].tempo;
                    let ready = follow.steady() && follow.heard() >= 6.0 && lead.steady();
                    // Twice a second is often enough to check.
                    if !ready || follow.now % (HOPS_PER_SEC as u64 / 2) >= (OUT / HOP) as u64 {
                        continue;
                    }
                    let (lead, follow) = pair(&mut self.decks, live, d);
                    self.follow[d] = match beatmatch(rate, &lead.tempo, follow, true) {
                        Ok(_) => Follow::Locked(Lock::new(d, load_f32(rate), frames_at(&follow.tempo))),
                        Err(e) => {
                            log::info!("square {}: {e}", square_name(d));
                            // Ten seconds on, the song may have moved somewhere that matches.
                            Follow::Apart { retry_at: now + HOPS_PER_SEC as f64 * 10.0 }
                        }
                    };
                }
            }
        }
    }

    /// How loud each square should be right now, and whether it has its lows.
    fn levels(&self) -> ([f32; SQUARES], [bool; SQUARES]) {
        let mut gains = [0.0; SQUARES];
        let mut lows = [true; SQUARES];
        match self.phase {
            Phase::Moving { from, to, mv, start, length } => {
                let p = ((self.now() - start) / length.max(1.0)) as f32;
                let ((out, out_low), (inn, in_low)) = blend_at(mv, p.clamp(0.0, 1.0));
                gains[from] = out;
                lows[from] = out_low;
                gains[to] = inn;
                lows[to] = in_low;
            }
            _ => gains[self.live] = 1.0,
        }
        (gains, lows)
    }
}

fn mixer_loop(shared: Arc<Shared>, volume: Box<dyn VolumeGetter + Send>) {
    let out = Output::open().map_err(|e| log::warn!("quad output: {e}")).ok();
    let mut monitor = Monitor::new();
    let mut m = Mixer::new();
    let mut started = false;
    let mut gain = volume.attenuation_factor();
    let mut scratch: [Vec<f64>; SQUARES] = std::array::from_fn(|_| vec![0.0f64; OUT * 2]);
    let mut mix = vec![0.0f64; OUT * 2];

    while !shared.quit.load(Ordering::Relaxed) {
        let playing: [bool; SQUARES] = std::array::from_fn(|d| shared.decks[d].playing.load(Ordering::Relaxed));

        // ---- the square being heard went quiet: carry on with another
        if !playing[m.live] && matches!(m.phase, Phase::Steady) {
            if let Some(d) = (1..SQUARES).map(|k| (m.live + k) % SQUARES).find(|d| playing[*d]) {
                let from = m.live;
                m.take_over(&shared, from, d);
                m.follow[from] = Follow::Listening;
            }
        }

        // ---- requests from the interface
        let requests: Vec<Request> = shared.requests.lock().unwrap().drain(..).collect();
        for request in requests {
            match request {
                Request::Go { to, slam } => m.go(&shared, to, slam),
                Request::Stutter => m.stutter_asked = true,
            }
        }

        // ---- near the end of the song being heard, move on by itself
        if matches!(m.phase, Phase::Steady) && playing[m.live] {
            let feed = shared.decks[m.live].feed.lock().unwrap();
            let left = feed.dur.saturating_sub(feed.pos) as f64 / RATE;
            drop(feed);
            if left < 14.0 && feed_known(&shared, m.live) {
                if let Some(d) = (1..SQUARES).map(|k| next_round(m.live, k)).find(|d| playing[*d]) {
                    m.notes.push(format!("Square {} is ending: over to square {}", square_name(m.live), square_name(d)));
                    m.since = f64::MIN / 4.0;
                    m.go(&shared, d, false);
                }
            }
        }

        m.advance(&shared);
        if let Some((to, slam)) = m.queued.take() {
            if matches!(m.phase, Phase::Steady) {
                m.go(&shared, to, slam);
            } else {
                m.queued = Some((to, slam));
            }
        }

        // ---- the leader's beat, for stutters and sweeps
        let now = m.now();
        let grid = m.grid();
        let beat_frames = grid.map_or(RATE as usize / 2, |g| (g.period * HOP as f64) as usize).max(OUT * 4);
        if std::mem::take(&mut m.stutter_asked) && m.stutter.is_none() {
            let at = grid.map_or(now, |g| next_on(now + 1.0, g.beat_at, g.period));
            m.stutter = Some((m.live, at, Stutter::new(beat_frames)));
        }
        if let Phase::Waiting { mv: Move::Roll, at, .. } = m.phase {
            // From a beat before the switch, or from now if that is later:
            // the roll always ends exactly as the new song drops.
            let begin = (at - beat_frames as f64 / HOP as f64).max(now);
            if now >= begin && m.stutter.is_none() {
                m.stutter = Some((m.live, begin, Stutter::new(((at - begin) * HOP as f64) as usize)));
            }
        }
        let sweeping = match m.phase {
            Phase::Waiting { mv: Move::Sweep, at, .. } => {
                let begin = at - beat_frames as f64 / HOP as f64;
                (now >= begin).then(|| ((now - begin) / (at - begin).max(1.0)) as f32)
            }
            _ => None,
        };
        if sweeping.is_some() && m.sweep.is_none() {
            m.sweep = Some(Sweep::new());
        }

        // ---- pull each square
        let mut any = false;
        let (targets, lows) = m.levels();
        for d in 0..SQUARES {
            let deck = &shared.decks[d];
            let block = &mut scratch[d];
            if !playing[d] {
                block.fill(0.0);
                m.decks[d].level *= 0.9;
                m.decks[d].was_playing = false;
                m.decks[d].tempo.idle(OUT);
                continue;
            }
            if !std::mem::replace(&mut m.decks[d].was_playing, true) {
                m.decks[d].tempo.reset();
                m.decks[d].eq.reset();
            }
            any = true;
            let rate = load_f32(&deck.rate) as f64;
            let mut feed = deck.feed.lock().unwrap();
            let real = pull(&mut feed, &mut m.decks[d], rate, block);
            let ran_out = real == 0 && feed.ended && feed.buf.len() < 4;
            drop(feed);
            if ran_out {
                deck.playing.store(false, Ordering::Relaxed);
                shared.info.lock().unwrap().finished.push(d);
            }
            m.decks[d].tempo.feed(block);
            m.lows[d] = lows[d];
            let steps = [if lows[d] { 0 } else { EQ_KILL }, 0, 0];
            m.decks[d].eq.run(block, steps);
            let peak = block.iter().fold(0.0f64, |p, v| p.max(v.abs())) as f32;
            m.decks[d].level = m.decks[d].level * 0.9 + peak * 0.1;
        }
        m.follow(&shared);

        // ---- effects on the square being heard
        let live = m.live;
        if let Some((deck, at, stutter)) = m.stutter.as_mut() {
            if now >= *at {
                stutter.run(&mut scratch[*deck]);
            }
        }
        if m.stutter.as_ref().is_some_and(|(_, _, s)| s.done()) {
            m.stutter = None;
        }
        if let (Some(t), Some(sweep)) = (sweeping, m.sweep.as_mut()) {
            sweep.run(&mut scratch[live], t);
        }

        if !any {
            if started {
                if let Some(out) = &out {
                    out.set_playing(false);
                }
                started = false;
                monitor.reset(&shared.hub);
            }
            publish(&shared, &mut m, &targets);
            let guard = shared.wake.0.lock().unwrap();
            let _ = shared.wake.1.wait_timeout(guard, Duration::from_millis(100));
            continue;
        }
        if !started {
            if let Some(out) = &out {
                out.set_playing(true);
            }
            started = true;
        }

        // ---- sum, gliding each square's level across the block
        mix.fill(0.0);
        for d in 0..SQUARES {
            let (from, to) = (m.gains[d], targets[d]);
            if from == 0.0 && to == 0.0 {
                continue;
            }
            for i in 0..OUT {
                let g = (from + (to - from) * (i as f32 / OUT as f32)) as f64;
                mix[2 * i] += scratch[d][2 * i] * g;
                mix[2 * i + 1] += scratch[d][2 * i + 1] * g;
            }
        }
        m.gains = targets;
        monitor.feed(&mix, &shared.hub, out.as_ref());
        let target = volume.attenuation_factor();
        let step = (target - gain) / OUT as f64;
        for pair in mix.chunks_exact_mut(2) {
            gain += step;
            for s in pair {
                let v = *s * gain;
                *s = if v.abs() > 0.9 { v.signum() * (0.9 + 0.1 * ((v.abs() - 0.9) / 0.1).tanh()) } else { v };
            }
        }
        gain = target;
        publish(&shared, &mut m, &targets);
        match &out {
            Some(out) => out.write(&mix),
            None => std::thread::sleep(Duration::from_secs_f64(OUT as f64 / RATE)),
        }
    }
    monitor.reset(&shared.hub);
}

/// The mixer's clock in frames, which is what a lock keeps time by,
/// from a tempo tracker's count of analysis steps.
fn frames_at(t: &Tempo) -> u64 {
    t.now * HOP as u64
}

/// Whether a deck's length is known yet (it is once its song has loaded).
fn feed_known(shared: &Shared, d: usize) -> bool {
    let feed = shared.decks[d].feed.lock().unwrap();
    feed.dur > 0 && !feed.loading
}

/// `k` squares clockwise round from `from`.
fn next_round(from: usize, k: usize) -> usize {
    (0..k).fold(from, |d, _| clockwise(d))
}

fn publish(shared: &Shared, m: &mut Mixer, gains: &[f32; SQUARES]) {
    let mut info = shared.info.lock().unwrap();
    info.live = m.live;
    info.stuttering = m.stutter.is_some();
    info.notes.append(&mut m.notes);
    for d in 0..SQUARES {
        let t = &m.decks[d].tempo;
        info.meters[d] = Meter {
            bpm: t.bpm,
            phase: t.phase(),
            bar: t.bar_phase(),
            heard: gains[d],
            sync: if d == m.live { Sync::Locked } else { m.follow[d].sync() },
        };
    }
    let now = m.now();
    let period = m.grid().map_or(HOPS_PER_SEC as f64 / 2.0, |g| g.period);
    info.next = match m.phase {
        Phase::Steady => None,
        Phase::Waiting { to, mv, at } => Some(Next { to, mv, beats_left: ((at - now) / period).max(0.0) as f32, progress: None }),
        Phase::Moving { to, mv, start, length, .. } => {
            Some(Next { to, mv, beats_left: 0.0, progress: Some((((now - start) / length.max(1.0)) as f32).clamp(0.0, 1.0)) })
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(period: f64, beat_at: f64, bar_at: Option<f64>) -> Option<Grid> {
        Some(Grid { period, beat_at, bar_at })
    }

    #[test]
    fn a_switch_waits_for_the_music() {
        // 120 BPM is a beat every 172.3 steps; beats at 1000 + k·period, bars from 1000.
        let p = HOPS_PER_SEC as f64 / 2.0;
        let g = grid(p, 1000.0 + p, Some(1000.0));
        // Asked for just after a bar began.
        let now = 1010.0;
        // A cut goes on the very next beat.
        assert!((when(now, g, Move::Cut) - (1000.0 + p)).abs() < 1e-6);
        // A blend waits for the next bar.
        assert!((when(now, g, Move::Blend) - (1000.0 + 4.0 * p)).abs() < 1e-6);
        // Asked for a moment before a bar, a roll has no beat to roll in, so it takes the bar after.
        let late = 1000.0 + 4.0 * p - 20.0;
        assert!((when(late, g, Move::Roll) - (1000.0 + 8.0 * p)).abs() < 1e-6);
        // But a cut still makes the next beat if there is time.
        assert!((when(1000.0 + p - 60.0, g, Move::Cut) - (1000.0 + p)).abs() < 1e-6);
        // Never in the past, and never too soon to be heard as meant.
        for now in [900.0, 1000.0, 1172.0, 5000.0] {
            for mv in [Move::Cut, Move::Blend, Move::Roll, Move::Sweep] {
                assert!(when(now, g, mv) >= now + HOPS_PER_SEC as f64 * 0.12 - 1e-6);
            }
        }
        // With no beat to go by, it simply happens quickly.
        let soon = when(500.0, None, Move::Blend);
        assert!(soon > 500.0 && soon < 500.0 + HOPS_PER_SEC as f64 * 0.2);
    }

    #[test]
    fn flicking_chops_and_staying_blends() {
        assert_eq!(choose(1.0, true, false), Move::Cut);
        assert_eq!(choose(8.0, true, false), Move::Roll);
        assert_eq!(choose(40.0, true, false), Move::Blend);
        assert_eq!(choose(40.0, true, true), Move::Roll);
        assert_eq!(choose(40.0, false, false), Move::Sweep);
    }

    #[test]
    fn a_blend_swaps_the_bass_half_way_and_never_dips() {
        for i in 0..=100 {
            let p = i as f32 / 100.0;
            let ((out, out_low), (inn, in_low)) = blend_at(Move::Blend, p);
            // Exactly one bass line at a time.
            assert!(out_low != in_low, "at {p}");
            // The sum of power never sags: no hole in the middle.
            assert!(out * out + inn * inn >= 0.99, "at {p}: {out} {inn}");
        }
        assert_eq!(blend_at(Move::Blend, 0.0), ((1.0, true), (0.0, false)));
        assert_eq!(blend_at(Move::Blend, 1.0), ((0.0, false), (1.0, true)));
        assert_eq!(blend_at(Move::Cut, 1.0), ((0.0, true), (1.0, true)));
    }

    #[test]
    fn a_stutter_repeats_the_start_of_the_beat() {
        let beat = 4096;
        let mut s = Stutter::new(beat);
        // A ramp, so where each sample came from can be read off it.
        let source: Vec<f64> = (0..beat).flat_map(|i| [i as f64, i as f64]).collect();
        let mut heard = Vec::new();
        for block in source.chunks(OUT * 2) {
            let mut block = block.to_vec();
            s.run(&mut block);
            heard.extend(block);
        }
        assert!(s.done());
        let slice = beat / 4;
        // The first slice passes through; after that it comes round again.
        assert_eq!(heard[2 * 100], 100.0);
        assert_eq!(heard[2 * (slice + 100)], 100.0);
        // Half-way, the slices halve.
        let half = beat / 2 + 100;
        assert_eq!(heard[2 * half], (half % (slice / 2)) as f64);
        // Nothing from beyond the slice is ever played again.
        assert!(heard[2 * slice..].iter().all(|v| *v < slice as f64));
    }

    fn shared() -> Shared {
        let shared = Shared {
            decks: Default::default(),
            requests: Mutex::default(),
            info: Mutex::default(),
            quit: AtomicBool::new(false),
            wake: Arc::new((Mutex::new(()), Condvar::new())),
            hub: Arc::new(Hub::default()),
        };
        for d in &shared.decks {
            d.playing.store(true, Ordering::Relaxed);
        }
        shared
    }

    /// Every deck's clock at `step`, the leader playing at 120 BPM with bars from step 0.
    fn at(m: &mut Mixer, step: u64) {
        let period = HOPS_PER_SEC / 2.0;
        for d in &mut m.decks {
            d.tempo.now = step;
        }
        let t = &mut m.decks[m.live].tempo;
        t.bpm = Some(120.0);
        t.period = period;
        t.beat_at = 0.0;
        t.bar_at = Some(0.0);
    }

    #[test]
    fn a_picked_square_comes_in_on_the_bar_and_takes_over() {
        let shared = shared();
        let mut m = Mixer::new();
        let bar = (HOPS_PER_SEC * 2.0) as f64;
        // A long while on square 1, with square 2 in time with it.
        at(&mut m, (bar * 10.3) as u64);
        m.follow[1] = Follow::Locked(Lock::new(1, 1.0, 0));
        m.go(&shared, 1, false);
        let Phase::Waiting { to: 1, mv: Move::Blend, at: when } = m.phase else { panic!("not waiting for a blend") };
        assert!((when - bar * 11.0).abs() < 1.0, "lands at {when}, not on the bar");
        // Nothing happens before then.
        at(&mut m, (bar * 10.9) as u64);
        m.advance(&shared);
        assert!(matches!(m.phase, Phase::Waiting { .. }));
        assert_eq!(m.levels().0, [1.0, 0.0, 0.0, 0.0]);
        // On the bar it starts, and half-way both are up with one bass line between them.
        at(&mut m, (bar * 11.0) as u64 + 1);
        m.advance(&shared);
        assert!(matches!(m.phase, Phase::Moving { from: 0, to: 1, .. }));
        at(&mut m, (bar * 11.5) as u64);
        let (gains, lows) = m.levels();
        assert!(gains[0] > 0.9 && gains[1] > 0.9, "{gains:?}");
        assert!(lows[0] != lows[1]);
        // A bar later square 2 is the one heard, and square 1 follows it.
        at(&mut m, (bar * 12.0) as u64 + 2);
        m.advance(&shared);
        assert!(matches!(m.phase, Phase::Steady));
        assert_eq!(m.live, 1);
        assert_eq!(m.levels().0, [0.0, 1.0, 0.0, 0.0]);
        assert!(matches!(m.follow[0], Follow::Locked(_)));
    }

    #[test]
    fn flicking_cuts_on_the_beat_and_changing_your_mind_retargets() {
        let shared = shared();
        let mut m = Mixer::new();
        let beat = (HOPS_PER_SEC / 2.0) as f64;
        at(&mut m, 10);
        m.since = 0.0;
        for d in 1..SQUARES {
            m.follow[d] = Follow::Locked(Lock::new(d, 1.0, 0));
        }
        m.go(&shared, 2, false);
        let Phase::Waiting { mv: Move::Cut, at: first, .. } = m.phase else { panic!("not a cut") };
        assert!((first - beat).abs() < 1.0);
        // A different square before it lands: that one instead, still a cut.
        m.go(&shared, 3, false);
        assert!(matches!(m.phase, Phase::Waiting { to: 3, mv: Move::Cut, .. }));
        // Back to the one playing: stay.
        m.go(&shared, 0, false);
        assert!(matches!(m.phase, Phase::Steady));
        // A square with nothing in it is not gone to.
        shared.decks[2].playing.store(false, Ordering::Relaxed);
        m.go(&shared, 2, false);
        assert!(matches!(m.phase, Phase::Steady));
        // One not yet in time is swept to, whatever was asked.
        m.follow[1] = Follow::Listening;
        m.go(&shared, 1, false);
        assert!(matches!(m.phase, Phase::Waiting { to: 1, mv: Move::Sweep, .. }));
    }

    #[test]
    fn the_grid_round() {
        assert_eq!((neighbour(0, true), neighbour(0, false)), (1, 2));
        assert_eq!((neighbour(3, true), neighbour(3, false)), (2, 1));
        let mut d = 0;
        let mut seen = vec![];
        for _ in 0..4 {
            seen.push(d);
            d = clockwise(d);
        }
        assert_eq!(seen, [0, 1, 3, 2]);
        assert_eq!(next_round(1, 2), 2);
    }
}
