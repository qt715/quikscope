//! The media engine end to end: recording sessions with synthetic sources,
//! decoding and exporting, on fixtures made by our own encoders
//! (`support/fixtures.rs`). `ffprobe` cross-checks files when installed.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use quikscope::media::decode::{self, AudioDecoder, DecodedFrame, Decoder};
use quikscope::media::export::{self, ExportOptions, Trim};
use quikscope::media::record::{Cancelled, Recorded, Session, SessionConfig};
use quikscope::media::{
    self, AudioChunk, AudioSource, AudioSpec, EncoderPreference, Frame, Kind, OutputFormat,
    VideoSource,
};
use quikscope::output::OutputFile;
use quikscope::raster::Image;

#[path = "support/fixtures.rs"]
mod fixtures;

use fixtures::{Clip, Tone, index_of, pattern};

fn init_logs() {
    let _ = env_logger::builder().is_test(true).try_init();
}

/// [`pattern`] frames numbered from 0 at `fps`, every frame new.
struct Synthetic {
    next: Instant,
    index: u32,
    fps: u32,
    size: (u32, u32),
    /// After this many frames, one frame of another size.
    resize_after: Option<u32>,
}

impl Synthetic {
    fn new(fps: u32) -> Self {
        Self {
            next: Instant::now(),
            index: 0,
            fps,
            size: (160, 96),
            resize_after: None,
        }
    }
}

impl VideoSource for Synthetic {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let now = Instant::now();
        if self.next > now {
            let delay = self.next - now;
            thread::sleep(delay.min(timeout));
            if delay > timeout {
                return Ok(None);
            }
        }
        let captured = self.next;
        let (width, height) = match self.resize_after {
            Some(n) if self.index >= n => (self.size.0 / 2, self.size.1 / 2),
            _ => self.size,
        };
        let image = pattern(width.max(128), height, self.index % 256);
        self.index += 1;
        self.next += Duration::from_secs_f64(1.0 / self.fps as f64);
        Ok(Some(Frame::from_image(&image, captured)))
    }
}

/// [`pattern`] frames like a live screen capture at `fps`: each call
/// delivers the picture of the current frame slot, captured then; slots
/// nobody asked for in time are gone, as a compositor drops them.
struct Live {
    start: Instant,
    interval: Duration,
    /// Pre-drawn pictures, cycled (drawing one per frame would be slower
    /// than the capture rate in a test build).
    pictures: Vec<Image>,
    last: Option<u64>,
}

impl Live {
    fn new(fps: u32, width: u32, height: u32) -> Self {
        Self {
            start: Instant::now(),
            interval: Duration::from_secs(1) / fps,
            pictures: (0..32).map(|i| pattern(width, height, i)).collect(),
            last: None,
        }
    }
}

impl VideoSource for Live {
    fn size(&self) -> (u32, u32) {
        (self.pictures[0].width, self.pictures[0].height)
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let slot_of = |at: Instant| (at - self.start).as_nanos() / self.interval.as_nanos();
        let mut slot = slot_of(Instant::now()) as u64;
        if self.last == Some(slot) {
            let next = self.start + self.interval * (slot as u32 + 1);
            let wait = next.saturating_duration_since(Instant::now());
            if wait > timeout {
                thread::sleep(timeout);
                return Ok(None);
            }
            thread::sleep(wait);
            slot += 1;
        }
        self.last = Some(slot);
        let picture = &self.pictures[slot as usize % self.pictures.len()];
        Ok(Some(Frame::from_image(picture, Instant::now())))
    }
}

/// A picture the test changes; delivered like a damage-driven capture: only
/// when it changes, plus once after opening and after every resume.
#[derive(Clone)]
struct Screen {
    picture: Arc<Mutex<(u32, Image)>>,
    pause_calls: Arc<AtomicU32>,
}

impl Screen {
    fn new(image: Image) -> Self {
        Self {
            picture: Arc::new(Mutex::new((0, image))),
            pause_calls: Arc::default(),
        }
    }

    fn show(&self, image: Image) {
        let mut picture = self.picture.lock().unwrap();
        *picture = (picture.0 + 1, image);
    }

    fn source(&self) -> Box<dyn VideoSource> {
        Box::new(Damage {
            screen: self.clone(),
            sent: None,
            paused: false,
        })
    }
}

struct Damage {
    screen: Screen,
    /// Generation last delivered.
    sent: Option<u32>,
    paused: bool,
}

impl VideoSource for Damage {
    fn size(&self) -> (u32, u32) {
        let picture = self.screen.picture.lock().unwrap();
        (picture.1.width, picture.1.height)
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        if !self.paused {
            let picture = self.screen.picture.lock().unwrap();
            if self.sent != Some(picture.0) {
                self.sent = Some(picture.0);
                return Ok(Some(Frame::from_image(&picture.1, Instant::now())));
            }
        }
        thread::sleep(timeout.min(Duration::from_millis(5)));
        Ok(None)
    }

    fn set_paused(&mut self, paused: bool) {
        self.screen.pause_calls.fetch_add(1, Ordering::Relaxed);
        self.paused = paused;
        if !paused {
            self.sent = None;
        }
    }
}

/// A sine in 10 ms chunks of contiguous samples whose timestamps jitter by
/// up to ±3 ms, like a real capture.
struct Sine {
    start: Instant,
    sample: usize,
    rate: u32,
    frequency: f32,
}

impl Sine {
    fn new(rate: u32, frequency: f32) -> Self {
        Self {
            start: Instant::now(),
            sample: 0,
            rate,
            frequency,
        }
    }
}

impl AudioSource for Sine {
    fn spec(&self) -> AudioSpec {
        AudioSpec {
            rate: self.rate,
            channels: 1,
        }
    }

    fn read(&mut self, timeout: Duration) -> Result<Option<AudioChunk>> {
        let count = self.rate as usize / 100;
        let ideal = self.start + Duration::from_secs_f64(self.sample as f64 / self.rate as f64);
        let ready = ideal + Duration::from_millis(10);
        let now = Instant::now();
        if ready > now {
            let delay = ready - now;
            thread::sleep(delay.min(timeout));
            if delay > timeout {
                return Ok(None);
            }
        }
        let samples = (self.sample..self.sample + count)
            .map(|i| {
                (i as f32 * self.frequency * std::f32::consts::TAU / self.rate as f32).sin() * 0.2
            })
            .collect();
        let jitter = [0, 3, -2, 1, -3, 2][self.sample / count % 6];
        let captured = if jitter >= 0 {
            ideal + Duration::from_millis(jitter as u64)
        } else {
            ideal
                .checked_sub(Duration::from_millis(-jitter as u64))
                .unwrap_or(ideal)
        };
        self.sample += count;
        Ok(Some(AudioChunk { samples, captured }))
    }
}

/// A session writing a new file, reserved as the daemon reserves recordings.
fn config(path: &Path, format: OutputFormat, encoder: EncoderPreference) -> SessionConfig {
    config_into(OutputFile::reserve(path).unwrap(), format, encoder)
}

fn config_into(
    output: OutputFile,
    format: OutputFormat,
    encoder: EncoderPreference,
) -> SessionConfig {
    SessionConfig {
        master: output.path().with_extension("master"),
        output,
        format,
        fps: 20,
        audio: vec![],
        encoder,
        fragmented: true,
    }
}

fn stop(session: Session) -> Result<Recorded> {
    session.stop(&|_| {}, &AtomicBool::new(false))
}

fn record_for(session: Session, length: Duration) -> Result<Recorded> {
    thread::sleep(length);
    assert!(session.failure().is_none(), "{:?}", session.failure());
    stop(session)
}

fn decoded(path: &Path) -> Result<Vec<DecodedFrame>> {
    let mut decoder = Decoder::open(path)?;
    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame()? {
        frames.push(frame);
    }
    Ok(frames)
}

/// The whole audio track at 48 kHz stereo and the time of its first sample.
fn decoded_audio(path: &Path) -> Result<(Vec<f32>, Duration)> {
    let mut audio = AudioDecoder::open(path, 48000, 2)?.context("no audio")?;
    let (mut samples, start) = audio.next_chunk()?.context("empty audio")?;
    while let Some((chunk, _)) = audio.next_chunk()? {
        samples.extend(chunk);
    }
    Ok((samples, start))
}

fn first_audio_time(path: &Path) -> Result<Duration> {
    let mut audio = AudioDecoder::open(path, 48000, 2)?.context("no audio")?;
    Ok(audio.next_chunk()?.context("empty audio")?.1)
}

fn options(format: OutputFormat) -> ExportOptions {
    ExportOptions {
        format,
        trim: None,
        mute: true,
        fps: None,
        fragmented: true,
        max_width: None,
        encoder: EncoderPreference::Software,
    }
}

fn export_to(src: &Path, dst: &Path, options: &ExportOptions) -> Result<PathBuf> {
    export::export(
        src,
        OutputFile::replace(dst)?,
        options,
        None,
        &|_| {},
        &AtomicBool::new(false),
    )
}

#[track_caller]
fn assert_near(actual: Duration, expected: Duration, tolerance: Duration) {
    assert!(
        actual.abs_diff(expected) <= tolerance,
        "{actual:?} is not within {tolerance:?} of {expected:?}"
    );
}

fn ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

/// The fixtures themselves: every container and animation format reads
/// back with its timing, content and seeking.
#[test]
fn fixtures_read_back_in_every_format() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    let clip = Clip {
        width: 320,
        height: 240,
        fps: 30,
        frames: 60,
        tone: Some(Tone {
            frequency: 440.0,
            amplitude: 0.2,
            start: Duration::ZERO,
            length: Duration::from_secs(2),
        }),
    };
    for format in OutputFormat::VIDEO {
        let path = dir.path().join(format!("clip.{format}"));
        fixtures::video(&path, format, &clip)?;
        fixtures::ffprobe(&path, &["h264", "opus"]);
        let info = decode::probe(&path)?;
        assert_eq!(
            (info.kind, info.width, info.height),
            (Kind::Video, 320, 240)
        );
        assert!(info.has_audio);
        assert!((info.fps - 30.0).abs() < 0.5, "{format}: {info:?}");
        assert_near(info.duration, clip.duration(), ms(40));
        let frames = decoded(&path)?;
        assert_eq!(frames.len(), 60, "{format}");
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(index_of(&frame.image), i as u32, "{format} frame {i}");
            assert_near(frame.pts, fixtures::frame_time(i as u32, 30), ms(1));
        }
        let mut decoder = Decoder::open(&path)?;
        decoder.seek(Duration::from_millis(1510))?;
        let frame = decoder.next_frame()?.context("seek")?;
        assert_eq!(index_of(&frame.image), 45, "{format} seek");
        assert_eq!(index_of(&decoder.next_frame()?.unwrap().image), 46);
        let (samples, start) = decoded_audio(&path)?;
        assert_near(start, Duration::ZERO, ms(1));
        assert_near(
            Duration::from_secs_f64(samples.len() as f64 / 96000.0),
            ms(2000),
            ms(30),
        );
    }
    let frames: Vec<_> = (0..6).map(|i| (pattern(160, 96, i), ms(100))).collect();
    for format in OutputFormat::ANIMATION {
        let path = dir.path().join(format!("clip.{format}"));
        fixtures::animation(&path, format, &frames)?;
        let info = decode::probe(&path)?;
        assert_eq!(info.kind, Kind::Animation, "{format}");
        assert_eq!((info.width, info.height, info.frames), (160, 96, Some(6)));
        assert_near(info.duration, ms(600), ms(5));
        let mut decoder = Decoder::open(&path)?;
        decoder.seek(ms(250))?;
        let frame = decoder.next_frame()?.context("seek")?;
        assert_eq!(index_of(&frame.image), 2, "{format} seek");
        assert_eq!(frame.pts, ms(200));
        // Forward within the frames decoded ahead, forward past them,
        // back, and past the end (the last frame).
        for (to, index) in [(310, 3), (520, 5), (120, 1), (5000, 5)] {
            decoder.seek(ms(to))?;
            let frame = decoder.next_frame()?.context("seek")?;
            assert_eq!(index_of(&frame.image), index, "{format} seek to {to} ms");
            assert_eq!(frame.pts, ms(100 * u64::from(index)), "{format}");
        }
    }
    Ok(())
}

/// Sessions write H.264 + Opus in every container, with both encoder
/// choices, and the files read back with picture and sound.
#[test]
fn recordings_in_every_container() -> Result<()> {
    init_logs();
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Session>();
    send_sync::<Recorded>();
    let dir = tempfile::tempdir()?;
    for preference in [EncoderPreference::Auto, EncoderPreference::Software] {
        for format in OutputFormat::VIDEO {
            let path = dir.path().join(format!("{preference:?}.{format}"));
            let audio: Vec<Box<dyn AudioSource>> = vec![
                Box::new(Sine::new(44100, 440.0)),
                Box::new(Sine::new(48000, 660.0)),
            ];
            let session = Session::start_with_audio(
                config(&path, format, preference),
                Box::new(Synthetic::new(20)),
                audio,
            )?;
            let recorded = record_for(session, ms(600))?;
            eprintln!("{format} {preference:?}: {}", recorded.encoder);
            assert_eq!(recorded.path, path);
            assert_eq!((recorded.width, recorded.height), (160, 96));
            assert!(recorded.thumbnail.width > 0);
            assert_no_leftovers(dir.path());
            fixtures::ffprobe(&path, &["h264", "opus"]);
            assert_ffprobe_times(&path)?;
            let info = decode::probe(&path)?;
            assert_eq!(info.kind, Kind::Video);
            assert!(info.has_audio);
            assert_near(info.duration, recorded.duration, ms(60));
            let frames = decoded(&path)?;
            // 12 captured; a loaded machine may drop some.
            assert!(frames.len() >= 4, "{} frames", frames.len());
            // Every capture is a new picture: encoded in order, none twice.
            for pair in frames.windows(2) {
                assert!(index_of(&pair[1].image) > index_of(&pair[0].image));
            }
            let (samples, _) = decoded_audio(&path)?;
            let rms = (samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>()
                / samples.len() as f64)
                .sqrt();
            assert!(rms > 0.1, "{format}: audio RMS {rms}");
        }
    }
    Ok(())
}

/// Every video packet of `path` as quikscope's demuxer times it.
fn video_packet_times(path: &Path) -> Result<Vec<Duration>> {
    let mut demuxer = media::container::open_demuxer(path)?.context("not a container")?;
    let track = demuxer
        .tracks()
        .iter()
        .position(|t| t.codec == media::container::Codec::H264)
        .context("no video track")?;
    let mut times = Vec::new();
    while let Some(packet) = demuxer.next_packet()? {
        if packet.track == track {
            times.push(packet.pts);
        }
    }
    Ok(times)
}

/// ffprobe (when installed) times every video packet as quikscope does:
/// variable frame timing reaches other players intact.
fn assert_ffprobe_times(path: &Path) -> Result<()> {
    let Some(theirs) = fixtures::ffprobe_video_times(path) else {
        return Ok(());
    };
    let ours = video_packet_times(path)?;
    assert_eq!(theirs.len(), ours.len(), "ffprobe {}", path.display());
    for (i, (a, b)) in theirs.iter().zip(&ours).enumerate() {
        assert!(
            a.abs_diff(*b) <= ms(1),
            "{} packet {i}: ffprobe {a:?}, quikscope {b:?}",
            path.display()
        );
    }
    Ok(())
}

/// No partial or temporary files next to the recordings.
fn assert_no_leftovers(dir: &Path) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            !name.ends_with(".part") && !name.ends_with(".classic"),
            "{name} left over"
        );
    }
}

/// A 161×97 capture records as 160×96 with every pixel where it was: the
/// odd column and row are cropped, nothing is resampled.
#[test]
fn odd_capture_sizes_are_cropped_not_scaled() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    // Black left half, white right half from column 80, a white bottom band
    // from row 48: sharp edges on macroblock boundaries.
    let mut image = Image::filled(161, 97, [0, 0, 0, 255]);
    fixtures::fill(&mut image, 80, 0, 81, 97, [255, 255, 255, 255]);
    fixtures::fill(&mut image, 0, 48, 161, 49, [255, 255, 255, 255]);
    let screen = Screen::new(image.clone());

    let video = dir.path().join("odd.mp4");
    let session = Session::start(
        config(&video, OutputFormat::Mp4, EncoderPreference::Software),
        screen.source(),
    )?;
    assert_eq!(session.size(), (160, 96));
    record_for(session, ms(300))?;
    let frame = &decoded(&video)?[0];
    assert_eq!((frame.image.width, frame.image.height), (160, 96));
    for (x, y, bright) in [
        (79, 10, false),
        (80, 10, true),
        (10, 47, false),
        (10, 48, true),
    ] {
        let value = frame.image.pixel(x, y)[1];
        assert!(
            if bright { value > 235 } else { value < 20 },
            "pixel {x},{y} is {value}: the edge moved or blurred"
        );
    }

    // Animations keep the exact size in their lossless master.
    let gif = dir.path().join("odd.gif");
    let session = Session::start(
        config(&gif, OutputFormat::Gif, EncoderPreference::Software),
        screen.source(),
    )?;
    assert_eq!(session.size(), (161, 97));
    let recorded = record_for(session, ms(300))?;
    assert_eq!(decoded(&recorded.master)?[0].image, image);
    Ok(())
}

/// Pausing stops the capture; the first frame after resuming shows what
/// changed meanwhile, at the time recording resumed. Checked by order and
/// by the session clock, not by wall-clock tolerances a loaded machine
/// misses.
#[test]
fn resume_shows_a_fresh_frame() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("paused.mkv");
    let screen = Screen::new(pattern(160, 96, 1));
    let started = Instant::now();
    let session = Session::start(
        config(&path, OutputFormat::Mkv, EncoderPreference::Software),
        screen.source(),
    )?;
    thread::sleep(ms(400));
    session.pause();
    assert!(session.is_paused());
    let at_pause = session.elapsed();
    let paused = Instant::now();
    thread::sleep(ms(150));
    screen.show(pattern(160, 96, 2));
    thread::sleep(ms(300));
    assert_eq!(session.elapsed(), at_pause);
    let resumed = Instant::now();
    session.resume();
    thread::sleep(ms(400));
    let recorded = stop(session)?;
    let wall = started.elapsed();
    assert!(screen.pause_calls.load(Ordering::Relaxed) >= 2);
    // At least the 800 ms slept while recording, and none of the pause.
    assert!(recorded.duration >= ms(800), "{:?}", recorded.duration);
    assert!(
        recorded.duration <= wall - (resumed - paused),
        "{:?} includes the pause",
        recorded.duration
    );
    let frames = decoded(&path)?;
    let fresh = frames
        .iter()
        .position(|f| index_of(&f.image) == 2)
        .context("the picture changed while paused never shows")?;
    assert!(fresh > 0);
    assert!(frames[..fresh].iter().all(|f| index_of(&f.image) == 1));
    assert!(frames[fresh..].iter().all(|f| index_of(&f.image) == 2));
    // The old picture ends where recording paused; the new one continues
    // the timeline from there (times are whole milliseconds).
    assert!(frames[fresh - 1].pts <= at_pause);
    assert!(
        frames[fresh].pts + ms(1) >= at_pause,
        "{:?}",
        frames[fresh].pts
    );
    Ok(())
}

/// A static screen is not a backlog of duplicate frames: the last picture
/// repeats once per keyframe interval (seekable), and a change shows at its
/// own time.
#[test]
fn static_screens_record_variable_frame_rate() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("static.mp4");
    let screen = Screen::new(pattern(160, 96, 1));
    let mut config = config(&path, OutputFormat::Mp4, EncoderPreference::Software);
    config.fps = 30;
    let session = Session::start(config, screen.source())?;
    thread::sleep(ms(2600));
    let changed_at = session.elapsed();
    screen.show(pattern(160, 96, 2));
    let recorded = record_for(session, ms(700))?;

    let frames = decoded(&path)?;
    let times: Vec<_> = frames.iter().map(|f| f.pts).collect();
    eprintln!("frame times: {times:?}");
    // Two pictures, a repeat per second (a slow machine records longer).
    let most = recorded.duration.as_secs() as usize + 3;
    assert!(
        frames.len() <= most,
        "{} frames for 2 pictures",
        frames.len()
    );
    // A repeat is due a second after the last picture; allow for a busy
    // encoder thread.
    let repeat = ms(1500);
    for pair in frames.windows(2) {
        assert!(pair[1].pts > pair[0].pts);
        assert!(pair[1].pts - pair[0].pts <= repeat, "{times:?}");
        assert_near(pair[0].duration, pair[1].pts - pair[0].pts, ms(1));
    }
    let change = frames.iter().position(|f| index_of(&f.image) == 2).unwrap();
    assert!(frames[change].pts + ms(1) >= changed_at, "{times:?}");
    assert!(frames[change].pts <= changed_at + ms(500), "{times:?}");
    let last = frames.last().unwrap();
    assert_near(last.pts + last.duration, recorded.duration, ms(5));
    // Every repeat is a keyframe: seeking anywhere decodes at once.
    let mut demuxer = media::container::open_demuxer(&path)?.unwrap();
    let mut keys = Vec::new();
    while let Some(packet) = demuxer.next_packet()? {
        if packet.keyframe {
            keys.push(packet.pts);
        }
    }
    assert!(keys.windows(2).all(|k| k[1] - k[0] <= repeat), "{keys:?}");
    // Other players see the same irregular timing.
    assert_ffprobe_times(&path)?;
    Ok(())
}

/// The first frames flow without a pause: the encoder (a GPU encoder takes
/// ~150–200 ms to open) is ready by the time capture starts, instead of
/// opening after the first frame while the capture waits.
#[test]
fn recordings_start_without_a_gap() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("start.mkv");
    let mut config = config(&path, OutputFormat::Mkv, EncoderPreference::Auto);
    config.fps = 60;
    let session = Session::start(config, Box::new(Live::new(60, 640, 360)))?;
    let recorded = record_for(session, ms(1000))?;
    eprintln!("encoder: {}", recorded.encoder);
    let times = video_packet_times(&path)?;
    let start: Vec<_> = times.iter().take_while(|&&t| t < ms(500)).collect();
    eprintln!("first half second: {start:?}");
    assert!(
        start.len() >= 15,
        "{} frames in the first 500 ms",
        start.len()
    );
    // Three frame intervals at 60 fps.
    for pair in start.windows(2) {
        assert!(
            *pair[1] - *pair[0] <= ms(50),
            "a gap at the start: {start:?}"
        );
    }
    Ok(())
}

/// Jittery capture timestamps do not crackle: the mixer places samples by
/// count, Opus carries them, and the decoded tone is continuous.
#[test]
fn recorded_audio_is_continuous() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("tone.mp4");
    let session = Session::start_with_audio(
        config(&path, OutputFormat::Mp4, EncoderPreference::Software),
        Box::new(Synthetic::new(20)),
        vec![Box::new(Sine::new(44100, 440.0))],
    )?;
    let recorded = record_for(session, ms(1500))?;
    let (samples, _) = decoded_audio(&path)?;
    let left: Vec<f32> = samples.iter().step_by(2).copied().collect();
    assert_near(
        Duration::from_secs_f64(left.len() as f64 / 48000.0),
        recorded.duration,
        ms(60),
    );
    // Skip the ramp-in and the tail; in between every 10 ms is the tone
    // (no silence) and no step exceeds the sine's own (no discontinuity).
    let body = &left[4800..left.len() - 4800];
    let steepest = 0.2 * std::f32::consts::TAU * 440.0 / 48000.0;
    let worst = body
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max);
    assert!(
        worst < steepest * 2.5,
        "a jump of {worst} (the sine steps {steepest})"
    );
    for window in body.chunks(480) {
        let rms = (window.iter().map(|s| s * s).sum::<f32>() / window.len() as f32).sqrt();
        assert!(rms > 0.1, "a quiet stretch: RMS {rms}");
    }
    Ok(())
}

/// When encoding fails mid-recording (here: the capture changes size), the
/// session stops, reports why, and keeps a playable file of what it wrote.
#[test]
fn encoder_failure_keeps_what_was_recorded() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    for format in OutputFormat::VIDEO {
        let path = dir.path().join(format!("salvaged.{format}"));
        let mut source = Synthetic::new(20);
        source.resize_after = Some(30);
        let session = Session::start(
            config(&path, format, EncoderPreference::Software),
            Box::new(source),
        )?;
        let deadline = Instant::now() + Duration::from_secs(4);
        while session.failure().is_none() && Instant::now() < deadline {
            thread::sleep(ms(10));
        }
        let failure = session.failure().context("the failure was not reported")?;
        assert!(failure.contains("size changed"), "{failure}");
        let recorded = stop(session)?;
        assert_eq!(recorded.path, path);
        let frames = decoded(&path)?;
        assert!(frames.len() >= 25, "{format}: {} frames kept", frames.len());
        // Every frame up to the failure, none lost (the recording starts
        // from the newest frame once the encoder is open, not always 0).
        assert_eq!(
            index_of(&frames.last().unwrap().image) - index_of(&frames[0].image),
            frames.len() as u32 - 1
        );
        fixtures::ffprobe(&path, &["h264"]);
        assert_ffprobe_times(&path)?;
    }
    Ok(())
}

/// A source that fails ends the recording; what it captured is saved.
#[test]
fn capture_failure_keeps_what_was_recorded() -> Result<()> {
    struct Failing {
        sent: bool,
    }
    impl VideoSource for Failing {
        fn size(&self) -> (u32, u32) {
            (160, 96)
        }
        fn next_frame(&mut self, _: Duration) -> Result<Option<Frame>> {
            anyhow::ensure!(!self.sent, "Synthetic source disappeared");
            self.sent = true;
            Ok(Some(Frame::from_image(
                &pattern(160, 96, 7),
                Instant::now(),
            )))
        }
    }
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("failure.mp4");
    let session = Session::start(
        config(&path, OutputFormat::Mp4, EncoderPreference::Software),
        Box::new(Failing { sent: false }),
    )?;
    let deadline = Instant::now() + Duration::from_secs(1);
    while session.failure().is_none() && Instant::now() < deadline {
        thread::sleep(ms(1));
    }
    assert_eq!(
        session.failure().as_deref(),
        Some("Synthetic source disappeared")
    );
    stop(session)?;
    assert_eq!(index_of(&decoded(&path)?[0].image), 7);
    Ok(())
}

/// An audio device that cannot be opened is skipped with a warning; the
/// recording goes on without it.
#[test]
fn unavailable_audio_device_is_a_warning() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("silent.mp4");
    let mut config = config(&path, OutputFormat::Mp4, EncoderPreference::Software);
    config.audio = vec!["quikscope-test-no-such-device".into()];
    let session = Session::start(config, Box::new(Synthetic::new(20)))?;
    assert_eq!(session.warnings().len(), 1, "{:?}", session.warnings());
    assert!(session.warnings()[0].contains("quikscope-test-no-such-device"));
    record_for(session, ms(300))?;
    assert!(!decode::probe(&path)?.has_audio);
    Ok(())
}

/// Cancelling discards the recording: a new name is released, an existing
/// file being replaced stays as it was, an animation master goes too.
#[test]
fn cancel_discards() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("discard.mp4");
    let session = Session::start(
        config(&path, OutputFormat::Mp4, EncoderPreference::Software),
        Box::new(Synthetic::new(20)),
    )?;
    thread::sleep(ms(100));
    session.cancel();
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);

    std::fs::write(&path, b"existing final file")?;
    let session = Session::start(
        config_into(
            OutputFile::replace(&path)?,
            OutputFormat::Mp4,
            EncoderPreference::Software,
        ),
        Box::new(Synthetic::new(20)),
    )?;
    session.cancel();
    assert_eq!(std::fs::read(&path)?, b"existing final file");

    let gif = dir.path().join("discard.gif");
    let session = Session::start(
        config(&gif, OutputFormat::Gif, EncoderPreference::Software),
        Box::new(Synthetic::new(20)),
    )?;
    thread::sleep(ms(100));
    session.cancel();
    assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
    Ok(())
}

/// Setting the cancel flag while an animation's final file is encoded stops
/// the encode and keeps the complete master; while a video is saved it
/// discards the video without finishing it.
#[test]
fn cancelling_while_saving() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    let avif = dir.path().join("cancelled.avif");
    let session = Session::start(
        config(&avif, OutputFormat::Avif, EncoderPreference::Software),
        Box::new(Synthetic::new(20)),
    )?;
    thread::sleep(ms(600));
    let cancel = AtomicBool::new(false);
    let reports = Mutex::new(Vec::new());
    let error = session
        .stop(
            &|progress| {
                reports.lock().unwrap().push(progress);
                if progress > 0.0 {
                    cancel.store(true, Ordering::Release);
                }
            },
            &cancel,
        )
        .unwrap_err();
    let cancelled = error
        .downcast_ref::<Cancelled>()
        .with_context(|| format!("not a cancel: {error:#}"))?;
    let master = avif.with_extension("master");
    assert_eq!(cancelled.master.as_deref(), Some(master.as_path()));
    assert!(error.to_string().contains(&master.display().to_string()));
    let reports = reports.into_inner().unwrap();
    assert_eq!(reports[0], 0.0);
    assert!(reports.iter().any(|&p| p > 0.0 && p < 1.0), "{reports:?}");
    // No final file, and the master is finished (indexed, every frame).
    let names: Vec<_> = std::fs::read_dir(dir.path())?
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["cancelled.master"]);
    let frames = decoded(&master)?;
    assert!(frames.len() >= 6, "{} frames", frames.len());
    let info = decode::probe(&master)?;
    let last = frames.last().unwrap();
    assert_eq!(info.duration, last.pts + last.duration);

    // A classic MP4 is not rewritten just to be deleted.
    let video = dir.path().join("cancelled.mp4");
    let mut config = config(&video, OutputFormat::Mp4, EncoderPreference::Software);
    config.fragmented = false;
    let session = Session::start(config, Box::new(Synthetic::new(20)))?;
    thread::sleep(ms(300));
    let error = session.stop(&|_| {}, &AtomicBool::new(true)).unwrap_err();
    assert_eq!(
        error.downcast_ref::<Cancelled>(),
        Some(&Cancelled { master: None })
    );
    let names: Vec<_> = std::fs::read_dir(dir.path())?
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["cancelled.master"]);
    Ok(())
}

/// Environment variable naming the file [`crash_while_recording`] records.
const CRASH_RECORDING: &str = "QUIKSCOPE_TEST_CRASH_RECORDING";
/// Its exit status.
const CRASHED: i32 = 86;

/// Not a test by itself: [`crashed_recordings_stay_playable`] runs it in a
/// child process, which records into the file the environment names and
/// then exits mid-recording: no destructor, no finish, buffered data lost,
/// as when quikscope is killed.
#[test]
fn crash_while_recording() -> Result<()> {
    let Some(path) = std::env::var_os(CRASH_RECORDING).map(PathBuf::from) else {
        return Ok(());
    };
    let format = OutputFormat::from_path(&path).context("no format")?;
    let session = Session::start(
        config(&path, format, EncoderPreference::Software),
        Box::new(Synthetic::new(20)),
    )?;
    thread::sleep(ms(2500));
    assert!(session.failure().is_none(), "{:?}", session.failure());
    std::process::exit(CRASHED);
}

/// Fragmented MP4 and Matroska recordings are written in place: a killed
/// recording leaves a playable file under its real name, with everything up
/// to its last complete fragment.
#[test]
fn crashed_recordings_stay_playable() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    for format in [OutputFormat::Mp4, OutputFormat::Mkv] {
        let path = dir.path().join(format!("crashed.{format}"));
        let child = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "crash_while_recording", "--nocapture"])
            .env(CRASH_RECORDING, &path)
            .output()?;
        assert_eq!(
            child.status.code(),
            Some(CRASHED),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        let names: Vec<_> = std::fs::read_dir(dir.path())?
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(&format!("crashed.{format}")))
            .collect();
        assert_eq!(names, [format!("crashed.{format}")]);
        let frames = decoded(&path)?;
        // Fragments end at keyframes, one a second: at 2.5 s the first two
        // seconds are on disk.
        assert!(frames.len() >= 20, "{format}: {} frames", frames.len());
        // Captures in order, each once.
        for pair in frames.windows(2) {
            assert!(
                index_of(&pair[1].image) > index_of(&pair[0].image),
                "{format}"
            );
        }
        // Other players read the same packets (they may also get some of
        // the cut-off fragment's).
        if let Some(theirs) = fixtures::ffprobe_video_times(&path) {
            let ours = video_packet_times(&path)?;
            assert!(theirs.len() >= ours.len(), "{format}: {theirs:?}");
            for (a, b) in theirs.iter().zip(&ours) {
                assert!(a.abs_diff(*b) <= ms(1), "{format}: {a:?} vs {b:?}");
            }
        }
    }
    Ok(())
}

/// A capture larger than any H.264 encoder takes (beyond level 5.2 for
/// OpenH264) records scaled down to the largest size that fits, with its
/// aspect ratio, and says so.
#[test]
fn oversized_captures_are_scaled_to_fit() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("ultrawide.mp4");
    let screen = Screen::new(pattern(8000, 2000, 5));
    let session = Session::start(
        config(&path, OutputFormat::Mp4, EncoderPreference::Software),
        screen.source(),
    )?;
    let (width, height) = session.size();
    eprintln!("8000x2000 records as {width}x{height}");
    assert!(width < 8000 && width % 2 == 0 && height % 2 == 0);
    assert!((width as f64 / height as f64 - 4.0).abs() < 0.01);
    // Level 5.2 allows 36864 macroblocks a frame: as close to that as the
    // aspect ratio allows.
    let macroblocks = width.div_ceil(16) * height.div_ceil(16);
    assert!((35_000..=36_864).contains(&macroblocks), "{macroblocks}");
    assert!(
        session
            .warnings()
            .iter()
            .any(|w| w.contains("8000x2000") && w.contains(&format!("{width}x{height}"))),
        "{:?}",
        session.warnings()
    );
    let recorded = record_for(session, ms(300))?;
    assert_eq!((recorded.width, recorded.height), (width, height));
    let info = decode::probe(&path)?;
    assert_eq!((info.width, info.height), (width, height));
    let frames = decoded(&path)?;
    assert_eq!(
        (frames[0].image.width, frames[0].image.height),
        (width, height)
    );
    fixtures::ffprobe(&path, &["h264"]);
    Ok(())
}

/// Animations record into a lossless master and are encoded from it at
/// stop with each frame's own timing; pauses are left out.
#[test]
fn animation_recordings_keep_their_timing() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    for format in OutputFormat::ANIMATION {
        let path = dir.path().join(format!("capture.{format}"));
        let session = Session::start(
            config(&path, format, EncoderPreference::Software),
            Box::new(Synthetic::new(20)),
        )?;
        thread::sleep(ms(300));
        session.pause();
        thread::sleep(ms(200));
        session.resume();
        let recorded = record_for(session, ms(300))?;
        assert_eq!(recorded.path, path);
        // 600 ms slept while recording; a loaded machine adds some.
        assert!(
            (ms(600)..ms(1500)).contains(&recorded.duration),
            "{:?}",
            recorded.duration
        );
        let master = decoded(&recorded.master)?;
        // The capture rate, which the pause does not lower (exports set
        // their encoder up from it).
        let fps = decode::probe(&recorded.master)?.fps;
        assert!((fps - 20.0).abs() < 1.5, "{format}: master at {fps} fps");
        let output = decoded(&path)?;
        let info = decode::probe(&path)?;
        assert_eq!(info.kind, Kind::Animation);
        assert_near(info.duration, recorded.duration, ms(25));
        assert_eq!(output.len(), master.len(), "{format}");
        for (a, b) in output.iter().zip(&master) {
            assert_eq!(index_of(&a.image), index_of(&b.image), "{format}");
            assert_near(a.pts, b.pts, ms(10));
        }
        // Frames continue across the 200 ms pause without a hole (they are
        // 50 ms apart; allow for a slow encoder).
        for pair in master.windows(2) {
            assert!(pair[1].pts - pair[0].pts < ms(150), "{format}");
        }
    }
    Ok(())
}

fn av_clip() -> Clip {
    Clip {
        width: 320,
        height: 240,
        fps: 30,
        frames: 90,
        tone: Some(Tone {
            frequency: 440.0,
            amplitude: 0.2,
            start: Duration::from_secs(1),
            length: ms(1500),
        }),
    }
}

/// Audio that starts after the picture keeps its offset when read, and
/// through exports with and without trimming.
#[test]
fn exports_keep_the_audio_offset() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    for format in [OutputFormat::Mkv, OutputFormat::Mp4] {
        let src = dir.path().join(format!("late-audio.{format}"));
        fixtures::video(&src, format, &av_clip())?;
        assert_near(first_audio_time(&src)?, ms(1000), ms(1));
        let mut options = options(OutputFormat::Mp4);
        options.mute = false;
        for (trim, expected) in [
            (None, ms(1000)),
            (Some((500, 2500)), ms(500)),
            (Some((1500, 2500)), Duration::ZERO),
        ] {
            options.trim = trim.map(|(start, end)| Trim {
                start: ms(start),
                end: ms(end),
            });
            let out = dir.path().join(format!("exported-{format}-{trim:?}.mp4"));
            export_to(&src, &out, &options)?;
            fixtures::ffprobe(&out, &["h264", "opus"]);
            assert_near(first_audio_time(&out)?, expected, ms(25));
        }
    }
    Ok(())
}

/// Trimming keeps exactly the frames shown in the range, timed from its
/// start, and the file lasts exactly as long as the range.
#[test]
fn trims_are_frame_exact() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("source.mp4");
    let mut clip = av_clip();
    clip.tone = None;
    fixtures::video(&src, OutputFormat::Mp4, &clip)?;

    let mut options = options(OutputFormat::Mkv);
    options.trim = Some(Trim {
        start: ms(1000),
        end: ms(2000),
    });
    let out = dir.path().join("trimmed.mkv");
    export_to(&src, &out, &options)?;
    let frames = decoded(&out)?;
    let indices: Vec<_> = frames.iter().map(|f| index_of(&f.image)).collect();
    assert_eq!(indices, (30..60).collect::<Vec<_>>());
    assert_eq!(frames[0].pts, Duration::ZERO);
    let last = frames.last().unwrap();
    assert_near(last.pts + last.duration, ms(1000), ms(1));
    assert_near(decode::probe(&out)?.duration, ms(1000), ms(1));

    // A start between frames opens with the frame showing there.
    options.trim = Some(Trim {
        start: ms(1010),
        end: ms(1100),
    });
    export_to(&src, &out, &options)?;
    let frames = decoded(&out)?;
    let indices: Vec<_> = frames.iter().map(|f| index_of(&f.image)).collect();
    assert_eq!(indices, [30, 31, 32]);

    // On a frame-rate grid: the frame showing at each tick.
    options.format = OutputFormat::Gif;
    options.fps = Some(10);
    options.trim = Some(Trim {
        start: ms(1000),
        end: ms(2000),
    });
    let gif = dir.path().join("trimmed.gif");
    export_to(&src, &gif, &options)?;
    let frames = decoded(&gif)?;
    let indices: Vec<_> = frames.iter().map(|f| index_of(&f.image)).collect();
    assert_eq!(indices, (30..60).step_by(3).collect::<Vec<_>>());
    assert!(frames.iter().all(|f| f.duration == ms(100)));
    Ok(())
}

/// A variable-delay GIF keeps every frame and delay through an export with
/// annotations, into every animation format.
#[test]
fn variable_delay_animations_survive_export_with_overlay() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    let delays = [1000, 120, 80, 40, 300];
    let frames: Vec<_> = delays
        .iter()
        .enumerate()
        .map(|(i, &delay)| (pattern(160, 96, i as u32), ms(delay)))
        .collect();
    let src = dir.path().join("variable.gif");
    fixtures::animation(&src, OutputFormat::Gif, &frames)?;
    // Painted at the source size; the bottom-right quarter turns green.
    let overlay = |image: &mut Image| {
        assert_eq!((image.width, image.height), (160, 96));
        fixtures::fill(image, 80, 48, 80, 48, [0, 255, 0, 255]);
    };
    for format in OutputFormat::ANIMATION {
        let out = dir.path().join(format!("annotated.{format}"));
        let mut options = options(format);
        options.max_width = Some(80);
        export::export(
            &src,
            OutputFile::replace(&out)?,
            &options,
            Some(&overlay),
            &|_| {},
            &AtomicBool::new(false),
        )?;
        let decoded = decoded(&out)?;
        assert_eq!(decoded.len(), delays.len(), "{format}");
        for (i, (frame, delay)) in decoded.iter().zip(delays).enumerate() {
            assert_eq!((frame.image.width, frame.image.height), (80, 48));
            assert_near(frame.duration, ms(delay), ms(10));
            let [b, g, r, _] = frame.image.pixel(60, 36);
            assert!(
                g > 200 && b < 60 && r < 60,
                "{format} frame {i}: overlay missing"
            );
            // The index blocks halve to 16 px; read the halved pattern.
            let scaled = convert_back(&frame.image);
            assert_eq!(index_of(&scaled), i as u32, "{format} frame {i}");
        }
    }
    Ok(())
}

/// Doubles a half-size frame so [`index_of`] reads its blocks.
fn convert_back(image: &Image) -> Image {
    media::convert::resize(image, image.width * 2, image.height * 2)
}

/// Video exports convert between containers, mute, and turn animations
/// into video and back.
#[test]
fn exports_between_videos_and_animations() -> Result<()> {
    init_logs();
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("source.mov");
    fixtures::video(&src, OutputFormat::Mov, &av_clip())?;
    let muted = dir.path().join("muted.mp4");
    export_to(&src, &muted, &options(OutputFormat::Mp4))?;
    let info = decode::probe(&muted)?;
    assert!(!info.has_audio);
    assert_near(info.duration, ms(3000), ms(1));
    fixtures::ffprobe(&muted, &["h264"]);

    let mut fragmented = options(OutputFormat::Mp4);
    fragmented.fragmented = false;
    let classic = dir.path().join("classic.mp4");
    export_to(&src, &classic, &fragmented)?;
    assert_eq!(decoded(&classic)?.len(), 90);

    let gif = dir.path().join("video.gif");
    let mut to_gif = options(OutputFormat::Gif);
    to_gif.fps = Some(15);
    export_to(&src, &gif, &to_gif)?;
    let frames = decoded(&gif)?;
    assert_eq!(frames.len(), 45);
    assert_eq!(index_of(&frames[1].image), 2);

    let back = dir.path().join("animation.mkv");
    export_to(&gif, &back, &options(OutputFormat::Mkv))?;
    let info = decode::probe(&back)?;
    assert_eq!(info.kind, Kind::Video);
    assert_near(info.duration, ms(3000), ms(15));
    assert_eq!(decoded(&back)?.len(), 45);
    Ok(())
}

/// Cancelling an export stops it with an error and leaves no file.
#[test]
fn cancelled_exports_leave_nothing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("source.mp4");
    fixtures::video(&src, OutputFormat::Mp4, &av_clip())?;
    for format in [OutputFormat::Mp4, OutputFormat::Gif] {
        let out = dir.path().join(format!("cancelled.{format}"));
        let cancel = AtomicBool::new(false);
        let result = export::export(
            &src,
            OutputFile::reserve(&out)?,
            &options(format),
            None,
            &|progress| {
                if progress > 0.2 {
                    cancel.store(true, Ordering::Release);
                }
            },
            &cancel,
        );
        assert!(result.is_err(), "{format}");
        let left: Vec<_> = std::fs::read_dir(dir.path())?
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["source.mp4"], "{format}");
    }
    Ok(())
}

/// Records a second of the desktop's real audio, so it runs only when asked
/// for: `QUIKSCOPE_TEST_REAL_AUDIO=1 cargo test --test media pulse`.
#[cfg(target_os = "linux")]
#[test]
fn pulse_devices_and_one_second_capture() -> Result<()> {
    if std::env::var_os("QUIKSCOPE_TEST_REAL_AUDIO").is_none_or(|v| v != "1") {
        eprintln!("Skipping real audio capture; set QUIKSCOPE_TEST_REAL_AUDIO=1 to run it");
        return Ok(());
    }
    let devices = media::audio::devices()?;
    eprintln!("Real audio devices: {devices:#?}");
    assert!(
        devices
            .iter()
            .any(|d| d.output && d.id != media::DEFAULT_OUTPUT)
    );
    let mut source = media::audio::open(media::DEFAULT_OUTPUT)?;
    let end = Instant::now() + Duration::from_secs(1);
    let mut chunks = 0;
    while Instant::now() < end {
        if let Some(chunk) = source.read(Duration::from_millis(40))? {
            assert!(!chunk.samples.is_empty());
            chunks += 1;
        }
    }
    assert!(chunks > 0);
    Ok(())
}
