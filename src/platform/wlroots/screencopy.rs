//! `wlr-screencopy-unstable-v1` client on a dedicated thread.
//!
//! The thread keeps its Wayland connection, bound globals, output metadata
//! and shared-memory buffers alive between captures, so a capture costs one
//! compositor frame plus a copy out of shared memory; nothing is re-bound or
//! re-allocated per shot. All requested outputs are captured concurrently.

use std::collections::HashMap;
use std::fs::File;
use std::os::fd::AsFd as _;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use memmap2::MmapMut;
use wayland_client::globals::{GlobalList, GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::capture::{OutputFrame, Snapshot};
use crate::geom::IRect;
use crate::platform::Capture;
use crate::raster::{Image, Transform};

/// Upper bound for one capture; a healthy compositor answers within a frame.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(3);

/// Handle to the capture thread.
pub struct Screencopy {
    tx: mpsc::Sender<Job>,
}

struct Job {
    outputs: Option<Vec<String>>,
    cursor: bool,
    reply: mpsc::Sender<Result<Snapshot>>,
}

/// Public description of an output, as last announced by the compositor.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputInfo {
    pub name: String,
    pub logical: IRect,
}

impl Screencopy {
    /// Connects to the compositor on a new thread. Fails if the compositor
    /// lacks `wlr-screencopy` or `wl_shm`.
    pub fn spawn() -> Result<Screencopy> {
        let (tx, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        std::thread::Builder::new()
            .name("quikscope-capture".into())
            .spawn(move || {
                let mut client = match Client::connect() {
                    Ok(c) => {
                        let _ = ready_tx.send(Ok(()));
                        c
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                for job in rx {
                    let result = client.capture(job.outputs.as_deref(), job.cursor);
                    if let Err(e) = &result {
                        log::error!("capture failed: {e:#}");
                    }
                    let _ = job.reply.send(result);
                }
            })?;
        ready_rx
            .recv()
            .map_err(|_| anyhow!("capture thread exited during startup"))??;
        Ok(Screencopy { tx })
    }
}

impl Capture for Screencopy {
    fn capture(&self, outputs: Option<&[String]>, cursor: bool) -> Result<Snapshot> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Job {
                outputs: outputs.map(<[String]>::to_vec),
                cursor,
                reply,
            })
            .map_err(|_| anyhow!("the capture thread is not running"))?;
        rx.recv()
            .map_err(|_| anyhow!("the capture thread dropped the request"))?
    }
}

#[derive(Default)]
struct Output {
    global: u32,
    wl: Option<wl_output::WlOutput>,
    xdg: Option<zxdg_output_v1::ZxdgOutputV1>,
    name: Option<String>,
    geometry_pos: (i32, i32),
    mode: (i32, i32),
    scale: i32,
    transform: Transform,
    logical_pos: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
}

impl Output {
    fn info(&self) -> Option<OutputInfo> {
        let name = self.name.clone()?;
        let (x, y) = self.logical_pos.unwrap_or(self.geometry_pos);
        let (w, h) = self.logical_size.unwrap_or_else(|| {
            let s = self.scale.max(1);
            let (mw, mh) = self.mode;
            if self.transform.swaps_axes() {
                (mh / s, mw / s)
            } else {
                (mw / s, mh / s)
            }
        });
        Some(OutputInfo {
            name,
            logical: IRect::new(x, y, w, h),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct BufferSpec {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

struct ShmBuffer {
    spec: BufferSpec,
    buffer: wl_buffer::WlBuffer,
    map: MmapMut,
    _file: File,
}

#[derive(PartialEq)]
enum FrameStatus {
    Pending,
    Ready,
    Failed,
}

struct Frame {
    output: usize,
    proxy: zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    offers: Vec<BufferSpec>,
    copied: bool,
    y_invert: bool,
    status: FrameStatus,
}

struct State {
    outputs: Vec<Output>,
    frames: Vec<Frame>,
    /// Buffers kept per output name and reused while the mode is unchanged.
    buffers: HashMap<String, ShmBuffer>,
    shm: wl_shm::WlShm,
    manager: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    xdg_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    error: Option<anyhow::Error>,
}

struct Client {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
}

impl Client {
    fn connect() -> Result<Client> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland display")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let shm = globals
            .bind::<wl_shm::WlShm, _, _>(&qh, 1..=1, ())
            .context("compositor has no wl_shm")?;
        let manager = globals
            .bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _, _>(&qh, 1..=3, ())
            .context("compositor does not support wlr-screencopy")?;
        let xdg_manager = globals
            .bind::<zxdg_output_manager_v1::ZxdgOutputManagerV1, _, _>(&qh, 2..=3, ())
            .ok();
        let mut state = State {
            outputs: Vec::new(),
            frames: Vec::new(),
            buffers: HashMap::new(),
            shm,
            manager,
            xdg_manager,
            error: None,
        };
        bind_existing_outputs(&globals, &qh, &mut state);
        // First roundtrip delivers wl_output/xdg_output metadata.
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        Ok(Client { conn, queue, state })
    }

    fn capture(&mut self, names: Option<&[String]>, cursor: bool) -> Result<Snapshot> {
        let started = Instant::now();
        let qh = self.queue.handle();
        // Pick up hotplugged outputs and metadata changes.
        self.queue.roundtrip(&mut self.state)?;

        let selected: Vec<usize> = self
            .state
            .outputs
            .iter()
            .enumerate()
            .filter(|(_, o)| o.wl.is_some() && o.name.is_some())
            .filter(|(_, o)| names.is_none_or(|n| n.iter().any(|n| Some(n) == o.name.as_ref())))
            .map(|(i, _)| i)
            .collect();
        if selected.is_empty() {
            match names {
                Some(n) => bail!("no output named {}", n.join(", ")),
                None => bail!("the compositor reports no outputs"),
            }
        }

        self.state.frames.clear();
        self.state.error = None;
        for &i in &selected {
            let wl = self.state.outputs[i].wl.as_ref().unwrap();
            let idx = self.state.frames.len();
            let proxy = self
                .state
                .manager
                .capture_output(cursor as i32, wl, &qh, idx);
            self.state.frames.push(Frame {
                output: i,
                proxy,
                offers: Vec::new(),
                copied: false,
                y_invert: false,
                status: FrameStatus::Pending,
            });
        }

        let deadline = Instant::now() + CAPTURE_TIMEOUT;
        while self
            .state
            .frames
            .iter()
            .any(|f| f.status == FrameStatus::Pending)
        {
            if let Some(e) = self.state.error.take() {
                return Err(e);
            }
            // v1/v2 managers have no `buffer_done`: copy once offers arrived.
            if self.state.manager.version() < 3 {
                for i in 0..self.state.frames.len() {
                    if !self.state.frames[i].copied && !self.state.frames[i].offers.is_empty() {
                        self.state.start_copy(i, &qh)?;
                    }
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("the compositor did not deliver a frame within {CAPTURE_TIMEOUT:?}");
            }
            dispatch_timeout(&self.conn, &mut self.queue, &mut self.state, remaining)?;
        }
        if let Some(e) = self.state.error.take() {
            return Err(e);
        }

        let mut outputs = Vec::with_capacity(self.state.frames.len());
        for frame in &self.state.frames {
            let output = &self.state.outputs[frame.output];
            let info = output.info().unwrap();
            if frame.status == FrameStatus::Failed {
                bail!("the compositor failed to copy output {}", info.name);
            }
            let buf = &self.state.buffers[&info.name];
            let mut image = convert(&buf.map, buf.spec)?;
            if frame.y_invert {
                image.flip_vertical();
            }
            // The buffer holds the output's native (pre-transform) pixels;
            // undo the transform to get what the user sees.
            if output.transform != Transform::Normal {
                image = image.transformed(output.transform.invert());
            }
            outputs.push(OutputFrame {
                name: info.name,
                logical: info.logical,
                image: Arc::new(image),
            });
        }
        for frame in self.state.frames.drain(..) {
            frame.proxy.destroy();
        }
        log::debug!(
            "captured {} output(s) in {:.1}ms",
            outputs.len(),
            started.elapsed().as_secs_f64() * 1e3
        );
        Ok(Snapshot { outputs })
    }
}

impl State {
    fn start_copy(&mut self, idx: usize, qh: &QueueHandle<State>) -> Result<()> {
        let frame = &self.frames[idx];
        let spec = *frame
            .offers
            .iter()
            .min_by_key(|s| format_rank(s.format))
            .context("the compositor offered no shared-memory buffer")?;
        if format_rank(spec.format) == u8::MAX {
            bail!("unsupported screencopy format {:?}", spec.format);
        }
        let name = self.outputs[frame.output].name.clone().unwrap_or_default();
        let reusable = self.buffers.get(&name).is_some_and(|b| b.spec == spec);
        if !reusable {
            if let Some(old) = self.buffers.remove(&name) {
                old.buffer.destroy();
            }
            let buf = alloc_buffer(&self.shm, spec, qh)?;
            self.buffers.insert(name.clone(), buf);
        }
        let frame = &mut self.frames[idx];
        frame.proxy.copy(&self.buffers[&name].buffer);
        frame.copied = true;
        Ok(())
    }
}

/// Lower is better; `u8::MAX` = cannot convert.
fn format_rank(format: wl_shm::Format) -> u8 {
    use wl_shm::Format as F;
    match format {
        F::Xrgb8888 | F::Argb8888 => 0,
        F::Xbgr8888 | F::Abgr8888 => 1,
        F::Xrgb2101010 | F::Argb2101010 | F::Xbgr2101010 | F::Abgr2101010 => 2,
        _ => u8::MAX,
    }
}

fn alloc_buffer(
    shm: &wl_shm::WlShm,
    spec: BufferSpec,
    qh: &QueueHandle<State>,
) -> Result<ShmBuffer> {
    use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
    let size = spec.stride as u64 * spec.height as u64;
    let fd = memfd_create("quikscope-screencopy", MemfdFlags::CLOEXEC)?;
    ftruncate(&fd, size)?;
    let file = File::from(fd);
    // SAFETY: the memfd is private to this process and the compositor; the
    // mapping lives exactly as long as the buffer it backs.
    let map = unsafe { MmapMut::map_mut(&file)? };
    let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        spec.width as i32,
        spec.height as i32,
        spec.stride as i32,
        spec.format,
        qh,
        (),
    );
    pool.destroy();
    Ok(ShmBuffer {
        spec,
        buffer,
        map,
        _file: file,
    })
}

/// Converts a shared-memory frame into an opaque BGRA image. Screen content
/// is always opaque, so alpha is forced to 255 regardless of what the
/// compositor left in the X/A channel.
fn convert(src: &[u8], spec: BufferSpec) -> Result<Image> {
    use wl_shm::Format as F;
    let (w, h, stride) = (
        spec.width as usize,
        spec.height as usize,
        spec.stride as usize,
    );
    if stride < w * 4 || src.len() < stride * h {
        bail!("screencopy buffer is smaller than announced");
    }
    let mut data = vec![0u8; w * h * 4];
    for (y, dst_row) in data.chunks_exact_mut(w * 4).enumerate() {
        let src_row = &src[y * stride..y * stride + w * 4];
        let pixels = dst_row
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(src_row.as_chunks::<4>().0);
        match spec.format {
            F::Xrgb8888 | F::Argb8888 => {
                for (d, &[b, g, r, _]) in pixels {
                    *d = [b, g, r, 255];
                }
            }
            F::Xbgr8888 | F::Abgr8888 => {
                for (d, &[r, g, b, _]) in pixels {
                    *d = [b, g, r, 255];
                }
            }
            F::Xrgb2101010 | F::Argb2101010 | F::Xbgr2101010 | F::Abgr2101010 => {
                let rgb_order = matches!(spec.format, F::Xrgb2101010 | F::Argb2101010);
                for (d, &s) in pixels {
                    let v = u32::from_le_bytes(s);
                    let hi = ((v >> 20) & 0x3ff) >> 2;
                    let mid = ((v >> 10) & 0x3ff) >> 2;
                    let lo = (v & 0x3ff) >> 2;
                    let (r, b) = if rgb_order { (hi, lo) } else { (lo, hi) };
                    *d = [b as u8, mid as u8, r as u8, 255];
                }
            }
            other => bail!("unsupported screencopy format {other:?}"),
        }
    }
    Image::from_bgra(spec.width, spec.height, data)
}

fn bind_existing_outputs(globals: &GlobalList, qh: &QueueHandle<State>, state: &mut State) {
    let outputs: Vec<(u32, u32)> = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == wl_output::WlOutput::interface().name)
            .map(|g| (g.name, g.version))
            .collect()
    });
    for (name, version) in outputs {
        let wl =
            globals
                .registry()
                .bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, name);
        state.add_output(name, wl, qh);
    }
}

impl State {
    fn add_output(&mut self, global: u32, wl: wl_output::WlOutput, qh: &QueueHandle<State>) {
        let xdg = self
            .xdg_manager
            .as_ref()
            .map(|m| m.get_xdg_output(&wl, qh, global));
        self.outputs.push(Output {
            global,
            wl: Some(wl),
            xdg,
            scale: 1,
            ..Default::default()
        });
    }

    fn output_mut(&mut self, global: u32) -> Option<&mut Output> {
        self.outputs.iter_mut().find(|o| o.global == global)
    }
}

/// Dispatches events, waiting at most `timeout` for the socket to become
/// readable. `blocking_dispatch` alone could hang forever on a wedged
/// compositor and take the daemon's capture thread with it.
fn dispatch_timeout(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    timeout: Duration,
) -> Result<()> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    queue.flush()?;
    if queue.dispatch_pending(state)? > 0 {
        return Ok(());
    }
    let Some(guard) = conn.prepare_read() else {
        queue.dispatch_pending(state)?;
        return Ok(());
    };
    let conn_fd = guard.connection_fd();
    let mut fds = [PollFd::new(&conn_fd, PollFlags::IN)];
    let ts = Timespec::try_from(timeout).unwrap_or(Timespec {
        tv_sec: 1,
        tv_nsec: 0,
    });
    let ready = poll(&mut fds, Some(&ts))?;
    if ready > 0 {
        guard.read()?;
    } else {
        drop(guard);
    }
    queue.dispatch_pending(state)?;
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == wl_output::WlOutput::interface().name => {
                let wl = registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, name);
                state.add_output(name, wl, qh);
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(pos) = state.outputs.iter().position(|o| o.global == name) {
                    let o = state.outputs.remove(pos);
                    if let Some(n) = &o.name
                        && let Some(b) = state.buffers.remove(n)
                    {
                        b.buffer.destroy();
                    }
                    if let Some(x) = o.xdg {
                        x.destroy();
                    }
                    if let Some(wl) = o.wl
                        && wl.version() >= 3
                    {
                        wl.release();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.output_mut(*global) else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                x, y, transform, ..
            } => {
                o.geometry_pos = (x, y);
                if let WEnum::Value(t) = transform {
                    o.transform = Transform::from_wl(t.into());
                }
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                if let WEnum::Value(f) = flags
                    && f.contains(wl_output::Mode::Current)
                {
                    o.mode = (width, height);
                }
            }
            wl_output::Event::Scale { factor } => o.scale = factor,
            wl_output::Event::Name { name } => o.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.output_mut(*global) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.logical_pos = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                o.logical_size = Some((width, height))
            }
            zxdg_output_v1::Event::Name { name } if o.name.is_none() => {
                o.name = Some(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        idx: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        let idx = *idx;
        if idx >= state.frames.len() {
            return;
        }
        match event {
            Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                state.frames[idx].offers.push(BufferSpec {
                    format,
                    width,
                    height,
                    stride,
                });
            }
            Event::BufferDone => {
                if let Err(e) = state.start_copy(idx, qh) {
                    state.frames[idx].status = FrameStatus::Failed;
                    state.error = Some(e);
                }
            }
            Event::Flags {
                flags: WEnum::Value(f),
            } => {
                state.frames[idx].y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
            }
            Event::Ready { .. } => state.frames[idx].status = FrameStatus::Ready,
            Event::Failed => state.frames[idx].status = FrameStatus::Failed,
            _ => {}
        }
    }
}

wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(State: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(format: wl_shm::Format, w: u32, h: u32, stride: u32) -> BufferSpec {
        BufferSpec {
            format,
            width: w,
            height: h,
            stride,
        }
    }

    #[test]
    fn converts_xrgb_with_row_padding_and_forces_opacity() {
        // 2x2, stride 12 (4 bytes padding per row), alpha byte garbage.
        let src = [
            1, 2, 3, 0, 4, 5, 6, 9, 0xEE, 0xEE, 0xEE, 0xEE, //
            7, 8, 9, 0, 10, 11, 12, 0, 0xEE, 0xEE, 0xEE, 0xEE,
        ];
        let img = convert(&src, spec(wl_shm::Format::Xrgb8888, 2, 2, 12)).unwrap();
        assert_eq!(img.pixel(1, 0), [4, 5, 6, 255]);
        assert_eq!(img.pixel(0, 1), [7, 8, 9, 255]);
    }

    #[test]
    fn converts_xbgr_by_swapping_red_and_blue() {
        let src = [10, 20, 30, 0];
        let img = convert(&src, spec(wl_shm::Format::Xbgr8888, 1, 1, 4)).unwrap();
        assert_eq!(img.pixel(0, 0), [30, 20, 10, 255]);
    }

    #[test]
    fn converts_10bit_formats() {
        // r = 0x3ff, g = 0x200, b = 0 in XRGB2101010.
        let v: u32 = (0x3ff << 20) | (0x200 << 10);
        let img = convert(&v.to_le_bytes(), spec(wl_shm::Format::Xrgb2101010, 1, 1, 4)).unwrap();
        assert_eq!(img.pixel(0, 0), [0, 128, 255, 255]);
        let img = convert(&v.to_le_bytes(), spec(wl_shm::Format::Xbgr2101010, 1, 1, 4)).unwrap();
        assert_eq!(img.pixel(0, 0), [255, 128, 0, 255]);
    }

    #[test]
    fn rejects_short_buffers_and_unknown_formats() {
        assert!(convert(&[0; 4], spec(wl_shm::Format::Xrgb8888, 2, 1, 8)).is_err());
        assert!(convert(&[0; 4], spec(wl_shm::Format::Rgb565, 1, 1, 4)).is_err());
    }

    #[test]
    fn prefers_native_byte_order() {
        let mut offers = [
            wl_shm::Format::Xbgr2101010,
            wl_shm::Format::Abgr8888,
            wl_shm::Format::Xrgb8888,
        ];
        offers.sort_by_key(|f| format_rank(*f));
        assert_eq!(offers[0], wl_shm::Format::Xrgb8888);
        assert_eq!(format_rank(wl_shm::Format::Rgb565), u8::MAX);
    }

    #[test]
    fn logical_geometry_falls_back_to_mode_and_scale() {
        let o = Output {
            name: Some("X".into()),
            geometry_pos: (100, 0),
            mode: (3840, 2160),
            scale: 2,
            transform: Transform::Rot90,
            ..Default::default()
        };
        assert_eq!(o.info().unwrap().logical, IRect::new(100, 0, 1080, 1920));
        let o = Output {
            logical_pos: Some((-5, 5)),
            logical_size: Some((1280, 720)),
            ..o
        };
        assert_eq!(o.info().unwrap().logical, IRect::new(-5, 5, 1280, 720));
    }
}
