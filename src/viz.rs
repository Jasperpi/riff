//! Spectrum analyser. A pass-through audio sink copies samples on their way to
//! the sound card, turns them into frequency bands, and timestamps each frame
//! with the moment it will actually be *heard* so the bars stay in sync.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use librespot_playback::audio_backend::{Sink, SinkResult};
use librespot_playback::convert::Converter;
use librespot_playback::decoder::AudioPacket;
use rustfft::{Fft, FftPlanner, num_complex::Complex};

pub const BANDS: usize = 48;
const FFT: usize = 2048;
const HOP: usize = 1024;
const RATE: f64 = 44_100.0;

struct Frame {
    at: Instant,
    bands: [f32; BANDS],
}

#[derive(Default)]
pub struct Tap {
    frames: Mutex<VecDeque<Frame>>,
}

impl Tap {
    /// The frame that should be on screen right now, if audio is flowing.
    pub fn current(&self) -> Option<[f32; BANDS]> {
        let now = Instant::now();
        let mut frames = self.frames.lock().unwrap();
        while frames.len() > 1 && frames[1].at <= now {
            frames.pop_front();
        }
        let f = frames.front()?;
        // A frame older than a quarter second means playback stopped.
        (f.at <= now && now.duration_since(f.at) < Duration::from_millis(250)).then_some(f.bands)
    }

    fn push(&self, at: Instant, bands: [f32; BANDS]) {
        let mut frames = self.frames.lock().unwrap();
        if frames.len() > 256 {
            frames.pop_front();
        }
        frames.push_back(Frame { at, bands });
    }

    fn clear(&self) {
        self.frames.lock().unwrap().clear();
    }
}

pub struct VizSink {
    inner: Box<dyn Sink>,
    tap: Arc<Tap>,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Mono samples awaiting analysis.
    pending: Vec<f32>,
    /// Edges of each band as FFT bin indices.
    edges: Vec<usize>,
    /// Wall-clock time at which sample 0 of this run reached the speakers.
    anchor: Option<Instant>,
    frames_written: u64,
    /// Slow-moving peak used as automatic gain, so quiet tracks still move.
    peak: f32,
}

impl VizSink {
    pub fn new(inner: Box<dyn Sink>, tap: Arc<Tap>) -> Self {
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
            inner,
            tap,
            fft: FftPlanner::new().plan_fft_forward(FFT),
            window,
            pending: Vec::with_capacity(FFT * 2),
            edges,
            anchor: None,
            frames_written: 0,
            peak: 1e-3,
        }
    }

    fn analyse(&mut self, samples: &[f64]) {
        let now = Instant::now();
        let ahead = Duration::from_secs_f64(self.frames_written as f64 / RATE);
        // The output drains at real time from the first write; if writes ever fall
        // behind that clock (stall, underrun), restart the clock from here.
        let anchor = match self.anchor {
            Some(a) if a + ahead >= now => a,
            _ => {
                let a = now.checked_sub(ahead).unwrap_or(now);
                self.anchor = Some(a);
                a
            }
        };

        let start = self.frames_written;
        let mut consumed = 0u64;
        for pair in samples.chunks_exact(2) {
            self.pending.push(((pair[0] + pair[1]) * 0.5) as f32);
            consumed += 1;
            if self.pending.len() >= FFT {
                let at = anchor + Duration::from_secs_f64((start + consumed) as f64 / RATE);
                let bands = self.spectrum();
                self.tap.push(at, bands);
                self.pending.drain(..HOP);
            }
        }
        self.frames_written += consumed;
    }

    fn spectrum(&mut self) -> [f32; BANDS] {
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
        bands
    }
}

impl Sink for VizSink {
    fn start(&mut self) -> SinkResult<()> {
        self.anchor = None;
        self.frames_written = 0;
        self.pending.clear();
        self.inner.start()
    }

    fn stop(&mut self) -> SinkResult<()> {
        self.tap.clear();
        self.inner.stop()
    }

    fn write(&mut self, packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        if let Ok(samples) = packet.samples() {
            self.analyse(samples);
        }
        self.inner.write(packet, converter)
    }
}

/// Stand-in used when no sound card can be opened: swallows audio at real-time
/// speed so playback state still advances normally.
pub struct NullSink;

impl Sink for NullSink {
    fn write(&mut self, packet: AudioPacket, _: &mut Converter) -> SinkResult<()> {
        if let Ok(samples) = packet.samples() {
            std::thread::sleep(Duration::from_secs_f64(samples.len() as f64 / 2.0 / RATE));
        }
        Ok(())
    }
}
