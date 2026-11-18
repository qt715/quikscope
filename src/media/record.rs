//! Capture workers and a monotonic, pause-aware recording clock.
use super::encode::{Muxer, OutputFile, VideoEncoder};
use super::{
    AudioChunk, AudioSource, AudioSpec, EncoderPreference, Frame, OutputFormat, VideoSource,
};
use crate::raster::Image;
use anyhow::{Context as _, Result, ensure};
use ffmpeg_next as ffmpeg;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// What to record into.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionConfig {
    /// Final file. Videos are written here directly (as `<path>.part` until
    /// finished); animations are first recorded losslessly to `master` and
    /// encoded into `path` when the session stops.
    pub path: PathBuf,
    pub format: OutputFormat,
    /// Lossless intermediate for animations (FFV1 in Matroska), kept after
    /// the session so the annotator can re-export without generation loss.
    /// Ignored for videos.
    pub master: PathBuf,
    pub fps: u32,
    /// Audio device ids for videos (see [`super::audio::devices`]); ignored
    /// for animations.
    pub audio: Vec<String>,
    pub encoder: EncoderPreference,
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
    /// A representative frame (for notifications and the clipboard preview).
    pub thumbnail: Image,
    /// Encoder actually used, e.g. "h264_nvenc" or "libx264", for logs.
    pub encoder: String,
}

struct Clock {
    start: Instant,
    pauses: Vec<(Instant, Option<Instant>)>,
    end: Option<Instant>,
}
impl Clock {
    fn at(&self, at: Instant) -> Option<Duration> {
        if at < self.start {
            return None;
        }
        let mut time = at.duration_since(self.start);
        for &(begin, end) in &self.pauses {
            if at < begin {
                break;
            }
            if end.is_none_or(|end| at < end) {
                return None;
            }
            time = time.saturating_sub(end.unwrap().duration_since(begin));
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
struct Control {
    stop: AtomicBool,
    clock: Mutex<Clock>,
    failure: Mutex<Option<String>>,
}
impl Control {
    fn fail(&self, error: impl std::fmt::Display) {
        let mut failure = self.failure.lock().unwrap();
        if failure.is_none() {
            *failure = Some(error.to_string());
        }
        self.clock
            .lock()
            .unwrap()
            .end
            .get_or_insert_with(Instant::now);
        self.stop.store(true, Ordering::Release);
    }
}
enum Event {
    Video(Frame, Duration),
    Audio(usize, AudioChunk, Duration),
}

pub struct Session {
    config: SessionConfig,
    size: (u32, u32),
    control: Arc<Control>,
    pulls: Vec<JoinHandle<()>>,
    encoder: Option<JoinHandle<Result<(String, OutputFile)>>>,
}
impl Session {
    pub fn start(config: SessionConfig, mut source: Box<dyn VideoSource>) -> Result<Self> {
        let mut audio = Vec::new();
        if config.format.is_video() {
            for id in &config.audio {
                audio.push(super::audio::open(id)?);
            }
            if let Some(source) = source.take_audio() {
                audio.push(source);
            }
        }
        Self::start_with_audio(config, source, audio)
    }

    /// Allows embedded capture sources to supply additional audio tracks.
    pub fn start_with_audio(
        config: SessionConfig,
        mut source: Box<dyn VideoSource>,
        mut audio: Vec<Box<dyn AudioSource>>,
    ) -> Result<Self> {
        super::init();
        ensure!(
            config.fps > 0 && config.fps <= 240,
            "Frame rate must be 1–240"
        );
        if !config.format.is_video() {
            audio.clear();
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let first = loop {
            if let Some(frame) = source.next_frame(Duration::from_millis(40))? {
                break frame;
            }
            ensure!(
                Instant::now() < deadline,
                "Screen source did not produce its first frame"
            );
        };
        let size = (first.width() & !1, first.height() & !1);
        let master = !config.format.is_video();
        ensure!(
            !master || config.master != config.path,
            "Animation master and output must differ"
        );
        let transaction = OutputFile::new(if master { &config.master } else { &config.path });
        let video = VideoEncoder::select(first.av(), config.fps, config.encoder, master)?;
        let mux = Muxer::new(
            &transaction.path,
            if master {
                "matroska"
            } else {
                mux_name(config.format)
            },
            video,
            !audio.is_empty(),
        )?;
        let mixer = Mixer::new(&audio.iter().map(|s| s.spec()).collect::<Vec<_>>())?;
        let control = Arc::new(Control {
            stop: AtomicBool::new(false),
            clock: Mutex::new(Clock {
                start: first.captured,
                pauses: Vec::new(),
                end: None,
            }),
            failure: Mutex::new(None),
        });
        let (send, receive) = mpsc::sync_channel(16);
        let worker_control = control.clone();
        let fps = config.fps;
        let encoder = thread::Builder::new()
            .name("quikscope-encode".into())
            .spawn(
                move || match encode_loop(mux, first, receive, &worker_control, fps, mixer) {
                    Ok(name) => Ok((name, transaction)),
                    Err(error) => {
                        worker_control.fail(&error);
                        Err(error)
                    }
                },
            )?;
        let mut pulls = Vec::new();
        let video_control = control.clone();
        let video_send = send.clone();
        pulls.push(
            thread::Builder::new()
                .name("quikscope-video".into())
                .spawn(move || {
                    while !video_control.stop.load(Ordering::Acquire) {
                        match source.next_frame(Duration::from_millis(40)) {
                            Ok(Some(frame)) => {
                                let time = video_control.clock.lock().unwrap().at(frame.captured);
                                if let Some(time) = time
                                    && let Err(mpsc::TrySendError::Disconnected(_)) =
                                        video_send.try_send(Event::Video(frame, time))
                                {
                                    break;
                                }
                            }
                            Ok(None) => {}
                            Err(error) => {
                                video_control.fail(error);
                                break;
                            }
                        }
                    }
                })
                .inspect_err(|_| {
                    control.stop.store(true, Ordering::Release);
                })?,
        );
        for (index, mut source) in audio.into_iter().enumerate() {
            let audio_control = control.clone();
            let send = send.clone();
            pulls.push(
                thread::Builder::new()
                    .name(format!("quikscope-audio-{index}"))
                    .spawn(move || {
                        while !audio_control.stop.load(Ordering::Acquire) {
                            match source.read(Duration::from_millis(40)) {
                                Ok(Some(chunk)) => {
                                    let time =
                                        audio_control.clock.lock().unwrap().at(chunk.captured);
                                    if let Some(time) = time
                                        && let Err(mpsc::TrySendError::Disconnected(_)) =
                                            send.try_send(Event::Audio(index, chunk, time))
                                    {
                                        break;
                                    }
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    audio_control.fail(error);
                                    break;
                                }
                            }
                        }
                    })
                    .inspect_err(|_| {
                        control.stop.store(true, Ordering::Release);
                    })?,
            );
        }
        drop(send);
        Ok(Self {
            config,
            size,
            control,
            pulls,
            encoder: Some(encoder),
        })
    }
    pub fn size(&self) -> (u32, u32) {
        self.size
    }
    pub fn pause(&self) {
        let mut clock = self.control.clock.lock().unwrap();
        if clock.pauses.last().is_none_or(|(_, end)| end.is_some()) {
            clock.pauses.push((Instant::now(), None));
        }
    }
    pub fn resume(&self) {
        let mut clock = self.control.clock.lock().unwrap();
        if let Some((_, end)) = clock.pauses.last_mut()
            && end.is_none()
        {
            *end = Some(Instant::now());
        }
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
        self.control.clock.lock().unwrap().elapsed()
    }
    pub fn failure(&self) -> Option<String> {
        self.control.failure.lock().unwrap().clone()
    }

    fn finish_workers(&mut self) -> Result<(String, OutputFile)> {
        self.control
            .clock
            .lock()
            .unwrap()
            .end
            .get_or_insert_with(Instant::now);
        self.control.stop.store(true, Ordering::Release);
        for pull in self.pulls.drain(..) {
            if pull.join().is_err() {
                self.control.fail("Capture thread panicked");
            }
        }
        self.encoder
            .take()
            .context("Recording already stopped")?
            .join()
            .map_err(|_| anyhow::anyhow!("Encoder thread panicked"))?
    }
    pub fn stop(mut self) -> Result<Recorded> {
        let (encoder, file) = self.finish_workers()?;
        file.commit()?;
        if !self.config.format.is_video() {
            super::export::export(
                &self.config.master,
                &self.config.path,
                &super::export::ExportOptions {
                    format: self.config.format,
                    trim: None,
                    mute: true,
                    fps: Some(self.config.fps),
                    max_width: None,
                    encoder: self.config.encoder,
                },
                None,
                &|_| {},
                &AtomicBool::new(false),
            )?;
        }
        let master = if self.config.format.is_video() {
            self.config.path.clone()
        } else {
            self.config.master.clone()
        };
        let thumbnail = super::decode::thumbnail(&master, 384)?;
        Ok(Recorded {
            path: self.config.path.clone(),
            format: self.config.format,
            master,
            width: self.size.0,
            height: self.size.1,
            duration: self.elapsed(),
            thumbnail,
            encoder,
        })
    }
    pub fn cancel(mut self) {
        let _ = self.finish_workers();
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if self.encoder.is_some() {
            let _ = self.finish_workers();
        }
    }
}
pub(crate) fn mux_name(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Mp4 => "mp4",
        OutputFormat::Mkv => "matroska",
        OutputFormat::Mov => "mov",
        OutputFormat::Gif => "gif",
        OutputFormat::Webp => "webp",
        OutputFormat::Avif => "avif",
        OutputFormat::Jxl => "image2pipe",
    }
}

struct Mixer {
    sources: Vec<ffmpeg::software::resampling::Context>,
    specs: Vec<AudioSpec>,
    pending: VecDeque<[f32; 2]>,
    cursor: usize,
}
impl Mixer {
    fn new(specs: &[AudioSpec]) -> Result<Self> {
        let mut sources = Vec::new();
        for spec in specs {
            ensure!(
                spec.rate > 0 && spec.channels > 0,
                "Invalid source audio specification"
            );
            sources.push(ffmpeg::software::resampling::Context::get(
                ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
                ffmpeg::ChannelLayout::default(spec.channels as i32),
                spec.rate,
                ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
                ffmpeg::ChannelLayout::STEREO,
                48000,
            )?);
        }
        Ok(Self {
            sources,
            specs: specs.to_vec(),
            pending: VecDeque::new(),
            cursor: 0,
        })
    }
    fn add(&mut self, source: usize, chunk: AudioChunk, at: Duration) -> Result<()> {
        let spec = self.specs[source];
        let count = chunk.samples.len() / spec.channels as usize;
        if count == 0 {
            return Ok(());
        }
        let mut input = ffmpeg::frame::Audio::new(
            ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Packed),
            count,
            ffmpeg::ChannelLayout::default(spec.channels as i32),
        );
        input.set_rate(spec.rate);
        unsafe {
            std::slice::from_raw_parts_mut(
                input.data_mut(0).as_mut_ptr().cast::<f32>(),
                count * spec.channels as usize,
            )
            .copy_from_slice(&chunk.samples[..count * spec.channels as usize]);
        }
        let delay = self.sources[source]
            .delay()
            .map_or(0, |d| d.output.max(0) as usize);
        let output = super::encode::resample(&mut self.sources[source], &input)?;
        let start = ((at.as_secs_f64() * 48000.0).round() as usize).saturating_sub(delay);
        let samples = unsafe {
            std::slice::from_raw_parts(output.data(0).as_ptr().cast::<f32>(), output.samples() * 2)
        };
        for (i, sample) in samples.chunks_exact(2).enumerate() {
            let position = start + i;
            if position < self.cursor {
                continue;
            }
            let offset = position - self.cursor;
            ensure!(
                offset <= 48000 * 10,
                "Audio timestamp is more than ten seconds ahead"
            );
            if self.pending.len() <= offset {
                self.pending.resize(offset + 1, [0.0; 2]);
            }
            self.pending[offset][0] += sample[0];
            self.pending[offset][1] += sample[1];
        }
        Ok(())
    }
    fn flush(&mut self, mux: &mut Muxer, until: Duration) -> Result<()> {
        if self.sources.is_empty() {
            return Ok(());
        }
        let end = (until.as_secs_f64() * 48000.0).round() as usize;
        let mut block = [0.0; 2048];
        while self.cursor < end {
            let count = (end - self.cursor).min(block.len() / 2);
            for pair in block[..count * 2].chunks_exact_mut(2) {
                pair.copy_from_slice(&self.pending.pop_front().unwrap_or([0.0; 2]));
            }
            mux.audio(&block[..count * 2])?;
            self.cursor += count;
        }
        Ok(())
    }
}

fn encode_loop(
    mut mux: Muxer,
    first: Frame,
    receive: mpsc::Receiver<Event>,
    control: &Control,
    fps: u32,
    mut mixer: Mixer,
) -> Result<String> {
    let name = mux.name().to_owned();
    let mut last = first;
    let mut tick = 1_u64;
    let time = |tick: u64| Duration::from_secs_f64(tick as f64 / fps as f64);
    mux.video(last.av(), Duration::ZERO)?;
    loop {
        match receive.recv_timeout(Duration::from_millis(10)) {
            Ok(Event::Video(frame, at)) => {
                let target = (at.as_secs_f64() * fps as f64).round() as u64;
                if target >= tick {
                    while tick < target {
                        mux.video(last.av(), time(tick))?;
                        tick += 1;
                    }
                    last = frame;
                    mux.video(last.av(), time(tick))?;
                    tick += 1;
                }
            }
            Ok(Event::Audio(source, chunk, at)) => mixer.add(source, chunk, at)?,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let elapsed = control.clock.lock().unwrap().elapsed();
                while time(tick + 1) <= elapsed {
                    mux.video(last.av(), time(tick))?;
                    tick += 1;
                }
            }
        }
        mixer.flush(
            &mut mux,
            control
                .clock
                .lock()
                .unwrap()
                .elapsed()
                .saturating_sub(Duration::from_millis(100)),
        )?;
    }
    let elapsed = control.clock.lock().unwrap().elapsed();
    while time(tick) < elapsed {
        mux.video(last.av(), time(tick))?;
        tick += 1;
    }
    mixer.flush(&mut mux, time(tick))?;
    mux.finish()?;
    Ok(name)
}
