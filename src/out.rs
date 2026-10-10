//! The sound card, with as little audio waiting in front of it as will play
//! without gaps.
//!
//! The stock backend queues about half a second before it pushes back, and
//! everything in that queue has already had the volume, the crossfader and
//! the decks' controls applied. That half second is how long a change took to
//! be heard. Here the sound card's own callback reads from a queue a few
//! milliseconds long, so what is written is heard almost at once.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, SampleFormat, SampleRate, SizedSample, StreamConfig};

const RATE: u32 = 44_100;
/// Frames asked of the sound card for its own buffer: 23 ms.
const DEVICE_FRAMES: u32 = 1024;
/// Samples (both channels) allowed to wait for the sound card before a
/// writer is held back: 17 ms. Enough that the writer can be a couple of
/// callbacks late without the sound card running dry.
const WAITING: usize = 1536;
/// After running dry, this much is gathered before sound resumes, so one late
/// write costs one gap rather than a run of them.
const PRIMED: usize = 768;

struct Queue {
    buf: VecDeque<f32>,
    /// Read position within the first frame, for devices that run at another rate.
    phase: f64,
    /// Enough has gathered to start playing from.
    primed: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    room: Condvar,
    playing: AtomicBool,
    /// How long after the callback the sound card says its audio is heard, in microseconds.
    device_us: AtomicU64,
    /// Callbacks that found too little audio waiting while playing.
    dropouts: AtomicU32,
    /// When the callback last ran, as milliseconds since `born`.
    alive_ms: AtomicU64,
    born: Instant,
}

/// An open output. It has to stay on the thread that opened it.
pub struct Output {
    shared: Arc<Shared>,
    _stream: cpal::Stream,
}

impl Output {
    /// Open the default output device. Fails if there is none.
    pub fn open() -> Result<Self> {
        let device = cpal::default_host().default_output_device().ok_or_else(|| anyhow!("no audio output device"))?;
        let supported: Vec<_> = device.supported_output_configs()?.collect();
        // Stereo at the music's own rate if the device will take it, in the
        // simplest sample format on offer; otherwise whatever it prefers.
        let pick = |format: SampleFormat| {
            supported
                .iter()
                .filter(|c| c.channels() == 2 && c.sample_format() == format)
                .find_map(|c| c.try_with_sample_rate(SampleRate(RATE)))
        };
        let chosen = match pick(SampleFormat::F32).or_else(|| pick(SampleFormat::I16)).or_else(|| pick(SampleFormat::I32)) {
            Some(config) => config,
            None => device.default_output_config()?,
        };
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue { buf: VecDeque::new(), phase: 0.0, primed: false }),
            room: Condvar::new(),
            playing: AtomicBool::new(false),
            device_us: AtomicU64::new(0),
            dropouts: AtomicU32::new(0),
            alive_ms: AtomicU64::new(0),
            born: Instant::now(),
        });

        let mut config: StreamConfig = chosen.config();
        config.buffer_size = BufferSize::Fixed(DEVICE_FRAMES);
        let stream = match build(&device, &config, chosen.sample_format(), shared.clone()) {
            Ok(stream) => stream,
            Err(e) => {
                // Some devices won't be told their buffer size.
                log::warn!("audio output: no {DEVICE_FRAMES}-frame buffer ({e}); using the device's own");
                config.buffer_size = BufferSize::Default;
                build(&device, &config, chosen.sample_format(), shared.clone())?
            }
        };
        stream.play()?;
        log::info!(
            "audio output: {} Hz, {} channels, {:?}",
            config.sample_rate.0,
            config.channels,
            chosen.sample_format()
        );
        Ok(Self { shared, _stream: stream })
    }

    /// Sound or silence. Stopping is immediate; what is waiting stays put and
    /// is the first thing heard on starting again.
    pub fn set_playing(&self, playing: bool) {
        self.shared.playing.store(playing, Ordering::Relaxed);
    }

    /// Hand over interleaved stereo. Returns once there is room for it, which
    /// in steady running is the time it takes to play: this is what paces the
    /// caller. If the device has stopped asking for audio, the samples are
    /// dropped after the time they would have taken, so playback still moves on.
    pub fn write(&self, samples: &[f64]) {
        let length = Duration::from_secs_f64(samples.len() as f64 / 2.0 / RATE as f64);
        let mut queue = self.shared.queue.lock().unwrap();
        let since = Instant::now();
        while queue.buf.len() > WAITING {
            if since.elapsed() > Duration::from_millis(300) && !self.alive() {
                drop(queue);
                std::thread::sleep(length);
                return;
            }
            queue = self.shared.room.wait_timeout(queue, Duration::from_millis(20)).unwrap().0;
        }
        queue.buf.extend(samples.iter().map(|s| *s as f32));
    }

    /// Frames written but not yet heard: those waiting here plus the sound card's own.
    pub fn lag_frames(&self) -> u64 {
        let waiting = self.shared.queue.lock().unwrap().buf.len() as u64 / 2;
        waiting + self.shared.device_us.load(Ordering::Relaxed) * RATE as u64 / 1_000_000
    }

    pub fn dropouts(&self) -> u32 {
        self.shared.dropouts.load(Ordering::Relaxed)
    }

    /// Whether the sound card has asked for audio lately.
    fn alive(&self) -> bool {
        let now = self.shared.born.elapsed().as_millis() as u64;
        now.saturating_sub(self.shared.alive_ms.load(Ordering::Relaxed)) < 250
    }
}

fn build(device: &cpal::Device, config: &StreamConfig, format: SampleFormat, shared: Arc<Shared>) -> Result<cpal::Stream> {
    match format {
        SampleFormat::F32 => stream::<f32>(device, config, shared),
        SampleFormat::I16 => stream::<i16>(device, config, shared),
        SampleFormat::I32 => stream::<i32>(device, config, shared),
        SampleFormat::U16 => stream::<u16>(device, config, shared),
        other => Err(anyhow!("the audio device wants {other:?} samples, which riff can't supply")),
    }
}

fn stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &StreamConfig,
    shared: Arc<Shared>,
) -> Result<cpal::Stream> {
    let channels = config.channels as usize;
    // Frames of ours used per frame of the device's.
    let step = RATE as f64 / config.sample_rate.0 as f64;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let stamp = info.timestamp();
            if let Some(delay) = stamp.playback.duration_since(&stamp.callback) {
                shared.device_us.store(delay.as_micros() as u64, Ordering::Relaxed);
            }
            shared.alive_ms.store(shared.born.elapsed().as_millis() as u64, Ordering::Relaxed);
            fill(&shared, data, channels, step);
        },
        |e| log::warn!("audio output: {e}"),
        None,
    )?;
    Ok(stream)
}

/// The sound card wants `data` filled, now.
fn fill<T: SizedSample + FromSample<f32>>(shared: &Shared, data: &mut [T], channels: usize, step: f64) {
    let silence = T::from_sample(0.0f32);
    if !shared.playing.load(Ordering::Relaxed) {
        data.fill(silence);
        return;
    }
    let mut queue = shared.queue.lock().unwrap();
    let Queue { buf, phase, primed } = &mut *queue;
    if !*primed {
        if buf.len() < PRIMED {
            data.fill(silence);
            return;
        }
        *primed = true;
    }
    let mut frames = data.chunks_exact_mut(channels);
    let mut short = false;
    for frame in &mut frames {
        let i = *phase as usize;
        // Reading between two frames needs the one after as well.
        let need = if step == 1.0 { i + 1 } else { i + 2 };
        if buf.len() < need * 2 {
            frame.fill(silence);
            short = true;
            continue;
        }
        let t = (*phase - i as f64) as f32;
        let at = |c: usize| {
            let a = buf[2 * i + c];
            if t == 0.0 { a } else { a + (buf[2 * (i + 1) + c] - a) * t }
        };
        let (left, right) = (at(0), at(1));
        match frame {
            [mono] => *mono = T::from_sample((left + right) * 0.5),
            [l, r, rest @ ..] => {
                *l = T::from_sample(left);
                *r = T::from_sample(right);
                rest.fill(silence);
            }
            [] => {}
        }
        *phase += step;
    }
    let used = (*phase as usize).min(buf.len() / 2);
    buf.drain(..used * 2);
    *phase -= used as f64;
    if short {
        *primed = false;
        shared.dropouts.fetch_add(1, Ordering::Relaxed);
    }
    drop(queue);
    shared.room.notify_all();
}
