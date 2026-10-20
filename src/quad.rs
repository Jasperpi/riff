//! Four songs at once, one of them heard: the four-square mode.
//!
//! All four songs play the whole time. The one being heard leads; the other
//! three run silently underneath it, each bent to its tempo and slid so its
//! beats, and where they can be found its bars, fall on the leader's. So
//! whichever square is picked next is already in time, and the switch can
//! land on the one.
//!
//! Picking a square is heard at once but lands on the beat: the new song's
//! top end creeps in, or the old song lifts, and on the next beat (the next
//! half-bar for a blend) the new one takes over. How it goes over suits how
//! it was asked for: a blend with the bass lines swapping on the beat, a
//! straight cut for quick flicking, a stutter into a drop on shift, or, for
//! a song whose tempo can't be matched, a sweep that clears the floor for it.

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
/// Audio kept ready in front of the sound card, in milliseconds.
const OUTPUT_MS: u32 = 90;

/// How one square takes over from another. Every move starts to be heard
/// the moment it is asked for and lands on the beat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    /// The new song's top end creeps in straight away under the old one;
    /// on the next half-bar its bass takes over and the old song bows out
    /// over a beat.
    Blend,
    /// The old song's last beat stutters, faster and faster, and the new one
    /// drops on the beat after.
    Roll,
    /// The old song lifts (its lows thin out) and the new one cuts in on
    /// the next beat.
    Cut,
    /// The old song is swept up and away and the new one drops in on the
    /// next beat. For a song that couldn't be put in time with the other.
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
}

/// The move that suits a switch. Flicking between squares chops on the
/// beat; settling on one blends; `slam` (shift) asks for the stutter.
pub fn choose(stayed_beats: f64, matched: bool, slam: bool) -> Move {
    if !matched {
        Move::Sweep
    } else if slam {
        Move::Roll
    } else if stayed_beats < 4.0 {
        Move::Cut
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

/// When a switch asked for at step `now` should land, no sooner than
/// `ready` (the new song may still be held back to line up its bar). A
/// blend lands on the next half-bar, the one or the three; a stutter on the
/// beat after a whole beat of it; anything else on the next beat. Without a
/// beat to go by, in a tenth of a second.
pub fn when(now: f64, ready: f64, grid: Option<Grid>, mv: Move) -> f64 {
    let soon = (now + HOPS_PER_SEC as f64 * 0.1).max(ready);
    let Some(g) = grid.filter(|g| g.period > 1.0) else { return soon };
    // A moment's notice, so a switch never lands mid-block and a blend has
    // a little time for the new song to be heard coming.
    let notice = HOPS_PER_SEC as f64 * if mv == Move::Blend { 0.25 } else { 0.12 };
    let earliest = (now + notice).max(ready);
    match mv {
        Move::Blend => next_on(earliest, g.bar_at.unwrap_or(g.beat_at), g.period * 2.0),
        Move::Roll => next_on(earliest + g.period, g.beat_at, g.period),
        Move::Cut | Move::Sweep => next_on(earliest, g.beat_at, g.period),
    }
}

/// How far into a switch: from the press to the landing, then from the
/// landing to done.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Stage {
    Coming(f32),
    Landing(f32),
}

/// One song in a switch: how loud, and its EQ (low, mid, high, in steps from flat).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Side {
    pub gain: f32,
    pub eq: [i8; 3],
}

const FULL: Side = Side { gain: 1.0, eq: [0, 0, 0] };
const SILENT: Side = Side { gain: 0.0, eq: [0, 0, 0] };

/// What the old and the new song sound like at a stage of a switch, and how
/// far the old one is swept up (0 not at all, 1 all the way).
pub fn shape(mv: Move, stage: Stage) -> (Side, Side, f32) {
    use std::f32::consts::FRAC_PI_2;
    let smooth = |t: f32| {
        let t = t.clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    match (mv, stage) {
        // The new song's highs, and some of its mids, come up under the old one.
        (Move::Blend, Stage::Coming(c)) => (FULL, Side { gain: 0.5 * smooth(c), eq: [EQ_KILL, -2, 0] }, 0.0),
        // On the beat the bass lines swap; the old song is gone a beat later.
        (Move::Blend, Stage::Landing(p)) => {
            let out = if p >= 1.0 { 0.0 } else { (p.clamp(0.0, 1.0) * FRAC_PI_2).cos() };
            (Side { gain: out, eq: [EQ_KILL, 0, 0] }, Side { gain: 0.5 + 0.5 * smooth(p * 2.0), eq: [0, 0, 0] }, 0.0)
        }
        (Move::Cut, Stage::Coming(c)) => (FULL, SILENT, 0.6 * c),
        (Move::Sweep, Stage::Coming(c)) => (FULL, SILENT, c),
        (Move::Roll, Stage::Coming(_)) => (FULL, SILENT, 0.0),
        // The old song stays lifted while it fades: letting its bass back in
        // at that moment would be heard as a click.
        (Move::Cut, Stage::Landing(_)) => (SILENT, FULL, 0.6),
        (Move::Sweep, Stage::Landing(_)) => (SILENT, FULL, 1.0),
        (_, Stage::Landing(_)) => (SILENT, FULL, 0.0),
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

    /// `t` runs 0..1 over the sweep: from 30 Hz up to 360, taking the bass
    /// and the kick away but leaving the voice and the tune.
    fn run(&mut self, block: &mut [f64], t: f32) {
        let to = 30.0 * 12f64.powf(t.clamp(0.0, 1.0) as f64);
        for channel in &mut self.filters {
            for f in channel.iter_mut() {
                f.retune(to, true);
            }
        }
        let fade = 1.0 - 0.15 * t as f64;
        for pair in block.chunks_exact_mut(2) {
            for (c, sample) in pair.iter_mut().enumerate() {
                let v = self.filters[c][0].run(*sample);
                *sample = self.filters[c][1].run(v) * fade;
            }
        }
    }
}

/// The old song's last moments, repeating and dying away under the new
/// one: what a DJ does when two songs won't go together any other way.
/// Repeats every half beat of the old song, thinned of its lows so the tail
/// never muddies the new song's bass.
struct Echo {
    line: Vec<f64>,
    at: usize,
    low: [f64; 2],
    /// Frames since anything was last sent in.
    quiet: usize,
}

impl Echo {
    const FEEDBACK: f64 = 0.5;
    const WET: f64 = 0.55;

    fn new(delay_frames: usize) -> Self {
        Self { line: vec![0.0; delay_frames.max(OUT) * 2], at: 0, low: [0.0; 2], quiet: 0 }
    }

    /// Add the echo of what has been sent so far to `mix`, sending in `send`
    /// (the old song, or nothing once it has gone).
    fn run(&mut self, send: Option<&[f64]>, mix: &mut [f64]) {
        self.quiet = if send.is_some() { 0 } else { self.quiet + mix.len() / 2 };
        for i in 0..mix.len() / 2 {
            for c in 0..2 {
                let slot = 2 * self.at + c;
                let out = self.line[slot];
                let into = send.map_or(0.0, |s| s[2 * i + c]);
                // Thin each repeat's lows a little more.
                self.low[c] += 0.03 * (out - self.low[c]);
                self.line[slot] = into + (out - self.low[c]) * Self::FEEDBACK;
                mix[2 * i + c] += out * Self::WET;
            }
            self.at = (self.at + 1) % (self.line.len() / 2);
        }
    }

    /// Died away: a few seconds with nothing new.
    fn done(&self) -> bool {
        self.quiet > (RATE * 3.0) as usize
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
    /// A switch asked for at step `asked`, to land at step `at`.
    Waiting { to: usize, mv: Move, asked: f64, at: f64 },
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
    echo: Option<Echo>,
    gains: [f32; SQUARES],
    /// Each deck's EQ as it is now. It moves a step a block towards where a
    /// switch wants it, so a bass swap takes 35 ms rather than thumping.
    eq: [[i8; 3]; SQUARES],
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
            echo: None,
            gains: [1.0, 0.0, 0.0, 0.0],
            eq: [[0; 3]; SQUARES],
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
        // A square still being held back to line up its bar would land in silence.
        let ready = now + (self.decks[to].hold / HOP) as f64;
        self.phase = Phase::Waiting { to, mv, asked: now, at: when(now, ready, grid, mv) };
    }

    /// Start a move that has come due, or finish one that is done.
    fn advance(&mut self, shared: &Shared) {
        let now = self.now();
        match self.phase {
            Phase::Waiting { to, mv, at, .. } if now >= at => {
                let length = match (mv, self.grid()) {
                    // The old song bows out over a beat.
                    (Move::Blend, Some(g)) => g.period,
                    (Move::Blend, None) => HOPS_PER_SEC as f64 * 0.5,
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
        // A stutter belongs to the song it was cutting up; once that song is
        // silent, it has done its job.
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

    /// How loud each square should be right now, where its EQ should be,
    /// and how far the one being left is swept up.
    fn levels(&self) -> ([f32; SQUARES], [[i8; 3]; SQUARES], f32) {
        let mut gains = [0.0; SQUARES];
        let mut eq = [[0; 3]; SQUARES];
        let (from, to, (out, inn, sweep)) = match self.phase {
            Phase::Steady => {
                gains[self.live] = 1.0;
                return (gains, eq, 0.0);
            }
            Phase::Waiting { to, mv, asked, at } => {
                let c = ((self.now() - asked) / (at - asked).max(1.0)) as f32;
                (self.live, to, shape(mv, Stage::Coming(c.clamp(0.0, 1.0))))
            }
            Phase::Moving { from, to, mv, start, length } => {
                let p = ((self.now() - start) / length.max(1.0)) as f32;
                (from, to, shape(mv, Stage::Landing(p.clamp(0.0, 1.0))))
            }
        };
        (gains[from], eq[from]) = (out.gain, out.eq);
        (gains[to], eq[to]) = (inn.gain, inn.eq);
        (gains, eq, sweep)
    }

    /// Mix one block of `OUT` frames into `mix`: take requests, move any
    /// switch along, pull each square, keep the silent ones in time, apply
    /// the effects and sum. Returns each square's level, and whether any
    /// square is playing at all.
    fn run_block(&mut self, shared: &Shared, scratch: &mut [Vec<f64>; SQUARES], mix: &mut [f64]) -> ([f32; SQUARES], bool) {
        let playing: [bool; SQUARES] = std::array::from_fn(|d| shared.decks[d].playing.load(Ordering::Relaxed));

        // ---- the square being heard went quiet: carry on with another
        if !playing[self.live] && matches!(self.phase, Phase::Steady) {
            if let Some(d) = (1..SQUARES).map(|k| (self.live + k) % SQUARES).find(|d| playing[*d]) {
                let from = self.live;
                self.take_over(shared, from, d);
                self.follow[from] = Follow::Listening;
            }
        }

        // ---- requests from the interface
        let requests: Vec<Request> = shared.requests.lock().unwrap().drain(..).collect();
        for request in requests {
            match request {
                Request::Go { to, slam } => self.go(shared, to, slam),
                Request::Stutter => self.stutter_asked = true,
            }
        }

        // ---- near the end of the song being heard, move on by itself
        if matches!(self.phase, Phase::Steady) && playing[self.live] {
            let feed = shared.decks[self.live].feed.lock().unwrap();
            let left = feed.dur.saturating_sub(feed.pos) as f64 / RATE;
            drop(feed);
            if left < 14.0 && feed_known(shared, self.live) {
                if let Some(d) = (1..SQUARES).map(|k| next_round(self.live, k)).find(|d| playing[*d]) {
                    self.notes.push(format!("Square {} is ending: over to square {}", square_name(self.live), square_name(d)));
                    self.since = f64::MIN / 4.0;
                    self.go(shared, d, false);
                }
            }
        }

        self.advance(shared);
        if let Some((to, slam)) = self.queued.take() {
            if matches!(self.phase, Phase::Steady) {
                self.go(shared, to, slam);
            } else {
                self.queued = Some((to, slam));
            }
        }

        // ---- the leader's beat, for stutters and sweeps
        let now = self.now();
        let grid = self.grid();
        let beat_frames = grid.map_or(RATE as usize / 2, |g| (g.period * HOP as f64) as usize).max(OUT * 4);
        if std::mem::take(&mut self.stutter_asked) && self.stutter.is_none() {
            let at = grid.map_or(now, |g| next_on(now + 1.0, g.beat_at, g.period));
            self.stutter = Some((self.live, at, Stutter::new(beat_frames)));
        }
        if let Phase::Waiting { mv: Move::Roll, at, .. } = self.phase {
            // A beat before the switch, which is on a beat itself (or from
            // now, with no beat to go by): it ends exactly as the new song drops.
            let begin = (at - beat_frames as f64 / HOP as f64).max(now);
            if now >= begin && self.stutter.is_none() {
                // And carries on a moment past it, while the old song fades:
                // going back to the song itself just then would click.
                let length = ((at - begin) * HOP as f64) as usize + OUT * 2;
                self.stutter = Some((self.live, begin, Stutter::new(length)));
            }
        }
        // ---- pull each square
        let mut any = false;
        let (targets, eq, sweeping) = self.levels();
        if sweeping > 0.0 && self.sweep.is_none() {
            self.sweep = Some(Sweep::new());
        }
        for d in 0..SQUARES {
            let deck = &shared.decks[d];
            let block = &mut scratch[d];
            if !playing[d] {
                block.fill(0.0);
                self.decks[d].level *= 0.9;
                self.decks[d].was_playing = false;
                self.decks[d].tempo.idle(OUT);
                continue;
            }
            if !std::mem::replace(&mut self.decks[d].was_playing, true) {
                self.decks[d].tempo.reset();
                self.decks[d].eq.reset();
            }
            any = true;
            let rate = load_f32(&deck.rate) as f64;
            let mut feed = deck.feed.lock().unwrap();
            let real = pull(&mut feed, &mut self.decks[d], rate, block);
            let ran_out = real == 0 && feed.ended && feed.buf.len() < 4;
            drop(feed);
            if ran_out {
                deck.playing.store(false, Ordering::Relaxed);
                shared.info.lock().unwrap().finished.push(d);
            }
            self.decks[d].tempo.on_request = true;
            self.decks[d].tempo.feed(block);
            for band in 0..3 {
                let (now, want) = (self.eq[d][band], eq[d][band]);
                self.eq[d][band] = now + (want - now).signum();
            }
            let steps = self.eq[d];
            self.decks[d].eq.run(block, steps);
            let peak = block.iter().fold(0.0f64, |p, v| p.max(v.abs())) as f32;
            self.decks[d].level = self.decks[d].level * 0.9 + peak * 0.1;
        }
        // One tempo estimate a block at most, the most overdue first: four
        // at once would take several blocks' worth of time in one.
        if let Some(t) = self.decks.iter_mut().map(|d| &mut d.tempo).filter(|t| t.due()).max_by_key(|t| t.waited()) {
            t.refresh();
        }
        self.follow(shared);

        // ---- effects on the square being heard
        let live = self.live;
        if let Some((deck, at, stutter)) = self.stutter.as_mut() {
            if now >= *at {
                stutter.run(&mut scratch[*deck]);
            }
        }
        if self.stutter.as_ref().is_some_and(|(_, _, s)| s.done()) {
            self.stutter = None;
        }
        if let (true, Some(sweep)) = (sweeping > 0.0, self.sweep.as_mut()) {
            sweep.run(&mut scratch[live], sweeping);
        }

        // ---- sum, gliding each square's level across the block
        mix.fill(0.0);
        for d in 0..SQUARES {
            let (from, to) = (self.gains[d], targets[d]);
            if from == 0.0 && to == 0.0 {
                continue;
            }
            for i in 0..OUT {
                let g = (from + (to - from) * (i as f32 / OUT as f32)) as f64;
                mix[2 * i] += scratch[d][2 * i] * g;
                mix[2 * i + 1] += scratch[d][2 * i + 1] * g;
            }
        }
        self.gains = targets;

        // ---- an echo of the old song, for moves that cut rather than blend:
        // its last half-beat (a whole beat for a sweep) is sent in, and the
        // repeats die away under the new one.
        let echoing = match self.phase {
            Phase::Waiting { mv: mv @ (Move::Cut | Move::Sweep | Move::Roll), at, .. } => {
                let send = if mv == Move::Sweep { 1.0 } else { 0.5 };
                (now >= at - send * beat_frames as f64 / HOP as f64).then_some(self.live)
            }
            _ => None,
        };
        if echoing.is_some() && self.echo.is_none() {
            self.echo = Some(Echo::new(beat_frames / 2));
        }
        if let Some(echo) = self.echo.as_mut() {
            echo.run(echoing.map(|d| scratch[d].as_slice()), mix);
            if echo.done() {
                self.echo = None;
            }
        }
        (targets, any)
    }
}

fn mixer_loop(shared: Arc<Shared>, volume: Box<dyn VolumeGetter + Send>) {
    // Switches are timed to the music rather than to the key, so the squares
    // can afford a deeper queue than the decks: one slow block (a tempo
    // estimate, a busy machine) is then absorbed instead of heard as a gap.
    let out = Output::open_with_room(OUTPUT_MS).map_err(|e| log::warn!("quad output: {e}")).ok();
    let mut monitor = Monitor::new();
    let mut m = Mixer::new();
    let mut started = false;
    let mut gain = volume.attenuation_factor();
    let mut scratch: [Vec<f64>; SQUARES] = std::array::from_fn(|_| vec![0.0f64; OUT * 2]);
    let mut mix = vec![0.0f64; OUT * 2];

    while !shared.quit.load(Ordering::Relaxed) {
        let (targets, any) = m.run_block(&shared, &mut scratch, &mut mix);
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
        Phase::Waiting { to, mv, at, .. } => Some(Next { to, mv, beats_left: ((at - now) / period).max(0.0) as f32, progress: None }),
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
    fn a_switch_lands_on_the_beat() {
        // 120 BPM is a beat every 172.3 steps; beats at 1000 + k·period, bars from 1000.
        let p = HOPS_PER_SEC as f64 / 2.0;
        let g = grid(p, 1000.0 + p, Some(1000.0));
        let now = 1010.0;
        // A cut goes on the very next beat.
        assert!((when(now, now, g, Move::Cut) - (1000.0 + p)).abs() < 1e-6);
        // A blend on the next half-bar: the one or the three, never a whole bar away.
        assert!((when(now, now, g, Move::Blend) - (1000.0 + 2.0 * p)).abs() < 1e-6);
        assert!((when(1000.0 + 2.5 * p, 1000.0 + 2.5 * p, g, Move::Blend) - (1000.0 + 4.0 * p)).abs() < 1e-6);
        // A stutter gets a whole beat, starting on a beat.
        let roll = when(now, now, g, Move::Roll);
        assert!((roll - (1000.0 + 2.0 * p)).abs() < 1e-6);
        assert!(roll - p >= now);
        // A cut still makes the next beat if there is time.
        assert!((when(1000.0 + p - 60.0, 1000.0 + p - 60.0, g, Move::Cut) - (1000.0 + p)).abs() < 1e-6);
        // Never a long wait: at most two beats for anything.
        for now in [900.0, 1000.0, 1172.0, 1300.0, 5000.0] {
            for mv in [Move::Cut, Move::Blend, Move::Roll, Move::Sweep] {
                let at = when(now, now, g, mv);
                assert!(at >= now + HOPS_PER_SEC as f64 * 0.12 - 1e-6);
                assert!(at - now <= 2.0 * p + HOPS_PER_SEC as f64 * 0.25 + 1e-6, "{mv:?} from {now} waits {}", at - now);
            }
        }
        // A square still held back to line up its bar is waited for.
        assert!(when(now, now + 3.0 * p, g, Move::Cut) >= now + 3.0 * p);
        // With no beat to go by, it simply happens quickly.
        let soon = when(500.0, 500.0, None, Move::Blend);
        assert!(soon > 500.0 && soon < 500.0 + HOPS_PER_SEC as f64 * 0.2);
    }

    #[test]
    fn flicking_chops_staying_blends_and_shift_stutters() {
        assert_eq!(choose(1.0, true, false), Move::Cut);
        assert_eq!(choose(8.0, true, false), Move::Blend);
        assert_eq!(choose(40.0, true, false), Move::Blend);
        assert_eq!(choose(40.0, true, true), Move::Roll);
        assert_eq!(choose(40.0, false, true), Move::Sweep);
    }

    #[test]
    fn a_switch_is_heard_the_moment_it_is_asked_for() {
        // Just after the press, something has already changed.
        let just = Stage::Coming(0.1);
        let (_, inn, _) = shape(Move::Blend, just);
        assert!(inn.gain > 0.0 && inn.eq[0] == EQ_KILL, "the new song's top end should be creeping in: {inn:?}");
        for mv in [Move::Cut, Move::Sweep] {
            assert!(shape(mv, just).2 > 0.0, "{mv:?}: the old song should be lifting");
        }
    }

    #[test]
    fn a_blend_swaps_the_bass_on_the_beat_and_is_short() {
        // Before the landing only the old song has bass; from it, only the new one.
        for i in 0..=10 {
            let (out, inn, _) = shape(Move::Blend, Stage::Coming(i as f32 / 10.0));
            assert_eq!((out.eq[0], inn.eq[0]), (0, EQ_KILL));
            assert_eq!(out.gain, 1.0);
            let (out, inn, _) = shape(Move::Blend, Stage::Landing(i as f32 / 10.0));
            assert_eq!((out.eq[0], inn.eq[0]), (EQ_KILL, 0));
            // No hole in the middle.
            assert!(out.gain * out.gain + inn.gain * inn.gain >= 0.99, "at {i}");
            assert!(inn.gain >= 0.5);
        }
        // Done means the old song is silent.
        let (out, inn, _) = shape(Move::Blend, Stage::Landing(1.0));
        assert_eq!((out.gain, inn.gain), (0.0, 1.0));
        // Anything else is straight over on landing.
        assert_eq!(shape(Move::Roll, Stage::Landing(0.0)), (SILENT, FULL, 0.0));
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
        let Phase::Waiting { to: 1, mv: Move::Blend, at: when, .. } = m.phase else { panic!("not waiting for a blend") };
        assert!((when - bar * 10.5).abs() < 1.0, "lands at {when}, not on the half-bar");
        // Before then the new song is creeping in, without its bass.
        at(&mut m, (bar * 10.45) as u64);
        m.advance(&shared);
        assert!(matches!(m.phase, Phase::Waiting { .. }));
        let (gains, eq, _) = m.levels();
        assert_eq!(gains[0], 1.0);
        assert!(gains[1] > 0.3 && eq[1][0] == EQ_KILL, "{gains:?} {eq:?}");
        // On the half-bar the bass lines swap.
        at(&mut m, (bar * 10.5) as u64 + 1);
        m.advance(&shared);
        assert!(matches!(m.phase, Phase::Moving { from: 0, to: 1, .. }));
        let (gains, eq, _) = m.levels();
        assert!(gains[0] > 0.9 && gains[1] >= 0.5, "{gains:?}");
        assert_eq!((eq[0][0], eq[1][0]), (EQ_KILL, 0));
        // A beat later square 2 is the one heard, and square 1 follows it.
        at(&mut m, (bar * 10.75) as u64 + 2);
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
        // A moment later the old song is already lifting.
        at(&mut m, 40);
        assert!(m.levels().2 > 0.0);
        at(&mut m, 10);
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

    /// A made-up song: a kick on every beat (harder on the one), a bell on
    /// the one, a bass note and a chord held under it all. In its own key.
    struct Song {
        bpm: f64,
        /// Seconds into the song at which its first bar starts.
        offset: f64,
        root: f64,
        /// Frames made so far.
        made: usize,
    }

    impl Song {
        fn frames(&mut self, n: usize) -> Vec<f64> {
            use std::f64::consts::TAU;
            let beat = 60.0 / self.bpm;
            let mut out = Vec::with_capacity(n * 2);
            for _ in 0..n {
                let t = self.made as f64 / RATE;
                self.made += 1;
                let since = (t - self.offset).rem_euclid(beat);
                let count = ((t - self.offset) / beat).floor() as i64;
                let one = count.rem_euclid(4) == 0;
                let kick = if since < 0.15 { (TAU * 58.0 * since).sin() * (-since * 26.0).exp() * if one { 0.9 } else { 0.6 } } else { 0.0 };
                // The beater's click, which is what the test listens for.
                let click = if since < 0.004 { (TAU * 4_000.0 * since).sin() * 0.2 * (1.0 - since / 0.004) } else { 0.0 };
                let bell = if one && since < 0.08 { (TAU * 2_000.0 * t).sin() * 0.25 * (1.0 - since / 0.08) } else { 0.0 };
                // A bass note on each beat, as a bass line plays: started
                // afresh, so it doesn't beat against the kick differently from
                // one beat to the next.
                let bass = (TAU * self.root * since).sin() * 0.18 * (1.0 - 0.4 * since / beat);
                let pad = [2.0, 2.52, 3.0].iter().map(|k| (TAU * self.root * k * t).sin()).sum::<f64>() * 0.04;
                // A tune in the middle of the range, a note a beat, as a voice would be.
                let note = [8.0, 10.08, 12.0, 10.08][count.rem_euclid(4) as usize];
                let lead = (TAU * self.root * note * t).sin() * 0.08 * (1.0 - (since / beat) * 0.5);
                out.extend([kick + click + bell + bass + pad + lead; 2]);
            }
            out
        }
    }

    /// How much of `hz` there is in each `ms` of `audio`, by Goertzel's method.
    fn tone(audio: &[f64], hz: f64, ms: f64) -> Vec<f64> {
        let hop = (RATE * ms / 1000.0) as usize;
        let w = std::f64::consts::TAU * hz / RATE;
        audio
            .chunks(hop)
            .map(|c| {
                let (mut a, mut b) = (0.0, 0.0);
                for v in c {
                    let n = v + 2.0 * w.cos() * a - b;
                    b = a;
                    a = n;
                }
                (a * a + b * b - 2.0 * w.cos() * a * b) / (c.len() * c.len()) as f64
            })
            .collect()
    }

    /// Where the kicks fall in `audio` (mono), in seconds: their beaters' 4 kHz clicks.
    fn kicks(audio: &[f64]) -> Vec<f64> {
        let power = tone(audio, 4_000.0, 2.0);
        let loudest = power.iter().fold(0.0f64, |a, b| a.max(*b));
        let mut found: Vec<f64> = Vec::new();
        for (i, p) in power.iter().enumerate() {
            let at = i as f64 * 0.002;
            if *p > loudest * 0.08 && found.last().is_none_or(|last| at - last > 0.02) {
                found.push(at);
            }
        }
        found
    }

    /// Where the bells (the ones) ring, in seconds: bursts of 2 kHz.
    fn ones(audio: &[f64]) -> Vec<f64> {
        let power = tone(audio, 2_000.0, 10.0);
        let loudest = power.iter().fold(0.0f64, |a, b| a.max(*b));
        let mut found: Vec<f64> = Vec::new();
        for (i, p) in power.iter().enumerate() {
            let at = i as f64 * 0.01;
            if *p > loudest * 0.1 && found.last().is_none_or(|last| at - last > 0.3) {
                found.push(at);
            }
        }
        found
    }

    /// Play four made-up songs through the real mixer, switching between
    /// them the ways a listener would, and listen to what comes out.
    /// `cargo test --release quad::tests::a_set -- --ignored --nocapture`;
    /// the result is saved to `target/squares.wav`.
    #[test]
    #[ignore]
    fn a_set_sounds_like_one_piece_of_music() {
        let shared = shared();
        let mut songs = [
            Song { bpm: 120.0, offset: 0.3, root: 55.0, made: 0 },
            Song { bpm: 124.0, offset: 1.1, root: 65.4, made: 0 },
            Song { bpm: 116.0, offset: 0.7, root: 82.4, made: 0 },
            // Too slow to bend to the others: it has to be swept in.
            Song { bpm: 92.0, offset: 0.2, root: 73.4, made: 0 },
        ];
        for d in &shared.decks {
            let mut feed = d.feed.lock().unwrap();
            feed.dur = (RATE * 600.0) as u64;
            feed.loading = false;
        }
        let mut m = Mixer::new();
        let mut scratch: [Vec<f64>; SQUARES] = std::array::from_fn(|_| vec![0.0; OUT * 2]);
        let mut mix = vec![0.0; OUT * 2];
        let mut heard: Vec<f64> = Vec::new();
        // (second, square, shift): settle, blend, blend, flick back, stutter in, the far-off one.
        let script = [(14.0, 1, false), (22.0, 2, false), (24.0, 0, false), (30.0, 2, true), (37.0, 3, false)];
        let mut landed: Vec<(f64, usize)> = Vec::new();
        let mut asked = 0;
        let end = 42.0;
        while (heard.len() as f64) < end * RATE {
            let seconds = heard.len() as f64 / RATE;
            for (d, song) in songs.iter_mut().enumerate() {
                let mut feed = shared.decks[d].feed.lock().unwrap();
                while feed.buf.len() < 16_384 {
                    let more = song.frames(1024);
                    feed.buf.extend(more);
                    feed.pos += 1024;
                }
            }
            if let Some(&(at, to, slam)) = script.get(asked) {
                if seconds >= at {
                    let sync: Vec<_> = m.follow.iter().map(|f| f.sync()).collect();
                    m.go(&shared, to, slam);
                    let how = match m.phase {
                        Phase::Waiting { mv, .. } => mv.name(),
                        _ => "nothing",
                    };
                    eprintln!("{seconds:.2} s: to square {} by {how}; in time then: {sync:?}", to + 1);
                    asked += 1;
                }
            }
            let live = m.live;
            m.run_block(&shared, &mut scratch, &mut mix);
            if m.live != live {
                landed.push((seconds, m.live));
            }
            heard.extend(mix.chunks_exact(2).map(|p| p[0]));
        }
        save_wav("target/squares.wav", &heard);
        eprintln!("switches done: {landed:?}");
        let sync: Vec<_> = m.follow.iter().map(|f| f.sync()).collect();
        eprintln!("in time with the last one: {sync:?}");
        assert_eq!(landed.iter().map(|l| l.1).collect::<Vec<_>>(), [1, 2, 0, 2, 3], "not every switch happened");

        // No gaps: every 10 ms has music in it.
        let window = (RATE * 0.01) as usize;
        let rms: Vec<f64> = heard.chunks(window).map(|c| (c.iter().map(|v| v * v).sum::<f64>() / c.len() as f64).sqrt()).collect();
        let mut sorted = rms.clone();
        sorted.sort_by(f64::total_cmp);
        let typical = sorted[sorted.len() / 2];
        let quietest = rms.iter().enumerate().skip(10).min_by(|a, b| a.1.total_cmp(b.1)).unwrap();
        eprintln!("quietest 10 ms: {:.3} at {:.2} s, typical {typical:.3}", quietest.1, quietest.0 as f64 * 0.01);
        assert!(*quietest.1 > typical * 0.2, "a gap at {:.2} s", quietest.0 as f64 * 0.01);

        // No clicks: the sound never jumps from one sample to the next by
        // more than the music itself does.
        let jumps: Vec<f64> = heard.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
        let normal = jumps[..(RATE * 12.0) as usize].iter().fold(0.0f64, |a, b| a.max(*b));
        let (worst_at, worst) = jumps.iter().enumerate().fold((0, 0.0f64), |a, (i, j)| if *j > a.1 { (i, *j) } else { a });
        eprintln!("largest step {worst:.4} at {:.3} s; a single song's is {normal:.4}", worst_at as f64 / RATE);
        assert!(worst < normal * 2.5, "a click at {:.3} s", worst_at as f64 / RATE);

        // In time: around each switch to a song that could be matched, no
        // flams, two kicks 30 to 200 ms apart, which is what beats that
        // don't line up sound like. (An echo's repeats fall on half-beats.)
        let found = kicks(&heard);
        for &(at, to) in &landed {
            if to == 3 {
                continue;
            }
            let near: Vec<f64> = found.iter().copied().filter(|k| (at - 2.5..at + 2.0).contains(k)).collect();
            let gaps: Vec<f64> = near.windows(2).map(|w| w[1] - w[0]).collect();
            eprintln!("switch to {} at {at:.2} s: kicks every {:?} ms", to + 1, gaps.iter().map(|g| (g * 1000.0).round()).collect::<Vec<_>>());
            assert!(near.len() >= 6, "switch to square {} at {at:.2} s: the kicks went missing", to + 1);
            let flams: Vec<&f64> = gaps.iter().filter(|g| (0.03..0.2).contains(*g)).collect();
            assert!(flams.is_empty(), "switch to square {} at {at:.2} s: flams {flams:?}", to + 1);
        }

        // On the one: the bells, a bar apart, carry on a bar apart across switches.
        let bells = ones(&heard);
        for (k, &(at, to)) in landed.iter().enumerate() {
            // Not the far-off song, nor the stutter, which repeats the one on purpose.
            if to == 3 || script[k].2 {
                continue;
            }
            let near: Vec<f64> = bells.iter().copied().filter(|b| (at - 4.5..at + 4.5).contains(b)).collect();
            let bars: Vec<f64> = near.windows(2).map(|w| (w[1] - w[0]) / 2.0).collect();
            eprintln!("switch to {} at {at:.2} s: bells every {:?} bars", to + 1, bars.iter().map(|b| (b * 100.0).round() / 100.0).collect::<Vec<_>>());
            assert!(bars.iter().all(|b| (b - b.round()).abs() < 0.06 && b.round() >= 1.0), "the bars slipped at {at:.2} s: {bars:?}");
        }
    }

    fn save_wav(path: &str, mono: &[f64]) {
        let mut out = Vec::with_capacity(44 + mono.len() * 2);
        let data = (mono.len() * 2) as u32;
        out.extend(b"RIFF");
        out.extend((36 + data).to_le_bytes());
        out.extend(b"WAVEfmt ");
        out.extend(16u32.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend((RATE as u32).to_le_bytes());
        out.extend((RATE as u32 * 2).to_le_bytes());
        out.extend(2u16.to_le_bytes());
        out.extend(16u16.to_le_bytes());
        out.extend(b"data");
        out.extend(data.to_le_bytes());
        for v in mono {
            out.extend(((v.clamp(-1.0, 1.0) * 32_000.0) as i16).to_le_bytes());
        }
        let _ = std::fs::write(path, out);
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

