//! Two decks and a crossfader.
//!
//! Each deck is its own decoder. Their audio meets in a small mixer thread
//! that applies each deck's tempo, the crossfader and the master volume, and
//! sends the sum to the sound card. The mixer also listens to each deck on its
//! own to find its tempo and where its beats fall, which is what lets one deck
//! be matched to the other.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use librespot_core::{Session, SpotifyUri};
use librespot_playback::audio_backend::{Sink, SinkResult};
use librespot_playback::config::{Bitrate, PlayerConfig};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use librespot_playback::mixer::{NoOpVolume, VolumeGetter};
use librespot_playback::player::{Player, PlayerEvent, PlayerEventChannel};

use crate::audio::{Hub, Monitor, RATE, frames};
use crate::model::Track;
use crate::out::Output;

/// Frames mixed at a time: about 6 ms, which keeps the controls feeling immediate.
const OUT: usize = 256;
/// Audio each deck keeps decoded ahead of the mixer: 0.25 s.
const AHEAD: usize = 22_050;
/// How far tempo may be bent either way.
pub const MAX_BEND: f32 = 0.12;

// ---- tempo and beat tracking ------------------------------------------------

/// Samples per analysis step: 344.5 steps a second, i.e. 2.9 ms resolution.
const HOP: usize = 128;
const HOPS_PER_SEC: f32 = RATE as f32 / HOP as f32;
/// Below this the music has no steady enough pulse to lock onto.
const MIN_STRENGTH: f32 = 0.13;
/// How much history the tempo estimate looks at.
const WINDOW: usize = (HOPS_PER_SEC * 10.0) as usize;

/// Follows one deck's audio and works out its tempo and where its beats fall.
pub struct Tempo {
    low: [f64; 2],
    sum_low: f64,
    sum_full: f64,
    filled: usize,
    /// Recent per-step energies (low band, full band), for smoothing.
    recent: VecDeque<(f32, f32)>,
    onsets: VecDeque<f32>,
    /// Onsets in the low end only: kick drums, which mark the beat itself
    /// rather than the off-beats in between.
    kicks: VecDeque<f32>,
    /// Steps seen in total; the clock all beat times are expressed on.
    pub now: u64,
    since_estimate: usize,
    pub bpm: Option<f32>,
    /// Beat length in steps.
    pub period: f32,
    /// Step at which a beat fell (any beat; the rest follow from `period`).
    pub beat_at: f64,
    /// How periodic the music is at the chosen tempo, 0..1.
    pub strength: f32,
    /// The last few period readings; their median is what gets reported.
    readings: VecDeque<f32>,
}

impl Default for Tempo {
    fn default() -> Self {
        Self {
            low: [0.0; 2],
            sum_low: 0.0,
            sum_full: 0.0,
            filled: 0,
            recent: VecDeque::with_capacity(SMOOTH + LOOK + 2),
            onsets: VecDeque::with_capacity(WINDOW + 8),
            kicks: VecDeque::with_capacity(WINDOW + 8),
            now: 0,
            since_estimate: 0,
            bpm: None,
            period: 0.0,
            beat_at: 0.0,
            strength: 0.0,
            readings: VecDeque::with_capacity(8),
        }
    }
}

/// Steps averaged into each energy reading (about 23 ms: longer than a bass cycle).
const SMOOTH: usize = 8;
/// Steps back that a reading is compared with to see a rise (about 12 ms).
const LOOK: usize = 4;

impl Tempo {
    /// Forget everything: the audio is about to jump or change speed.
    pub fn reset(&mut self) {
        let now = self.now;
        *self = Self { now, ..Self::default() };
    }

    /// Let time pass without audio (the deck is stopped). Both decks' clocks
    /// keep counting together, so their beat times stay comparable.
    pub fn idle(&mut self, frames: usize) {
        self.filled += frames;
        self.now += (self.filled / HOP) as u64;
        self.filled %= HOP;
        self.sum_low = 0.0;
        self.sum_full = 0.0;
    }

    /// Feed interleaved stereo at the mixer's rate.
    pub fn feed(&mut self, stereo: &[f64]) {
        // Kick drums and bass carry the beat; a gentle low-pass isolates them.
        const A: f64 = 0.021;
        for pair in stereo.chunks_exact(2) {
            let x = (pair[0] + pair[1]) * 0.5;
            self.low[0] += A * (x - self.low[0]);
            self.low[1] += A * (self.low[0] - self.low[1]);
            self.sum_low += self.low[1] * self.low[1];
            self.sum_full += x * x;
            self.filled += 1;
            if self.filled == HOP {
                self.recent.push_back((self.sum_low as f32, self.sum_full as f32));
                if self.recent.len() > SMOOTH + LOOK {
                    self.recent.pop_front();
                }
                self.sum_low = 0.0;
                self.sum_full = 0.0;
                self.filled = 0;
                // An onset is a rise in loudness. Energy is averaged over a
                // short window so the bass waveform itself doesn't read as one,
                // and compared in decibel terms so quiet songs count the same.
                let level = |from: usize| -> (f32, f32) {
                    // The floor keeps near-silence from registering as huge swings.
                    let (mut low, mut full) = (1e-3f32, 1e-3f32);
                    for (l, f) in self.recent.iter().skip(from).take(SMOOTH) {
                        low += l;
                        full += f;
                    }
                    (low.ln(), full.ln())
                };
                let (onset, kick) = if self.recent.len() == SMOOTH + LOOK {
                    let (now, before) = (level(LOOK), level(0));
                    let kick = (now.0 - before.0).max(0.0);
                    // Capped, so one enormous hit (music starting from silence)
                    // cannot drown out the regular pulse.
                    ((kick * 2.0 + (now.1 - before.1).max(0.0)).min(2.5), kick.min(1.5))
                } else {
                    (0.0, 0.0)
                };
                self.push(onset, kick);
            }
        }
    }

    fn push(&mut self, onset: f32, kick: f32) {
        if self.onsets.len() >= WINDOW {
            self.onsets.pop_front();
            self.kicks.pop_front();
        }
        self.onsets.push_back(onset);
        self.kicks.push_back(kick);
        self.now += 1;
        self.since_estimate += 1;
        // Re-estimate twice a second once there are four seconds to go on.
        if self.since_estimate >= (HOPS_PER_SEC / 2.0) as usize
            && self.onsets.len() >= (HOPS_PER_SEC * 4.0) as usize
        {
            self.since_estimate = 0;
            self.estimate();
        }
    }

    fn estimate(&mut self) {
        let n = self.onsets.len();
        let mean = self.onsets.iter().sum::<f32>() / n as f32;
        let o: Vec<f32> = self.onsets.iter().map(|v| v - mean).collect();
        let energy: f32 = o.iter().map(|v| v * v).sum::<f32>() / n as f32;
        if energy < 1e-10 {
            self.bpm = None;
            return;
        }
        let acf = |lag: usize| -> f32 {
            if lag >= n {
                return 0.0;
            }
            o[lag..].iter().zip(&o[..n - lag]).map(|(a, b)| a * b).sum::<f32>() / (n - lag) as f32
        };
        // 60 to 200 beats per minute.
        let (shortest, longest) = ((HOPS_PER_SEC * 60.0 / 200.0) as usize, (HOPS_PER_SEC * 60.0 / 60.0) as usize);
        let table: Vec<f32> = (0..=longest * 4 + 1).map(acf).collect();
        let mut best = (0.0f32, 0usize);
        for lag in shortest..=longest {
            let bpm = 60.0 * HOPS_PER_SEC / lag as f32;
            // A true beat also repeats every two, three and four beats, which
            // an off-beat or syncopated pattern does not. Most music sits near 120.
            let prior = (-0.5 * ((bpm / 120.0).log2() / 0.55).powi(2)).exp();
            let mut score = table[lag];
            for (k, weight) in [(2, 0.8), (3, 0.6), (4, 0.5)] {
                // Only as far as there is enough history for the comparison to mean something.
                if lag * k < n * 2 / 3 {
                    score += weight * table[lag * k];
                }
            }
            let score = score * prior;
            if score > best.0 {
                best = (score, lag);
            }
        }
        let lag = best.1;
        self.strength = if lag == 0 { 0.0 } else { (table[lag] / energy).max(0.0) };
        if lag == 0 || self.strength < MIN_STRENGTH {
            self.bpm = None;
            return;
        }
        // Refine between steps by fitting a parabola through the peak.
        let peak = |a: f32, b: f32, c: f32| -> f32 {
            let denom = a - 2.0 * b + c;
            if denom.abs() > 1e-12 { (0.5 * (a - c) / denom).clamp(-0.5, 0.5) } else { 0.0 }
        };
        let mut shift = peak(table[lag - 1], table[lag], table[lag + 1]);
        // Then sharpen it: the same pulse also lines up with itself 2, 3, 4…
        // beats later, and measuring across k beats is k times as precise.
        // Matching two decks needs the tempo to a small fraction of a BPM.
        let mut precise = lag as f32 + shift;
        let reach = ((n as f32 * 0.6) / precise) as usize;
        for k in 2..=reach.min(16) {
            let centre = (precise * k as f32).round() as usize;
            let Some(best_lag) = (centre.saturating_sub(2)..=centre + 2)
                .filter(|l| *l + 1 < n)
                .max_by(|a, b| acf(*a).total_cmp(&acf(*b)))
            else {
                break;
            };
            let at_k = best_lag as f32 + peak(acf(best_lag - 1), acf(best_lag), acf(best_lag + 1));
            let candidate = at_k / k as f32;
            // A jump means this multiple latched onto something else; stop refining.
            if (candidate / precise - 1.0).abs() > 0.01 {
                break;
            }
            precise = candidate;
        }
        shift = precise - lag as f32;
        // Single readings wobble by a fraction of a BPM; the median of the last
        // few seconds is steadier, which matters when two decks must stay locked.
        let reading = lag as f32 + shift;
        if self.readings.len() >= 7 {
            self.readings.pop_front();
        }
        // A real tempo change (or a different reading of the pulse) starts afresh.
        if self.readings.back().is_some_and(|last| (reading / last - 1.0).abs() > 0.06) {
            self.readings.clear();
        }
        self.readings.push_back(reading);
        let mut sorted: Vec<f32> = self.readings.iter().copied().collect();
        sorted.sort_by(f32::total_cmp);
        let period = sorted[sorted.len() / 2];

        // Phase: slide a comb of that period over the onsets; where it lines up
        // best is where the beats are. Kick drums say where the beat is far
        // more reliably than everything else, so use them when the song has them.
        let kick_share = self.kicks.iter().sum::<f32>() * 2.0 / self.onsets.iter().sum::<f32>().max(1e-6);
        let series = if kick_share > 0.25 { &self.kicks } else { &self.onsets };
        let at = |x: f32| -> f32 {
            if x < 0.0 {
                return 0.0;
            }
            let i = x as usize;
            if i + 1 >= n {
                return 0.0;
            }
            let t = x - i as f32;
            series[i] * (1.0 - t) + series[i + 1] * t
        };
        let last = (n - 1) as f32;
        let mut phase_best = (f32::MIN, 0usize);
        let steps = period.ceil() as usize;
        // Only the last few beats: where the beat is now, not where it was.
        let comb = |back: f32| -> f32 { (0..10).map(|k| at(last - back - k as f32 * period)).sum() };
        for back in 0..steps {
            let score = comb(back as f32);
            if score > phase_best.0 {
                phase_best = (score, back);
            }
        }
        let mut back = phase_best.1 as f32;
        // Off-beats (hi-hats, syncopation) can score nearly as well as the beat
        // and would flip the grid by half a beat from one reading to the next.
        // If the grid we already had still fits about as well, keep it.
        if self.bpm.is_some() {
            let expected = ((self.now - 1) as f64 - self.beat_at).rem_euclid(period as f64) as f32;
            let nearby = (-3..=3)
                .map(|d| expected + d as f32)
                .map(|b| (comb(b), b))
                .fold((f32::MIN, expected), |best, c| if c.0 > best.0 { c } else { best });
            let apart = (nearby.1 - back).abs();
            let apart = apart.min(period - apart);
            if apart > period * 0.15 && nearby.0 >= phase_best.0 * 0.75 {
                back = nearby.1.rem_euclid(period);
            }
        }
        let (a, b, c) = (comb(back - 1.0), comb(back), comb(back + 1.0));
        let denom = a - 2.0 * b + c;
        let fine = if denom.abs() > 1e-9 { (0.5 * (a - c) / denom).clamp(-0.5, 0.5) } else { 0.0 };

        self.period = period;
        self.bpm = Some(60.0 * HOPS_PER_SEC / period);
        self.beat_at = (self.now - 1) as f64 - (back + fine) as f64;
    }

    /// Have the last few readings agreed? An intro or a breakdown can read
    /// as almost anything for a moment; a tempo worth acting on holds still.
    pub fn steady(&self) -> bool {
        if self.bpm.is_none() || self.readings.len() < 4 {
            return false;
        }
        let recent = self.readings.iter().rev().take(4);
        let (lo, hi) = recent.fold((f32::MAX, f32::MIN), |(lo, hi), r| (lo.min(*r), hi.max(*r)));
        (hi - lo) / lo < 0.012
    }

    /// Seconds of music this reading is based on.
    pub fn heard(&self) -> f32 {
        self.onsets.len() as f32 / HOPS_PER_SEC
    }

    /// Fraction of the way through the current beat, 0..1.
    pub fn phase(&self) -> Option<f32> {
        self.bpm?;
        Some((((self.now as f64 - self.beat_at) / self.period as f64).rem_euclid(1.0)) as f32)
    }
}

/// The tempo ratio that brings `from` onto `to`, allowing for one deck being
/// counted at half or double time. `None` if they are too far apart to bend.
pub fn match_ratio(from_bpm: f32, to_bpm: f32) -> Option<f32> {
    let mut ratio = to_bpm / from_bpm;
    while ratio > 1.42 {
        ratio /= 2.0;
    }
    while ratio < 0.71 {
        ratio *= 2.0;
    }
    ((1.0 - MAX_BEND)..=(1.0 + MAX_BEND)).contains(&ratio).then_some(ratio)
}

/// How far (in steps) the follower must be delayed (+) or advanced (-) so its
/// beats land on the leader's, once both run at the leader's period.
pub fn beat_offset(leader_beat: f64, follower_beat: f64, period: f64) -> f64 {
    let diff = (leader_beat - follower_beat).rem_euclid(period);
    if diff > period / 2.0 { diff - period } else { diff }
}

// ---- three-band EQ ----------------------------------------------------------

/// Where the bands divide: lows below the first, highs above the second.
const LOW_HZ: f64 = 250.0;
const HIGH_HZ: f64 = 2_500.0;
/// A band's knob, in steps either side of flat: -6 is a full kill, +2 is +6 dB.
pub const EQ_KILL: i8 = -6;
pub const EQ_MAX: i8 = 2;
/// The gain at each step, from the kill up: -24, -18, -12, -6, -3, 0, +3, +6 dB.
const EQ_GAIN: [f32; 9] = [0.0, 0.063, 0.126, 0.251, 0.501, 0.708, 1.0, 1.413, 1.995];
pub const BAND_NAMES: [&str; 3] = ["low", "mid", "high"];

pub fn eq_gain(step: i8) -> f32 {
    EQ_GAIN[(step.clamp(EQ_KILL, EQ_MAX) - EQ_KILL) as usize]
}

/// A knob position in words: "kill", "-6 dB", "flat".
pub fn eq_label(step: i8) -> &'static str {
    ["kill", "-24 dB", "-18 dB", "-12 dB", "-6 dB", "-3 dB", "flat", "+3 dB", "+6 dB"]
        [(step.clamp(EQ_KILL, EQ_MAX) - EQ_KILL) as usize]
}

/// One second-order filter section.
#[derive(Clone, Copy, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    /// A Butterworth low-pass or high-pass at `hz`.
    fn new(hz: f64, high: bool) -> Self {
        let w = std::f64::consts::TAU * hz / RATE;
        let (sin, cos) = w.sin_cos();
        let alpha = sin / std::f64::consts::SQRT_2;
        let a0 = 1.0 + alpha;
        let b = if high {
            [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0]
        } else {
            [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0]
        };
        Self { b: b.map(|v| v / a0), a: [-2.0 * cos / a0, (1.0 - alpha) / a0], z: [0.0; 2] }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// Splits one channel into lows, mids and highs that add back up to the
/// original (two sections per side of each divide, the kind of crossover
/// whose halves stay in step with each other).
#[derive(Clone, Copy)]
struct Bands {
    low: [Biquad; 2],
    rest: [Biquad; 2],
    mid: [Biquad; 2],
    high: [Biquad; 2],
}

impl Default for Bands {
    fn default() -> Self {
        Self {
            low: [Biquad::new(LOW_HZ, false); 2],
            rest: [Biquad::new(LOW_HZ, true); 2],
            mid: [Biquad::new(HIGH_HZ, false); 2],
            high: [Biquad::new(HIGH_HZ, true); 2],
        }
    }
}

impl Bands {
    fn split(&mut self, x: f64) -> [f64; 3] {
        let through = |pair: &mut [Biquad; 2], v: f64| {
            let v = pair[0].run(v);
            pair[1].run(v)
        };
        let low = through(&mut self.low, x);
        let rest = through(&mut self.rest, x);
        [low, through(&mut self.mid, rest), through(&mut self.high, rest)]
    }
}

/// One deck's EQ: the filters for both channels, the gains as they are being
/// applied (they glide to where the knobs are), and how loud each band is.
#[derive(Default)]
struct Eq {
    channels: [Bands; 2],
    gain: Option<[f32; 3]>,
    level: [f32; 3],
}

impl Eq {
    /// Apply the knob positions `steps` to a block of interleaved stereo.
    fn run(&mut self, block: &mut [f64], steps: [i8; 3]) {
        let target = steps.map(eq_gain);
        let from = self.gain.unwrap_or(target);
        let frames = (block.len() / 2).max(1) as f32;
        let mut peak = [0.0f32; 3];
        for (i, pair) in block.chunks_exact_mut(2).enumerate() {
            let k = i as f32 / frames;
            for (c, sample) in pair.iter_mut().enumerate() {
                let bands = self.channels[c].split(*sample);
                let mut sum = 0.0;
                for band in 0..3 {
                    // Glide across the block so a turned knob never clicks.
                    let heard = bands[band] * (from[band] + (target[band] - from[band]) * k) as f64;
                    peak[band] = peak[band].max(heard.abs() as f32);
                    sum += heard;
                }
                *sample = sum;
            }
        }
        self.gain = Some(target);
        for band in 0..3 {
            self.level[band] = self.level[band] * 0.9 + peak[band] * 0.1;
        }
    }

    /// Start from silence: no ringing carried over from whatever played before.
    fn reset(&mut self) {
        *self = Self::default();
    }
}

// ---- shared state -----------------------------------------------------------

/// Decoded audio waiting to be mixed, plus what the decoder knows.
#[derive(Default)]
struct Feed {
    buf: VecDeque<f64>,
    /// Decode position in the song, in frames.
    pos: u64,
    dur: u64,
    loading: bool,
    /// The decoder reached the end; `buf` holds the last of the song.
    ended: bool,
    failed: bool,
    /// Bumped on every load and seek so the mixer starts clean.
    generation: u64,
}

struct DeckShared {
    feed: Mutex<Feed>,
    playing: AtomicBool,
    /// Playback speed as f32 bits; 1.0 is the song's own tempo.
    rate: AtomicU32,
    /// EQ knobs for low, mid and high, in steps from flat (see `EQ_KILL`).
    eq: [std::sync::atomic::AtomicI8; 3],
    events: Mutex<Option<PlayerEventChannel>>,
}

impl Default for DeckShared {
    fn default() -> Self {
        Self {
            feed: Mutex::default(),
            playing: AtomicBool::new(false),
            rate: AtomicU32::new(1.0f32.to_bits()),
            eq: Default::default(),
            events: Mutex::default(),
        }
    }
}

/// Keeps one deck's beats on the other's after they have been matched.
/// Songs with a live drummer wander; this follows them.
struct Lock {
    follower: usize,
    /// The follower's tempo setting that the corrections are made around.
    base: f32,
    integral: f32,
    next_at: u64,
    /// The last reading that was believed, and how many since have disagreed.
    last: Option<f32>,
    doubts: u8,
    /// A gap, in beats, the listener has nudged in on purpose. The lock holds
    /// the decks to it instead of pulling it back out.
    offset: f32,
}

impl Lock {
    /// Twice a second: if the follower's beats are landing late, run it a
    /// touch faster, and the reverse. The corrections are small and slow so the
    /// change in pitch is never heard, and a reading that looks wrong is
    /// ignored rather than acted on: this must never make things worse.
    fn steer(&mut self, shared: &Shared, decks: &[DeckMix; 2], clock: u64) {
        if clock < self.next_at {
            return;
        }
        self.next_at = clock + (RATE / 2.0) as u64;
        let (lead, follow) = (&decks[1 - self.follower].tempo, &decks[self.follower].tempo);
        // Until the follower has been re-measured there is nothing to correct,
        // and a faint or unsteady pulse is not worth chasing.
        if follow.heard() < 4.5 || follow.strength < 0.16 || lead.strength < 0.16 {
            return;
        }
        let (Some(_), Some(_)) = (lead.phase(), follow.phase()) else { return };
        // Both are placed on the shorter of the two beat lengths. Matching
        // allows one deck to be counted at half the other's tempo, and where
        // each is within its own beat says nothing when the beats differ.
        let period = lead.period.min(follow.period) as f64;
        if period <= 0.0 {
            return;
        }
        let place = |t: &Tempo| ((t.now as f64 - t.beat_at).rem_euclid(period) / period) as f32;
        let mut behind = place(lead) - place(follow) - self.offset;
        behind -= behind.round();
        // Close to half a beat out there is no telling which way to go, and
        // tempo is the wrong tool for a gap that size anyway.
        if behind.abs() > 0.25 {
            return;
        }
        // A sudden jump is far more likely a misreading than the music: wait
        // for it to be confirmed several times before believing it.
        if let Some(last) = self.last {
            if (behind - last).abs() > 0.15 && self.doubts < 4 {
                self.doubts += 1;
                return;
            }
        }
        self.doubts = 0;
        self.last = Some(behind);

        self.integral = (self.integral + behind * 0.006).clamp(-0.03, 0.03);
        let wanted = self.base * (1.0 + (behind * 0.08).clamp(-0.012, 0.012) + self.integral);
        // Move towards it gradually.
        let now = load_f32(&shared.decks[self.follower].rate);
        let rate = now + (wanted - now).clamp(-0.003, 0.003);
        let rate = rate.clamp(1.0 - MAX_BEND, 1.0 + MAX_BEND);
        shared.decks[self.follower].rate.store(rate.to_bits(), Ordering::Relaxed);
    }
}

impl Lock {
    fn new(follower: usize, base: f32, clock: u64) -> Self {
        Self { follower, base, integral: 0.0, next_at: clock, last: None, doubts: 0, offset: 0.0 }
    }
}

enum Request {
    /// Glide the crossfader to deck `to` over `seconds`, beat-matching first if asked.
    Mix { to: usize, seconds: f32, beatmatch: bool },
    Sync { deck: usize },
    /// Shift a deck in time by this many milliseconds (+ later, - earlier).
    Nudge { deck: usize, ms: f32 },
    Cancel,
    /// The listener took the tempo into their own hands.
    Unlock,
}

#[derive(Clone, Copy, Default)]
pub struct DeckMeter {
    pub bpm: Option<f32>,
    /// 0..1 through the current beat, when the tempo is known.
    pub phase: Option<f32>,
    pub level: f32,
    /// How loud the lows, mids and highs are after the EQ, 0..1.
    pub bands: [f32; 3],
}

#[derive(Clone, Default)]
struct Info {
    meters: [DeckMeter; 2],
    /// A transition in progress: (deck being mixed in, progress 0..1, still listening).
    auto: Option<(usize, f32, bool)>,
    /// The deck being held on the other's beat, if any.
    locked: Option<usize>,
    notes: Vec<String>,
    /// Decks the mixer has finished with and that should stop decoding.
    finished: Vec<usize>,
}

struct Shared {
    decks: [DeckShared; 2],
    /// Crossfader position as f32 bits: 0 is all deck A, 1 is all deck B.
    fader: AtomicU32,
    requests: Mutex<Vec<Request>>,
    info: Mutex<Info>,
    quit: AtomicBool,
    wake: (Mutex<()>, Condvar),
    hub: Arc<Hub>,
}

fn load_f32(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

// ---- deck sink (runs on each deck's decoder thread) -------------------------

/// Apply the decoder's events to a deck's feed. The decoder calls this just
/// before queueing each packet (events are emitted on its own thread, in order
/// with the audio, so they mark exactly where a seek or a new song starts).
/// The interface calls it too, so a paused deck still reports that it loaded.
fn pump(deck: &DeckShared, feed: &mut Feed) {
    let mut events = deck.events.lock().unwrap();
    let Some(rx) = events.as_mut() else { return };
    while let Ok(event) = rx.try_recv() {
        match event {
            PlayerEvent::Loading { position_ms, .. } => {
                feed.buf.clear();
                feed.generation += 1;
                feed.loading = true;
                feed.ended = false;
                feed.failed = false;
                feed.pos = frames(position_ms);
            }
            PlayerEvent::TrackChanged { audio_item } => {
                feed.buf.clear();
                feed.generation += 1;
                feed.dur = frames(audio_item.duration_ms);
                feed.loading = false;
                feed.ended = false;
            }
            PlayerEvent::Playing { position_ms, .. } | PlayerEvent::Paused { position_ms, .. } => {
                feed.loading = false;
                // Only trust this while nothing is queued; otherwise our own count is exact.
                if feed.buf.is_empty() {
                    feed.pos = frames(position_ms);
                }
            }
            PlayerEvent::Seeked { position_ms, .. } => {
                feed.buf.clear();
                feed.generation += 1;
                feed.ended = false;
                feed.pos = frames(position_ms);
            }
            PlayerEvent::EndOfTrack { .. } => feed.ended = true,
            PlayerEvent::Unavailable { .. } => {
                feed.failed = true;
                feed.loading = false;
            }
            _ => {}
        }
    }
}

struct DeckSink {
    shared: Arc<Shared>,
    deck: usize,
}

impl Sink for DeckSink {
    fn stop(&mut self) -> SinkResult<()> {
        let deck = &self.shared.decks[self.deck];
        pump(deck, &mut deck.feed.lock().unwrap());
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, _: &mut Converter) -> SinkResult<()> {
        let Ok(samples) = packet.samples() else { return Ok(()) };
        let deck = &self.shared.decks[self.deck];
        let over = {
            let mut feed = deck.feed.lock().unwrap();
            pump(deck, &mut feed);
            feed.buf.extend(samples);
            feed.pos += (samples.len() / 2) as u64;
            feed.buf.len() > AHEAD * 2
        };
        self.shared.wake.1.notify_all();
        // Pace the decoder rather than block it, so seek and pause get through.
        if over {
            let length = Duration::from_secs_f64(samples.len() as f64 / 2.0 / RATE);
            std::thread::sleep(length.mul_f64(1.5).min(Duration::from_millis(120)));
        }
        Ok(())
    }
}

// ---- mixer thread -----------------------------------------------------------

#[derive(Default)]
struct DeckMix {
    /// Fractional read position within the feed, in frames.
    phase: f64,
    generation: u64,
    /// Silence still owed before this deck resumes (used to line beats up).
    hold: usize,
    tempo: Tempo,
    eq: Eq,
    level: f32,
    was_playing: bool,
}

/// Read `OUT` frames from a deck at `rate`, resampling as needed. Returns how
/// many frames were real audio.
fn pull(feed: &mut Feed, mix: &mut DeckMix, rate: f64, out: &mut [f64]) -> usize {
    out.fill(0.0);
    if feed.generation != mix.generation {
        mix.generation = feed.generation;
        mix.phase = 0.0;
        mix.hold = 0;
        mix.tempo.reset();
    }
    let mut produced = 0;
    while mix.hold > 0 && produced < OUT {
        mix.hold -= 1;
        produced += 1;
    }
    let available = feed.buf.len() / 2;
    let mut real = 0;
    // Back at normal speed, read whole samples again. A fraction left over
    // from a bend would have every sample interpolated, dulling the highs.
    if rate == 1.0 {
        mix.phase = mix.phase.round();
    }
    while produced < OUT {
        let i = mix.phase as usize;
        if i + 1 >= available {
            break;
        }
        let t = mix.phase - i as f64;
        for c in 0..2 {
            let (a, b) = (feed.buf[2 * i + c], feed.buf[2 * (i + 1) + c]);
            out[2 * produced + c] = a + (b - a) * t;
        }
        mix.phase += rate;
        produced += 1;
        real += 1;
    }
    let consumed = (mix.phase as usize).min(available);
    feed.buf.drain(..consumed * 2);
    mix.phase -= consumed as f64;
    real
}

enum Auto {
    Idle,
    /// The incoming deck is playing silently while its beat is found.
    /// `problem` is why the last attempt to match it failed, if one did.
    Listen { to: usize, until: u64, seconds: f32, problem: Option<String> },
    Fade { to: usize, from_value: f32, start: u64, length: u64 },
}

/// When set, deck A's audio is copied here as it is mixed. Used by
/// `riff doctor --dj --capture` to save real music for tuning the beat tracker.
pub static CAPTURE: Mutex<Option<Vec<f32>>> = Mutex::new(None);

fn mixer_loop(shared: Arc<Shared>, volume: Box<dyn VolumeGetter + Send>) {
    let out = Output::open().map_err(|e| log::warn!("dj output: {e}")).ok();
    let mut monitor = Monitor::new();
    let mut decks = [DeckMix::default(), DeckMix::default()];
    let mut auto = Auto::Idle;
    let mut lock: Option<Lock> = None;
    let mut started = false;
    let mut gain = volume.attenuation_factor();
    // Frames mixed since the thread began: the mixer's own clock.
    let mut clock: u64 = 0;
    let mut scratch = [vec![0.0f64; OUT * 2], vec![0.0f64; OUT * 2]];
    let mut last_fader = load_f32(&shared.fader);

    while !shared.quit.load(Ordering::Relaxed) {
        // ---- requests from the interface
        for request in shared.requests.lock().unwrap().drain(..) {
            match request {
                Request::Cancel => auto = Auto::Idle,
                Request::Unlock => lock = None,
                Request::Nudge { deck, ms } => {
                    let frames = (ms.abs() / 1000.0 * RATE as f32) as usize;
                    if ms > 0.0 {
                        decks[deck].hold += frames;
                    } else {
                        let mut feed = shared.decks[deck].feed.lock().unwrap();
                        let drop = (frames * 2).min(feed.buf.len()) & !1;
                        feed.buf.drain(..drop);
                    }
                    decks[deck].tempo.beat_at += (ms / 1000.0 * HOPS_PER_SEC) as f64;
                    if let Some(lock) = lock.as_mut() {
                        let period = decks[0].tempo.period.min(decks[1].tempo.period);
                        if period > 0.0 {
                            let beats = ms / 1000.0 * HOPS_PER_SEC / period;
                            lock.offset += if deck == lock.follower { beats } else { -beats };
                        }
                    }
                }
                Request::Sync { deck } => {
                    let note = beatmatch(&shared, &mut decks, deck);
                    if note.is_ok() {
                        lock = Some(Lock::new(deck, load_f32(&shared.decks[deck].rate), clock));
                    }
                    shared.info.lock().unwrap().notes.push(note.unwrap_or_else(|e| e.to_string()));
                }
                Request::Mix { to, seconds, beatmatch } => {
                    let already_matched = lock.as_ref().is_some_and(|l| l.follower == to);
                    auto = if beatmatch && !already_matched {
                        // Usually six seconds is enough; an intro that hasn't found
                        // its groove yet can take longer. Past eleven, stop waiting
                        // and blend without matching.
                        Auto::Listen { to, until: clock + (RATE * 11.0) as u64, seconds, problem: None }
                    } else {
                        Auto::Fade { to, from_value: load_f32(&shared.fader), start: clock, length: (seconds as f64 * RATE) as u64 }
                    };
                }
            }
        }

        // ---- a transition in progress
        // The deck being brought in has to be sounding. If it was paused or
        // reloaded part-way, gliding over to it and stopping the other one
        // would end in silence.
        if let Auto::Listen { to, .. } | Auto::Fade { to, .. } = &auto {
            if !shared.decks[*to].playing.load(Ordering::Relaxed) {
                let note = format!("Deck {} stopped: mix called off", deck_name(*to));
                shared.info.lock().unwrap().notes.push(note);
                auto = Auto::Idle;
            }
        }
        let mut progress = None;
        match auto {
            Auto::Idle => {}
            Auto::Listen { to, until, seconds, ref mut problem } => {
                // A tempo read from under six seconds, or one that is still
                // changing, is not exact enough to hold two songs together.
                let ready = decks[to].tempo.steady()
                    && decks[to].tempo.heard() >= 6.0
                    && decks[1 - to].tempo.steady();
                progress = Some((to, 0.0, true));
                let mut begin = None;
                if ready && clock % (RATE as u64 / 2) < OUT as u64 {
                    match beatmatch(&shared, &mut decks, to) {
                        Ok(note) => {
                            lock = Some(Lock::new(to, load_f32(&shared.decks[to].rate), clock));
                            begin = Some(note);
                        }
                        // An intro can read as the wrong tempo for a while; keep
                        // listening in case it settles into something matchable.
                        Err(e) => *problem = Some(e.to_string()),
                    }
                }
                if begin.is_none() && clock >= until {
                    let why = problem.take().unwrap_or_else(|| "No clear beat to match".to_string());
                    begin = Some(format!("{why}; plain blend instead"));
                }
                if let Some(note) = begin {
                    shared.info.lock().unwrap().notes.push(note);
                    auto = Auto::Fade { to, from_value: load_f32(&shared.fader), start: clock, length: (seconds as f64 * RATE) as u64 };
                }
            }
            Auto::Fade { to, from_value, start, length } => {
                let t = ((clock - start) as f32 / length.max(1) as f32).min(1.0);
                let target = to as f32;
                shared.fader.store((from_value + (target - from_value) * t).to_bits(), Ordering::Relaxed);
                progress = Some((to, t, false));
                if t >= 1.0 {
                    auto = Auto::Idle;
                    progress = None;
                    // The outgoing deck has done its job.
                    shared.decks[1 - to].playing.store(false, Ordering::Relaxed);
                    shared.info.lock().unwrap().finished.push(1 - to);
                }
            }
        }

        // ---- keep matched decks together, for as long as both are playing
        let both = shared.decks[0].playing.load(Ordering::Relaxed) && shared.decks[1].playing.load(Ordering::Relaxed);
        match lock.as_mut() {
            Some(l) if both => l.steer(&shared, &decks, clock),
            Some(_) => lock = None,
            None => {}
        }

        // ---- pull each deck
        let mut any = false;
        for d in 0..2 {
            let deck = &shared.decks[d];
            let out = &mut scratch[d];
            if !deck.playing.load(Ordering::Relaxed) {
                out.fill(0.0);
                decks[d].level *= 0.9;
                decks[d].was_playing = false;
                decks[d].tempo.idle(OUT);
                continue;
            }
            if !std::mem::replace(&mut decks[d].was_playing, true) {
                // Fresh start: measure the beat from here, not from the silence before.
                decks[d].tempo.reset();
                decks[d].eq.reset();
            }
            any = true;
            let rate = load_f32(&deck.rate) as f64;
            let mut feed = deck.feed.lock().unwrap();
            let real = pull(&mut feed, &mut decks[d], rate, out);
            let ran_out = real == 0 && feed.ended && feed.buf.len() < 4;
            drop(feed);
            if ran_out {
                deck.playing.store(false, Ordering::Relaxed);
                shared.info.lock().unwrap().finished.push(d);
            }
            if d == 0 {
                if let Some(tape) = CAPTURE.lock().unwrap().as_mut() {
                    tape.extend(out.iter().map(|v| *v as f32));
                }
            }
            // The beat tracker hears the song as recorded: a killed bass line
            // must not cost it the kick drum it counts by.
            decks[d].tempo.feed(out);
            decks[d].eq.run(out, std::array::from_fn(|band| deck.eq[band].load(Ordering::Relaxed)));
            let peak = out.iter().fold(0.0f64, |m, v| m.max(v.abs())) as f32;
            decks[d].level = decks[d].level * 0.9 + peak * 0.1;
        }

        if !any {
            if started {
                if let Some(out) = &out {
                    out.set_playing(false);
                }
                started = false;
                monitor.reset(&shared.hub);
            }
            publish(&shared, &decks, progress, locked_deck(&lock));
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

        // ---- crossfade, sum, master volume
        let fader = load_f32(&shared.fader).clamp(0.0, 1.0);
        let target = volume.attenuation_factor();
        let mut mix = vec![0.0f64; OUT * 2];
        for i in 0..OUT {
            let k = i as f32 / OUT as f32;
            // Glide the crossfader and volume across the block: no zipper noise.
            let f = (last_fader + (fader - last_fader) * k) as f64;
            let (ga, gb) = ((f * std::f64::consts::FRAC_PI_2).cos(), (f * std::f64::consts::FRAC_PI_2).sin());
            for c in 0..2 {
                mix[2 * i + c] = scratch[0][2 * i + c] * ga + scratch[1][2 * i + c] * gb;
            }
        }
        last_fader = fader;
        monitor.feed(&mix, &shared.hub, out.as_ref());
        let step = (target - gain) / OUT as f64;
        for pair in mix.chunks_exact_mut(2) {
            gain += step;
            for s in pair {
                // Two songs at once can overshoot; round the peaks off instead of clipping.
                let v = *s * gain;
                *s = if v.abs() > 0.9 { v.signum() * (0.9 + 0.1 * ((v.abs() - 0.9) / 0.1).tanh()) } else { v };
            }
        }
        gain = target;
        clock += OUT as u64;
        publish(&shared, &decks, progress, locked_deck(&lock));

        match &out {
            Some(out) => out.write(&mix),
            None => std::thread::sleep(Duration::from_secs_f64(OUT as f64 / RATE)),
        }
    }
    monitor.reset(&shared.hub);
}

fn publish(shared: &Shared, decks: &[DeckMix; 2], auto: Option<(usize, f32, bool)>, locked: Option<usize>) {
    let mut info = shared.info.lock().unwrap();
    info.locked = locked;
    for d in 0..2 {
        info.meters[d] = DeckMeter {
            bpm: decks[d].tempo.bpm,
            phase: decks[d].tempo.phase(),
            level: decks[d].level,
            bands: decks[d].eq.level,
        };
    }
    info.auto = auto;
}

fn locked_deck(lock: &Option<Lock>) -> Option<usize> {
    lock.as_ref().map(|l| l.follower)
}

/// Bend deck `d`'s tempo to the other deck's and slide it so the beats coincide.
fn beatmatch(shared: &Shared, decks: &mut [DeckMix; 2], d: usize) -> Result<String> {
    let (lead_bpm, lead_period, lead_beat) = {
        let t = &decks[1 - d].tempo;
        (t.bpm, t.period, t.beat_at)
    };
    let (follow_bpm, period, follow_beat, now) = {
        let t = &decks[d].tempo;
        (t.bpm, t.period as f64, t.beat_at, t.now as f64)
    };
    let (Some(lead_bpm), Some(follow_bpm)) = (lead_bpm, follow_bpm) else {
        return Err(anyhow!("Both decks need to play a few seconds before they can be matched"));
    };
    let ratio = match_ratio(follow_bpm, lead_bpm)
        .ok_or_else(|| anyhow!("{follow_bpm:.0} and {lead_bpm:.0} BPM are too far apart to match"))?;
    let new_rate = load_f32(&shared.decks[d].rate) * ratio;
    if !((1.0 - MAX_BEND)..=(1.0 + MAX_BEND)).contains(&new_rate) {
        return Err(anyhow!("Matching would bend the tempo more than {:.0}%", MAX_BEND * 100.0));
    }

    // Where the follower's next beat falls once it runs at the new speed.
    let until_next = (follow_beat - now).rem_euclid(period);
    let next_beat = now + until_next / ratio as f64;
    let offset = beat_offset(lead_beat, next_beat, lead_period as f64);
    // Delaying is always possible; advancing needs audio already decoded. A
    // delay of one full beat minus the advance lands in the same place.
    let delay_steps = if offset >= 0.0 { offset } else { offset + lead_period as f64 };

    shared.decks[d].rate.store(new_rate.to_bits(), Ordering::Relaxed);
    decks[d].hold += (delay_steps * HOP as f64) as usize;
    // The follower now shares the leader's grid; carry that over so the
    // display and any further matching stay right while it re-measures.
    let tempo = &mut decks[d].tempo;
    tempo.reset();
    tempo.bpm = Some(lead_bpm);
    tempo.period = lead_period;
    tempo.beat_at = lead_beat;
    Ok(format!("Beat-matched at {lead_bpm:.0} BPM ({:+.1}% tempo)", (new_rate - 1.0) * 100.0))
}

// ---- the interface's handle -------------------------------------------------

/// One deck as the interface draws it.
#[derive(Clone, Default)]
pub struct DeckView {
    pub track: Option<Track>,
    pub playing: bool,
    pub loading: bool,
    pub ended: bool,
    pub failed: bool,
    pub pos_ms: u32,
    /// Tempo change from the song's own, e.g. 0.03 for +3%.
    pub bend: f32,
    /// EQ knobs for low, mid and high, in steps from flat.
    pub eq: [i8; 3],
    pub meter: DeckMeter,
}

#[derive(Clone, Default)]
pub struct View {
    pub decks: [DeckView; 2],
    /// 0 is all deck A, 1 is all deck B.
    pub fader: f32,
    pub auto: Option<(usize, f32, bool)>,
    /// The deck being held on the other's beat, if any.
    pub locked: Option<usize>,
}

pub struct Dj {
    shared: Arc<Shared>,
    players: [Arc<Player>; 2],
    tracks: [Option<Track>; 2],
}

impl Dj {
    pub fn start(
        session: Session,
        volume: Box<dyn VolumeGetter + Send>,
        hub: Arc<Hub>,
        bitrate: Bitrate,
        normalise: bool,
    ) -> Self {
        let shared = Arc::new(Shared {
            decks: [DeckShared::default(), DeckShared::default()],
            fader: AtomicU32::new(0.0f32.to_bits()),
            requests: Mutex::default(),
            info: Mutex::default(),
            quit: AtomicBool::new(false),
            wake: (Mutex::new(()), Condvar::new()),
            hub,
        });
        let players = [0, 1].map(|deck| {
            let sink_shared = shared.clone();
            let config = PlayerConfig { bitrate, normalisation: normalise, ..Default::default() };
            let player = Player::new(config, session.clone(), Box::new(NoOpVolume), move || -> Box<dyn Sink> {
                Box::new(DeckSink { shared: sink_shared, deck })
            });
            *shared.decks[deck].events.lock().unwrap() = Some(player.get_player_event_channel());
            player
        });
        let mixer_shared = shared.clone();
        std::thread::Builder::new()
            .name("riff-dj".into())
            .spawn(move || mixer_loop(mixer_shared, volume))
            .ok();
        Self { shared, players, tracks: [None, None] }
    }

    /// After a reconnect the decks must fetch audio through the new session.
    pub fn set_session(&self, session: &Session) {
        for p in &self.players {
            p.set_session(session.clone());
        }
    }

    pub fn load(&mut self, deck: usize, track: Track, start_ms: u32, play: bool) -> Result<()> {
        let uri = SpotifyUri::from_uri(&track.uri).map_err(|_| anyhow!("That can't be loaded on a deck"))?;
        let d = &self.shared.decks[deck];
        d.rate.store(1.0f32.to_bits(), Ordering::Relaxed);
        {
            let mut feed = d.feed.lock().unwrap();
            feed.buf.clear();
            feed.generation += 1;
            feed.loading = true;
            feed.ended = false;
            feed.failed = false;
            feed.pos = frames(start_ms);
            feed.dur = frames(track.duration_ms);
        }
        d.playing.store(play, Ordering::Relaxed);
        self.players[deck].load(uri, play, start_ms);
        self.tracks[deck] = Some(track);
        self.shared.wake.1.notify_all();
        Ok(())
    }

    pub fn is_loaded(&self, deck: usize) -> bool {
        self.tracks[deck].is_some()
    }

    pub fn is_playing(&self, deck: usize) -> bool {
        self.shared.decks[deck].playing.load(Ordering::Relaxed)
    }

    pub fn set_playing(&mut self, deck: usize, play: bool) {
        if self.tracks[deck].is_none() {
            return;
        }
        // The mixer obeys this flag at once; the decoder follows a moment later.
        self.shared.decks[deck].playing.store(play, Ordering::Relaxed);
        if play {
            let ended = self.shared.decks[deck].feed.lock().unwrap().ended;
            if ended {
                // Played to the end: start it again from the top.
                if let Some(track) = self.tracks[deck].clone() {
                    let _ = self.load(deck, track, 0, true);
                }
                return;
            }
            self.players[deck].play();
        } else {
            self.players[deck].pause();
        }
        self.shared.wake.1.notify_all();
    }

    pub fn toggle(&mut self, deck: usize) {
        let now = self.is_playing(deck);
        self.set_playing(deck, !now);
    }

    pub fn seek_by(&mut self, deck: usize, delta_ms: i64) {
        let Some(track) = &self.tracks[deck] else { return };
        let now = self.position(deck) as i64;
        let to = (now + delta_ms).clamp(0, (track.duration_ms as i64 - 1000).max(0)) as u32;
        self.players[deck].seek(to);
    }

    fn position(&self, deck: usize) -> u32 {
        let feed = self.shared.decks[deck].feed.lock().unwrap();
        let heard = feed.pos.saturating_sub((feed.buf.len() / 2) as u64);
        (heard * 10 / 441) as u32
    }

    /// Change a deck's tempo by `delta` (0.005 = half a percent).
    pub fn bend(&self, deck: usize, delta: f32) -> f32 {
        self.request(Request::Unlock);
        let d = &self.shared.decks[deck];
        let rate = if delta == 0.0 { 1.0 } else { (load_f32(&d.rate) + delta).clamp(1.0 - MAX_BEND, 1.0 + MAX_BEND) };
        d.rate.store(rate.to_bits(), Ordering::Relaxed);
        rate - 1.0
    }

    pub fn eq(&self, deck: usize) -> [i8; 3] {
        std::array::from_fn(|band| self.shared.decks[deck].eq[band].load(Ordering::Relaxed))
    }

    /// Put one band of a deck's EQ at `step` (see `EQ_KILL`, `EQ_MAX`).
    pub fn set_eq(&self, deck: usize, band: usize, step: i8) {
        self.shared.decks[deck].eq[band].store(step.clamp(EQ_KILL, EQ_MAX), Ordering::Relaxed);
    }

    pub fn fader(&self) -> f32 {
        load_f32(&self.shared.fader)
    }

    /// Move the crossfader by hand; this takes over from any transition in progress.
    pub fn set_fader(&self, value: f32) {
        self.request(Request::Cancel);
        self.shared.fader.store(value.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    fn request(&self, r: Request) {
        self.shared.requests.lock().unwrap().push(r);
        self.shared.wake.1.notify_all();
    }

    /// The deck that is currently heard most.
    pub fn live_deck(&self) -> usize {
        match (self.is_playing(0), self.is_playing(1)) {
            (true, false) => 0,
            (false, true) => 1,
            _ => (self.fader() > 0.5) as usize,
        }
    }

    /// Bring the other deck in: start it if needed, match its beat to the one
    /// playing, and glide the crossfader across over `seconds`.
    pub fn mix(&mut self, seconds: f32) -> Result<usize> {
        let from = self.live_deck();
        let to = 1 - from;
        if self.tracks[to].is_none() {
            return Err(anyhow!("Load a song on deck {} first: press {} on a song", deck_name(to), to + 1));
        }
        let already = self.is_playing(to);
        if !already {
            // Make sure it comes in silent, then let it run.
            self.shared.fader.store((from as f32).to_bits(), Ordering::Relaxed);
            self.set_playing(to, true);
        }
        // Matching shifts the incoming deck in time, which is only done while it
        // can't be heard. If you have already brought it up, it is left alone.
        let fader = self.fader();
        let silent = if to == 1 { fader < 0.05 } else { fader > 0.95 };
        self.request(Request::Mix { to, seconds, beatmatch: silent && self.is_playing(from) });
        Ok(to)
    }

    pub fn sync(&self, deck: usize) {
        self.request(Request::Sync { deck });
    }

    pub fn nudge(&self, deck: usize, ms: f32) {
        self.request(Request::Nudge { deck, ms });
    }

    /// Everything the interface needs to draw the decks. Also carries out what
    /// the mixer has asked for (stopping a finished deck) and returns notes
    /// for the listener.
    pub fn view(&mut self) -> (View, Vec<String>) {
        let (info, notes, finished) = {
            let mut info = self.shared.info.lock().unwrap();
            let notes = std::mem::take(&mut info.notes);
            let finished = std::mem::take(&mut info.finished);
            (info.clone(), notes, finished)
        };
        for deck in finished {
            self.players[deck].pause();
        }
        let mut view = View { fader: self.fader(), auto: info.auto, locked: info.locked, ..Default::default() };
        for d in 0..2 {
            let (loading, ended, failed) = {
                let deck = &self.shared.decks[d];
                let mut feed = deck.feed.lock().unwrap();
                pump(deck, &mut feed);
                (feed.loading, feed.ended && feed.buf.len() < 4, feed.failed)
            };
            view.decks[d] = DeckView {
                track: self.tracks[d].clone(),
                playing: self.is_playing(d),
                loading,
                ended,
                failed,
                pos_ms: self.position(d),
                bend: load_f32(&self.shared.decks[d].rate) - 1.0,
                eq: self.eq(d),
                meter: info.meters[d],
            };
        }
        (view, notes)
    }
}

impl Drop for Dj {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
        for p in &self.players {
            p.stop();
        }
        self.shared.wake.1.notify_all();
    }
}

pub fn deck_name(deck: usize) -> &'static str {
    if deck == 0 { "A" } else { "B" }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A kick drum every beat with some hiss, `seconds` long, first beat at `offset` s.
    fn click_track(bpm: f64, offset: f64, seconds: f64) -> Vec<f64> {
        let total = (RATE * seconds) as usize;
        let beat = 60.0 / bpm;
        let mut out = Vec::with_capacity(total * 2);
        for i in 0..total {
            let t = i as f64 / RATE;
            let into = (t - offset).rem_euclid(beat);
            let kick = if into < 0.09 { (into * 58.0 * std::f64::consts::TAU).sin() * (1.0 - into / 0.09).powi(2) } else { 0.0 };
            let hiss = (((i * 7919) % 1009) as f64 / 1009.0 - 0.5) * 0.03;
            out.extend([kick * 0.8 + hiss; 2]);
        }
        out
    }

    fn analyse(bpm: f64, offset: f64) -> Tempo {
        let mut tempo = Tempo::default();
        for chunk in click_track(bpm, offset, 12.0).chunks(OUT * 2) {
            tempo.feed(chunk);
        }
        tempo
    }

    #[test]
    fn finds_the_tempo() {
        for bpm in [92.0, 120.0, 128.0, 140.0, 174.0] {
            let found = analyse(bpm, 0.1).bpm.unwrap_or(0.0) as f64;
            // Half or double time is an acceptable reading of the same pulse.
            let ok = [0.5, 1.0, 2.0].iter().any(|m| (found - bpm * m).abs() < 0.6);
            assert!(ok, "{bpm} BPM read as {found:.2}");
        }
    }

    #[test]
    fn silence_and_noise_have_no_tempo() {
        let mut tempo = Tempo::default();
        tempo.feed(&vec![0.0; (RATE * 8.0) as usize * 2]);
        assert!(tempo.bpm.is_none());
    }

    #[test]
    fn finds_where_the_beats_fall() {
        // Two tracks at the same tempo, one 150 ms behind the other.
        let (a, b) = (analyse(120.0, 0.10), analyse(120.0, 0.25));
        let period = a.period as f64;
        let offset_steps = beat_offset(a.beat_at, b.beat_at, period);
        let offset_ms = offset_steps / HOPS_PER_SEC as f64 * 1000.0;
        // b's beats come 150 ms after a's, so b must move 150 ms earlier.
        assert!((offset_ms + 150.0).abs() < 8.0, "measured {offset_ms:.1} ms");
    }

    #[test]
    fn matching_picks_the_nearest_octave() {
        assert!((match_ratio(120.0, 126.0).unwrap() - 1.05).abs() < 1e-4);
        // 87 against 174 is the same pulse counted two ways: no change needed.
        assert!((match_ratio(87.0, 174.0).unwrap() - 1.0).abs() < 1e-4);
        assert!((match_ratio(170.0, 88.0).unwrap() - 88.0 * 2.0 / 170.0).abs() < 1e-4);
        // A waltz against drum and bass is not happening.
        assert!(match_ratio(100.0, 135.0).is_none());
    }

    #[test]
    fn offsets_take_the_short_way_round() {
        assert_eq!(beat_offset(10.0, 4.0, 100.0), 6.0);
        assert_eq!(beat_offset(4.0, 10.0, 100.0), -6.0);
        assert_eq!(beat_offset(95.0, 5.0, 100.0), -10.0);
        assert_eq!(beat_offset(1005.0, 95.0, 100.0), 10.0);
    }

    #[test]
    fn pull_at_normal_speed_is_exact_and_resampling_keeps_pitch_ratio() {
        let source: Vec<f64> = (0..4000).flat_map(|i| [i as f64, -(i as f64)]).collect();
        let mut feed = Feed { buf: source.iter().copied().collect(), ..Default::default() };
        let mut mix = DeckMix::default();
        let mut out = vec![0.0; OUT * 2];
        assert_eq!(pull(&mut feed, &mut mix, 1.0, &mut out), OUT);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[2 * 100], 100.0);
        assert_eq!(out[2 * 100 + 1], -100.0);
        assert_eq!(feed.buf.len(), source.len() - OUT * 2);

        // 5% faster: 256 output frames consume about 269 input frames.
        let before = feed.buf.len();
        pull(&mut feed, &mut mix, 1.05, &mut out);
        let used = (before - feed.buf.len()) / 2;
        assert!((268..=270).contains(&used), "{used}");
        // Still a smooth ramp, now stepping by 1.05.
        assert!((out[2 * 11] - out[2 * 10] - 1.05).abs() < 1e-6);
    }

    #[test]
    fn hold_delays_a_deck_by_exactly_that_much() {
        let mut feed = Feed { buf: std::iter::repeat_n(1.0, 4000).collect(), ..Default::default() };
        let mut mix = DeckMix { hold: 100, ..Default::default() };
        let mut out = vec![0.0; OUT * 2];
        let real = pull(&mut feed, &mut mix, 1.0, &mut out);
        assert_eq!(real, OUT - 100);
        assert_eq!(out[2 * 99], 0.0);
        assert_eq!(out[2 * 100], 1.0);
    }

    /// Loudness of a steady tone after the EQ at `steps`, relative to the tone itself.
    fn through_eq(hz: f64, steps: [i8; 3]) -> f64 {
        let mut eq = Eq::default();
        let tone = |i: usize| (i as f64 * std::f64::consts::TAU * hz / RATE).sin() * 0.5;
        let (mut heard, mut sent) = (0.0, 0.0);
        for block in 0..80 {
            let mut audio: Vec<f64> = (0..OUT).flat_map(|i| [tone(block * OUT + i); 2]).collect();
            eq.run(&mut audio, steps);
            // Skip the first blocks while the filters settle.
            if block >= 40 {
                heard += audio.iter().map(|v| v * v).sum::<f64>();
                sent += (0..OUT).map(|i| 2.0 * tone(block * OUT + i).powi(2)).sum::<f64>();
            }
        }
        (heard / sent).sqrt()
    }

    #[test]
    fn a_flat_eq_changes_nothing() {
        for hz in [50.0, 120.0, 250.0, 600.0, 1000.0, 2500.0, 6000.0, 12000.0] {
            let level = through_eq(hz, [0, 0, 0]);
            assert!((level - 1.0).abs() < 0.03, "{hz} Hz came out at {level}");
        }
    }

    #[test]
    fn each_band_is_cut_and_boosted_on_its_own() {
        // A bass kill takes the kick out and leaves the rest.
        assert!(through_eq(60.0, [EQ_KILL, 0, 0]) < 0.02);
        assert!((through_eq(1000.0, [EQ_KILL, 0, 0]) - 1.0).abs() < 0.05);
        assert!((through_eq(8000.0, [EQ_KILL, 0, 0]) - 1.0).abs() < 0.05);
        // Mids and highs likewise.
        assert!(through_eq(900.0, [0, EQ_KILL, 0]) < 0.1);
        assert!((through_eq(60.0, [0, EQ_KILL, 0]) - 1.0).abs() < 0.05);
        assert!(through_eq(10_000.0, [0, 0, EQ_KILL]) < 0.02);
        // One step down is 3 dB, the top step is +6 dB.
        assert!((through_eq(60.0, [-1, 0, 0]) - 0.708).abs() < 0.03);
        assert!((through_eq(60.0, [EQ_MAX, 0, 0]) - 1.995).abs() < 0.06);
    }

    #[test]
    fn running_dry_gives_silence_not_garbage() {
        let mut feed = Feed { buf: std::iter::repeat_n(0.5, 20).collect(), ..Default::default() };
        let mut mix = DeckMix::default();
        let mut out = vec![9.0; OUT * 2];
        let real = pull(&mut feed, &mut mix, 1.0, &mut out);
        assert_eq!(real, 9);
        assert!(out[2 * 9..].iter().all(|v| *v == 0.0));
    }
}
