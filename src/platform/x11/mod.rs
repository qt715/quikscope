//! X11 screen capture, EWMH desktop knowledge and selection ownership.

mod desktop;
mod input;
mod selection;

pub use desktop::Ewmh;
pub use input::SelectionFocus;
pub use selection::SelectionOwner;

use std::sync::{Arc, mpsc};

use anyhow::{Context, Result, bail, ensure};
use x11rb::{
    connection::Connection,
    protocol::{
        randr::ConnectionExt as _,
        shm::ConnectionExt as _,
        xfixes::ConnectionExt as _,
        xproto::{ConnectionExt as _, ImageFormat, ImageOrder, VisualClass},
    },
    rust_connection::RustConnection,
};

use crate::{
    capture::{OutputFrame, Snapshot},
    geom::IRect,
    raster::Image,
};

#[derive(Clone, Debug)]
pub(super) struct Monitor {
    pub name: String,
    pub rect: IRect,
}

pub(super) fn monitors(conn: &RustConnection, screen: usize) -> Result<Vec<Monitor>> {
    let root = conn.setup().roots[screen].root;
    if let Ok(cookie) = conn.randr_get_monitors(root, true)
        && let Ok(reply) = cookie.reply()
    {
        let mut outputs = Vec::new();
        for monitor in reply.monitors {
            if monitor.width == 0 || monitor.height == 0 {
                continue;
            }
            let name = conn.get_atom_name(monitor.name)?.reply()?.name;
            outputs.push(Monitor {
                name: String::from_utf8_lossy(&name).into_owned(),
                rect: monitor_rect(monitor.x, monitor.y, monitor.width, monitor.height),
            });
        }
        if !outputs.is_empty() {
            return Ok(outputs);
        }
    }
    // Geometry, unlike the setup reply, reflects RandR root resizes.
    let geometry = conn.get_geometry(root)?.reply()?;
    Ok(vec![Monitor {
        name: format!("screen-{screen}"),
        rect: monitor_rect(0, 0, geometry.width, geometry.height),
    }])
}

fn monitor_rect(x: i16, y: i16, width: u16, height: u16) -> IRect {
    IRect {
        x: x.into(),
        y: y.into(),
        w: width.into(),
        h: height.into(),
    }
}

struct Job {
    outputs: Option<Vec<String>>,
    cursor: bool,
    reply: mpsc::SyncSender<Result<Snapshot>>,
}

/// A persistent connection and reusable MIT-SHM buffer owned by one worker.
pub struct ScreenCapture {
    jobs: mpsc::Sender<Job>,
}

impl ScreenCapture {
    pub fn spawn() -> Result<Self> {
        Self::start(true)
    }

    /// Exercises the network-compatible GetImage path in the integration suite.
    #[cfg(feature = "e2e")]
    pub fn spawn_without_shm() -> Result<Self> {
        Self::start(false)
    }

    fn start(shm: bool) -> Result<Self> {
        let (jobs, requests) = mpsc::channel::<Job>();
        let (ready, started) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("quikscope-x11-capture".into())
            .spawn(move || {
                let mut worker = match Worker::connect(shm) {
                    Ok(w) => {
                        let _ = ready.send(Ok(()));
                        w
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                for job in requests {
                    let _ = job
                        .reply
                        .send(worker.capture(job.outputs.as_deref(), job.cursor));
                }
            })?;
        started.recv().context("X11 capture worker stopped")??;
        Ok(Self { jobs })
    }
}

impl super::Capture for ScreenCapture {
    fn capture(&self, outputs: Option<&[String]>, cursor: bool) -> Result<Snapshot> {
        let (reply, result) = mpsc::sync_channel(1);
        self.jobs
            .send(Job {
                outputs: outputs.map(<[String]>::to_vec),
                cursor,
                reply,
            })
            .context("X11 capture worker stopped")?;
        result.recv().context("X11 capture worker stopped")?
    }
}

struct Worker {
    conn: RustConnection,
    screen: usize,
    format: PixelFormat,
    shm_enabled: bool,
    buffer: Option<ShmBuffer>,
}

impl Worker {
    fn connect(shm: bool) -> Result<Self> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = &conn.setup().roots[screen];
        ensure!(
            matches!(root.root_depth, 24 | 32),
            "X11 requires 24/32-bit TrueColor"
        );
        let visual = root
            .allowed_depths
            .iter()
            .flat_map(|d| &d.visuals)
            .find(|v| v.visual_id == root.root_visual)
            .context("missing X11 root visual")?;
        ensure!(
            visual.class == VisualClass::TRUE_COLOR,
            "X11 root is not TrueColor"
        );
        let pixmap = conn
            .setup()
            .pixmap_formats
            .iter()
            .find(|f| f.depth == root.root_depth)
            .context("missing root pixmap format")?;
        ensure!(
            matches!(pixmap.bits_per_pixel, 24 | 32),
            "unsupported X11 pixel format"
        );
        let format = PixelFormat {
            bits: pixmap.bits_per_pixel,
            pad: pixmap.scanline_pad,
            little_endian: conn.setup().image_byte_order == ImageOrder::LSB_FIRST,
            red: visual.red_mask,
            green: visual.green_mask,
            blue: visual.blue_mask,
        };
        let shm_enabled = shm
            && conn
                .shm_query_version()
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some();
        // XFixes 2 is sufficient for GetCursorImage; lack of it only disables cursor capture.
        let _ = conn
            .xfixes_query_version(2, 0)
            .ok()
            .and_then(|cookie| cookie.reply().ok());
        Ok(Self {
            conn,
            screen,
            format,
            shm_enabled,
            buffer: None,
        })
    }

    fn capture(&mut self, names: Option<&[String]>, cursor: bool) -> Result<Snapshot> {
        let outputs = monitors(&self.conn, self.screen)?;
        if let Some(names) = names {
            for name in names {
                ensure!(
                    outputs.iter().any(|o| &o.name == name),
                    "unknown X11 output {name}"
                );
            }
        }
        let root = self.conn.setup().roots[self.screen].root;
        let mut snapshot = Snapshot::default();
        for output in outputs
            .into_iter()
            .filter(|o| names.is_none_or(|n| n.contains(&o.name)))
        {
            let rect = output.rect;
            let width = u16::try_from(rect.w)?;
            let height = u16::try_from(rect.h)?;
            let size = self.format.stride(width.into()) * usize::from(height);
            if self.shm_enabled && self.buffer.as_ref().is_none_or(|b| b.len < size) {
                if let Some(old) = self.buffer.take() {
                    let _ = self.conn.shm_detach(old.id);
                }
                match ShmBuffer::new(&self.conn, size) {
                    Ok(buffer) => self.buffer = Some(buffer),
                    Err(e) => {
                        log::debug!("MIT-SHM unavailable, using GetImage: {e:#}");
                        self.shm_enabled = false;
                    }
                }
            }
            let shared = if let Some(buffer) = self.buffer.as_ref().filter(|_| self.shm_enabled) {
                self.conn
                    .shm_get_image(
                        root,
                        rect.x as i16,
                        rect.y as i16,
                        width,
                        height,
                        u32::MAX,
                        ImageFormat::Z_PIXMAP.into(),
                        buffer.id,
                        0,
                    )?
                    .reply()
                    .map(|_| ())
                    .is_ok()
            } else {
                false
            };
            let image = if shared {
                let buffer = self.buffer.as_ref().context("missing X11 SHM buffer")?;
                self.format
                    .decode(buffer.bytes(), width.into(), height.into())?
            } else {
                self.shm_enabled = false;
                let reply = self
                    .conn
                    .get_image(
                        ImageFormat::Z_PIXMAP,
                        root,
                        rect.x as i16,
                        rect.y as i16,
                        width,
                        height,
                        u32::MAX,
                    )?
                    .reply()?;
                self.format
                    .decode(&reply.data, width.into(), height.into())?
            };
            snapshot.outputs.push(OutputFrame {
                name: output.name,
                logical: rect,
                image: Arc::new(image),
            });
        }
        if cursor
            && let Ok(cookie) = self.conn.xfixes_get_cursor_image()
            && let Ok(cursor) = cookie.reply()
        {
            for output in &mut snapshot.outputs {
                composite_cursor(
                    Arc::make_mut(&mut output.image),
                    output.logical,
                    i32::from(cursor.x) - i32::from(cursor.xhot),
                    i32::from(cursor.y) - i32::from(cursor.yhot),
                    cursor.width.into(),
                    &cursor.cursor_image,
                );
            }
        }
        Ok(snapshot)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(buffer) = &self.buffer {
            let _ = self.conn.shm_detach(buffer.id);
            let _ = self.conn.flush();
        }
    }
}

struct ShmBuffer {
    id: u32,
    address: *mut libc::c_void,
    len: usize,
}

impl ShmBuffer {
    fn new(conn: &RustConnection, len: usize) -> Result<Self> {
        // Mark for removal only after the server attached; either process dying then releases it.
        let sys_id = unsafe { libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600) };
        if sys_id < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let address = unsafe { libc::shmat(sys_id, std::ptr::null(), 0) };
        if address == (-1isize) as *mut libc::c_void {
            unsafe {
                libc::shmctl(sys_id, libc::IPC_RMID, std::ptr::null_mut());
            }
            return Err(std::io::Error::last_os_error().into());
        }
        let id = match conn.generate_id() {
            Ok(id) => id,
            Err(e) => {
                unsafe {
                    libc::shmdt(address);
                    libc::shmctl(sys_id, libc::IPC_RMID, std::ptr::null_mut());
                }
                return Err(e.into());
            }
        };
        let attached = conn
            .shm_attach(id, sys_id as u32, false)
            .map_err(anyhow::Error::from)
            .and_then(|c| c.check().map_err(Into::into));
        unsafe {
            libc::shmctl(sys_id, libc::IPC_RMID, std::ptr::null_mut());
        }
        if let Err(e) = attached {
            unsafe {
                libc::shmdt(address);
            }
            return Err(e);
        }
        Ok(Self { id, address, len })
    }
    fn bytes(&self) -> &[u8] {
        // GetImage's reply synchronizes the server's writes before we read the segment.
        unsafe { std::slice::from_raw_parts(self.address.cast(), self.len) }
    }
}
impl Drop for ShmBuffer {
    fn drop(&mut self) {
        unsafe {
            libc::shmdt(self.address);
        }
    }
}

#[derive(Clone, Copy)]
struct PixelFormat {
    bits: u8,
    pad: u8,
    little_endian: bool,
    red: u32,
    green: u32,
    blue: u32,
}
impl PixelFormat {
    fn stride(self, width: u32) -> usize {
        (width as usize * usize::from(self.bits)).div_ceil(usize::from(self.pad))
            * usize::from(self.pad)
            / 8
    }
    fn decode(self, bytes: &[u8], width: u32, height: u32) -> Result<Image> {
        let stride = self.stride(width);
        ensure!(bytes.len() >= stride * height as usize, "short X11 image");
        // The native XRGB path avoids mask extraction and divisions per channel.
        if self.bits == 32
            && self.little_endian
            && (self.red, self.green, self.blue) == (0xff0000, 0xff00, 0xff)
        {
            let mut data = bytes[..stride * height as usize].to_vec();
            for pixel in data.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
            return Ok(Image {
                width,
                height,
                data,
            });
        }
        let mut data = vec![0; width as usize * height as usize * 4];
        let bpp = usize::from(self.bits / 8);
        for (src, dst) in bytes
            .chunks_exact(stride)
            .take(height as usize)
            .zip(data.chunks_exact_mut(width as usize * 4))
        {
            for (src, dst) in src
                .chunks_exact(bpp)
                .take(width as usize)
                .zip(dst.as_chunks_mut::<4>().0)
            {
                let mut pixel = 0u32;
                for (index, byte) in src.iter().enumerate() {
                    let shift = if self.little_endian {
                        index * 8
                    } else {
                        (bpp - index - 1) * 8
                    };
                    pixel |= u32::from(*byte) << shift;
                }
                for (out, mask) in dst[..3].iter_mut().zip([self.blue, self.green, self.red]) {
                    if mask == 0 {
                        bail!("invalid X11 color mask");
                    }
                    let value = (pixel & mask) >> mask.trailing_zeros();
                    *out =
                        ((u64::from(value) * 255) / u64::from(mask >> mask.trailing_zeros())) as u8;
                }
                dst[3] = 255;
            }
        }
        Ok(Image {
            width,
            height,
            data,
        })
    }
}

fn composite_cursor(image: &mut Image, rect: IRect, x: i32, y: i32, width: usize, pixels: &[u32]) {
    if width == 0 {
        return;
    }
    for (index, &argb) in pixels.iter().enumerate() {
        let dx = x + (index % width) as i32 - rect.x;
        let dy = y + (index / width) as i32 - rect.y;
        if dx < 0 || dy < 0 || dx >= rect.w || dy >= rect.h {
            continue;
        }
        let dst = &mut image.data[(dy as usize * image.width as usize + dx as usize) * 4..][..4];
        let alpha = argb >> 24;
        for (channel, shift) in dst[..3].iter_mut().zip([0, 8, 16]) {
            // XFixes cursor pixels are premultiplied ARGB.
            *channel = (((argb >> shift) & 255) + (u32::from(*channel) * (255 - alpha) + 127) / 255)
                .min(255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn format(bits: u8, little_endian: bool) -> PixelFormat {
        PixelFormat {
            bits,
            pad: 32,
            little_endian,
            red: 0xff0000,
            green: 0xff00,
            blue: 0xff,
        }
    }
    #[test]
    fn truecolor_padding_and_endianness() {
        assert_eq!(
            format(24, true)
                .decode(&[3, 2, 1, 0, 6, 5, 4, 0], 1, 2)
                .unwrap()
                .data,
            [3, 2, 1, 255, 6, 5, 4, 255]
        );
        assert_eq!(
            format(32, false).decode(&[0, 1, 2, 3], 1, 1).unwrap().data,
            [3, 2, 1, 255]
        );
        assert!(format(32, true).decode(&[0], 1, 1).is_err());
    }
    #[test]
    fn nonstandard_masks() {
        let f = PixelFormat {
            red: 0xff,
            blue: 0xff0000,
            ..format(32, true)
        };
        assert_eq!(f.decode(&[1, 2, 3, 0], 1, 1).unwrap().data, [3, 2, 1, 255]);
    }
    #[test]
    fn monitor_coordinates_and_cursor_clipping() {
        assert_eq!(
            monitor_rect(-1920, 0, 1920, 1080),
            IRect {
                x: -1920,
                y: 0,
                w: 1920,
                h: 1080
            }
        );
        let mut image = Image {
            width: 1,
            height: 1,
            data: vec![100, 100, 100, 255],
        };
        composite_cursor(
            &mut image,
            monitor_rect(10, 20, 1, 1),
            9,
            20,
            2,
            &[0xffffffff, 0x80800000],
        );
        assert_eq!(image.data, [50, 50, 178, 255]);
    }
}
