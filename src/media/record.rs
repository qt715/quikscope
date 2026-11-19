//! Recording sessions: capture threads feed one encoder thread through
//! separate video and audio queues, timestamped by a monotonic, pause-aware
//! clock.
//!
//! Policies:
//! - Video is variable frame rate: every captured frame is encoded at its
//!   own time (whole milliseconds, strictly increasing). Sources deliver
//!   only changed frames; a static screen repeats its last picture once per
//!   keyframe interval as a keyframe, so files stay seekable, and nothing
//!   ever encodes a backlog of duplicate ticks.
//! - The video queue holds [`VIDEO_QUEUE`] frames (each may hold a capture
//!   buffer); when the encoder falls behind, the oldest waiting frame is
//!   dropped and counted, so the newest picture always gets encoded.
//! - Audio is never dropped: its queue only fails the recording when the
//!   encoder has stalled for [`AUDIO_BACKLOG`]. Sources are mixed by sample
//!   count ([`super::mix`]) into 20 ms Opus packets timed by sample count. An
//!   audio source that fails ends its own track only.
//! - Odd capture sizes are cropped to even (H.264 needs 4:2:0), never
//!   scaled; only a capture larger than every H.264 encoder takes is scaled
//!   down (and [`Session::warnings`] says so). Animations record their
//!   exact size into a lossless master.
//! - Fragmented MP4 and Matroska are written straight into the reserved
//!   final file: both are playable at every flushed fragment, so a crash or
//!   power loss leaves a playable recording under its real name. Classic
//!   MP4 and MOV are written fragmented into a partial file and rewritten
//!   at the end.
//! - An encoder or muxer failure stops the recording ([`Session::failure`])
//!   and [`Session::stop`] keeps what was written: fragmented MP4 and
//!   Matroska are playable up to the last fragment. When the rewrite of a
//!   classic MP4/MOV fails, the complete fragmented file is kept under the
//!   same name.

use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};

use super::anim::{self, master::MasterWriter};
use super::codec::Yuv420;
use super::container::{self, Codec, Muxer};
use super::export::{self, AUDIO_TRACK, H264Track, OpusTrack, VIDEO_TRACK};
use super::mix::Mixer;
use super::{
    AudioSource, EncoderPreference, Frame, OutputFormat, PixelFormat, VideoSource, convert,
};
use crate::output::OutputFile;
use crate::raster::Image;

/// What to record into.
#[derive(Debug)]
pub struct SessionConfig {
    /// The final file. A [reserved](OutputFile::reserve) fragmented MP4 or
    /// Matroska file is written in place; other videos stream into its
    /// partial file; animations are first recorded losslessly to `master`
    /// and encoded into it when the session stops.
    pub output: OutputFile,
    pub format: OutputFormat,
    /// Lossless intermediate for animations, kept after the session so the
    /// annotator can re-export without generation loss. Ignored for videos.
    pub master: PathBuf,
    pub fps: u32,
    /// Audio device ids for videos (see [`super::audio::devices`]); ignored
    /// for animations. Devices that cannot be opened are skipped and
    /// reported by [`Session::warnings`].
    pub audio: Vec<String>,
    pub encoder: EncoderPreference,
    /// MP4 layout (see [`super::container::Container::Mp4`]).
    pub fragmented: bool,
}

/// A finished recording.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub path: PathBuf,
    pub format: OutputFormat,
    /// For animations the lossless master; for videos the same as `path`.
    pub master: PathBuf,
    pub width: u32,
    pub height: u32,
    /// Recorded time, pauses excluded.
    pub duration: Duration,
    /// A representative frame (for notifications and the clipboard preview):
    /// the middle of the recording, or its first frame when the file cannot
    /// be read back.
    pub thumbnail: Image,
    /// Encoder actually used, e.g. "OpenH264", for logs.
    pub encoder: String,
}

/// Why [`Session::stop`] saved nothing: its `cancel` flag was set while it
/// was saving. Returned as the error, so callers find it with
/// `error.downcast_ref::<Cancelled>()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cancelled {
    /// For an animation, its lossless master, complete and kept (it opens
    /// in the annotator, which can export it later); `None` for a video,
    /// which is discarded.
    pub master: Option<PathBuf>,
}

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.master {
            Some(master) => write!(
                f,
                "Saving was cancelled; the lossless recording is kept at {}",
                master.display()
            ),
            None => f.write_str("Recording discarded"),
        }
    }
}

impl std::error::Error for Cancelled {}

/// Longest side of [`Recorded::thumbnail`].
const THUMBNAIL: u32 = 384;
/// How often [`Session::stop`] checks its cancel flag while the encoder
/// thread finishes.
const ABORT_POLL: Duration = Duration::from_millis(10);
/// How long capture threads block before checking for stop and pause.
const POLL: Duration = Duration::from_millis(40);
/// How long the encoder thread waits for work before checking the clock
/// (static-screen repeats, audio).
const IDLE: Duration = Duration::from_millis(20);
/// Video frames waiting for the encoder.
const VIDEO_QUEUE: usize = 2;
/// Queued audio that means the encoder has stalled.
const AUDIO_BACKLOG: Duration = Duration::from_secs(30);

struct Clock {
    start: Instant,
    pauses: Vec<(Instant, Option<Instant>)>,
    end: Option<Instant>,
}

impl Clock {
    /// Session time of `at`; `None` before the start or while paused.
    fn at(&self, at: Instant) -> Option<Duration> {
        if at < self.start {
            return None;
        }
        let mut time = at.duration_since(self.start);
        for &(begin, end) in &self.pauses {
            if at < begin {
                break;
            }
            let end = end.filter(|&end| at >= end)?;
            time = time.saturating_sub(end.duration_since(begin));
        }
        Some(time)
    }

    fn elapsed(&self) -> Duration {
        let at = self.end.unwrap_or_else(Instant::now);
        let at = self
            .pauses
            .last()
            .filter(|(_, end)| end.is_none())
            .map_or(at, |(begin, _)| *begin);
        at.duration_since(self.start).saturating_sub(
            self.pauses
                .iter()
                .filter_map(|(a, b)| b.map(|b| b.duration_since(*a)))
                .sum(),
        )
    }
}

/// Work for the encoder thread.
#[derive(Default)]
struct Queues {
    video: VecDeque<(Frame, Duration)>,
    /// Source, interleaved samples, session time.
    audio: VecDeque<(usize, Vec<f32>, Duration)>,
    audio_queued: Duration,
    dropped: u64,
    closed: bool,
}

#[derive(Default)]
struct Inbox {
    queues: Mutex<Queues>,
    ready: Condvar,
}

/// What the encoder thread takes in one go.
struct Batch {
    audio: Vec<(usize, Vec<f32>, Duration)>,
    video: Option<(Frame, Duration)>,
    /// More frames wait after `video`.
    more_video: bool,
    /// Nothing more will come.
    closed: bool,
}

impl Inbox {
    /// Queues a frame, dropping the oldest waiting one when full. Returns
    /// the number of frames dropped so far when this push dropped one.
    fn push_video(&self, frame: Frame, at: Duration) -> Option<u64> {
        let mut queues = self.queues.lock().unwrap();
        let dropped = (queues.video.len() >= VIDEO_QUEUE).then(|| {
            queues.video.pop_front();
            queues.dropped += 1;
            queues.dropped
        });
        queues.video.push_back((frame, at));
        drop(queues);
        self.ready.notify_one();
        dropped
    }

    fn push_audio(
        &self,
        source: usize,
        samples: Vec<f32>,
        at: Duration,
        length: Duration,
    ) -> Result<()> {
        let mut queues = self.queues.lock().unwrap();
        ensure!(
            queues.audio_queued + length <= AUDIO_BACKLOG,
            "The encoder stopped taking audio ({} s queued)",
            AUDIO_BACKLOG.as_secs()
        );
        queues.audio_queued += length;
        queues.audio.push_back((source, samples, at));
        drop(queues);
        self.ready.notify_one();
        Ok(())
    }

    /// All queued audio and the oldest frame, waiting up to `timeout` for
    /// any.
    fn take(&self, timeout: Duration) -> Batch {
        let mut queues = self.queues.lock().unwrap();
        if queues.video.is_empty() && queues.audio.is_empty() && !queues.closed {
            queues = self.ready.wait_timeout(queues, timeout).unwrap().0;
        }
        queues.audio_queued = Duration::ZERO;
        let audio = queues.audio.drain(..).collect();
        let video = queues.video.pop_front();
        Batch {
            audio,
            more_video: !queues.video.is_empty(),
            closed: queues.closed && queues.video.is_empty(),
            video,
        }
    }

    fn close(&self) {
        self.queues.lock().unwrap().closed = true;
        self.ready.notify_one();
    }

    fn dropped(&self) -> u64 {
        self.queues.lock().unwrap().dropped
    }
}

struct Control {
    stop: AtomicBool,
    /// Set when the recording is being thrown away: the encoder thread
    /// drops its file unfinished instead of finishing (and, for classic
    /// MP4/MOV, rewriting) what will be deleted anyway.
    abort: AtomicBool,
    paused: AtomicBool,
    clock: Mutex<Clock>,
    failure: Mutex<Option<String>>,
    inbox: Inbox,
}

impl Control {
    /// Stops the recording because of `error` (the first one is kept).
    fn fail(&self, error: &anyhow::Error) {
        let mut failure = self.failure.lock().unwrap();
        if failure.is_none() {
            *failure = Some(format!("{error:#}"));
        }
        self.clock
            .lock()
            .unwrap()
            .end
            .get_or_insert_with(Instant::now);
        self.stop.store(true, Ordering::Release);
    }

    fn elapsed(&self) -> Duration {
        self.clock.lock().unwrap().elapsed()
    }

    fn time(&self, at: Instant) -> Option<Duration> {
        self.clock.lock().unwrap().at(at)
    }
}

/// How the encoder thread ended.
struct Outcome {
    /// Encoder name, for logs.
    encoder: String,
    /// Why encoding stopped early or finishing failed; what was written may
    /// still be usable.
    error: Option<anyhow::Error>,
}

pub struct Session {
    format: OutputFormat,
    master: PathBuf,
    encoder_preference: EncoderPreference,
    fragmented: bool,
    output: Option<OutputFile>,
    size: (u32, u32),
    /// The first frame, for when the recording cannot be read back.
    thumbnail: Image,
    control: Arc<Control>,
    pulls: Vec<JoinHandle<()>>,
    encoder: Option<JoinHandle<Outcome>>,
    warnings: Vec<String>,
}

impl Session {
    pub fn start(config: SessionConfig, source: Box<dyn VideoSource>) -> Result<Self> {
        let mut audio = Vec::new();
        let mut warnings = Vec::new();
        if config.format.is_video() {
            for id in &config.audio {
                match super::audio::open(id) {
                    Ok(device) => audio.push(device),
                    Err(error) => {
                        log::warn!("audio device {id} unavailable: {error:#}");
                        warnings.push(format!("Recording without {id}: {error:#}"));
                    }
                }
            }
        }
        let mut session = Self::start_with_audio(config, source, audio)?;
        warnings.append(&mut session.warnings);
        session.warnings = warnings;
        Ok(session)
    }

    /// Allows embedded capture sources to supply additional audio tracks.
    pub fn start_with_audio(
        mut config: SessionConfig,
        mut source: Box<dyn VideoSource>,
        mut audio: Vec<Box<dyn AudioSource>>,
    ) -> Result<Self> {
        ensure!(
            config.fps > 0 && config.fps <= 240,
            "Frame rate must be 1–240"
        );
        if !config.format.is_video() {
            audio.clear();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let first = loop {
            if let Some(frame) = source.next_frame(POLL)? {
                break frame;
            }
            ensure!(
                Instant::now() < deadline,
                "Screen source did not produce its first frame"
            );
        };
        let thumbnail = first.to_image().thumbnail(THUMBNAIL);
        let mut warnings = Vec::new();
        let (sink, size) = if config.format.is_video() {
            let in_place = matches!(
                export::container_for(config.format, config.fragmented)?,
                container::Container::Mp4 { fragmented: true } | container::Container::Mkv
            );
            if in_place && config.output.is_reserved() {
                config.output.write_in_place()?;
            }
            let sink = VideoSink::create(&config, (first.width(), first.height()), &audio)?;
            let size = sink.encoded;
            if sink.scaled {
                let warning = format!(
                    "Recording scaled down to {}x{}: no H.264 encoder here takes the full {}x{} capture at {} fps",
                    size.0,
                    size.1,
                    first.width(),
                    first.height(),
                    config.fps
                );
                log::warn!("{warning}");
                warnings.push(warning);
            }
            (Sink::Video(sink), size)
        } else {
            ensure!(
                config.master != config.output.path(),
                "Animation master and output must differ"
            );
            let size = (first.width(), first.height());
            anim::check_size(config.format, size.0, size.1)?;
            let writer = MasterWriter::create(&config.master, size.0, size.1)?;
            (
                Sink::Master(MasterSink {
                    writer,
                    size,
                    last_pts: None,
                }),
                size,
            )
        };
        let specs: Vec<_> = audio.iter().map(|source| source.spec()).collect();
        let control = Arc::new(Control {
            stop: AtomicBool::new(false),
            abort: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            clock: Mutex::new(Clock {
                start: first.captured,
                pauses: Vec::new(),
                end: None,
            }),
            failure: Mutex::new(None),
            inbox: Inbox::default(),
        });
        control.inbox.push_video(first, Duration::ZERO);
        let SessionConfig {
            output,
            format,
            master,
            encoder: encoder_preference,
            fragmented,
            ..
        } = config;
        let encoder = {
            let control = control.clone();
            thread::Builder::new()
                .name("quikscope-encode".into())
                .spawn(move || encode(sink, &control))?
        };
        let mut session = Self {
            format,
            master,
            encoder_preference,
            fragmented,
            output: Some(output),
            size,
            thumbnail,
            control,
            pulls: Vec::new(),
            encoder: Some(encoder),
            warnings,
        };
        // A failed spawn drops `session`, which stops what already runs.
        let control = session.control.clone();
        session.pulls.push(
            thread::Builder::new()
                .name("quikscope-video".into())
                .spawn(move || capture_video(source, &control))?,
        );
        for (index, (source, spec)) in audio.into_iter().zip(specs).enumerate() {
            let control = session.control.clone();
            let samples_per_second = f64::from(spec.rate) * f64::from(spec.channels);
            session.pulls.push(
                thread::Builder::new()
                    .name(format!("quikscope-audio-{index}"))
                    .spawn(move || capture_audio(index, source, samples_per_second, &control))?,
            );
        }
        Ok(session)
    }

    /// Problems that did not stop the recording: skipped audio devices, a
    /// capture scaled down to a size H.264 encoders take.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Stops the clock and the capture (the source stops copying frames).
    pub fn pause(&self) {
        let mut clock = self.control.clock.lock().unwrap();
        if clock.pauses.last().is_none_or(|(_, end)| end.is_some()) {
            clock.pauses.push((Instant::now(), None));
        }
        self.control.paused.store(true, Ordering::Release);
    }

    /// Restarts the clock; the source then delivers a fresh frame whether
    /// or not the screen changed while paused.
    pub fn resume(&self) {
        let mut clock = self.control.clock.lock().unwrap();
        if let Some((_, end)) = clock.pauses.last_mut()
            && end.is_none()
        {
            *end = Some(Instant::now());
        }
        self.control.paused.store(false, Ordering::Release);
    }

    pub fn is_paused(&self) -> bool {
        self.control
            .clock
            .lock()
            .unwrap()
            .pauses
            .last()
            .is_some_and(|(_, end)| end.is_none())
    }

    pub fn elapsed(&self) -> Duration {
        self.control.elapsed()
    }

    /// Why the recording stopped by itself (capture or encoding failed);
    /// [`stop`](Self::stop) still saves what was recorded.
    pub fn failure(&self) -> Option<String> {
        self.control.failure.lock().unwrap().clone()
    }

    /// Stops every thread; `None` if that already happened. With `abort`,
    /// the flag is watched until the encoder thread ends: once set, the
    /// thread drops its file instead of finishing it.
    fn finish_workers(&mut self, abort: Option<&AtomicBool>) -> Option<Outcome> {
        let encoder = self.encoder.take()?;
        self.control
            .clock
            .lock()
            .unwrap()
            .end
            .get_or_insert_with(Instant::now);
        self.control.stop.store(true, Ordering::Release);
        for pull in self.pulls.drain(..) {
            if pull.join().is_err() {
                self.control
                    .fail(&anyhow::anyhow!("A capture thread panicked"));
            }
        }
        self.control.inbox.close();
        if let Some(abort) = abort {
            while !encoder.is_finished() {
                if abort.load(Ordering::Acquire) {
                    self.control.abort.store(true, Ordering::Release);
                }
                thread::sleep(ABORT_POLL);
            }
        }
        Some(encoder.join().unwrap_or_else(|_| Outcome {
            encoder: "unknown".into(),
            error: Some(anyhow::anyhow!("The encoder thread panicked")),
        }))
    }

    /// Stops and saves the recording. After a failure mid-recording (see
    /// [`failure`](Self::failure)) this saves what was written before it.
    /// For animations, an error here means the final file could not be
    /// encoded; the master at [`SessionConfig::master`] is kept and opens
    /// in the annotator.
    ///
    /// `progress` receives 0.0–1.0 while saving: an animation's final
    /// encode, which can take minutes, reports as it goes; a video reports
    /// only its start and end. Setting `cancel` while saving stops with a
    /// [`Cancelled`] error: an animation keeps its complete master (and no
    /// final file), a video is discarded (once a classic MP4/MOV rewrite has
    /// begun, it completes before the file is deleted).
    pub fn stop(
        mut self,
        progress: &(dyn Fn(f32) + Sync),
        cancel: &AtomicBool,
    ) -> Result<Recorded> {
        progress(0.0);
        let video = self.format.is_video();
        // An animation's master is finished even when cancelled: it is what
        // a cancel keeps.
        let outcome = self
            .finish_workers(video.then_some(cancel))
            .context("Recording already stopped")?;
        let output = self.output.take().context("Recording already stopped")?;
        let cancelled = || cancel.load(Ordering::Acquire);
        let kept_master = || Cancelled {
            master: (!video).then(|| self.master.clone()),
        };
        if cancelled() {
            // Dropping the output deletes the unfinished file.
            return Err(kept_master().into());
        }
        let (path, encoder) = if video {
            if let Some(error) = outcome.error {
                ensure!(
                    playable(output.part()),
                    "{error:#}; nothing of the recording could be saved"
                );
                log::error!("recording: {error:#}; keeping what was written");
            }
            let path = output.commit()?;
            progress(1.0);
            (path, outcome.encoder)
        } else {
            if let Some(error) = outcome.error {
                // The master reads up to its last complete frame.
                log::error!("recording the animation master: {error:#}");
            }
            let exported = export::export(
                &self.master,
                output,
                &export::ExportOptions {
                    format: self.format,
                    trim: None,
                    mute: true,
                    // Captured at the configured rate already; keep its timing.
                    fps: None,
                    fragmented: self.fragmented,
                    max_width: None,
                    encoder: self.encoder_preference,
                },
                None,
                progress,
                cancel,
            );
            let path = match exported {
                Ok(path) => path,
                Err(_) if cancelled() => return Err(kept_master().into()),
                Err(error) => {
                    return Err(error.context(format!("encoding the {}", self.format.label())));
                }
            };
            (
                path,
                format!("{}, {}", outcome.encoder, self.format.label()),
            )
        };
        let master = if video {
            path.clone()
        } else {
            self.master.clone()
        };
        let thumbnail = super::decode::thumbnail(&master, THUMBNAIL).unwrap_or_else(|e| {
            log::warn!("thumbnail of {}: {e:#}", master.display());
            self.thumbnail.clone()
        });
        Ok(Recorded {
            path,
            format: self.format,
            master,
            width: self.size.0,
            height: self.size.1,
            duration: self.elapsed(),
            thumbnail,
            encoder,
        })
    }

    /// Stops and discards the recording (master included), without
    /// finishing files that are deleted anyway.
    pub fn cancel(mut self) {
        self.discard();
    }

    fn discard(&mut self) {
        self.control.abort.store(true, Ordering::Release);
        if self.finish_workers(None).is_some() && !self.format.is_video() {
            let _ = std::fs::remove_file(&self.master);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Neither stopped nor cancelled (an error while starting): discard.
        self.discard();
    }
}

/// Whether `path` holds at least one readable H.264 packet.
fn playable(path: &Path) -> bool {
    let Ok(Some(mut demuxer)) = container::open_demuxer(path) else {
        return false;
    };
    let Some(video) = demuxer.tracks().iter().position(|t| t.codec == Codec::H264) else {
        return false;
    };
    loop {
        match demuxer.next_packet() {
            Ok(Some(packet)) if packet.track == video => return true,
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => return false,
        }
    }
}

fn capture_video(mut source: Box<dyn VideoSource>, control: &Control) {
    let mut paused = false;
    while !control.stop.load(Ordering::Acquire) {
        let pause = control.paused.load(Ordering::Acquire);
        if pause != paused {
            source.set_paused(pause);
            paused = pause;
        }
        match source.next_frame(POLL) {
            // Frames captured while paused have no session time.
            Ok(Some(frame)) => {
                if let Some(at) = control.time(frame.captured)
                    && let Some(dropped) = control.inbox.push_video(frame, at)
                    && (dropped == 1 || dropped.is_multiple_of(100))
                {
                    log::warn!("the encoder falls behind the capture: {dropped} frames dropped");
                }
            }
            Ok(None) => {}
            Err(error) => {
                control.fail(&error);
                break;
            }
        }
    }
}

fn capture_audio(
    index: usize,
    mut source: Box<dyn AudioSource>,
    samples_per_second: f64,
    control: &Control,
) {
    while !control.stop.load(Ordering::Acquire) {
        match source.read(POLL) {
            Ok(Some(chunk)) => {
                let Some(at) = control.time(chunk.captured) else {
                    continue;
                };
                let length =
                    Duration::from_secs_f64(chunk.samples.len() as f64 / samples_per_second);
                if let Err(error) = control.inbox.push_audio(index, chunk.samples, at, length) {
                    control.fail(&error);
                    break;
                }
            }
            Ok(None) => {}
            Err(error) => {
                // A lost device ends its own track; the mix plays silence for it.
                log::warn!("audio source {index} ended; recording continues without it: {error:#}");
                break;
            }
        }
    }
}

/// The encoder thread.
fn encode(mut sink: Sink, control: &Control) -> Outcome {
    let encoder = sink.name();
    let result = (|| -> Result<()> {
        loop {
            if control.abort.load(Ordering::Acquire) {
                return Ok(());
            }
            let batch = control.inbox.take(IDLE);
            for (source, samples, at) in batch.audio {
                sink.add_audio(source, &samples, at);
            }
            if let Some((frame, at)) = batch.video {
                sink.frame(frame, at)?;
            }
            // A static screen repeats its picture, never ahead of a queued one.
            sink.tick(control.elapsed(), !batch.more_video)?;
            if batch.closed {
                return Ok(());
            }
        }
    })();
    // Frames still queued hold capture buffers.
    let clear_queue = || control.inbox.queues.lock().unwrap().video.clear();
    if control.abort.load(Ordering::Acquire) {
        // Thrown away: the session deletes the file, unfinished.
        clear_queue();
        return Outcome {
            encoder,
            error: None,
        };
    }
    let dropped = control.inbox.dropped();
    if dropped > 0 {
        log::warn!("{dropped} frames were dropped because the encoder fell behind");
    }
    let end = control.elapsed();
    let error = match result {
        Ok(()) => sink.finish(end).err(),
        Err(error) => {
            control.fail(&error);
            clear_queue();
            if let Err(e) = sink.salvage(control.elapsed()) {
                log::warn!("finishing what was recorded: {e:#}");
            }
            Some(error)
        }
    };
    Outcome { encoder, error }
}

enum Sink {
    Video(VideoSink),
    Master(MasterSink),
}

impl Sink {
    fn name(&self) -> String {
        match self {
            Sink::Video(video) => video.video.name().to_owned(),
            Sink::Master(_) => "lossless master".to_owned(),
        }
    }

    fn add_audio(&mut self, source: usize, samples: &[f32], at: Duration) {
        if let Sink::Video(video) = self
            && let Some((mixer, _)) = &mut video.audio
        {
            mixer.add(source, samples, at);
        }
    }

    fn frame(&mut self, frame: Frame, at: Duration) -> Result<()> {
        let size = match self {
            Sink::Video(video) => video.source_size,
            Sink::Master(master) => master.size,
        };
        ensure!(
            (frame.width(), frame.height()) == size,
            "The captured size changed from {}x{} to {}x{} while recording",
            size.0,
            size.1,
            frame.width(),
            frame.height()
        );
        match self {
            Sink::Video(video) => video.frame(frame, at),
            Sink::Master(master) => master.frame(frame, at),
        }
    }

    /// Writes the audio mixed up to `now`; with `repeat`, repeats a static
    /// picture that is due. A master writes out the frames compressed so
    /// far, so a static screen's frames reach the disk too.
    fn tick(&mut self, now: Duration, repeat: bool) -> Result<()> {
        match self {
            Sink::Video(video) => video.tick(now, repeat),
            Sink::Master(master) => master.writer.write_ready(),
        }
    }

    fn finish(self, end: Duration) -> Result<()> {
        match self {
            Sink::Video(video) => video.finish(end),
            Sink::Master(master) => master.finish(end),
        }
    }

    /// Completes the file after a failure, keeping everything already
    /// encoded and leaving the possibly broken encoder alone.
    fn salvage(self, end: Duration) -> Result<()> {
        match self {
            Sink::Video(mut video) => {
                let held = video.video.finish_held(end, &mut *video.mux);
                let audio = video.finish_audio(end);
                video.mux.finish().and(held).and(audio)
            }
            Sink::Master(master) => master.finish(end),
        }
    }
}

struct VideoSink {
    mux: Box<dyn Muxer>,
    video: H264Track,
    /// Captured frame size.
    source_size: (u32, u32),
    /// Encoded size: the capture cropped to even, or scaled down when no
    /// encoder takes that.
    encoded: (u32, u32),
    scaled: bool,
    /// The latest picture, repeated on a static screen.
    last: Option<Yuv420>,
    audio: Option<(Mixer, OpusTrack)>,
}

impl VideoSink {
    fn create(
        config: &SessionConfig,
        source_size: (u32, u32),
        audio: &[Box<dyn AudioSource>],
    ) -> Result<Self> {
        let even = (source_size.0 & !1, source_size.1 & !1);
        ensure!(
            even.0 > 0 && even.1 > 0,
            "A {}x{} capture is too small to record",
            source_size.0,
            source_size.1
        );
        let encoded = export::encodable_size(even, config.fps, config.encoder);
        let video = H264Track::new(
            VIDEO_TRACK,
            encoded.0,
            encoded.1,
            config.fps,
            config.encoder,
        )?;
        let audio = if audio.is_empty() {
            None
        } else {
            let specs: Vec<_> = audio.iter().map(|source| source.spec()).collect();
            Some((
                Mixer::new(&specs)?,
                OpusTrack::new(AUDIO_TRACK, Duration::ZERO)?,
            ))
        };
        let mut tracks = vec![video.track()];
        tracks.extend(audio.as_ref().map(|(_, opus)| opus.track()));
        let mux = container::create_muxer(
            config.output.part(),
            export::container_for(config.format, config.fragmented)?,
            &tracks,
        )?;
        Ok(Self {
            mux,
            video,
            source_size,
            encoded,
            scaled: encoded != even,
            last: None,
            audio,
        })
    }

    fn frame(&mut self, frame: Frame, at: Duration) -> Result<()> {
        let (width, height) = self.encoded;
        let picture = if self.scaled {
            let image = convert::resize(&frame.to_image(), width, height);
            drop(frame);
            convert::to_yuv420(
                &image.data,
                image.stride(),
                PixelFormat::Bgra,
                width,
                height,
                self.video.layout(),
            )
        } else {
            // Odd sizes are cropped by converting the top-left even part.
            let picture = convert::to_yuv420(
                frame.pixels(),
                frame.stride(),
                frame.format(),
                width,
                height,
                self.video.layout(),
            );
            // Hands a shared capture buffer back to its pool.
            drop(frame);
            picture
        };
        self.video.push(&picture, at, false, &mut *self.mux)?;
        self.last = Some(picture);
        Ok(())
    }

    fn tick(&mut self, now: Duration, repeat: bool) -> Result<()> {
        if repeat
            && let (Some(last), Some(picture)) = (self.video.last_pts(), &self.last)
            && now >= last + self.video.key_interval()
        {
            self.video.push(picture, now, true, &mut *self.mux)?;
        }
        if let Some((mixer, opus)) = &mut self.audio {
            opus.push(&mixer.take(now), &mut *self.mux)?;
        }
        Ok(())
    }

    fn finish_audio(&mut self, end: Duration) -> Result<()> {
        if let Some((mixer, opus)) = &mut self.audio {
            opus.push(&mixer.finish(end), &mut *self.mux)?;
            opus.finish(&mut *self.mux)?;
        }
        Ok(())
    }

    /// Finishes the file even when draining the encoder or flushing audio
    /// fails, so it keeps everything written; reports the first error.
    fn finish(mut self, end: Duration) -> Result<()> {
        let video = self.video.finish(end, &mut *self.mux);
        let audio = self.finish_audio(end);
        let mux = self.mux.finish();
        video.and(audio).and(mux)
    }
}

struct MasterSink {
    writer: MasterWriter,
    size: (u32, u32),
    last_pts: Option<Duration>,
}

impl MasterSink {
    fn frame(&mut self, frame: Frame, at: Duration) -> Result<()> {
        let image = frame.to_image();
        drop(frame);
        let pts = match self.last_pts {
            Some(last) if at <= last => last + Duration::from_millis(1),
            _ => at,
        };
        self.writer.push(image, pts)?;
        self.last_pts = Some(pts);
        Ok(())
    }

    fn finish(self, end: Duration) -> Result<()> {
        self.writer
            .finish(end.max(self.last_pts.unwrap_or_default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_excludes_pauses() {
        let start = Instant::now();
        let s = Duration::from_secs;
        let clock = Clock {
            start,
            pauses: vec![(start + s(2), Some(start + s(5))), (start + s(7), None)],
            end: None,
        };
        assert_eq!(clock.at(start + s(1)), Some(s(1)));
        assert_eq!(clock.at(start + s(3)), None);
        assert_eq!(clock.at(start + s(6)), Some(s(3)));
        assert_eq!(clock.at(start + s(8)), None);
        assert_eq!(clock.elapsed(), s(4));
    }
}
