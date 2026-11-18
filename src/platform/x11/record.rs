//! MIT-SHM region recording on a connection independent of still capture.
use super::*;
use crate::media::{Frame, VideoSource};
use crate::platform::{RecordTarget, ScreenRecorder};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use x11rb::protocol::xproto::{CreateWindowAux, WindowClass};

pub struct Recorder;

struct Source {
    frames: mpsc::Receiver<Result<Frame>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    size: (u32, u32),
}
impl Drop for Source {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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
        match self.frames.recv_timeout(timeout) {
            Ok(frame) => frame.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(_) => bail!("X11 recording thread stopped"),
        }
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
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::Builder::new()
            .name("quikscope-x11-record".into())
            .spawn(move || {
                let mut worker = match Worker::connect(true) {
                    Ok(w) => w,
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                let setup = (|| -> Result<()> {
                    ensure!(worker.shm_enabled, "X11 recording requires MIT-SHM");
                    let size = worker.format.stride(width.into()) * usize::from(height);
                    worker.buffer = Some(ShmBuffer::new(&worker.conn, size)?);
                    Ok(())
                })();
                let failed = setup.is_err();
                let _ = ready.send(setup);
                if failed {
                    return;
                }
                let period = Duration::from_secs_f64(1.0 / f64::from(fps));
                let mut next = Instant::now();
                while !stopping.load(Ordering::Relaxed) {
                    let result = (|| -> Result<Frame> {
                        let root = worker.conn.setup().roots[worker.screen].root;
                        let buffer = worker.buffer.as_ref().context("missing MIT-SHM buffer")?;
                        worker
                            .conn
                            .shm_get_image(
                                root,
                                i16::try_from(rect.x)?,
                                i16::try_from(rect.y)?,
                                width,
                                height,
                                u32::MAX,
                                ImageFormat::Z_PIXMAP.into(),
                                buffer.id,
                                0,
                            )?
                            .reply()?;
                        let captured = Instant::now();
                        if !cursor
                            && worker.format.bits == 32
                            && worker.format.little_endian
                            && (worker.format.red, worker.format.green, worker.format.blue)
                                == (0xff0000, 0xff00, 0xff)
                        {
                            return Ok(Frame::from_bgra(
                                width.into(),
                                height.into(),
                                worker.format.stride(width.into()),
                                buffer.bytes(),
                                true,
                                captured,
                            ));
                        }
                        let mut image =
                            worker
                                .format
                                .decode(buffer.bytes(), width.into(), height.into())?;
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
                        Ok(Frame::from_image(&image, captured))
                    })();
                    let failed = result.is_err();
                    if let Err(mpsc::TrySendError::Disconnected(_)) = tx.try_send(result) {
                        break;
                    }
                    if failed {
                        break;
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
            stop,
            thread: Some(thread),
            size: (width.into(), height.into()),
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
