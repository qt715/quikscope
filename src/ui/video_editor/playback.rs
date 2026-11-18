//! Playback for the video editor: a decode thread that keeps a few frames
//! ahead of the playhead, audio output through cpal, the filmstrip
//! thumbnails, and the clock frames are painted against.
//!
//! Threads never touch GPUI. They fill shared queues and poke a wake channel;
//! the editor pulls what is due when it paints. Every seek bumps a generation
//! so frames and samples decoded for an older position are dropped instead
//! of flashing on screen.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures::channel::mpsc::UnboundedSender;
use gpui::RenderImage;

use crate::media::decode::{AudioDecoder, Decoder};

/// Frames decoded ahead of the playhead: enough to ride out a slow keyframe
/// without holding seconds of BGRA in memory.
const QUEUE: usize = 6;
/// Audio decoded ahead, in seconds. Short, because a seek throws it away.
const AUDIO_AHEAD: f64 = 0.3;
/// Audio positions older than this mean the device stopped taking samples
/// (underrun, end of the track, a stalled device): the clock runs freely.
const AUDIO_STALE: Duration = Duration::from_millis(250);

/// Tells the editor something was decoded.
pub type Wake = UnboundedSender<()>;

fn wake(w: &Wake) {
    let _ = w.unbounded_send(());
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking decode thread must not take the UI down with it.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One decoded frame, ready to paint.
pub struct Frame {
    pub image: Arc<RenderImage>,
    pub pts: Duration,
    pub duration: Duration,
}

impl Frame {
    pub fn covers(&self, at: Duration) -> bool {
        self.pts <= at && at < self.pts + self.duration
    }
}

#[derive(Clone, Copy)]
struct Request {
    at: Duration,
    size: (u32, u32),
    generation: u64,
}

#[derive(Default)]
struct VideoState {
    /// A seek the decode thread has not picked up yet.
    request: Option<Request>,
    /// Bumped by every seek.
    generation: u64,
    size: (u32, u32),
    queue: VecDeque<Frame>,
    /// The decoder ran out of frames for this generation.
    end: bool,
    error: Option<String>,
    quit: bool,
}

struct VideoShared {
    state: Mutex<VideoState>,
    cond: Condvar,
}

/// Decodes video frames on its own thread, a few ahead of what is shown.
pub struct VideoPlayer {
    shared: Arc<VideoShared>,
}

impl VideoPlayer {
    /// Starts the decode thread; nothing is decoded until the first [`seek`](Self::seek).
    pub fn spawn(path: PathBuf, wake: Wake) -> VideoPlayer {
        let shared = Arc::new(VideoShared {
            state: Mutex::new(VideoState::default()),
            cond: Condvar::new(),
        });
        let s = shared.clone();
        std::thread::Builder::new()
            .name("quikscope-video".into())
            .spawn(move || video_thread(&path, &s, &wake))
            .expect("spawning the video decode thread");
        VideoPlayer { shared }
    }

    /// Restarts decoding at `at` with frames scaled to `size`, dropping
    /// everything queued.
    pub fn seek(&self, at: Duration, size: (u32, u32)) {
        let mut s = lock(&self.shared.state);
        s.generation += 1;
        s.request = Some(Request {
            at,
            size,
            generation: s.generation,
        });
        s.size = size;
        s.queue.clear();
        s.end = false;
        self.shared.cond.notify_one();
    }

    /// Frame size the decoder was last asked for.
    pub fn size(&self) -> (u32, u32) {
        lock(&self.shared.state).size
    }

    /// Pops every queued frame due at `at`; returns the newest of them.
    pub fn take_due(&self, at: Duration) -> Option<Frame> {
        let mut s = lock(&self.shared.state);
        let mut due = None;
        while s.queue.front().is_some_and(|f| f.pts <= at) {
            due = s.queue.pop_front();
        }
        if due.is_some() {
            self.shared.cond.notify_one();
        }
        due
    }

    /// The first frame decoded since the last seek: the one showing at the
    /// seek target.
    pub fn take_first(&self) -> Option<Frame> {
        let mut s = lock(&self.shared.state);
        let first = s.queue.pop_front();
        if first.is_some() {
            self.shared.cond.notify_one();
        }
        first
    }

    /// Whether the queue already holds the frame showing at `at` (or one
    /// past it), so stepping there needs no decoder seek.
    pub fn covers(&self, at: Duration, size: (u32, u32)) -> bool {
        let s = lock(&self.shared.state);
        s.request.is_none()
            && s.size == size
            && (s.end || s.queue.back().is_some_and(|f| f.pts > at))
    }

    /// Decoding reached the end and every frame was taken.
    pub fn ended(&self) -> bool {
        let s = lock(&self.shared.state);
        s.end && s.queue.is_empty() && s.request.is_none()
    }

    pub fn error(&self) -> Option<String> {
        lock(&self.shared.state).error.clone()
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        // Not joined: a decode in progress finishes in the background rather
        // than stalling the window close.
        lock(&self.shared.state).quit = true;
        self.shared.cond.notify_one();
    }
}

fn video_thread(path: &Path, shared: &VideoShared, w: &Wake) {
    let mut decoder = match Decoder::open(path) {
        Ok(d) => d,
        Err(e) => {
            lock(&shared.state).error = Some(format!("{e:#}"));
            wake(w);
            return;
        }
    };
    let mut size = (0, 0);
    let mut generation = 0;
    // Nothing to decode until the first seek says where and how large.
    let mut idle = true;
    loop {
        let request = {
            let mut s = lock(&shared.state);
            loop {
                if s.quit {
                    return;
                }
                if let Some(r) = s.request.take() {
                    break Some(r);
                }
                if !idle && s.queue.len() < QUEUE {
                    break None;
                }
                s = shared.cond.wait(s).unwrap_or_else(|e| e.into_inner());
            }
        };
        if let Some(r) = request {
            generation = r.generation;
            if r.size != size {
                decoder.set_output_size(r.size.0, r.size.1);
                size = r.size;
            }
            idle = false;
            if let Err(e) = decoder.seek(r.at) {
                let mut s = lock(&shared.state);
                if s.generation == generation {
                    s.error = Some(format!("Seeking failed: {e:#}"));
                    s.end = true;
                }
                idle = true;
                drop(s);
                wake(w);
            }
            continue;
        }
        let next = decoder.next_frame();
        let mut s = lock(&shared.state);
        if s.generation != generation || s.request.is_some() {
            continue;
        }
        match next {
            Ok(Some(f)) => s.queue.push_back(Frame {
                image: super::super::render_image(f.image.width, f.image.height, f.image.data),
                pts: f.pts,
                duration: f.duration,
            }),
            Ok(None) => {
                s.end = true;
                idle = true;
            }
            Err(e) => {
                s.error = Some(format!("Decoding failed: {e:#}"));
                s.end = true;
                idle = true;
            }
        }
        drop(s);
        wake(w);
    }
}

#[derive(Default)]
struct AudioState {
    samples: VecDeque<f32>,
    /// Media time of the first queued sample.
    front: Duration,
    /// Media time of the last samples handed to the device, and when the
    /// device will play them.
    heard: Option<(Duration, Instant)>,
    seek: Option<Duration>,
    end: bool,
    quit: bool,
}

struct AudioShared {
    state: Mutex<AudioState>,
    cond: Condvar,
    playing: AtomicBool,
}

/// The default output device playing the file's audio. Dropping it stops
/// the stream and the decode thread.
pub struct AudioOut {
    shared: Arc<AudioShared>,
    stream: cpal::Stream,
}

impl AudioOut {
    /// Opens the default output device (paused) and starts decoding at `at`.
    /// `Ok(None)` when the file has no audio.
    pub fn open(path: &Path, at: Duration) -> Result<Option<AudioOut>> {
        use cpal::SampleFormat as F;
        use cpal::traits::{DeviceTrait as _, HostTrait as _};

        let device = cpal::default_host()
            .default_output_device()
            .context("No audio output device")?;
        let supported = device
            .default_output_config()
            .context("Reading the audio output format")?;
        let config = supported.config();
        let (rate, channels) = (config.sample_rate, config.channels);
        let Some(mut decoder) = AudioDecoder::open(path, rate, channels)? else {
            return Ok(None);
        };
        if at > Duration::ZERO {
            decoder.seek(at)?;
        }
        let shared = Arc::new(AudioShared {
            state: Mutex::new(AudioState {
                front: at,
                ..AudioState::default()
            }),
            cond: Condvar::new(),
            playing: AtomicBool::new(false),
        });
        let stream = match supported.sample_format() {
            F::F32 => build::<f32>(&device, &config, &shared),
            F::F64 => build::<f64>(&device, &config, &shared),
            F::I16 => build::<i16>(&device, &config, &shared),
            F::I32 => build::<i32>(&device, &config, &shared),
            F::I8 => build::<i8>(&device, &config, &shared),
            F::U8 => build::<u8>(&device, &config, &shared),
            F::U16 => build::<u16>(&device, &config, &shared),
            F::U32 => build::<u32>(&device, &config, &shared),
            other => anyhow::bail!("Unsupported audio output sample format {other}"),
        }?;
        let cap = (rate as f64 * AUDIO_AHEAD) as usize * channels as usize;
        let s = shared.clone();
        std::thread::Builder::new()
            .name("quikscope-audio".into())
            .spawn(move || audio_thread(decoder, &s, cap))
            .context("Spawning the audio decode thread")?;
        Ok(Some(AudioOut { shared, stream }))
    }

    pub fn set_playing(&self, playing: bool) {
        use cpal::traits::StreamTrait as _;
        self.shared.playing.store(playing, Ordering::Release);
        // Hosts that cannot pause keep calling back; the flag makes those
        // calls play silence without consuming samples.
        let result = if playing {
            self.stream.play()
        } else {
            lock(&self.shared.state).heard = None;
            self.stream.pause()
        };
        if let Err(e) = result {
            log::debug!("audio stream play/pause: {e}");
        }
    }

    /// Drops buffered audio and continues from `at`.
    pub fn seek(&self, at: Duration) {
        let mut s = lock(&self.shared.state);
        s.seek = Some(at);
        s.samples.clear();
        s.front = at;
        s.heard = None;
        s.end = false;
        self.shared.cond.notify_one();
    }

    /// Media time being heard now, while the device is taking samples.
    pub fn position(&self) -> Option<Duration> {
        let (media, at) = lock(&self.shared.state).heard?;
        let now = Instant::now();
        if now > at + AUDIO_STALE {
            return None;
        }
        Some(if now >= at {
            media + (now - at)
        } else {
            media.saturating_sub(at - now)
        })
    }
}

impl Drop for AudioOut {
    fn drop(&mut self) {
        self.shared.playing.store(false, Ordering::Release);
        lock(&self.shared.state).quit = true;
        self.shared.cond.notify_one();
    }
}

fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: &Arc<AudioShared>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait as _;

    let shared = shared.clone();
    let channels = config.channels.max(1) as usize;
    let rate = config.sample_rate.max(1) as f64;
    device
        .build_output_stream::<T, _, _>(
            *config,
            move |out: &mut [T], info: &cpal::OutputCallbackInfo| {
                let t = info.timestamp();
                let latency = t
                    .playback
                    .checked_duration_since(t.callback)
                    .unwrap_or_default();
                let mut s = lock(&shared.state);
                let n = if shared.playing.load(Ordering::Acquire) {
                    out.len().min(s.samples.len()) / channels * channels
                } else {
                    0
                };
                for (o, v) in out.iter_mut().zip(s.samples.drain(..n)) {
                    *o = T::from_sample(v);
                }
                for o in &mut out[n..] {
                    *o = T::EQUILIBRIUM;
                }
                if n > 0 {
                    let first = s.front;
                    s.front += Duration::from_secs_f64((n / channels) as f64 / rate);
                    s.heard = Some((first, Instant::now() + latency));
                    shared.cond.notify_one();
                }
            },
            |e| log::warn!("audio playback: {e}"),
            None,
        )
        .context("Opening the audio output stream")
}

fn audio_thread(mut decoder: AudioDecoder, shared: &AudioShared, cap: usize) {
    loop {
        let seek = {
            let mut s = lock(&shared.state);
            loop {
                if s.quit {
                    return;
                }
                if let Some(at) = s.seek.take() {
                    break Some(at);
                }
                if !s.end && s.samples.len() < cap {
                    break None;
                }
                s = shared.cond.wait(s).unwrap_or_else(|e| e.into_inner());
            }
        };
        if let Some(at) = seek {
            if let Err(e) = decoder.seek(at) {
                log::warn!("audio seek: {e:#}");
                lock(&shared.state).end = true;
            }
            continue;
        }
        let chunk = decoder.next_chunk();
        let mut s = lock(&shared.state);
        if s.seek.is_some() {
            continue;
        }
        match chunk {
            Ok(Some((samples, pts))) => {
                if s.samples.is_empty() {
                    s.front = pts;
                }
                s.samples.extend(samples);
            }
            Ok(None) => s.end = true,
            Err(e) => {
                log::warn!("audio decoding: {e:#}");
                s.end = true;
            }
        }
    }
}

/// Where playback is: media time advancing with the monotonic clock since
/// `since`. Audio playback re-anchors it so picture follows sound.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    base: Duration,
    since: Instant,
}

impl Clock {
    /// Drift tolerated before snapping the clock to the audio position;
    /// below a frame at 30 fps, above callback jitter.
    const SYNC: Duration = Duration::from_millis(30);

    pub fn start(at: Duration) -> Clock {
        Clock {
            base: at,
            since: Instant::now(),
        }
    }

    pub fn now(&self) -> Duration {
        self.base + self.since.elapsed()
    }

    /// Follows `audio` when it has drifted noticeably.
    pub fn sync(&mut self, audio: Duration) {
        let now = self.now();
        if audio.abs_diff(now) > Self::SYNC {
            *self = Clock::start(audio);
        }
    }
}

/// Thumbnails and the media time each shows.
type Thumbs = Arc<Mutex<Vec<(Duration, Arc<RenderImage>)>>>;

/// Small frames spread over the whole file for the timeline, decoded in the
/// background and painted as they arrive.
pub struct Filmstrip {
    thumbs: Thumbs,
    quit: Arc<AtomicBool>,
}

impl Filmstrip {
    /// Thumbnails `height` pixels tall at `count` evenly spaced times.
    pub fn spawn(
        path: PathBuf,
        info: &crate::media::MediaInfo,
        height: u32,
        count: usize,
        w: Wake,
    ) -> Filmstrip {
        let thumbs = Arc::new(Mutex::new(Vec::new()));
        let quit = Arc::new(AtomicBool::new(false));
        let (out, stop) = (thumbs.clone(), quit.clone());
        let duration = info.duration;
        let width = (height as f64 * info.width.max(1) as f64 / info.height.max(1) as f64)
            .round()
            .max(1.0) as u32;
        std::thread::Builder::new()
            .name("quikscope-filmstrip".into())
            .spawn(move || {
                let mut decoder = match Decoder::open(&path) {
                    Ok(d) => d,
                    Err(e) => {
                        log::debug!("filmstrip: {e:#}");
                        return;
                    }
                };
                decoder.set_output_size(width, height);
                for i in 0..count {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    let at = duration.mul_f64((i as f64 + 0.5) / count as f64);
                    let frame = decoder.seek(at).and_then(|()| decoder.next_frame());
                    match frame {
                        Ok(Some(f)) => {
                            let image = super::super::render_image(
                                f.image.width,
                                f.image.height,
                                f.image.data,
                            );
                            lock(&out).push((at, image));
                            wake(&w);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            log::debug!("filmstrip at {at:?}: {e:#}");
                            return;
                        }
                    }
                }
            })
            .expect("spawning the filmstrip thread");
        Filmstrip { thumbs, quit }
    }

    /// The thumbnail nearest to `at`.
    pub fn nearest(&self, at: Duration) -> Option<Arc<RenderImage>> {
        let thumbs = lock(&self.thumbs);
        thumbs
            .iter()
            .min_by_key(|(t, _)| t.abs_diff(at))
            .map(|(_, image)| image.clone())
    }
}

impl Drop for Filmstrip {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Release);
    }
}
