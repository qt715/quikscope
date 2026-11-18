use std::{path::Path, sync::atomic::{AtomicBool, Ordering}, thread, time::{Duration, Instant}};
use anyhow::{Context as _, Result};
use quikscope::{media::{self, AudioChunk, AudioSource, AudioSpec, EncoderPreference, Frame, Kind, OutputFormat, VideoSource, decode::{self, AudioDecoder, Decoder}, export::{self, ExportOptions, Trim}, record::{Session, SessionConfig}}, raster::Image};

struct Synthetic { next: Instant, index: u32, fps: u32, size: (u32,u32), static_after: Option<u32> }
impl Synthetic {
    fn new(fps: u32) -> Self { Self { next: Instant::now(), index: 0, fps, size: (160,96), static_after: None } }
}
impl VideoSource for Synthetic {
    fn size(&self) -> (u32,u32) { self.size }
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        if self.static_after.is_some_and(|n| self.index >= n) { thread::sleep(timeout); return Ok(None); }
        let now = Instant::now();
        if self.next > now {
            let delay = self.next - now;
            thread::sleep(delay.min(timeout));
            if delay > timeout { return Ok(None); }
        }
        let captured = self.next;
        let image = Image::filled(self.size.0, self.size.1, [(self.index * 13 % 200) as u8, 80, 180, 255]);
        self.index += 1;
        self.next += Duration::from_secs_f64(1.0 / self.fps as f64);
        Ok(Some(Frame::from_image(&image, captured)))
    }
}
struct Tone { next: Instant, sample: usize, rate: u32 }
impl AudioSource for Tone {
    fn spec(&self) -> AudioSpec { AudioSpec { rate: self.rate, channels: 1 } }
    fn read(&mut self, timeout: Duration) -> Result<Option<AudioChunk>> {
        let now = Instant::now();
        if self.next > now {
            let delay = self.next - now;
            thread::sleep(delay.min(timeout));
            if delay > timeout { return Ok(None); }
        }
        let count = self.rate as usize / 100;
        let samples = (self.sample..self.sample+count).map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / self.rate as f32).sin() * 0.2).collect();
        self.sample += count;
        let captured = self.next;
        self.next += Duration::from_millis(10);
        Ok(Some(AudioChunk { samples, captured }))
    }
}
fn config(path: &Path, format: OutputFormat, encoder: EncoderPreference) -> SessionConfig {
    SessionConfig { path: path.into(), master: path.with_extension("master.mkv"), format, fps: 20, audio: vec![], encoder }
}
fn record(path: &Path, format: OutputFormat, preference: EncoderPreference, audio: bool) -> Result<media::record::Recorded> {
    let _ = env_logger::builder().is_test(true).try_init();
    let tone: Vec<Box<dyn AudioSource>> = if audio {
        [44100, 48000].into_iter().map(|rate| Box::new(Tone { next: Instant::now(), sample: 0, rate }) as Box<dyn AudioSource>).collect()
    } else { vec![] };
    let session = Session::start_with_audio(config(path, format, preference), Box::new(Synthetic::new(20)), tone)?;
    thread::sleep(Duration::from_millis(450));
    assert!(session.failure().is_none(), "{:?}", session.failure());
    let recorded = session.stop()?;
    assert!(!path.with_extension(format!("{}.part", format.extension())).exists());
    eprintln!("{format} {preference:?}: {}", recorded.encoder);
    Ok(recorded)
}
fn decoded(path: &Path) -> Result<Vec<decode::DecodedFrame>> {
    let mut decoder = Decoder::open(path)?;
    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame()? { frames.push(frame); }
    Ok(frames)
}
fn options(format: OutputFormat) -> ExportOptions {
    ExportOptions { format, trim: None, mute: true, fps: Some(20), max_width: None, encoder: EncoderPreference::Software }
}
#[test]
fn cpu_video_audio_containers() -> Result<()> {
    fn send_sync<T: Send + Sync>() {}
    fn send<T: Send>() {}
    send_sync::<Session>(); send::<media::record::Recorded>(); send::<media::MediaInfo>();
    let dir = tempfile::tempdir()?;
    for preference in [EncoderPreference::Auto, EncoderPreference::Software] {
        for format in OutputFormat::VIDEO {
            let path = dir.path().join(format!("{preference:?}.{}", format.extension()));
            let recorded = record(&path, format, preference, true)?;
            assert_eq!(recorded.width, 160);
            let info = decode::probe(&path)?;
            assert_eq!(info.kind, Kind::Video);
            assert!(info.has_audio);
            assert!(info.duration >= Duration::from_millis(400), "{info:?}");
            let frames = decoded(&path)?;
            assert!(frames.len() >= 8, "{} frames", frames.len());
            let mut audio = AudioDecoder::open(&path, 48000, 2)?.context("No audio")?;
            let mut sum = 0.0;
            let mut count = 0;
            while let Some((samples, _)) = audio.next_chunk()? {
                sum += samples.iter().map(|s| (*s as f64).powi(2)).sum::<f64>(); count += samples.len();
            }
            assert!(count > 10000);
            assert!((sum / count as f64).sqrt() > 0.03, "Audio was silent");
            audio.seek(Duration::from_millis(200))?;
            assert!(audio.next_chunk()?.is_some());
        }
    }
    Ok(())
}
#[test]
fn animations_seek_trim_overlay_cancel() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let gif = dir.path().join("capture.gif");
    let recorded = record(&gif, OutputFormat::Gif, EncoderPreference::Software, false)?;
    let original = decoded(&recorded.master)?;
    assert!(original.len() >= 8);
    let target = &original[original.len()/2];
    let mut decoder = Decoder::open(&recorded.master)?;
    decoder.seek(target.pts + Duration::from_millis(10))?;
    let sought = decoder.next_frame()?.context("Seek returned EOF")?;
    assert_eq!(sought.image.data[0], target.image.data[0], "Per-frame color code differed after seek");
    let next = decoder.next_frame()?.context("Seek lost lookahead")?;
    assert!(next.pts > sought.pts);
    for format in OutputFormat::ANIMATION {
        let path = dir.path().join(format!("converted.{}", format.extension()));
        export::export(&recorded.master, &path, &options(format), None, &|_| {}, &AtomicBool::new(false))?;
        let info = decode::probe(&path)?;
        let frames = decoded(&path)?;
        eprintln!("{format}: {info:?}; {} decoded frames", frames.len());
        assert_eq!(info.kind, Kind::Animation);
        assert!((frames.len() as i64 - original.len() as i64).abs() <= 1, "{format}: frame count");
        assert!((info.duration.as_secs_f64() - recorded.duration.as_secs_f64()).abs() < 0.15, "{format}: duration {info:?}");
        let expected = &frames[frames.len() / 2];
        let mut seeker = Decoder::open(&path)?;
        seeker.seek(expected.pts + Duration::from_millis(1))?;
        let selected = seeker.next_frame()?.context("Animation seek returned EOF")?;
        assert_eq!(selected.image.data, expected.image.data, "{format}: seek selected wrong frame");
    }
    let video = dir.path().join("source.mp4");
    record(&video, OutputFormat::Mp4, EncoderPreference::Software, true)?;
    let muted = dir.path().join("trimmed.mp4");
    let mut trim = options(OutputFormat::Mp4);
    trim.trim = Some(Trim { start: Duration::from_millis(100), end: Duration::from_millis(350) });
    export::export(&video, &muted, &trim, None, &|_| {}, &AtomicBool::new(false))?;
    let info = decode::probe(&muted)?;
    assert!(!info.has_audio);
    assert!((info.duration.as_secs_f64() - 0.25).abs() <= 0.051, "{info:?}");
    trim.mute = false;
    let with_audio = dir.path().join("trimmed-audio.mov");
    trim.format = OutputFormat::Mov;
    export::export(&video, &with_audio, &trim, None, &|_| {}, &AtomicBool::new(false))?;
    assert!(decode::probe(&with_audio)?.has_audio);
    let back = dir.path().join("animation-video.mp4");
    export::export(&gif, &back, &options(OutputFormat::Mp4), None, &|_| {}, &AtomicBool::new(false))?;
    assert_eq!(decode::probe(&back)?.kind, Kind::Video);
    let overlaid = dir.path().join("overlay.gif");
    let overlay = |image: &mut Image| {
        for y in 0..32 { for x in 0..32 { let i = (y * image.width as usize + x) * 4; image.data[i..i+4].copy_from_slice(&[0,255,0,255]); } }
    };
    let mut small = options(OutputFormat::Gif); small.max_width = Some(80);
    export::export(&video, &overlaid, &small, Some(&overlay), &|_| {}, &AtomicBool::new(false))?;
    let frames = decoded(&overlaid)?;
    assert!(frames.iter().all(|f| f.image.data[1] > 240 && f.image.data[2] < 15));
    assert_eq!(frames[0].image.width, 80);
    let cancelled = dir.path().join("cancelled.mp4");
    let cancel = AtomicBool::new(false);
    assert!(export::export(&video, &cancelled, &options(OutputFormat::Mp4), None, &|_| cancel.store(true, Ordering::Release), &cancel).is_err());
    assert!(!cancelled.exists()); assert!(!dir.path().join("cancelled.mp4.part").exists());
    Ok(())
}
#[test]
fn pause_static_failure_and_discard() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("static.mp4");
    let mut source = Synthetic::new(20); source.static_after = Some(1);
    let session = Session::start(config(&path, OutputFormat::Mp4, EncoderPreference::Software), Box::new(source))?;
    thread::sleep(Duration::from_millis(110)); session.pause();
    let elapsed = session.elapsed(); thread::sleep(Duration::from_millis(120));
    assert_eq!(session.elapsed(), elapsed); session.resume();
    thread::sleep(Duration::from_millis(110));
    let recorded = session.stop()?;
    let frames = decoded(&path)?;
    assert!(frames.len() >= 4);
    assert!(recorded.duration < Duration::from_millis(450));
    let cancel = dir.path().join("discard.mp4");
    let session = Session::start(config(&cancel, OutputFormat::Mp4, EncoderPreference::Software), Box::new(Synthetic::new(20)))?;
    session.cancel(); assert!(!cancel.exists());
    std::fs::write(&cancel, b"existing final file")?;
    let session = Session::start(config(&cancel, OutputFormat::Mp4, EncoderPreference::Software), Box::new(Synthetic::new(20)))?;
    session.cancel(); assert_eq!(std::fs::read(&cancel)?, b"existing final file");
    Ok(())
}
#[cfg(target_os = "linux")]
#[test]
fn pulse_devices_and_one_second_capture() -> Result<()> {
    if std::env::var_os("CI").is_some() { eprintln!("Skipping PulseAudio hardware capture on CI"); return Ok(()); }
    let devices = media::audio::devices()?;
    eprintln!("Real audio devices: {devices:#?}");
    assert!(devices.iter().any(|d| d.output && d.id != media::DEFAULT_OUTPUT));
    let mut source = media::audio::open(media::DEFAULT_OUTPUT)?;
    let end = Instant::now() + Duration::from_secs(1);
    let mut chunks = 0;
    while Instant::now() < end {
        if let Some(chunk) = source.read(Duration::from_millis(40))? { assert!(!chunk.samples.is_empty()); chunks += 1; }
    }
    assert!(chunks > 0);
    Ok(())
}

#[cfg(target_os = "linux")]
struct DrmSource {
    graph: ffmpeg_next::filter::Graph,
    next: Instant,
    size: (u32, u32),
}
#[cfg(target_os = "linux")]
impl DrmSource {
    fn new(size: (u32, u32)) -> Result<Self> {
        use ffmpeg_next::{filter, ffi};
        let _ = env_logger::builder().is_test(true).try_init();
        media::init();
        let mut graph = filter::Graph::new();
        unsafe { (*graph.as_mut_ptr()).nb_threads = 8; }
        let pixel: ffi::AVPixelFormat = ffmpeg_next::format::Pixel::BGRA.into();
        graph.add(&filter::find("buffer").unwrap(), "in",
            &format!("video_size={}x{}:pix_fmt={}:time_base=1/90000:pixel_aspect=1/1", size.0, size.1, pixel as i32))?;
        graph.add(&filter::find("buffersink").unwrap(), "out", "")?;
        unsafe {
            let mut device = std::ptr::null_mut();
            let result = ffi::av_hwdevice_ctx_create(&mut device, ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                c"/dev/dri/renderD129".as_ptr(), std::ptr::null_mut(), 0);
            anyhow::ensure!(result >= 0, "Opening AMD VAAPI: {}", ffmpeg_next::Error::from(result));
            let upload = ffi::avfilter_graph_alloc_filter(graph.as_mut_ptr(),
                filter::find("hwupload").unwrap().as_ptr(), c"upload".as_ptr());
            anyhow::ensure!(!upload.is_null(), "Allocating test upload filter");
            (*upload).hw_device_ctx = device;
            let result = ffi::avfilter_init_str(upload, std::ptr::null());
            anyhow::ensure!(result >= 0, "Initializing test upload: {}", ffmpeg_next::Error::from(result));
        }
        graph.output("in", 0)?.input("upload", 0)?.parse("[in]format=nv12[upload]")?;
        graph.output("upload", 0)?.input("out", 0)?.parse("[upload]hwmap=mode=read,format=drm_prime[out]")?;
        unsafe {
            let device = (*graph.get("upload").unwrap().as_mut_ptr()).hw_device_ctx;
            for index in 0..(*graph.as_mut_ptr()).nb_filters as usize {
                let filter = *(*graph.as_mut_ptr()).filters.add(index);
                if (*filter).hw_device_ctx.is_null() { (*filter).hw_device_ctx = ffi::av_buffer_ref(device); }
            }
        }
        graph.validate()?;
        Ok(Self { graph, next: Instant::now(), size })
    }
}
#[cfg(target_os = "linux")]
impl VideoSource for DrmSource {
    fn size(&self) -> (u32, u32) { self.size }
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let now = Instant::now();
        if self.next > now {
            let delay = self.next - now;
            thread::sleep(delay.min(timeout));
            if delay > timeout { return Ok(None); }
        }
        let captured = self.next;
        self.next += Duration::from_secs_f64(1.0 / 30.0);
        let image = Image::filled(self.size.0, self.size.1, [40, 90, 180, 255]);
        let mut input = Frame::from_image(&image, captured).into_av();
        input.set_pts(Some(0));
        self.graph.get("in").unwrap().source().add(&input)?;
        let mut drm = ffmpeg_next::frame::Video::empty();
        self.graph.get("out").unwrap().sink().frame(&mut drm)?;
        assert_eq!(drm.format(), ffmpeg_next::format::Pixel::DRM_PRIME);
        Ok(Some(Frame::from_av(drm, captured)))
    }
}
#[cfg(target_os = "linux")]
#[test]
fn real_drm_prime_without_wayland() -> Result<()> {
    if !Path::new("/dev/dri/renderD129").exists() {
        eprintln!("Skipping DRM test: AMD render node absent"); return Ok(());
    }
    let dir = tempfile::tempdir()?;
    for preference in [EncoderPreference::Auto, EncoderPreference::Software] {
        let path = dir.path().join(format!("drm-{preference:?}.mp4"));
        let source = DrmSource::new((2560, 1440))?;
        let start = Instant::now();
        let mut config = config(&path, OutputFormat::Mp4, preference); config.fps = 30;
        let session = Session::start(config, Box::new(source))?;
        let opened = start.elapsed();
        thread::sleep(Duration::from_millis(350));
        let recorded = session.stop()?;
        let frames = decoded(&path)?;
        assert!(frames.len() >= 8);
        assert_eq!(frames[0].image.width, 2560);
        eprintln!("DRM PRIME 2560x1440 {preference:?}: {}, open {opened:?}, total {:?}, {} frames", recorded.encoder, start.elapsed(), frames.len());
    }
    Ok(())
}

struct Failing { sent: bool }
impl VideoSource for Failing {
    fn size(&self) -> (u32, u32) { (160, 96) }
    fn next_frame(&mut self, _: Duration) -> Result<Option<Frame>> {
        if self.sent { anyhow::bail!("Synthetic source disappeared"); }
        self.sent = true;
        Ok(Some(Frame::from_image(&Image::filled(160, 96, [40, 90, 180, 255]), Instant::now())))
    }
}
#[test]
fn capture_failure_preserves_recorded_frames() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("failure.mp4");
    let session = Session::start(config(&path, OutputFormat::Mp4, EncoderPreference::Software), Box::new(Failing { sent: false }))?;
    let deadline = Instant::now() + Duration::from_secs(1);
    while session.failure().is_none() && Instant::now() < deadline { thread::sleep(Duration::from_millis(1)); }
    assert_eq!(session.failure().as_deref(), Some("Synthetic source disappeared"));
    session.stop()?;
    assert!(!decoded(&path)?.is_empty());
    Ok(())
}
