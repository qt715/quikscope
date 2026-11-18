//! Continuous ScreenCaptureKit capture. Cocoa outlines stay on the main queue.
use crate::{
    geom::IRect,
    media::{AudioChunk, AudioSource, AudioSpec, Frame, VideoSource},
    platform::{RecordTarget, ScreenRecorder},
};
use anyhow::{Context, Result, anyhow, ensure};
use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::{
    AnyThread, DefinedClass, define_class, msg_send, rc::Retained, runtime::ProtocolObject,
};
use objc2_core_media::{CMSampleBuffer, CMTimeMake};
use objc2_core_video::*;
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSProcessInfo, NSString,
};
use objc2_screen_capture_kit::*;
use std::{
    cell::RefCell,
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

pub struct Recorder;
pub(crate) fn system_audio_supported() -> bool {
    NSProcessInfo::processInfo()
        .operatingSystemVersion()
        .majorVersion
        >= 13
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

impl ScreenRecorder for Recorder {
    fn open(&self, target: &RecordTarget, fps: u32, cursor: bool) -> Result<Box<dyn VideoSource>> {
        Ok(Box::new(Source::open(Some(target.clone()), fps, cursor)?))
    }
    fn outline(&self, region: IRect) -> Result<Box<dyn Send>> {
        outline(region)
    }
}

struct Lifetime(mpsc::Sender<()>);
impl Drop for Lifetime {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}
struct Source {
    size: (u32, u32),
    video: mpsc::Receiver<Result<Frame>>,
    audio: Option<Audio>,
    _life: Arc<Lifetime>,
}
struct Audio {
    rx: mpsc::Receiver<Result<AudioChunk>>,
    _life: Arc<Lifetime>,
}
impl AudioSource for Audio {
    fn spec(&self) -> AudioSpec {
        AudioSpec {
            rate: 48000,
            channels: 2,
        }
    }
    fn read(&mut self, timeout: Duration) -> Result<Option<AudioChunk>> {
        receive(&self.rx, timeout)
    }
}
fn receive<T>(rx: &mpsc::Receiver<Result<T>>, timeout: Duration) -> Result<Option<T>> {
    match rx.recv_timeout(timeout) {
        Ok(result) => result.map(Some),
        Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
        Err(_) => Err(anyhow!("ScreenCaptureKit stream ended")),
    }
}
impl VideoSource for Source {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        receive(&self.video, timeout)
    }
    fn take_audio(&mut self) -> Option<Box<dyn AudioSource>> {
        self.audio.take().map(|a| Box::new(a) as _)
    }
}
pub(crate) fn open_system_audio() -> Result<Box<dyn AudioSource>> {
    ensure!(
        system_audio_supported(),
        "Desktop audio requires macOS 13 or later"
    );
    let mut source = Source::open(None, 1, false)?;
    source
        .take_audio()
        .context("ScreenCaptureKit audio output unavailable")
}
impl Source {
    fn open(target: Option<RecordTarget>, fps: u32, cursor: bool) -> Result<Self> {
        ensure!(
            fps > 0 && fps <= i32::MAX as u32,
            "invalid recording frame rate"
        );
        ensure!(
            target
                .as_ref()
                .is_none_or(|t| !t.region.is_empty()
                    && t.region.intersect(&t.output_rect) == Some(t.region)),
            "recording region is outside the display"
        );
        if !unsafe { CGPreflightScreenCaptureAccess() } {
            unsafe { CGRequestScreenCaptureAccess() };
            anyhow::bail!(
                "Screen Recording permission is required. Enable quikscope in System Settings → Privacy & Security → Screen Recording, then retry (macOS may require restarting the app)."
            );
        }
        let (vtx, vrx) = mpsc::sync_channel(3);
        let (atx, arx) = mpsc::sync_channel(64);
        let (stop, stopped) = mpsc::channel();
        let life = Arc::new(Lifetime(stop));
        let (ready, started) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("quikscope-sck-record".into())
            .spawn(move || {
                objc2::rc::autoreleasepool(|_| {
                    let result = run(target, fps, cursor, vtx, atx, stopped, &ready);
                    if let Err(error) = result {
                        let _ = ready.send(Err(error));
                    }
                });
            })?;
        let size = started
            .recv_timeout(Duration::from_secs(20))
            .context("starting ScreenCaptureKit recording")??;
        Ok(Self {
            size,
            video: vrx,
            audio: system_audio_supported().then(|| Audio {
                rx: arx,
                _life: life.clone(),
            }),
            _life: life,
        })
    }
}
fn completion(stream: &SCStream, start: bool) -> Result<()> {
    let (tx, rx) = mpsc::sync_channel(1);
    let block = RcBlock::new(move |error: *mut NSError| {
        let error = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string());
        let _ = tx.send(error);
    });
    unsafe {
        if start {
            stream.startCaptureWithCompletionHandler(Some(&block));
        } else {
            stream.stopCaptureWithCompletionHandler(Some(&block));
        }
    }
    if let Some(error) = rx
        .recv_timeout(Duration::from_secs(10))
        .context("ScreenCaptureKit stream completion timed out")?
    {
        anyhow::bail!("ScreenCaptureKit: {error}");
    }
    Ok(())
}
fn run(
    target: Option<RecordTarget>,
    fps: u32,
    cursor: bool,
    video: mpsc::SyncSender<Result<Frame>>,
    audio: mpsc::SyncSender<Result<AudioChunk>>,
    stop: mpsc::Receiver<()>,
    ready: &mpsc::SyncSender<Result<(u32, u32)>>,
) -> Result<()> {
    let (tx, rx) = mpsc::sync_channel(1);
    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = unsafe {
                if let Some(e) = error.as_ref() {
                    Err(anyhow!("ScreenCaptureKit: {}", e.localizedDescription()))
                } else {
                    content
                        .as_ref()
                        .map(|c| c.displays())
                        .context("no ScreenCaptureKit displays")
                }
            };
            let _ = tx.send(result);
        },
    );
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(false, true, &block);
    }
    let displays = rx.recv_timeout(Duration::from_secs(10))??;
    let display = displays
        .iter()
        .find(|d| {
            target
                .as_ref()
                .is_none_or(|t| unsafe { d.displayID() }.to_string() == t.output)
        })
        .context("recording display disconnected")?;
    let id = unsafe { display.displayID() };
    let logical = super::desktop::display_rect(id);
    let mode = core_graphics::display::CGDisplay::new(id)
        .display_mode()
        .context("recording display disconnected")?;
    let region = target
        .as_ref()
        .map_or(IRect::new(logical.x, logical.y, 2, 2), |t| t.region);
    let size = if target.is_some() {
        super::desktop::physical_size(
            IRect::new(0, 0, region.w, region.h),
            mode.width(),
            mode.pixel_width(),
        )
    } else {
        (2, 2)
    };
    let output = Receiver::new(video, audio);
    let queue = DispatchQueue::new("io.github.qt715.quikscope.record", None);
    unsafe {
        let filter = SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &NSArray::<SCWindow>::new(),
        );
        let config = SCStreamConfiguration::new();
        config.setWidth(size.0 as usize);
        config.setHeight(size.1 as usize);
        config.setSourceRect(objc2_foundation::NSRect::new(
            objc2_foundation::NSPoint::new(
                (region.x - logical.x) as f64,
                (region.y - logical.y) as f64,
            ),
            objc2_foundation::NSSize::new(region.w as f64, region.h as f64),
        ));
        config.setMinimumFrameInterval(CMTimeMake(1, fps as i32));
        config.setPixelFormat(u32::from_be_bytes(*b"BGRA"));
        config.setShowsCursor(cursor);
        config.setQueueDepth(3);
        if system_audio_supported() {
            config.setCapturesAudio(true);
            config.setExcludesCurrentProcessAudio(true);
            config.setSampleRate(48000);
            config.setChannelCount(2);
        }
        let stream = SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(ProtocolObject::from_ref(&*output)),
        );
        let protocol = ProtocolObject::from_ref(&*output);
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                protocol,
                SCStreamOutputType::Screen,
                Some(&queue),
            )
            .map_err(|e| anyhow!("SCK video output: {}", e.localizedDescription()))?;
        if system_audio_supported() {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    protocol,
                    SCStreamOutputType::Audio,
                    Some(&queue),
                )
                .map_err(|e| anyhow!("SCK audio output: {}", e.localizedDescription()))?;
        }
        if let Err(error) = completion(&stream, true) {
            let _ = completion(&stream, false);
            return Err(error);
        }
        let _ = ready.send(Ok(size));
        let _ = stop.recv();
        if let Err(error) = completion(&stream, false) {
            log::warn!("stopping ScreenCaptureKit recording: {error:#}");
        }
        let _ = stream.removeStreamOutput_type_error(protocol, SCStreamOutputType::Screen);
        if system_audio_supported() {
            let _ = stream.removeStreamOutput_type_error(protocol, SCStreamOutputType::Audio);
        }
    }
    Ok(())
}
struct ReceiverState {
    video: Mutex<mpsc::SyncSender<Result<Frame>>>,
    audio: Mutex<mpsc::SyncSender<Result<AudioChunk>>>,
}
define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = ReceiverState]
    struct Receiver;
    unsafe impl NSObjectProtocol for Receiver {}
    unsafe impl SCStreamDelegate for Receiver {
        #[unsafe(method(stream:didStopWithError:))]
        fn stopped(&self, _stream: &SCStream, error: &NSError) {
            let message = error.localizedDescription().to_string();
            if let Ok(sender) = self.ivars().video.lock() {
                let _ = sender.try_send(Err(anyhow!("ScreenCaptureKit stopped: {message}")));
            }
            if let Ok(sender) = self.ivars().audio.lock() {
                let _ = sender.try_send(Err(anyhow!("ScreenCaptureKit stopped: {message}")));
            }
        }
    }
    unsafe impl SCStreamOutput for Receiver {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn receive(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if kind == SCStreamOutputType::Audio {
                    let chunk = crate::media::audio::macos::sample_chunk(sample);
                    let _ = self.ivars().audio.lock().unwrap().try_send(chunk);
                } else if kind == SCStreamOutputType::Screen {
                    if let Some(frame) = video_frame(sample) {
                        let _ = self.ivars().video.lock().unwrap().try_send(frame);
                    }
                }
            }));
            if result.is_err() {
                log::error!("ScreenCaptureKit sample callback panicked");
            }
        }
    }
);
impl Receiver {
    fn new(
        video: mpsc::SyncSender<Result<Frame>>,
        audio: mpsc::SyncSender<Result<AudioChunk>>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(ReceiverState {
            video: Mutex::new(video),
            audio: Mutex::new(audio),
        });
        unsafe { msg_send![super(this), init] }
    }
}
fn video_frame(sample: &CMSampleBuffer) -> Option<Result<Frame>> {
    let attachments = unsafe { sample.sample_attachments_array(false) }?;
    let array = unsafe {
        &*(std::ptr::from_ref(&*attachments).cast::<NSArray<NSDictionary<NSString, NSObject>>>())
    };
    let status = array
        .firstObject()?
        .objectForKey(unsafe { SCStreamFrameInfoStatus })?;
    if status.downcast_ref::<NSNumber>()?.integerValue() != SCFrameStatus::Complete.0 {
        return None;
    }
    let buffer = unsafe { sample.image_buffer() }?;
    Some(unsafe {
        let flags = CVPixelBufferLockFlags::ReadOnly;
        if CVPixelBufferLockBaseAddress(&buffer, flags) != 0 {
            return Some(Err(anyhow!("locking recording pixel buffer")));
        }
        let width = CVPixelBufferGetWidth(&buffer);
        let height = CVPixelBufferGetHeight(&buffer);
        let stride = CVPixelBufferGetBytesPerRow(&buffer);
        let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
        // SCK owns a finite IOSurface pool. Copying releases it immediately rather
        // than letting a slow encoder pin surfaces and stall WindowServer capture.
        let result = if base.is_null() || width == 0 || height == 0 || stride < width * 4 {
            Err(anyhow!("invalid recording pixel buffer"))
        } else {
            Ok(Frame::from_bgra(
                width as u32,
                height as u32,
                stride,
                std::slice::from_raw_parts(base, stride * height),
                true,
                Instant::now(),
            ))
        };
        CVPixelBufferUnlockBaseAddress(&buffer, flags);
        result
    })
}

thread_local! { static OUTLINES: RefCell<HashMap<u64, Vec<Retained<objc2_app_kit::NSPanel>>>> = RefCell::new(HashMap::new()); }
static NEXT_OUTLINE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
struct Outline(u64);
impl Drop for Outline {
    fn drop(&mut self) {
        let id = self.0;
        super::shell::on_main(move || {
            OUTLINES.with(|panels| {
                if let Some(panels) = panels.borrow_mut().remove(&id) {
                    for panel in panels {
                        panel.orderOut(None);
                    }
                }
            });
        });
    }
}
fn outline(region: IRect) -> Result<Box<dyn Send>> {
    ensure!(!region.is_empty(), "empty recording outline");
    let id = NEXT_OUTLINE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    super::shell::on_main(move || {
        use objc2_app_kit::{
            NSBackingStoreType, NSColor, NSPanel, NSScreen, NSWindowCollectionBehavior,
            NSWindowSharingType, NSWindowStyleMask,
        };
        use objc2_foundation::{NSPoint, NSRect, NSSize};
        let mtm = objc2::MainThreadMarker::new().unwrap();
        let screens = NSScreen::screens(mtm);
        let top = screens
            .firstObject()
            .map(|s| s.frame().size.height)
            .unwrap_or(0.0);
        let mut panels = Vec::new();
        for r in [
            IRect::new(region.x - 2, region.y - 2, region.w + 4, 2),
            IRect::new(region.x - 2, region.bottom(), region.w + 4, 2),
            IRect::new(region.x - 2, region.y, 2, region.h),
            IRect::new(region.right(), region.y, 2, region.h),
        ] {
            let rect = NSRect::new(
                NSPoint::new(r.x as f64, top - r.bottom() as f64),
                NSSize::new(r.w as f64, r.h as f64),
            );
            let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
                NSPanel::alloc(mtm),
                rect,
                NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
                NSBackingStoreType::Buffered,
                false,
            );
            unsafe {
                panel.setReleasedWhenClosed(false);
            }
            panel.setSharingType(NSWindowSharingType::None);
            panel.setIgnoresMouseEvents(true);
            panel.setLevel(101);
            panel.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary,
            );
            panel.setBackgroundColor(Some(&NSColor::systemRedColor()));
            panel.orderFrontRegardless();
            panels.push(panel);
        }
        OUTLINES.with(|p| {
            p.borrow_mut().insert(id, panels);
        });
    });
    Ok(Box::new(Outline(id)))
}
