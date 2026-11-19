//! MIT-SHM region recording on a connection independent of still capture.
use super::*;
use crate::media::{self, Frame, Pixels, VideoSource};
use crate::platform::{RecordTarget, ScreenRecorder};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use x11rb::protocol::xproto::{CreateWindowAux, WindowClass};

pub struct Recorder;

/// Shared-memory segments per recording. Frames lend theirs to the encoder
/// until dropped; with every segment lent, capture skips a tick.
const SEGMENTS: usize = 4;

/// State the capture thread shares with its [`Source`].
#[derive(Default)]
struct Control {
    stop: AtomicBool,
    paused: AtomicBool,
    /// Number of resumes; frames carry the value they were captured under.
    epoch: AtomicU64,
}

/// An MIT-SHM segment the X server copies the region into.
struct Segment(ShmBuffer);

// SAFETY: a segment is plain shared memory. The server writes it only
// during a GetImage on the capture thread, and a segment lent to a frame is
// not captured into again until the frame drops it.
unsafe impl Send for Segment {}

/// A captured segment lent to a frame; dropping the frame returns it.
struct Lease {
    segment: Option<Segment>,
    len: usize,
    pool: mpsc::Sender<Segment>,
}

impl AsRef<[u8]> for Lease {
    fn as_ref(&self) -> &[u8] {
        &self.segment.as_ref().expect("taken only in drop").0.bytes()[..self.len]
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(segment) = self.segment.take() {
            // After the recording ended the segment is simply detached.
            let _ = self.pool.send(segment);
        }
    }
}

struct Source {
    frames: mpsc::Receiver<Result<(u64, Frame)>>,
    control: Arc<Control>,
    thread: Option<std::thread::JoinHandle<()>>,
    size: (u32, u32),
    epoch: u64,
}
impl Drop for Source {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl VideoSource for Source {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let deadline = Instant::now() + timeout;
        loop {
            match self
                .frames
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                // Captured before the last resume.
                Ok(Ok((epoch, _))) if epoch != self.epoch => {}
                Ok(frame) => return frame.map(|(_, frame)| Some(frame)),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                Err(_) => bail!("X11 recording thread stopped"),
            }
        }
    }
    fn set_paused(&mut self, paused: bool) {
        if !paused {
            self.epoch = self.control.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        }
        self.control.paused.store(paused, Ordering::Relaxed);
    }
}
impl ScreenRecorder for Recorder {
    fn open(&self, target: &RecordTarget, fps: u32, cursor: bool) -> Result<Box<dyn VideoSource>> {
        ensure!(fps > 0 && fps <= 240, "recording fps must be in 1..=240");
        let rect = target.region;
        let width = u16::try_from(rect.w)?;
        let height = u16::try_from(rect.h)?;
        ensure!(width > 0 && height > 0, "empty recording region");
        let (tx, frames) = mpsc::sync_channel(4);
        let (ready, started) = mpsc::sync_channel(1);
        let control = Arc::new(Control::default());
        let shared = control.clone();
        let thread = std::thread::Builder::new()
            .name("quikscope-x11-record".into())
            .spawn(move || {
                let setup = (|| -> Result<(Worker, Vec<Segment>)> {
                    let worker = Worker::connect(true)?;
                    ensure!(worker.shm_enabled, "X11 recording requires MIT-SHM");
                    let size = worker.format.stride(width.into()) * usize::from(height);
                    let segments = (0..SEGMENTS)
                        .map(|_| ShmBuffer::new(&worker.conn, size).map(Segment))
                        .collect::<Result<_>>()?;
                    Ok((worker, segments))
                })();
                let (worker, mut free) = match setup {
                    Ok(setup) => {
                        let _ = ready.send(Ok(()));
                        setup
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                // XRGB in little-endian 32-bit words is B, G, R, X in memory:
                // those frames borrow the segment as is.
                let native = worker.format.bits == 32
                    && worker.format.little_endian
                    && (worker.format.red, worker.format.green, worker.format.blue)
                        == (0xff0000, 0xff00, 0xff);
                let stride = worker.format.stride(width.into());
                let (pool, returned) = mpsc::channel::<Segment>();
                let period = Duration::from_secs_f64(1.0 / f64::from(fps));
                let mut next = Instant::now();
                while !shared.stop.load(Ordering::Relaxed) {
                    if shared.paused.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(20));
                        next = Instant::now();
                        continue;
                    }
                    let epoch = shared.epoch.load(Ordering::Relaxed);
                    free.extend(returned.try_iter());
                    // Every segment is still with the encoder: skip this tick.
                    let result = free.pop().map(|segment| -> Result<Frame> {
                        let root = worker.conn.setup().roots[worker.screen].root;
                        let grabbed = worker
                            .conn
                            .shm_get_image(
                                root,
                                i16::try_from(rect.x)?,
                                i16::try_from(rect.y)?,
                                width,
                                height,
                                u32::MAX,
                                ImageFormat::Z_PIXMAP.into(),
                                segment.0.id,
                                0,
                            )
                            .map_err(anyhow::Error::from)
                            .and_then(|c| c.reply().map_err(Into::into));
                        let captured = Instant::now();
                        if let Err(e) = grabbed {
                            free.push(segment);
                            return Err(e);
                        }
                        if native && !cursor {
                            return Ok(Frame::from_pixels(
                                width.into(),
                                height.into(),
                                stride,
                                media::PixelFormat::Bgrx,
                                Pixels::Shared(Box::new(Lease {
                                    segment: Some(segment),
                                    len: stride * usize::from(height),
                                    pool: pool.clone(),
                                })),
                                captured,
                            ));
                        }
                        let decoded =
                            worker
                                .format
                                .decode(segment.0.bytes(), width.into(), height.into());
                        free.push(segment);
                        let mut image = decoded?;
                        if cursor {
                            let c = worker.conn.xfixes_get_cursor_image()?.reply()?;
                            composite_cursor(
                                &mut image,
                                rect,
                                i32::from(c.x) - i32::from(c.xhot),
                                i32::from(c.y) - i32::from(c.yhot),
                                c.width.into(),
                                &c.cursor_image,
                            );
                        }
                        let stride = image.stride();
                        Ok(Frame::from_pixels(
                            image.width,
                            image.height,
                            stride,
                            media::PixelFormat::Bgrx,
                            Pixels::Owned(image.data),
                            captured,
                        ))
                    });
                    if let Some(result) = result {
                        let failed = result.is_err();
                        if let Err(mpsc::TrySendError::Disconnected(_)) =
                            tx.try_send(result.map(|frame| (epoch, frame)))
                        {
                            break;
                        }
                        if failed {
                            break;
                        }
                    }
                    next = (next + period).max(Instant::now());
                    std::thread::sleep(next.saturating_duration_since(Instant::now()));
                }
            })?;
        started
            .recv()
            .context("X11 recorder stopped during startup")??;
        Ok(Box::new(Source {
            frames,
            control,
            thread: Some(thread),
            size: (width.into(), height.into()),
            epoch: 0,
        }))
    }
    fn outline(&self, region: IRect) -> Result<Box<dyn Send>> {
        let (conn, screen) = x11rb::connect(None)?;
        conn.xfixes_query_version(2, 0)?.reply()?;
        // Whole-output outlines would sit on neighbouring monitors and cannot surround an edge.
        if monitors(&conn, screen)?.iter().any(|m| m.rect == region) {
            return Ok(Box::new(()));
        }
        let root = conn.setup().roots[screen].root;
        let mut windows = Vec::new();
        for r in [
            IRect::new(region.x - 2, region.y - 2, region.w + 4, 2),
            IRect::new(region.x - 2, region.y + region.h, region.w + 4, 2),
            IRect::new(region.x - 2, region.y, 2, region.h),
            IRect::new(region.x + region.w, region.y, 2, region.h),
        ] {
            let window = conn.generate_id()?;
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                i16::try_from(r.x)?,
                i16::try_from(r.y)?,
                u16::try_from(r.w)?,
                u16::try_from(r.h)?,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new()
                    .override_redirect(1)
                    .background_pixel(0x8f8cf2),
            )?
            .check()?;
            // Input-only shape is empty: the visible four strips must not eat clicks.
            let empty = conn.generate_id()?;
            conn.xfixes_create_region(empty, &[])?.check()?;
            conn.xfixes_set_window_shape_region(
                window,
                x11rb::protocol::shape::SK::INPUT,
                0,
                0,
                empty,
            )?
            .check()?;
            conn.xfixes_destroy_region(empty)?.check()?;
            conn.map_window(window)?;
            windows.push(window);
        }
        conn.flush()?;
        Ok(Box::new(Outline {
            _conn: conn,
            _windows: windows,
        }))
    }
}
struct Outline {
    _conn: RustConnection,
    _windows: Vec<u32>,
}
