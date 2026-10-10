//! The audio output stage. Everything the player decodes passes through here
//! on its way to the sound card, which is what makes three things possible:
//!
//! - **Mixing**: the end of one song is blended into the start of the next.
//! - **Analysis**: a spectrum and beat detector, timed to what is being heard.
//! - **An exact clock**: the position of the sound actually leaving the
//!   speakers, rather than of whatever the decoder has got to.
//!
//! Layout: the player thread hands packets to [`MixSink`], which places them in
//! a short queue. A dedicated output thread drains that queue into the real
//! audio backend. A crossfade is done by mixing the incoming song directly into
//! the not-yet-played tail of the outgoing one, while it waits in the queue.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::out::Output;
use librespot_playback::audio_backend::{Sink, SinkResult};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use librespot_playback::mixer::VolumeGetter;
use librespot_playback::player::{PlayerEvent, PlayerEventChannel};
use rustfft::{Fft, FftPlanner, num_complex::Complex};

pub const BANDS: usize = 48;
pub const RATE: f64 = 44_100.0;
const FFT: usize = 2048;
const HOP: usize = 1024;

/// Samples (both channels) handed to the sound card at a time: about 6 ms.
const CHUNK: usize = 512;
/// Queue kept between player and output in normal running: 0.2 s.
const BASE: usize = 17_640;
/// Extra unmixed lead-in kept ahead of a crossfade: 0.5 s.
const MARGIN: usize = 44_100;
/// How long before the end of a song its tail starts being gathered.
const LEAD: u64 = 66_150;
/// Short blend used for skips and seeks so they never click: 0.12 s.
const MICRO: usize = 10_584;

pub fn frames(ms: u32) -> u64 {
    (ms as u64 * 441) / 10
}

// ---- what the rest of the app sees ----------------------------------------

struct Frame {
    at: Instant,
    bands: [f32; BANDS],
    beat: bool,
    bass: f32,
}

/// One look at the music, as it sounds right now.
#[derive(Clone, Copy)]
pub struct Snapshot {
    pub bands: [f32; BANDS],
    /// A beat landed since the previous poll.
    pub beat: bool,
    /// Low-end energy, 0..1.
    pub bass: f32,
}

#[derive(Default)]
struct Clock {
    uri: String,
    pos_ms: i64,
    at: Option<Instant>,
    running: bool,
}

/// Shared between the interface, the engine and the audio threads.
#[derive(Default)]
pub struct Hub {
    frames: Mutex<VecDeque<Frame>>,
    clock: Mutex<Clock>,
    /// Crossfade length in milliseconds; 0 turns mixing off.
    mix_ms: AtomicU32,
    events: Mutex<Option<PlayerEventChannel>>,
    /// Sound waiting inside the backend's own buffer, in frames.
    backend_lag: AtomicU64,
    /// Chunks played from a stretch the incoming song had not reached yet.
    blend_overruns: AtomicU32,
    /// Delay between audio leaving the mixer and being heard, in milliseconds (f32 bits).
    output_ms: AtomicU32,
    /// Times the sound card ran out of audio.
    dropouts: AtomicU32,
}

impl Hub {
    /// The analysis frame for this instant, if audio is flowing.
    pub fn poll(&self) -> Option<Snapshot> {
        let now = Instant::now();
        let mut frames = self.frames.lock().unwrap();
        let mut beat = false;
        while frames.len() > 1 && frames[1].at <= now {
            beat |= frames.pop_front().is_some_and(|f| f.beat);
        }
        let f = frames.front_mut()?;
        if f.at > now || now.duration_since(f.at) > Duration::from_millis(250) {
            return None;
        }
        // A beat is reported once, then cleared, so a slow poller never misses one.
        beat |= std::mem::take(&mut f.beat);
        Some(Snapshot { bands: f.bands, beat, bass: f.bass })
    }

    /// Where `uri` is in its playback, to the millisecond, if it is the track
    /// sounding now. May be slightly negative while the previous song fades out.
    pub fn position(&self, uri: &str) -> Option<i64> {
        let c = self.clock.lock().unwrap();
        if c.uri != uri {
            return None;
        }
        let at = c.at?;
        Some(c.pos_ms + if c.running { at.elapsed().as_millis() as i64 } else { 0 })
    }

    pub fn set_mix(&self, seconds: u32) {
        self.mix_ms.store(seconds.min(12) * 1000, Ordering::Relaxed);
    }

    /// Give the sink the player's event stream; it reads track boundaries from it.
    pub fn attach(&self, events: PlayerEventChannel) {
        *self.events.lock().unwrap() = Some(events);
    }

    /// The player has jumped to `ms`. While paused no audio flows to carry
    /// the news to the clock, so it is told directly.
    pub fn seeked(&self, ms: u32) {
        let mut clock = self.clock.lock().unwrap();
        if !clock.running {
            clock.pos_ms = ms as i64;
            clock.at = Some(Instant::now());
        }
    }

    /// How many chunks of output came from a stretch of a blend the incoming
    /// song had not reached. Zero unless that song stalled: playback should
    /// never overtake a blend that is being fed.
    pub fn blend_overruns(&self) -> u32 {
        self.blend_overruns.load(Ordering::Relaxed)
    }

    /// How long after a change is made it is heard, in milliseconds, and how
    /// often the sound card has run dry.
    pub fn output_health(&self) -> (f32, u32) {
        (f32::from_bits(self.output_ms.load(Ordering::Relaxed)), self.dropouts.load(Ordering::Relaxed))
    }

    fn mix_samples(&self) -> usize {
        (frames(self.mix_ms.load(Ordering::Relaxed)) * 2) as usize
    }
}

// ---- crossfading ----------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Entry {
    /// The previous song ran out: blend into this one.
    Natural,
    /// The listener jumped (skip, seek, picked a song): cut over at once.
    Jump,
}

/// Places incoming audio into the pending queue, overlapping it with what is
/// already there when a song is starting.
#[derive(Default)]
pub struct Deck {
    overlap_left: usize,
    overlap_total: usize,
    /// See `Hub::blend_overruns`.
    overruns: u32,
}

impl Deck {
    /// A new stretch of audio is about to arrive. Marks the end of the queue as
    /// the region it will be blended into.
    pub fn begin(&mut self, entry: Entry, mix: usize, queue: &mut VecDeque<f64>) {
        let region = match entry {
            Entry::Natural => mix.min(queue.len()),
            Entry::Jump => {
                // Drop what was queued of the old audio, keeping a sliver to fade.
                queue.truncate(MICRO.min(queue.len()) & !1);
                queue.len()
            }
        } & !1;
        self.overlap_total = region;
        self.overlap_left = region;
    }

    /// Samples still to be blended; they are not new queue length.
    pub fn pending_overlap(&self) -> usize {
        self.overlap_left
    }

    pub fn push(&mut self, mut samples: &[f64], queue: &mut VecDeque<f64>) {
        if self.overlap_left > 0 {
            // `played` keeps the region within the queue as the output eats
            // into it; this only guards against the two getting out of step.
            self.overlap_left = self.overlap_left.min(queue.len() & !1);
            let n = self.overlap_left.min(samples.len()) & !1;
            let base = queue.len() - self.overlap_left;
            let done = self.overlap_total - self.overlap_left;
            for (i, incoming) in samples[..n].iter().enumerate() {
                // Equal-power curve: perceived loudness stays level through the blend.
                let k = ((done + i) / 2) as f64 / (self.overlap_total / 2).max(1) as f64;
                let angle = k * std::f64::consts::FRAC_PI_2;
                let mixed = queue[base + i] * angle.cos() + incoming * angle.sin();
                queue[base + i] = mixed.clamp(-1.0, 1.0);
            }
            self.overlap_left -= n;
            samples = &samples[n..];
        }
        queue.extend(samples);
    }

    /// The output is about to play `chunk`, taken from the front of a queue
    /// that held `queued` samples. Normally all of it is finished audio. If
    /// the incoming song stalls part-way through a blend (a skip, a seek, a
    /// slow connection), playback reaches the part of the old song it never
    /// got to. That part goes out on the fade it was following, rather than
    /// jumping back to full level; the incoming song rejoins where the fade
    /// has got to.
    pub fn played(&mut self, chunk: &mut [f64], queued: usize) {
        let finished = queued.saturating_sub(self.overlap_left);
        if chunk.len() <= finished {
            return;
        }
        let done = self.overlap_total - self.overlap_left;
        for (i, sample) in chunk[finished..].iter_mut().enumerate() {
            let k = ((done + i) / 2) as f64 / (self.overlap_total / 2).max(1) as f64;
            *sample *= (k * std::f64::consts::FRAC_PI_2).cos();
        }
        self.overlap_left -= chunk.len() - finished;
        self.overruns += 1;
    }
}

/// Whether enough finished audio is waiting ahead of the playhead for the
/// player to be held back a moment. The stretch a new song is still being
/// blended into doesn't count: it is in the queue already, and counting it
/// holds the new song below real time until the playhead overtakes the blend.
fn enough(queued: usize, blending: usize, target: usize) -> bool {
    queued.saturating_sub(blending) > target
}

// ---- the queue between player and output -----------------------------------

#[derive(Default)]
struct Ring {
    buf: VecDeque<f64>,
    /// Where a blend in progress has got to. Kept with the queue because
    /// both the player's thread and the output's need it.
    deck: Deck,
    running: bool,
    /// Play out what is queued, then stop.
    finish: bool,
    quit: bool,
}

#[derive(Default)]
struct Pipe {
    ring: Mutex<Ring>,
    wake: Condvar,
}

/// The sink given to the player.
pub struct MixSink {
    pipe: Arc<Pipe>,
    hub: Arc<Hub>,
    events: Option<PlayerEventChannel>,
    uri: String,
    /// Decode position within the current song, in frames.
    pos: u64,
    dur: u64,
    ended: bool,
    /// The player has the song after this one ready, so there is something
    /// for this one's ending to be blended into.
    next_ready: bool,
    incoming: Option<Entry>,
    seeked: bool,
}

impl MixSink {
    /// The sound card is opened on the output thread, because an audio stream
    /// must stay on the thread that created it. `on_missing` is told if it
    /// can't be opened.
    pub fn new(
        hub: Arc<Hub>,
        volume: Box<dyn VolumeGetter + Send>,
        on_missing: impl FnOnce() + Send + 'static,
    ) -> Self {
        let pipe = Arc::new(Pipe::default());
        let (thread_pipe, thread_hub) = (pipe.clone(), hub.clone());
        std::thread::Builder::new()
            .name("riff-audio".into())
            .spawn(move || output_loop(thread_pipe, thread_hub, volume, on_missing))
            .ok();
        Self {
            pipe,
            hub,
            events: None,
            uri: String::new(),
            pos: 0,
            dur: 0,
            ended: false,
            next_ready: false,
            incoming: None,
            seeked: false,
        }
    }

    /// Read the player's events. They are emitted on this same thread, in order
    /// with the packets, so they mark exactly where one song stops and the next
    /// begins in the stream of samples.
    fn pump(&mut self) {
        if self.events.is_none() {
            self.events = self.hub.events.lock().unwrap().take();
        }
        let Some(rx) = self.events.as_mut() else { return };
        while let Ok(event) = rx.try_recv() {
            match event {
                PlayerEvent::EndOfTrack { .. } => self.ended = true,
                PlayerEvent::Preloading { .. } => self.next_ready = true,
                PlayerEvent::TrackChanged { audio_item } => {
                    self.incoming = Some(if self.ended { Entry::Natural } else { Entry::Jump });
                    self.ended = false;
                    self.next_ready = false;
                    self.uri = audio_item.uri.clone();
                    self.dur = frames(audio_item.duration_ms);
                    self.pos = 0;
                }
                PlayerEvent::Playing { position_ms, .. }
                | PlayerEvent::Loading { position_ms, .. } => self.pos = frames(position_ms),
                PlayerEvent::Seeked { position_ms, .. } => {
                    self.pos = frames(position_ms);
                    self.seeked = true;
                }
                _ => {}
            }
        }
    }

    fn publish(&self, ring: &Ring) {
        let queued = (ring.buf.len().saturating_sub(ring.deck.pending_overlap()) / 2) as i64;
        let backend = self.hub.backend_lag.load(Ordering::Relaxed) as i64;
        let audible = self.pos as i64 - queued - backend;
        let mut clock = self.hub.clock.lock().unwrap();
        if clock.uri != self.uri {
            clock.uri = self.uri.clone();
        }
        clock.pos_ms = audible * 10 / 441;
        clock.at = Some(Instant::now());
        clock.running = ring.running;
    }
}

impl Sink for MixSink {
    fn start(&mut self) -> SinkResult<()> {
        // Pick up anything that happened while paused, a seek above all.
        self.pump();
        let mut ring = self.pipe.ring.lock().unwrap();
        ring.running = true;
        ring.finish = false;
        self.publish(&ring);
        self.pipe.wake.notify_all();
        Ok(())
    }

    fn stop(&mut self) -> SinkResult<()> {
        self.pump();
        let mut ring = self.pipe.ring.lock().unwrap();
        if self.ended {
            // The last song ran out with nothing after it: let its ending play.
            ring.finish = true;
        } else {
            ring.running = false;
        }
        self.publish(&ring);
        self.pipe.wake.notify_all();
        Ok(())
    }

    fn write(&mut self, packet: AudioPacket, _: &mut Converter) -> SinkResult<()> {
        self.pump();
        let Ok(samples) = packet.samples() else { return Ok(()) };
        let mix = self.hub.mix_samples();

        let over = {
            let mut guard = self.pipe.ring.lock().unwrap();
            let ring = &mut *guard;
            if let Some(entry) = self.incoming.take() {
                ring.deck.begin(entry, mix, &mut ring.buf);
                self.seeked = false;
            } else if std::mem::take(&mut self.seeked) {
                ring.deck.begin(Entry::Jump, mix, &mut ring.buf);
            }
            ring.deck.push(samples, &mut ring.buf);
            self.pos += (samples.len() / 2) as u64;
            // Never let a stuck output grow the queue without bound.
            let excess = ring.buf.len().saturating_sub(BASE + 60 * 88_200);
            ring.buf.drain(..excess & !1);
            self.publish(ring);

            // Near the end of a song, gather its tail so the next one can be
            // blended into it; otherwise keep the queue short. The last song
            // of a list has nothing to blend into, and gathering its tail
            // would only have the player report it over while it still sounds.
            let ending = self.dur > 0 && self.dur.saturating_sub(self.pos) <= mix as u64 / 2 + LEAD;
            let target = if mix > 0 && ending && self.next_ready { mix + MARGIN } else { BASE };
            enough(ring.buf.len(), ring.deck.pending_overlap(), target)
        };
        self.pipe.wake.notify_all();

        // Pace the player rather than block it: it is never held for more than
        // a moment, so pause, seek and skip always get through promptly.
        if over {
            let length = Duration::from_secs_f64(samples.len() as f64 / 2.0 / RATE);
            std::thread::sleep(length.mul_f64(1.25).min(Duration::from_millis(120)));
        }
        Ok(())
    }
}

impl Drop for MixSink {
    fn drop(&mut self) {
        self.pipe.ring.lock().unwrap().quit = true;
        self.pipe.wake.notify_all();
    }
}

// ---- output thread ----------------------------------------------------------

fn output_loop(
    pipe: Arc<Pipe>,
    hub: Arc<Hub>,
    volume: Box<dyn VolumeGetter + Send>,
    on_missing: impl FnOnce(),
) {
    // With no output device, carry on silently instead, consuming audio at
    // normal speed so playback state still advances.
    let out = match Output::open() {
        Ok(out) => Some(out),
        Err(e) => {
            log::warn!("audio output: {e}");
            on_missing();
            None
        }
    };
    let mut monitor = Monitor::new();
    let mut started = false;
    let mut gain = volume.attenuation_factor();

    loop {
        let mut chunk: Vec<f64> = {
            let mut ring = pipe.ring.lock().unwrap();
            loop {
                if ring.quit {
                    return;
                }
                if ring.running && ring.finish && ring.buf.len() < 2 {
                    ring.running = false;
                    ring.finish = false;
                }
                if ring.running && ring.buf.len() >= 2 {
                    break;
                }
                if !ring.running && started {
                    break;
                }
                ring = pipe.wake.wait_timeout(ring, Duration::from_millis(250)).unwrap().0;
            }
            if !ring.running {
                Vec::new()
            } else {
                let ring = &mut *ring;
                let queued = ring.buf.len();
                let n = queued.min(CHUNK) & !1;
                let mut chunk: Vec<f64> = ring.buf.drain(..n).collect();
                ring.deck.played(&mut chunk, queued);
                hub.blend_overruns.store(ring.deck.overruns, Ordering::Relaxed);
                chunk
            }
        };

        if chunk.is_empty() {
            if let Some(out) = &out {
                out.set_playing(false);
            }
            started = false;
            monitor.reset(&hub);
            continue;
        }
        if !started {
            if let Some(out) = &out {
                out.set_playing(true);
            }
            started = true;
        }

        monitor.feed(&chunk, &hub, out.as_ref());

        // Volume is applied here, after the queue, so it responds instantly.
        // It glides across the chunk to avoid zipper noise.
        let target = volume.attenuation_factor();
        if gain != 1.0 || target != 1.0 {
            let step = (target - gain) / (chunk.len() / 2).max(1) as f64;
            for pair in chunk.chunks_exact_mut(2) {
                gain += step;
                pair[0] *= gain;
                pair[1] *= gain;
            }
        }
        gain = target;

        match &out {
            Some(out) => out.write(&chunk),
            None => std::thread::sleep(Duration::from_secs_f64(chunk.len() as f64 / 2.0 / RATE)),
        }
    }
}

// ---- spectrum and beat ------------------------------------------------------

/// Watches the audio on its way to the backend: publishes the spectrum and
/// beats, each stamped with the moment it will be heard.
pub struct Monitor {
    analyser: Analyser,
    /// Wall-clock moment at which the first frame of this run was heard.
    anchor: Option<Instant>,
    written: u64,
}

impl Monitor {
    pub fn new() -> Self {
        Self { analyser: Analyser::new(), anchor: None, written: 0 }
    }

    /// Call with each chunk just before it is written to the backend.
    pub fn feed(&mut self, chunk: &[f64], hub: &Hub, out: Option<&Output>) {
        // The sound card drains in real time from the first write. If writes
        // ever fall behind that clock (a stall), restart the clock from here.
        let now = Instant::now();
        let ahead = Duration::from_secs_f64(self.written as f64 / RATE);
        let start = match self.anchor {
            Some(a) if a + ahead >= now => a,
            _ => {
                let a = now.checked_sub(ahead).unwrap_or(now);
                self.anchor = Some(a);
                a
            }
        };
        let heard = (now.duration_since(start).as_secs_f64() * RATE) as u64;
        // What is waiting ahead of the speakers: measured when there is a
        // sound card to ask, reckoned from the clock when there isn't.
        let lag = out.map_or(self.written.saturating_sub(heard), Output::lag_frames);
        hub.backend_lag.store(lag, Ordering::Relaxed);
        if let Some(out) = out {
            hub.output_ms.store((lag as f32 * 1000.0 / RATE as f32).to_bits(), Ordering::Relaxed);
            hub.dropouts.store(out.dropouts(), Ordering::Relaxed);
        }
        self.analyser.feed(chunk, start, self.written, hub);
        self.written += (chunk.len() / 2) as u64;
    }

    /// Output stopped: forget the timeline and clear what was published.
    pub fn reset(&mut self, hub: &Hub) {
        self.anchor = None;
        self.written = 0;
        self.analyser.reset();
        hub.frames.lock().unwrap().clear();
        hub.backend_lag.store(0, Ordering::Relaxed);
    }
}

struct Analyser {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    pending: Vec<f32>,
    /// Edges of each band as FFT bin indices.
    edges: Vec<usize>,
    /// Slow-moving peak used as automatic gain, so quiet tracks still move.
    peak: f32,
    previous: [f32; BANDS],
    /// Recent onset strengths, for the adaptive beat threshold.
    history: VecDeque<f32>,
    since_beat: u32,
    seen: u64,
}

impl Analyser {
    fn new() -> Self {
        let window = (0..FFT)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT as f32).cos())
            .collect();
        // Log-spaced from 40 Hz to 16 kHz: equal screen space per octave.
        let (lo, hi) = (40.0f64, 16_000.0f64);
        let mut edges: Vec<usize> = (0..=BANDS)
            .map(|i| {
                let hz = lo * (hi / lo).powf(i as f64 / BANDS as f64);
                (hz / RATE * FFT as f64).round() as usize
            })
            .collect();
        for i in 1..edges.len() {
            if edges[i] <= edges[i - 1] {
                edges[i] = edges[i - 1] + 1;
            }
        }
        Self {
            fft: FftPlanner::new().plan_fft_forward(FFT),
            window,
            pending: Vec::with_capacity(FFT * 2),
            edges,
            peak: 1e-3,
            previous: [0.0; BANDS],
            history: VecDeque::with_capacity(64),
            since_beat: 0,
            seen: 0,
        }
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.history.clear();
        self.previous = [0.0; BANDS];
    }

    /// `start` is when frame 0 of this run was heard; `offset` is how many
    /// frames of the run came before this chunk.
    fn feed(&mut self, samples: &[f64], start: Instant, offset: u64, hub: &Hub) {
        for (i, pair) in samples.chunks_exact(2).enumerate() {
            self.pending.push(((pair[0] + pair[1]) * 0.5) as f32);
            if self.pending.len() >= FFT {
                let at = start + Duration::from_secs_f64((offset + i as u64) as f64 / RATE);
                let frame = self.frame(at);
                let mut frames = hub.frames.lock().unwrap();
                if frames.len() > 256 {
                    frames.pop_front();
                }
                frames.push_back(frame);
                drop(frames);
                self.pending.drain(..HOP);
            }
        }
    }

    fn frame(&mut self, at: Instant) -> Frame {
        let mut buf: Vec<Complex<f32>> = self.pending[..FFT]
            .iter()
            .zip(&self.window)
            .map(|(s, w)| Complex::new(s * w, 0.0))
            .collect();
        self.fft.process(&mut buf);

        let mut bands = [0f32; BANDS];
        let mut loudest = 0f32;
        for (i, band) in bands.iter_mut().enumerate() {
            let (a, b) = (self.edges[i], self.edges[i + 1].min(FFT / 2));
            let mut sum = 0f32;
            for c in &buf[a.min(b)..b] {
                sum += c.norm_sqr();
            }
            let rms = (sum / (b.saturating_sub(a)).max(1) as f32).sqrt();
            // Tilt upward with frequency: music carries far less energy up high.
            *band = rms * (1.0 + i as f32 / BANDS as f32 * 3.0);
            loudest = loudest.max(*band);
        }
        self.peak = (self.peak * 0.995).max(loudest).max(1e-3);
        for band in &mut bands {
            *band = (*band / self.peak).powf(0.6).clamp(0.0, 1.0);
        }

        // Beat = a sudden rise in energy, weighted towards the low end where
        // kicks and bass live, that stands out from the last second or so.
        let mut onset = 0f32;
        for (i, (now, before)) in bands.iter().zip(&self.previous).enumerate() {
            let weight = if i < 10 { 2.5 } else if i < 24 { 1.0 } else { 0.4 };
            onset += (now - before).max(0.0) * weight;
        }
        self.previous = bands;
        let n = self.history.len().max(1) as f32;
        let mean = self.history.iter().sum::<f32>() / n;
        let spread = (self.history.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / n).sqrt();
        self.since_beat += 1;
        self.seen += 1;
        // At most one beat per ~0.26 s, i.e. up to about 230 per minute.
        let beat = self.history.len() >= 12
            && self.since_beat >= 11
            && onset > mean + spread * 1.4
            && onset > 0.6;
        if beat {
            self.since_beat = 0;
        }
        if self.history.len() >= 43 {
            self.history.pop_front();
        }
        self.history.push_back(onset);

        let bass = bands[..8].iter().sum::<f32>() / 8.0;
        Frame { at, bands, beat, bass }
    }
}

/// Keeps the "no audio device" notice from repeating on every reconnect.
pub static WARNED_NO_DEVICE: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_of(v: f64, n: usize) -> VecDeque<f64> {
        std::iter::repeat_n(v, n).collect()
    }

    #[test]
    fn natural_entry_overlaps_instead_of_appending() {
        let mut q = queue_of(1.0, 1000);
        let mut deck = Deck::default();
        deck.begin(Entry::Natural, 400, &mut q);
        assert_eq!(deck.pending_overlap(), 400);
        deck.push(&vec![1.0; 600], &mut q);
        // 400 samples were blended in place; only the other 200 extended the queue.
        assert_eq!(q.len(), 1200);
        assert_eq!(deck.pending_overlap(), 0);
        // Before the region: untouched. Start of region: all outgoing song.
        assert_eq!(q[599], 1.0);
        assert!((q[600] - 1.0).abs() < 1e-9);
        // After the region: all incoming song.
        assert_eq!(q[1000], 1.0);
    }

    #[test]
    fn blend_keeps_power_level_and_never_clips() {
        // Two unrelated signals of equal power should hold that power through the blend.
        let a: Vec<f64> = (0..4000).map(|i| (i as f64 * 0.37).sin() * 0.5).collect();
        let b: Vec<f64> = (0..4000).map(|i| (i as f64 * 0.91 + 1.0).cos() * 0.5).collect();
        let mut q: VecDeque<f64> = a.iter().copied().collect();
        let mut deck = Deck::default();
        deck.begin(Entry::Natural, 4000, &mut q);
        deck.push(&b, &mut q);
        let power = |s: &[f64]| s.iter().map(|v| v * v).sum::<f64>() / s.len() as f64;
        let mid: Vec<f64> = q.iter().skip(1500).take(1000).copied().collect();
        let reference = power(&a[1500..2500]);
        assert!((power(&mid) / reference - 1.0).abs() < 0.25, "power drifted");
        assert!(q.iter().all(|v| v.abs() <= 1.0));
        // Fully correlated worst case is limited rather than allowed to clip.
        let mut q = queue_of(1.0, 1000);
        deck.begin(Entry::Natural, 1000, &mut q);
        deck.push(&vec![1.0; 1000], &mut q);
        assert!(q.iter().all(|v| *v <= 1.0));
    }

    #[test]
    fn arrives_in_pieces() {
        let mut q = queue_of(1.0, 800);
        let mut deck = Deck::default();
        deck.begin(Entry::Natural, 600, &mut q);
        for _ in 0..10 {
            deck.push(&[0.0; 100], &mut q);
        }
        assert_eq!(q.len(), 800 + 400);
        assert_eq!(deck.pending_overlap(), 0);
        // The outgoing song has faded to nothing by the end of the region.
        assert!(q[799].abs() < 0.01);
        assert!((q[200] - 1.0).abs() < 1e-6);
    }

    /// Take `n` samples off the front the way the output thread does.
    fn play(deck: &mut Deck, q: &mut VecDeque<f64>, n: usize) -> Vec<f64> {
        let queued = q.len();
        let mut chunk: Vec<f64> = q.drain(..n).collect();
        deck.played(&mut chunk, queued);
        chunk
    }

    #[test]
    fn a_stalled_blend_keeps_the_old_song_on_its_fade() {
        // A blend of 800 gets a quarter of the way, then the incoming song
        // stops arriving (a skip, a seek, a slow connection).
        let mut q = queue_of(1.0, 1000);
        let mut deck = Deck::default();
        deck.begin(Entry::Natural, 800, &mut q);
        deck.push(&vec![0.0; 200], &mut q);
        // Playback carries on: 200 untouched, 200 blended, then 300 the
        // incoming song never reached.
        let mut heard = Vec::new();
        for _ in 0..7 {
            heard.extend(play(&mut deck, &mut q, 100));
        }
        assert_eq!(heard[199], 1.0);
        // The old song goes on fading; it never steps back up to full level.
        let steps_up = heard.windows(2).filter(|w| w[1] > w[0] + 1e-9).count();
        assert_eq!(steps_up, 0, "the old song jumped back up");
        // Five eighths of the way through the blend by now: cos(0.62 * 90°).
        assert!((heard[699] - 0.559).abs() < 0.01, "at {} where the fade should have it at 0.56", heard[699]);
        assert_eq!(deck.pending_overlap(), 300);

        // The incoming song comes back and rejoins where the fade has got to.
        deck.push(&vec![0.0; 500], &mut q);
        assert_eq!(q.len(), 300 + 200);
        assert_eq!(deck.pending_overlap(), 0);
        assert!((q[0] - heard[699]).abs() < 0.01, "a step where it rejoined");
        assert!(q[299].abs() < 0.02);
    }

    #[test]
    fn finished_audio_is_played_as_it_is() {
        let mut q = queue_of(1.0, 1000);
        let mut deck = Deck::default();
        deck.begin(Entry::Natural, 400, &mut q);
        deck.push(&vec![1.0; 100], &mut q);
        // 600 untouched and 100 blended lie ahead of the unfinished stretch.
        let heard = play(&mut deck, &mut q, 600);
        assert!(heard.iter().all(|v| *v == 1.0));
        assert_eq!(deck.overruns, 0);
        assert_eq!(deck.pending_overlap(), 300);
    }

    #[test]
    fn jump_cuts_over_but_does_not_click() {
        let mut q = queue_of(1.0, 50_000);
        let mut deck = Deck::default();
        deck.begin(Entry::Jump, 264_600, &mut q);
        assert_eq!(q.len(), MICRO);
        deck.push(&vec![0.0; 20_000], &mut q);
        assert_eq!(q.len(), 20_000);
        assert!((q[0] - 1.0).abs() < 1e-9);
        let biggest_step = q.iter().zip(q.iter().skip(2)).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
        assert!(biggest_step < 0.01, "audible discontinuity: {biggest_step}");
    }

    #[test]
    fn a_long_blend_runs_its_whole_length() {
        // Six seconds of crossfade into a gathered tail. The output plays in
        // real time while the player, far faster than that, is paced by `enough`.
        let mix = 6 * 88_200;
        let mut queue: VecDeque<f64> = std::iter::repeat_n(1.0, mix + MARGIN).collect();
        let mut deck = Deck::default();
        deck.begin(Entry::Natural, mix, &mut queue);
        let packet = vec![0.5; 4096];
        let pause = packet.len() as f64 / 2.0 / RATE * 1.25;
        let (mut wait, mut owed) = (0.0f64, 0.0f64);
        for ms in 0..7000 {
            owed += 88.2;
            let n = (owed as usize & !1).min(queue.len());
            owed -= n as f64;
            play(&mut deck, &mut queue, n);
            wait -= 0.001;
            while wait <= 0.0 {
                deck.push(&packet, &mut queue);
                if enough(queue.len(), deck.pending_overlap(), BASE) {
                    wait = pause;
                }
            }
            assert_eq!(deck.overruns, 0, "the playhead caught the blend {ms} ms in");
        }
        assert_eq!(deck.pending_overlap(), 0, "the blend never finished");
    }

    #[test]
    fn empty_queue_is_a_plain_start() {
        let mut q = VecDeque::new();
        let mut deck = Deck::default();
        deck.begin(Entry::Natural, 1000, &mut q);
        deck.push(&[0.5; 10], &mut q);
        assert_eq!(q.len(), 10);
        assert_eq!(q[0], 0.5);
    }

    #[test]
    fn steady_pulse_is_detected_as_beats() {
        // 120 bpm kick: a short 60 Hz burst every half second.
        let hub = Hub::default();
        let mut analyser = Analyser::new();
        let start = Instant::now() - Duration::from_secs(20);
        let total = (RATE * 8.0) as usize;
        let mut samples = Vec::with_capacity(total * 2);
        for i in 0..total {
            let t = i as f64 / RATE;
            let into_beat = t % 0.5;
            let kick = if into_beat < 0.08 { (t * 60.0 * std::f64::consts::TAU).sin() * (1.0 - into_beat / 0.08) } else { 0.0 };
            let hiss = ((i * 7919 % 1000) as f64 / 1000.0 - 0.5) * 0.02;
            samples.extend([kick * 0.8 + hiss; 2]);
        }
        let mut beats = 0;
        for (n, chunk) in samples.chunks(CHUNK).enumerate() {
            analyser.feed(chunk, start, (n * CHUNK / 2) as u64, &hub);
            beats += hub.frames.lock().unwrap().drain(..).filter(|f| f.beat).count();
        }
        // 16 kicks in 8 s; the first second is spent learning the level.
        assert!((11..=18).contains(&beats), "found {beats} beats");
    }
}
