//! The capture worker: one thread owning every Desktop Duplication and GDI
//! resource of the process. DXGI allows one duplication per output and
//! process, so screenshots and recordings share it: a recording pulls its
//! frames through this worker (see `record.rs`), and a screenshot taken
//! meanwhile reads the same duplication's newest image. Outputs left unused
//! for [`IDLE`] are released, duplication and D3D11 device included, unless a
//! recording holds them; repeated screenshots within that time skip setup.
//! `new_with_backend` allows an external benchmark to compare DXGI (with
//! availability fallback) against forced GDI.
use super::{
    desktop::{Monitor, PhysicalDpi, monitors},
    duplication::{Duplication, Update},
};
use crate::{
    capture::{OutputFrame, Snapshot},
    geom::IRect,
    media::{Frame, PixelFormat, Pixels},
    platform::windows_pixels::{self as pixels, Rotation},
    raster::Image,
};
use anyhow::{Context, Result, anyhow, bail};
use std::{
    collections::HashMap,
    ffi::c_void,
    mem::size_of,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::HANDLE,
        Graphics::Gdi::*,
        System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
        UI::WindowsAndMessaging::{
            CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HICON,
            ICONINFO,
        },
    },
    core::{PCWSTR, w},
};

/// How long an output's capture resources outlive their last use.
const IDLE: Duration = Duration::from_secs(10);
/// Pause before retrying a duplication that could not be made (secure
/// desktop, unsupported virtual output), so failures stay cheap.
const RETRY: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureBackend {
    Dxgi,
    Gdi,
}

pub(super) enum Request {
    Snapshot {
        outputs: Option<Vec<String>>,
        cursor: bool,
        reply: mpsc::SyncSender<Result<Snapshot>>,
    },
    /// Starts a recording stream of `region` (global physical pixels).
    Open {
        output: String,
        region: IRect,
        cursor: bool,
        reply: mpsc::SyncSender<Result<u64>>,
    },
    /// The stream's next frame: waits up to `wait` for a change, or returns
    /// the newest image regardless when `fresh`.
    Frame {
        stream: u64,
        wait: Duration,
        fresh: bool,
        reply: mpsc::SyncSender<Result<Option<Frame>>>,
    },
    Close {
        stream: u64,
    },
}

/// Only messages cross threads; all COM and GDI objects stay on the worker.
#[derive(Clone)]
pub struct WindowsCapture {
    requests: mpsc::Sender<Request>,
}
impl WindowsCapture {
    pub fn new() -> Result<Self> {
        let backend = match std::env::var("QUIKSCOPE_WINDOWS_CAPTURE").as_deref() {
            Ok("gdi") => CaptureBackend::Gdi,
            Ok("dxgi") | Err(std::env::VarError::NotPresent) => CaptureBackend::Dxgi,
            _ => bail!("QUIKSCOPE_WINDOWS_CAPTURE must be dxgi or gdi"),
        };
        Self::new_with_backend(backend)
    }
    pub fn new_with_backend(backend: CaptureBackend) -> Result<Self> {
        let (requests, receiver) = mpsc::channel::<Request>();
        let (ready, startup) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("windows-capture".into())
            .spawn(move || {
                let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() };
                if let Err(error) = initialized {
                    let _ = ready.send(Err(anyhow!(error)));
                    return;
                }
                let _dpi = PhysicalDpi::enter();
                let mut worker = Worker {
                    backend,
                    outputs: HashMap::new(),
                    streams: HashMap::new(),
                    next_stream: 0,
                };
                let _ = ready.send(monitors().map(|_| ()));
                loop {
                    match receiver.recv_timeout(IDLE) {
                        Ok(request) => worker.handle(request),
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                    worker.release_idle();
                }
                drop(worker);
                unsafe {
                    CoUninitialize();
                }
            })?;
        startup
            .recv()
            .context("capture worker exited during startup")??;
        Ok(Self { requests })
    }

    /// Sends a request built around a reply channel and waits for the reply.
    pub(super) fn call<T>(
        &self,
        request: impl FnOnce(mpsc::SyncSender<Result<T>>) -> Request,
    ) -> Result<T> {
        let (reply, response) = mpsc::sync_channel(1);
        self.requests
            .send(request(reply))
            .map_err(|_| anyhow!("capture worker stopped"))?;
        response.recv().context("capture worker stopped")?
    }

    pub(super) fn close(&self, stream: u64) {
        let _ = self.requests.send(Request::Close { stream });
    }
}
impl crate::platform::Capture for WindowsCapture {
    fn capture(&self, outputs: Option<&[String]>, cursor: bool) -> Result<Snapshot> {
        self.call(|reply| Request::Snapshot {
            outputs: outputs.map(<[String]>::to_vec),
            cursor,
            reply,
        })
    }
}

struct Worker {
    backend: CaptureBackend,
    outputs: HashMap<String, Output>,
    streams: HashMap<u64, Stream>,
    next_stream: u64,
}
struct Output {
    monitor: Monitor,
    gdi: Option<Gdi>,
    dxgi: Option<Duplication>,
    dxgi_retry: Option<Instant>,
    used: Instant,
    /// Counts desktop and pointer updates across duplication re-creations,
    /// so each stream sees every change even when another reader acquired it.
    desktop_updates: u64,
    pointer_updates: u64,
}
impl Output {
    fn new(monitor: Monitor) -> Self {
        Self {
            monitor,
            gdi: None,
            dxgi: None,
            dxgi_retry: None,
            used: Instant::now(),
            desktop_updates: 0,
            pointer_updates: 0,
        }
    }
    /// The output's duplication, made on demand unless a recent attempt
    /// failed.
    fn duplication(&mut self) -> Option<&mut Duplication> {
        if self.dxgi.is_none() && self.dxgi_retry.is_none_or(|retry| Instant::now() >= retry) {
            match Duplication::new(&self.monitor) {
                Ok(duplication) => {
                    self.dxgi = Some(duplication);
                    self.dxgi_retry = None;
                }
                Err(error) => {
                    log::debug!("DXGI {} unavailable: {error:#}", self.monitor.name);
                    self.dxgi_retry = Some(Instant::now() + RETRY);
                }
            }
        }
        self.dxgi.as_mut()
    }
    /// Acquires the next update into the duplication (made on demand). One
    /// that fails is dropped so the next use re-creates factory, device,
    /// duplication and textures; `dxgi` is `None` afterwards.
    fn acquire(&mut self, wait: Duration) {
        let Some(dxgi) = self.duplication() else {
            return;
        };
        match dxgi.acquire(wait) {
            Ok(Update { desktop, pointer }) => {
                self.desktop_updates += u64::from(desktop);
                self.pointer_updates += u64::from(pointer);
            }
            Err(error) => {
                // Includes ACCESS_LOST (mode change, secure desktop) and
                // device removal.
                log::debug!("DXGI capture {} lost: {error:#}", self.monitor.name);
                self.dxgi = None;
            }
        }
    }
}

struct Stream {
    output: String,
    /// The output's rectangle when the stream opened; a mode or layout
    /// change ends the recording rather than recording the wrong pixels.
    monitor: IRect,
    /// Desktop orientation, relative to the output.
    region: IRect,
    cursor: bool,
    /// Update counts of the last delivered frame.
    seen: Option<(u64, u64)>,
    pool: Pool,
}

impl Worker {
    fn handle(&mut self, request: Request) {
        match request {
            Request::Snapshot {
                outputs,
                cursor,
                reply,
            } => {
                let _ = reply.send(self.capture(outputs.as_deref(), cursor));
            }
            Request::Open {
                output,
                region,
                cursor,
                reply,
            } => {
                let _ = reply.send(self.open(output, region, cursor));
            }
            Request::Frame {
                stream,
                wait,
                fresh,
                reply,
            } => {
                let _ = reply.send(self.frame(stream, wait, fresh));
            }
            Request::Close { stream } => {
                self.streams.remove(&stream);
            }
        }
    }

    fn release_idle(&mut self) {
        let streams = &self.streams;
        self.outputs.retain(|name, output| {
            output.used.elapsed() < IDLE || streams.values().any(|s| &s.output == name)
        });
    }

    fn capture(&mut self, selected: Option<&[String]>, cursor: bool) -> Result<Snapshot> {
        let monitors = monitors()?;
        // A recording on a removed or changed output notices the missing or
        // replaced entry itself and ends.
        self.outputs.retain(|name, output| {
            monitors
                .iter()
                .any(|m| &m.name == name && *m == output.monitor)
        });
        let mut snapshot = Snapshot::default();
        for monitor in monitors {
            if selected.is_some_and(|names| !names.contains(&monitor.name)) {
                continue;
            }
            let output = self
                .outputs
                .entry(monitor.name.clone())
                .or_insert_with(|| Output::new(monitor.clone()));
            output.used = Instant::now();
            let mut dxgi = None;
            if self.backend == CaptureBackend::Dxgi {
                // A resident duplication need not wait for a change; a new one
                // waits briefly for its first image.
                let wait = if output.dxgi.as_ref().is_some_and(Duplication::has_image) {
                    Duration::ZERO
                } else {
                    Duration::from_millis(100)
                };
                output.acquire(wait);
                if let Some(duplication) = &mut output.dxgi {
                    let separate_pointer = duplication.separate_pointer();
                    match duplication.image() {
                        Ok(image) => dxgi = image.map(|image| (image, separate_pointer)),
                        Err(error) => {
                            log::debug!("DXGI readback {} failed: {error:#}", monitor.name);
                            output.dxgi = None;
                        }
                    }
                }
            }
            let gdi = match &mut output.gdi {
                Some(gdi) => gdi,
                gdi => gdi.insert(Gdi::new(&monitor)?),
            };
            let image = match dxgi {
                // An embedded software pointer cannot be removed from DXGI
                // pixels; GDI excludes it for cursor-free shots.
                Some((image, true)) if cursor => gdi.with_cursor(&image, &monitor)?,
                Some((image, separate_pointer)) if cursor || separate_pointer => image,
                // No first DXGI image yet, or a cursor-free shot of an
                // embedded pointer: capture a fresh GDI frame.
                _ => gdi.capture(&monitor, cursor)?,
            };
            snapshot.outputs.push(OutputFrame {
                name: monitor.name,
                logical: monitor.logical,
                image,
            });
        }
        if snapshot.outputs.is_empty() {
            bail!("no matching active Windows outputs");
        }
        Ok(snapshot)
    }

    fn open(&mut self, name: String, region: IRect, cursor: bool) -> Result<u64> {
        let monitor = monitors()?
            .into_iter()
            .find(|m| m.name == name)
            .context("recording monitor disconnected")?;
        let m = monitor.physical;
        anyhow::ensure!(
            region.w > 0 && region.h > 0 && m.intersect(&region) == Some(region),
            "recording region lies outside monitor"
        );
        let output = self
            .outputs
            .entry(name.clone())
            .or_insert_with(|| Output::new(monitor.clone()));
        if output.monitor != monitor {
            *output = Output::new(monitor.clone());
        }
        output.used = Instant::now();
        // Opening is the moment to report a missing duplication; later
        // losses (secure desktop, mode switch) are retried while recording.
        output.dxgi_retry = None;
        output
            .duplication()
            .with_context(|| format!("Desktop Duplication of {name} unavailable"))?;
        let id = self.next_stream;
        self.next_stream += 1;
        self.streams.insert(
            id,
            Stream {
                output: name,
                monitor: m,
                region: IRect::new(region.x - m.x, region.y - m.y, region.w, region.h),
                cursor,
                seen: None,
                pool: Pool::default(),
            },
        );
        Ok(id)
    }

    fn frame(&mut self, id: u64, wait: Duration, fresh: bool) -> Result<Option<Frame>> {
        let stream = self
            .streams
            .get_mut(&id)
            .context("recording stream closed")?;
        let output = self
            .outputs
            .get_mut(&stream.output)
            .context("recording monitor disconnected")?;
        if output.monitor.physical != stream.monitor {
            bail!("recording monitor {} changed its layout", stream.output);
        }
        output.used = Instant::now();
        output.acquire(wait);
        let Some(dxgi) = output.dxgi.as_mut() else {
            // No duplication right now (secure desktop, mode switch in
            // progress): keep the recording, but notice a vanished monitor.
            if !monitors()?
                .iter()
                .any(|m| m.name == stream.output && m.physical == stream.monitor)
            {
                bail!(
                    "recording monitor {} disconnected or changed mode",
                    stream.output
                );
            }
            thread::sleep(wait);
            return Ok(None);
        };
        let updates = (output.desktop_updates, output.pointer_updates);
        let changed = stream.seen.is_none_or(|(desktop, pointer)| {
            desktop != updates.0 || (stream.cursor && pointer != updates.1)
        });
        if !dxgi.has_image() || !(fresh || changed) {
            return Ok(None);
        }
        let (width, height) = dxgi.desktop_size();
        let region = stream.region;
        if region.right() > width as i32 || region.bottom() > height as i32 {
            bail!("recording monitor {} changed resolution", stream.output);
        }
        let mut pixels = stream.pool.take(region.w as usize * region.h as usize * 4);
        if let Err(error) = dxgi.read_region(region, stream.cursor, &mut pixels) {
            log::debug!("DXGI readback {} failed: {error:#}", stream.output);
            output.dxgi = None;
            return Ok(None);
        }
        stream.seen = Some(updates);
        Ok(Some(Frame::from_pixels(
            region.w as u32,
            region.h as u32,
            region.w as usize * 4,
            PixelFormat::Bgrx,
            Pixels::Shared(Box::new(Pooled {
                pixels,
                pool: stream.pool.clone(),
            })),
            Instant::now(),
        )))
    }
}

/// Reuses a recording's frame buffers once the pipeline drops them.
#[derive(Clone, Default)]
struct Pool(Arc<Mutex<Vec<Vec<u8>>>>);
impl Pool {
    /// Frames the pipeline may hold at once before buffers are freed.
    const KEEP: usize = 4;
    fn take(&self, len: usize) -> Vec<u8> {
        let mut pixels = self.0.lock().unwrap().pop().unwrap_or_default();
        pixels.resize(len, 0);
        pixels
    }
}
struct Pooled {
    pixels: Vec<u8>,
    pool: Pool,
}
impl AsRef<[u8]> for Pooled {
    fn as_ref(&self) -> &[u8] {
        &self.pixels
    }
}
impl Drop for Pooled {
    fn drop(&mut self) {
        let mut pool = self.pool.0.lock().unwrap();
        if pool.len() < Pool::KEEP {
            pool.push(std::mem::take(&mut self.pixels));
        }
    }
}

/// Top-down DIB and both DCs remain resident until a mode/topology change.
struct Gdi {
    source: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut c_void,
    width: u32,
    height: u32,
}
impl Gdi {
    fn new(monitor: &Monitor) -> Result<Self> {
        let name: Vec<u16> = monitor.name.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let source = CreateDCW(w!("DISPLAY"), PCWSTR(name.as_ptr()), PCWSTR::null(), None);
            if source.is_invalid() {
                return Err(windows::core::Error::from_win32().into());
            }
            let memory = CreateCompatibleDC(source);
            if memory.is_invalid() {
                let _ = DeleteDC(source);
                bail!("CreateCompatibleDC failed");
            }
            let mut info = BITMAPINFO::default();
            info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = monitor.physical.w;
            info.bmiHeader.biHeight = -monitor.physical.h;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB.0;
            let mut bits = std::ptr::null_mut();
            let bitmap = match CreateDIBSection(
                source,
                &info,
                DIB_RGB_COLORS,
                &mut bits,
                HANDLE::default(),
                0,
            ) {
                Ok(bitmap) => bitmap,
                Err(e) => {
                    let _ = DeleteDC(memory);
                    let _ = DeleteDC(source);
                    return Err(e.into());
                }
            };
            let previous = SelectObject(memory, bitmap);
            if previous.is_invalid() || previous.0 as isize == -1 {
                let _ = DeleteObject(bitmap);
                let _ = DeleteDC(memory);
                let _ = DeleteDC(source);
                bail!("SelectObject failed");
            }
            Ok(Self {
                source,
                memory,
                bitmap,
                previous,
                bits,
                width: monitor.physical.w as u32,
                height: monitor.physical.h as u32,
            })
        }
    }
    fn capture(&mut self, monitor: &Monitor, cursor: bool) -> Result<Arc<Image>> {
        unsafe {
            BitBlt(
                self.memory,
                0,
                0,
                self.width as i32,
                self.height as i32,
                self.source,
                0,
                0,
                SRCCOPY | CAPTUREBLT,
            )?;
        }
        if cursor {
            self.draw_cursor(monitor)?;
        }
        unsafe {
            let _ = GdiFlush();
        }
        Ok(Arc::new(self.image()))
    }
    fn with_cursor(&mut self, image: &Image, monitor: &Monitor) -> Result<Arc<Image>> {
        if image.width != self.width || image.height != self.height {
            bail!("DXGI output dimensions changed");
        }
        let bytes =
            unsafe { std::slice::from_raw_parts_mut(self.bits as *mut u8, image.data.len()) };
        bytes.copy_from_slice(&image.data);
        self.draw_cursor(monitor)?;
        unsafe {
            let _ = GdiFlush();
        }
        Ok(Arc::new(self.image()))
    }
    fn image(&self) -> Image {
        let bytes = unsafe {
            std::slice::from_raw_parts(
                self.bits as *const u8,
                self.width as usize * self.height as usize * 4,
            )
        };
        pixels::to_image(
            bytes,
            self.width as usize * 4,
            self.width,
            self.height,
            Rotation::Identity,
        )
    }
    fn draw_cursor(&self, monitor: &Monitor) -> Result<()> {
        unsafe {
            let mut cursor = CURSORINFO {
                cbSize: size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            GetCursorInfo(&mut cursor)?;
            if cursor.flags != CURSOR_SHOWING {
                return Ok(());
            }
            let icon = HICON(cursor.hCursor.0);
            let mut info = ICONINFO::default();
            GetIconInfo(icon, &mut info)?;
            let result = DrawIconEx(
                self.memory,
                cursor.ptScreenPos.x - monitor.physical.x - info.xHotspot as i32,
                cursor.ptScreenPos.y - monitor.physical.y - info.yHotspot as i32,
                icon,
                0,
                0,
                0,
                None,
                DI_NORMAL,
            );
            if !info.hbmMask.is_invalid() {
                let _ = DeleteObject(info.hbmMask);
            }
            if !info.hbmColor.is_invalid() {
                let _ = DeleteObject(info.hbmColor);
            }
            result?;
        }
        Ok(())
    }
}
impl Drop for Gdi {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.memory, self.previous);
            let _ = DeleteObject(self.bitmap);
            let _ = DeleteDC(self.memory);
            let _ = DeleteDC(self.source);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires an interactive Windows desktop; captures screen pixels"]
    fn capture_latency() {
        use crate::platform::Capture as _;
        use std::time::Instant;
        for backend in [CaptureBackend::Gdi, CaptureBackend::Dxgi] {
            let capture = WindowsCapture::new_with_backend(backend).unwrap();
            let snapshot = capture.capture(None, false).unwrap();
            let resolution: Vec<_> = snapshot
                .outputs
                .iter()
                .map(|o| format!("{}={}x{}", o.name, o.image.width, o.image.height))
                .collect();
            for _ in 0..10 {
                capture.capture(None, false).unwrap();
            }
            let mut timings = Vec::with_capacity(100);
            for _ in 0..100 {
                let start = Instant::now();
                capture.capture(None, false).unwrap();
                timings.push(start.elapsed().as_secs_f64() * 1000.);
            }
            timings.sort_by(f64::total_cmp);
            println!(
                "{backend:?} {}: median {:.3}ms p95 {:.3}ms (100 warm captures; DXGI may fall back to GDI)",
                resolution.join(", "),
                timings[50],
                timings[95]
            );
        }
    }
}
