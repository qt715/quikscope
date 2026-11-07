//! Resident per-output capture. DXGI is the default architectural choice: it
//! retains duplication/device/staging resources and reuses unchanged frames.
//! No hardware timing claim is implied. `new_with_backend` allows an external
//! benchmark to compare DXGI (with availability fallback) against forced GDI.
use super::desktop::{Monitor, PhysicalDpi, monitors};
use crate::{
    capture::{OutputFrame, Snapshot},
    raster::Image,
};
use anyhow::{Context, Result, anyhow, bail};
use std::{
    collections::HashMap,
    ffi::c_void,
    mem::size_of,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HMODULE},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::*,
            Dxgi::{Common::*, *},
            Gdi::*,
        },
        System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
        UI::WindowsAndMessaging::{
            CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HICON,
            ICONINFO,
        },
    },
    core::{Interface, PCWSTR, w},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureBackend {
    Dxgi,
    Gdi,
}

struct Request {
    outputs: Option<Vec<String>>,
    cursor: bool,
    reply: mpsc::SyncSender<Result<Snapshot>>,
}

/// Only messages cross threads; all COM and GDI objects stay on their owner.
pub struct WindowsCapture {
    requests: mpsc::Sender<Request>,
}
impl WindowsCapture {
    pub fn new() -> Result<Self> {
        let backend = match std::env::var("QUICKSCOPE_WINDOWS_CAPTURE").as_deref() {
            Ok("gdi") => CaptureBackend::Gdi,
            Ok("dxgi") | Err(std::env::VarError::NotPresent) => CaptureBackend::Dxgi,
            _ => bail!("QUICKSCOPE_WINDOWS_CAPTURE must be dxgi or gdi"),
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
                };
                let _ = ready.send(monitors().map(|_| ()));
                for request in receiver {
                    let result = worker.capture(request.outputs.as_deref(), request.cursor);
                    let _ = request.reply.send(result);
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
}
impl crate::platform::Capture for WindowsCapture {
    fn capture(&self, outputs: Option<&[String]>, cursor: bool) -> Result<Snapshot> {
        let (reply, response) = mpsc::sync_channel(1);
        self.requests
            .send(Request {
                outputs: outputs.map(<[String]>::to_vec),
                cursor,
                reply,
            })
            .map_err(|_| anyhow!("capture worker stopped"))?;
        response.recv().context("capture worker stopped")?
    }
}

struct Worker {
    backend: CaptureBackend,
    outputs: HashMap<String, Output>,
}
struct Output {
    monitor: Monitor,
    gdi: Gdi,
    dxgi: Option<Duplication>,
    dxgi_retry: Option<Instant>,
}
impl Worker {
    fn capture(&mut self, selected: Option<&[String]>, cursor: bool) -> Result<Snapshot> {
        let monitors = monitors()?;
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
            if !self.outputs.contains_key(&monitor.name) {
                let gdi = Gdi::new(&monitor)?;
                self.outputs.insert(
                    monitor.name.clone(),
                    Output {
                        monitor: monitor.clone(),
                        gdi,
                        dxgi: None,
                        dxgi_retry: None,
                    },
                );
            }
            let output = self.outputs.get_mut(&monitor.name).unwrap();
            let mut frame = None;
            if self.backend == CaptureBackend::Dxgi {
                if output.dxgi.is_none()
                    && output
                        .dxgi_retry
                        .is_none_or(|retry| Instant::now() >= retry)
                {
                    match Duplication::new(&monitor) {
                        Ok(duplication) => {
                            output.dxgi = Some(duplication);
                            output.dxgi_retry = None;
                        }
                        Err(error) => {
                            // An unsupported virtual output must not pay device
                            // creation on every capture. Retry after a pause so
                            // unlocking/switching desktops can recover.
                            log::debug!("DXGI {} unavailable; using GDI: {error:#}", monitor.name);
                            output.dxgi_retry = Some(Instant::now() + Duration::from_secs(1));
                        }
                    }
                }
                if let Some(dxgi) = &mut output.dxgi {
                    match dxgi.frame() {
                        Ok(Some((image, separate_pointer))) => {
                            // An embedded software pointer cannot be removed from
                            // DXGI pixels; GDI excludes it for cursor-free shots.
                            if cursor || separate_pointer {
                                frame = Some(if cursor && separate_pointer {
                                    output.gdi.with_cursor(&image, &monitor)?
                                } else {
                                    image
                                });
                            }
                        }
                        Ok(None) => {} // no first DXGI frame yet: capture a fresh GDI frame
                        Err(error) => {
                            log::debug!(
                                "DXGI capture {} unavailable, recreating: {error:#}",
                                monitor.name
                            );
                            // Includes ACCESS_LOST/device removal; next request recreates
                            // factory, adapter, device, duplication, and staging together.
                            output.dxgi = None;
                        }
                    }
                }
            }
            let image = match frame {
                Some(image) => image,
                None => output.gdi.capture(&monitor, cursor)?,
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
}

struct Duplication {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    size: (u32, u32),
    rotation: DXGI_MODE_ROTATION,
    cached: Option<Arc<Image>>,
    separate_pointer: bool,
}
impl Duplication {
    fn new(monitor: &Monitor) -> Result<Self> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut index = 0;
            loop {
                let adapter = match factory.EnumAdapters1(index) {
                    Ok(adapter) => adapter,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => return Err(e.into()),
                };
                index += 1;
                let mut output_index = 0;
                loop {
                    let output = match adapter.EnumOutputs(output_index) {
                        Ok(output) => output,
                        Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                        Err(e) => return Err(e.into()),
                    };
                    output_index += 1;
                    let desc = output.GetDesc()?;
                    let end = desc
                        .DeviceName
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(desc.DeviceName.len());
                    if String::from_utf16_lossy(&desc.DeviceName[..end]) != monitor.name {
                        continue;
                    }
                    let mut device = None;
                    let mut context = None;
                    D3D11CreateDevice(
                        &adapter,
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                        None,
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        Some(&mut context),
                    )?;
                    let device = device.context("D3D11 returned no device")?;
                    let duplication = output.cast::<IDXGIOutput1>()?.DuplicateOutput(&device)?;
                    return Ok(Self {
                        device,
                        context: context.context("D3D11 returned no context")?,
                        rotation: duplication.GetDesc().Rotation,
                        duplication,
                        staging: None,
                        size: (0, 0),
                        cached: None,
                        separate_pointer: false,
                    });
                }
            }
        }
        bail!("no DXGI output for {}", monitor.name)
    }
    fn frame(&mut self) -> Result<Option<(Arc<Image>, bool)>> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            // Resident duplication means idle desktops need not wait for a change.
            match self.duplication.AcquireNextFrame(
                if self.cached.is_some() { 0 } else { 100 },
                &mut info,
                &mut resource,
            ) {
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    return Ok(self
                        .cached
                        .clone()
                        .map(|image| (image, self.separate_pointer)));
                }
                Err(e) => return Err(e.into()),
                Ok(()) => {}
            }
            if info.LastMouseUpdateTime != 0 {
                self.separate_pointer = info.PointerPosition.Visible.as_bool();
            }
            // Pointer-only updates have no new desktop pixels. In particular,
            // a new duplication may deliver one before its first desktop frame;
            // its texture must not be read or cached as a black desktop.
            if info.LastPresentTime == 0 {
                self.duplication.ReleaseFrame()?;
                return Ok(self
                    .cached
                    .clone()
                    .map(|image| (image, self.separate_pointer)));
            }
            let result = (|| -> Result<Arc<Image>> {
                let texture: ID3D11Texture2D =
                    resource.context("DXGI returned no frame")?.cast()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                    bail!("unsupported duplication format");
                }
                if self.size != (desc.Width, desc.Height) {
                    self.staging = None;
                    desc.Usage = D3D11_USAGE_STAGING;
                    desc.BindFlags = 0;
                    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                    desc.MiscFlags = 0;
                    self.device
                        .CreateTexture2D(&desc, None, Some(&mut self.staging))?;
                    self.size = (desc.Width, desc.Height);
                }
                let staging = self.staging.as_ref().context("missing staging texture")?;
                self.context.CopyResource(staging, &texture);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.context
                    .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                let bytes = std::slice::from_raw_parts(
                    mapped.pData as *const u8,
                    mapped.RowPitch as usize * self.size.1 as usize,
                );
                let image = copy_bgra(
                    bytes,
                    mapped.RowPitch as usize,
                    self.size.0,
                    self.size.1,
                    self.rotation,
                );
                self.context.Unmap(staging, 0);
                Ok(Arc::new(image))
            })();
            let released = self.duplication.ReleaseFrame();
            let image = result?;
            released?;
            self.cached = Some(image.clone());
            Ok(Some((image, self.separate_pointer)))
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
        copy_bgra(
            bytes,
            self.width as usize * 4,
            self.width,
            self.height,
            DXGI_MODE_ROTATION_IDENTITY,
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

fn copy_bgra(
    bytes: &[u8],
    pitch: usize,
    width: u32,
    height: u32,
    rotation: DXGI_MODE_ROTATION,
) -> Image {
    let quarter =
        rotation == DXGI_MODE_ROTATION_ROTATE90 || rotation == DXGI_MODE_ROTATION_ROTATE270;
    let mut image = Image::new(
        if quarter { height } else { width },
        if quarter { width } else { height },
    );
    if rotation == DXGI_MODE_ROTATION_IDENTITY || rotation == DXGI_MODE_ROTATION_UNSPECIFIED {
        let row_bytes = width as usize * 4;
        for (source, target) in bytes
            .chunks(pitch)
            .zip(image.data.chunks_exact_mut(row_bytes))
        {
            target.copy_from_slice(&source[..row_bytes]);
            for alpha in target.iter_mut().skip(3).step_by(4) {
                *alpha = 255;
            }
        }
        return image;
    }
    for y in 0..height {
        for x in 0..width {
            let (dx, dy) = if rotation == DXGI_MODE_ROTATION_ROTATE90 {
                (height - 1 - y, x)
            } else if rotation == DXGI_MODE_ROTATION_ROTATE180 {
                (width - 1 - x, height - 1 - y)
            } else if rotation == DXGI_MODE_ROTATION_ROTATE270 {
                (y, width - 1 - x)
            } else {
                (x, y)
            };
            let src = y as usize * pitch + x as usize * 4;
            let dst = (dy as usize * image.width as usize + dx as usize) * 4;
            image.data[dst..dst + 4].copy_from_slice(&[
                bytes[src],
                bytes[src + 1],
                bytes[src + 2],
                255,
            ]);
        }
    }
    image
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

    #[test]
    fn pitched_bgra_stays_bgra_and_becomes_opaque() {
        let image = copy_bgra(
            &[
                1, 2, 3, 0, 4, 5, 6, 0, 99, 99, 99, 99, 7, 8, 9, 0, 10, 11, 12, 0,
            ],
            12,
            2,
            2,
            DXGI_MODE_ROTATION_IDENTITY,
        );
        assert_eq!(
            image.data,
            [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );
    }
    #[test]
    fn all_rotations_preserve_pixel_positions() {
        let bytes = [
            1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0, 5, 0, 0, 0, 6, 0, 0, 0,
        ];
        for (rotation, dimensions, expected) in [
            (DXGI_MODE_ROTATION_IDENTITY, (2, 3), vec![1, 2, 3, 4, 5, 6]),
            (DXGI_MODE_ROTATION_ROTATE90, (3, 2), vec![5, 3, 1, 6, 4, 2]),
            (DXGI_MODE_ROTATION_ROTATE180, (2, 3), vec![6, 5, 4, 3, 2, 1]),
            (DXGI_MODE_ROTATION_ROTATE270, (3, 2), vec![2, 4, 6, 1, 3, 5]),
        ] {
            let image = copy_bgra(&bytes, 8, 2, 3, rotation);
            assert_eq!((image.width, image.height), dimensions);
            assert_eq!(
                image.data.chunks_exact(4).map(|p| p[0]).collect::<Vec<_>>(),
                expected
            );
        }
    }
}
