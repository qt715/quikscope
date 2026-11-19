//! wlroots screencopy recording: the Wayland side of the capture helper
//! (`helper.rs` runs it in a child process) and the recording outline.
//!
//! The compositor copies each frame into one of a few buffers we own: GBM
//! DMA-BUFs read back through Vulkan (`gpu.rs`), or shared memory only when
//! the compositor offers no DMA-BUF at all, because an SHM screencopy makes
//! the compositor read the frame back on its own render path.
use crate::{
    geom::IRect,
    media::{PixelFormat, VideoSource},
    platform::{RecordTarget, ScreenRecorder},
};
use anyhow::{Context, Result, bail, ensure};
use memmap2::MmapMut;
use std::{
    fs::File,
    os::fd::{AsFd, BorrowedFd},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool,
        wl_surface,
    },
};
use wayland_protocols::{
    wp::{
        linux_dmabuf::zv1::client::{
            zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
        },
        viewporter::client::{wp_viewport, wp_viewporter},
    },
    xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1},
};
use wayland_protocols_wlr::{
    layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1},
    screencopy::v1::client::{zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1},
};

use super::gpu::{self, fourcc_name};

pub struct Recorder;
impl Recorder {
    /// Diagnostic only: every frame is a full copy, so a static desktop
    /// measures the pipeline's throughput, not how often content changes.
    pub fn open_benchmark(
        &self,
        target: &RecordTarget,
        fps: u32,
        cursor: bool,
    ) -> Result<Box<dyn VideoSource>> {
        super::helper::spawn(target, fps, cursor, true)
    }
}
impl ScreenRecorder for Recorder {
    /// Captures in a helper process (see `helper.rs` for the NVIDIA reason).
    fn open(&self, target: &RecordTarget, fps: u32, cursor: bool) -> Result<Box<dyn VideoSource>> {
        super::helper::spawn(target, fps, cursor, false)
    }
    fn outline(&self, region: IRect) -> Result<Box<dyn Send>> {
        outline(region)
    }
}

/// Buffers per recording: one the compositor copies into while up to two
/// earlier frames are read back.
const BUFFERS: usize = 3;
/// How long the compositor may take to offer buffers for a requested frame.
const OFFER_TIMEOUT: Duration = Duration::from_secs(3);

/// Size and DRM fourcc of the frames the compositor copies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Spec {
    pub format: u32,
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Display for Spec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}x{} {}",
            self.width,
            self.height,
            fourcc_name(self.format)
        )
    }
}

/// What the compositor offered for one frame.
#[derive(Clone, Copy, Debug)]
enum Offer {
    Dma(Spec),
    Shm {
        spec: Spec,
        stride: u32,
        format: wl_shm::Format,
    },
}

/// Where a buffer's pixels are read from.
enum Store {
    Vulkan { copy: gpu::VkCopy, bo: gpu::Bo },
    Mapped { map: gpu::Mapped, bo: gpu::Bo },
    Shm(Shm),
}

/// The buffers frames are copied into, shared with the readback thread.
pub(super) struct Buffers {
    pub spec: Spec,
    /// Byte order of the pixels `read` hands out.
    pub format: PixelFormat,
    dma: bool,
    stores: Vec<Store>,
}

impl Buffers {
    /// Calls `f` with the pixels of buffer `index` (rows `stride` bytes
    /// apart) once the compositor's copy into it has finished. One caller
    /// at a time per buffer.
    pub fn read<R>(&self, index: usize, f: impl FnOnce(&[u8], usize) -> R) -> Result<R> {
        match &self.stores[index] {
            Store::Vulkan { copy, bo } => {
                copy.read(bo.fd(), |pixels| f(pixels, self.spec.width as usize * 4))
            }
            Store::Mapped { map, bo } => {
                map.read(bo.offset as usize, |pixels| f(pixels, bo.stride as usize))
            }
            Store::Shm(shm) => Ok(f(&shm.map, shm.stride as usize)),
        }
    }
}

/// A finished compositor copy.
pub(super) struct Ready {
    pub buffer: usize,
    pub captured: Instant,
    /// Rows arrived bottom-up.
    pub inverted: bool,
}

/// The one screencopy request in flight.
struct Pending {
    frame: zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    buffer: Option<usize>,
    requested: Instant,
}

/// Region screencopy of one output, one request at a time.
pub(super) struct Capturer {
    client: Client,
    output: wl_output::WlOutput,
    /// Output-relative logical rectangle.
    region: IRect,
    cursor: bool,
    force_copy: bool,
    /// The next copy must not wait for damage: just opened or resumed.
    fresh: bool,
    pending: Option<Pending>,
    wl: Vec<wl_buffer::WlBuffer>,
    busy: Vec<bool>,
    buffers: Option<Arc<Buffers>>,
}

impl Drop for Capturer {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.frame.destroy();
        }
        for wl in self.wl.drain(..) {
            wl.destroy();
        }
        let _ = self.client.conn.flush();
    }
}

impl Capturer {
    pub fn connect(target: &RecordTarget, cursor: bool, force_copy: bool) -> Result<Capturer> {
        ensure!(
            target.region.w > 0 && target.region.h > 0,
            "empty recording region"
        );
        let client = Client::connect()?;
        let output = client
            .state
            .outputs
            .iter()
            .find(|o| o.name == target.output)
            .context("recording output disappeared")?;
        ensure!(
            output.transform == 0,
            "recording rotated/flipped Wayland outputs is not supported"
        );
        let wl = output.wl.clone().context("output removed")?;
        let (r, o) = (target.region, target.output_rect);
        Ok(Capturer {
            client,
            output: wl,
            region: IRect::new(r.x - o.x, r.y - o.y, r.w, r.h),
            cursor,
            force_copy,
            fresh: true,
            pending: None,
            wl: Vec::new(),
            busy: Vec::new(),
            buffers: None,
        })
    }

    /// The buffers, once the first frame's offer allocated them.
    pub fn buffers(&self) -> Option<&Arc<Buffers>> {
        self.buffers.as_ref()
    }

    /// No request is in flight and a buffer is free for the next one.
    pub fn can_request(&self) -> bool {
        self.pending.is_none() && (self.buffers.is_none() || self.busy.contains(&false))
    }

    /// Asks the compositor for the next frame (see [`progress`](Self::progress)).
    pub fn request(&mut self) {
        debug_assert!(self.can_request());
        let s = &mut self.client.state;
        s.frame += 1;
        s.dma_offer = None;
        s.shm_offer = None;
        s.offers_done = false;
        s.ready = false;
        s.failed = false;
        s.invert = false;
        let r = self.region;
        let frame = s.manager.capture_output_region(
            self.cursor as i32,
            &self.output,
            r.x,
            r.y,
            r.w,
            r.h,
            &self.client.queue.handle(),
            s.frame,
        );
        self.pending = Some(Pending {
            frame,
            buffer: None,
            requested: Instant::now(),
        });
    }

    /// Drops the request in flight, if any; the next copy will not wait
    /// for damage, so resuming starts with the current picture.
    pub fn pause(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.frame.destroy();
            if let Some(buffer) = pending.buffer {
                self.busy[buffer] = false;
            }
        }
        self.fresh = true;
    }

    /// `buffer` was read back and may be copied into again.
    pub fn release(&mut self, buffer: usize) {
        self.busy[buffer] = false;
    }

    /// Waits up to `timeout` for Wayland events (and dispatches them) or for
    /// one of `others` to become readable; reports which of `others` are.
    pub fn wait<const N: usize>(
        &mut self,
        others: [BorrowedFd<'_>; N],
        timeout: Duration,
    ) -> Result<[bool; N]> {
        self.client.wait(others, timeout)
    }

    /// Advances the request in flight after events were dispatched: picks
    /// a buffer once the compositor offered its formats, and returns the
    /// frame once it is copied. The buffer stays busy until
    /// [`release`](Self::release).
    pub fn progress(&mut self) -> Result<Option<Ready>> {
        let Some(pending) = self.pending.take() else {
            return Ok(None);
        };
        let s = &self.client.state;
        if s.failed {
            pending.frame.destroy();
            if let Some(buffer) = pending.buffer {
                self.busy[buffer] = false;
            }
            bail!("the compositor failed to copy the recorded region");
        }
        if s.ready {
            pending.frame.destroy();
            let buffer = pending.buffer.context("screencopy ready before copy")?;
            return Ok(Some(Ready {
                buffer,
                captured: s.captured,
                inverted: s.invert,
            }));
        }
        if pending.buffer.is_some() || !s.offers_done {
            if pending.buffer.is_none() && pending.requested.elapsed() > OFFER_TIMEOUT {
                pending.frame.destroy();
                bail!("the compositor offered no screencopy buffer");
            }
            self.pending = Some(pending);
            return Ok(None);
        }
        let offer = match (s.dma_offer, s.shm_offer) {
            (Some(spec), _) if s.dmabuf.is_some() => Offer::Dma(spec),
            (_, Some(shm)) => shm,
            _ => {
                pending.frame.destroy();
                bail!("the compositor offered no supported recording buffer");
            }
        };
        if let Err(e) = self.allocate(offer) {
            pending.frame.destroy();
            return Err(e);
        }
        let buffer = self
            .busy
            .iter()
            .position(|busy| !busy)
            .context("no free screencopy buffer")?;
        self.busy[buffer] = true;
        // A damage-tracking copy waits for the next change, which on a
        // static screen may never come.
        if self.fresh || self.force_copy {
            pending.frame.copy(&self.wl[buffer]);
        } else {
            pending.frame.copy_with_damage(&self.wl[buffer]);
        }
        self.fresh = false;
        self.pending = Some(Pending {
            buffer: Some(buffer),
            ..pending
        });
        Ok(None)
    }

    /// Creates the buffers for `offer` on the first frame. Later frames
    /// must match it: the size and format of a recording are fixed.
    fn allocate(&mut self, offer: Offer) -> Result<()> {
        let (spec, dma) = match offer {
            Offer::Dma(spec) => (spec, true),
            Offer::Shm { spec, .. } => (spec, false),
        };
        if let Some(buffers) = &self.buffers {
            ensure!(
                buffers.spec == spec && buffers.dma == dma,
                "the recorded output changed from {} to {} while recording",
                buffers.spec,
                spec
            );
            return Ok(());
        }
        let format = gpu::pixel_format(spec.format)
            .with_context(|| format!("recording {spec} screencopies is not supported"))?;
        let qh = self.client.queue.handle();
        let s = &self.client.state;
        let mut stores = Vec::with_capacity(BUFFERS);
        let mut wl = Vec::with_capacity(BUFFERS);
        // Partially created wl_buffers are destroyed if a later step fails.
        let result = (|| -> Result<()> {
            match offer {
                Offer::Dma(spec) => {
                    let dmabuf = s.dmabuf.as_ref().context("missing linux-dmabuf")?;
                    let gbm = gpu::Gbm::open(s.node_device)?;
                    let offered: Vec<u64> = s
                        .modifiers
                        .iter()
                        .filter(|&&(f, m)| f == spec.format && m != gpu::MOD_INVALID)
                        .map(|&(_, m)| m)
                        .collect();
                    let vulkan = gpu::Vulkan::new(gbm.rdev).and_then(|vk| {
                        let modifiers = vk.importable(spec.format, &offered);
                        ensure!(
                            !modifiers.is_empty(),
                            "Vulkan on {} cannot import any {} modifier the compositor offers ({offered:#x?})",
                            vk.description,
                            fourcc_name(spec.format)
                        );
                        Ok((vk, modifiers))
                    });
                    match vulkan {
                        Ok((vk, modifiers)) => {
                            for _ in 0..BUFFERS {
                                let bo =
                                    gbm.allocate(spec.width, spec.height, spec.format, &modifiers)?;
                                let copy = gpu::VkCopy::new(
                                    &vk,
                                    &bo,
                                    spec.width,
                                    spec.height,
                                    spec.format,
                                )?;
                                wl.push(dmabuf_buffer(dmabuf, &bo, spec, &qh));
                                stores.push(Store::Vulkan { copy, bo });
                            }
                            if let Some(Store::Vulkan { bo, .. }) = stores.first() {
                                log::info!(
                                    "Wayland recording: {spec} DMA-BUFs on {}, modifier {:#x}, Vulkan readback on {}",
                                    gbm.node,
                                    bo.modifier,
                                    vk.description
                                );
                            }
                        }
                        Err(e) if offered.contains(&gpu::MOD_LINEAR) => {
                            log::warn!(
                                "Wayland recording: no Vulkan readback ({e:#}); reading linear DMA-BUFs with the CPU instead, which is slow on discrete GPUs"
                            );
                            for _ in 0..BUFFERS {
                                let bo = gbm.allocate(spec.width, spec.height, spec.format, &[])?;
                                let map = gpu::Mapped::new(&bo)?;
                                wl.push(dmabuf_buffer(dmabuf, &bo, spec, &qh));
                                stores.push(Store::Mapped { map, bo });
                            }
                        }
                        Err(e) => {
                            return Err(e.context(
                                "recording needs Vulkan to read the compositor's DMA-BUFs",
                            ));
                        }
                    }
                }
                Offer::Shm {
                    spec,
                    stride,
                    format,
                } => {
                    log::warn!(
                        "Wayland recording: the compositor offers no DMA-BUF screencopy; \
                         shared-memory copies make it read every frame back itself, which can make games stutter"
                    );
                    ensure!(
                        stride >= spec.width * 4,
                        "invalid screencopy stride {stride}"
                    );
                    for _ in 0..BUFFERS {
                        let (buffer, shm) =
                            allocate_shm(&s.shm, spec.width, spec.height, stride, format, &qh)?;
                        wl.push(buffer);
                        stores.push(Store::Shm(shm));
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            for buffer in wl {
                buffer.destroy();
            }
            return Err(e);
        }
        self.busy = vec![false; wl.len()];
        self.wl = wl;
        self.buffers = Some(Arc::new(Buffers {
            spec,
            format,
            dma,
            stores,
        }));
        Ok(())
    }
}

fn dmabuf_buffer(
    dmabuf: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    bo: &gpu::Bo,
    spec: Spec,
    qh: &QueueHandle<State>,
) -> wl_buffer::WlBuffer {
    let params = dmabuf.create_params(qh, ());
    params.add(
        bo.fd(),
        0,
        bo.offset,
        bo.stride,
        (bo.modifier >> 32) as u32,
        bo.modifier as u32,
    );
    let buffer = params.create_immed(
        spec.width as i32,
        spec.height as i32,
        spec.format,
        zwp_linux_buffer_params_v1::Flags::empty(),
        qh,
        (),
    );
    params.destroy();
    buffer
}

/// A shared-memory buffer (its `wl_buffer` is kept by whoever attaches it).
struct Shm {
    map: MmapMut,
    _file: File,
    stride: u32,
}

#[derive(Default)]
struct Output {
    wl: Option<wl_output::WlOutput>,
    name: String,
    rect: IRect,
    transform: u32,
    scale: i32,
    /// The current mode in physical pixels, before the transform.
    mode: (i32, i32),
}

impl Output {
    /// Physical pixels per logical pixel along x and y: the current mode,
    /// turned like the output, over the logical size. Unlike the integer
    /// `scale`, this is the fractional scale the compositor renders at.
    fn pixel_scale(&self) -> (f64, f64) {
        // Odd wl_output transforms turn the output by 90 or 270 degrees.
        let (w, h) = if self.transform % 2 == 1 {
            (self.mode.1, self.mode.0)
        } else {
            self.mode
        };
        if w <= 0 || h <= 0 || self.rect.w <= 0 || self.rect.h <= 0 {
            let scale = f64::from(self.scale.max(1));
            return (scale, scale);
        }
        (
            f64::from(w) / f64::from(self.rect.w),
            f64::from(h) / f64::from(self.rect.h),
        )
    }
}

/// The physical pixels the logical span `start..start + len` covers at
/// `scale`, rounding both edges as compositors round surface boxes.
fn physical_span(start: i32, len: i32, scale: f64) -> i32 {
    let edge = |at: i32| (f64::from(at) * scale).round() as i32;
    edge(start + len) - edge(start)
}
struct State {
    outputs: Vec<Output>,
    node_device: Option<u64>,
    manager: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    shm: wl_shm::WlShm,
    /// Serial of the screencopy frame the fields below describe; events of
    /// earlier (destroyed) frames are ignored.
    frame: u64,
    dma_offer: Option<Spec>,
    shm_offer: Option<Offer>,
    offers_done: bool,
    ready: bool,
    failed: bool,
    invert: bool,
    captured: Instant,
    configured: Vec<bool>,
    closed: bool,
    format_table: Vec<(u32, u64)>,
    modifiers: Vec<(u32, u64)>,
}
struct Client {
    conn: Connection,
    globals: GlobalList,
    queue: EventQueue<State>,
    state: State,
}
impl Client {
    fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env()?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let manager = globals
            .bind(&qh, 3..=3, ())
            .context("recording requires wlr-screencopy v3")?;
        let shm = globals.bind(&qh, 1..=1, ())?;
        let dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1> =
            globals.bind(&qh, 3..=4, ()).ok();
        let xdg: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1> =
            globals.bind(&qh, 2..=3, ()).ok();
        let mut state = State {
            outputs: Vec::new(),
            node_device: None,
            manager,
            dmabuf,
            shm,
            frame: 0,
            dma_offer: None,
            shm_offer: None,
            offers_done: false,
            ready: false,
            failed: false,
            invert: false,
            captured: Instant::now(),
            configured: Vec::new(),
            closed: false,
            format_table: Vec::new(),
            modifiers: Vec::new(),
        };
        if let Some(d) = &state.dmabuf
            && d.version() >= 4
        {
            d.get_default_feedback(&qh, ());
        }
        globals.contents().with_list(|list| {
            for g in list.iter().filter(|g| g.interface == "wl_output") {
                let i = state.outputs.len();
                let wl = globals.registry().bind::<wl_output::WlOutput, _, _>(
                    g.name,
                    g.version.min(4),
                    &qh,
                    i,
                );
                if let Some(xdg) = &xdg {
                    xdg.get_xdg_output(&wl, &qh, i);
                }
                state.outputs.push(Output {
                    wl: Some(wl),
                    scale: 1,
                    ..Default::default()
                });
            }
        });
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        Ok(Self {
            conn,
            globals,
            queue,
            state,
        })
    }
    fn dispatch(&mut self, timeout: Duration) -> Result<()> {
        self.wait([], timeout).map(|_| ())
    }
    fn wait<const N: usize>(
        &mut self,
        others: [BorrowedFd<'_>; N],
        timeout: Duration,
    ) -> Result<[bool; N]> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        self.queue.flush()?;
        // Events already queued are handled now; then only a quick look at
        // the other descriptors.
        let dispatched = self.queue.dispatch_pending(&mut self.state)? > 0;
        let timeout = Timespec::try_from(if dispatched { Duration::ZERO } else { timeout })?;
        let mut ready = [false; N];
        let guard = self.conn.prepare_read();
        let wayland = guard.as_ref().map(|g| g.connection_fd());
        let mut fds: Vec<PollFd<'_>> = wayland
            .iter()
            .chain(others.iter())
            .map(|fd| PollFd::new(fd, PollFlags::IN))
            .collect();
        match poll(&mut fds, Some(&timeout)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
        let events: Vec<bool> = fds.iter().map(|fd| !fd.revents().is_empty()).collect();
        drop(fds);
        let offset = usize::from(wayland.is_some());
        for (i, ready) in ready.iter_mut().enumerate() {
            *ready = events[offset + i];
        }
        if let Some(guard) = guard
            && events[0]
        {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(ready)
    }
}

fn allocate_shm(
    shm: &wl_shm::WlShm,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    qh: &QueueHandle<State>,
) -> Result<(wl_buffer::WlBuffer, Shm)> {
    let fd = rustix::fs::memfd_create("quikscope-screencopy", rustix::fs::MemfdFlags::CLOEXEC)?;
    let len = u64::from(stride) * u64::from(height);
    rustix::fs::ftruncate(&fd, len)?;
    let file = File::from(fd);
    let map = unsafe { MmapMut::map_mut(&file)? };
    let pool = shm.create_pool(file.as_fd(), i32::try_from(len)?, qh, ());
    let wl = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        qh,
        (),
    );
    pool.destroy();
    Ok((
        wl,
        Shm {
            map,
            _file: file,
            stride,
        },
    ))
}

/// The DRM fourcc of a wl_shm format (which uses its own 0 and 1 for the
/// two mandatory formats and fourccs for the rest).
fn shm_fourcc(format: wl_shm::Format) -> u32 {
    match format {
        wl_shm::Format::Argb8888 => gpu::ARGB8888,
        wl_shm::Format::Xrgb8888 => gpu::XRGB8888,
        other => other as u32,
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        s: &mut Self,
        _: &wl_output::WlOutput,
        e: wl_output::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let o = &mut s.outputs[*i];
        match e {
            wl_output::Event::Name { name } => o.name = name,
            wl_output::Event::Geometry {
                x,
                y,
                transform: WEnum::Value(t),
                ..
            } => {
                o.rect.x = x;
                o.rect.y = y;
                o.transform = t as u32;
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                o.rect.w = width;
                o.rect.h = height;
                if let WEnum::Value(flags) = flags
                    && flags.contains(wl_output::Mode::Current)
                {
                    o.mode = (width, height);
                }
            }
            wl_output::Event::Scale { factor } => o.scale = factor,
            _ => {}
        }
    }
}
impl Dispatch<zxdg_output_v1::ZxdgOutputV1, usize> for State {
    fn event(
        s: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        e: zxdg_output_v1::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let o = &mut s.outputs[*i];
        match e {
            zxdg_output_v1::Event::Name { name } => o.name = name,
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                o.rect.x = x;
                o.rect.y = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                o.rect.w = width;
                o.rect.h = height;
            }
            _ => {}
        }
    }
}
impl Dispatch<zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        e: zwp_linux_dmabuf_feedback_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_linux_dmabuf_feedback_v1::Event;
        match e {
            Event::MainDevice { device } if device.len() == 8 => {
                s.node_device = Some(u64::from_ne_bytes(device.try_into().unwrap()));
            }
            Event::FormatTable { fd, size } => {
                let file = File::from(fd);
                if let Ok(map) =
                    unsafe { memmap2::MmapOptions::new().len(size as usize).map(&file) }
                {
                    s.format_table = map
                        .as_chunks::<16>()
                        .0
                        .iter()
                        .map(|entry| {
                            (
                                u32::from_ne_bytes(entry[..4].try_into().unwrap()),
                                u64::from_ne_bytes(entry[8..16].try_into().unwrap()),
                            )
                        })
                        .collect();
                }
            }
            Event::TrancheFormats { indices } => {
                for &bytes in indices.as_chunks::<2>().0 {
                    if let Some(entry) = s.format_table.get(u16::from_ne_bytes(bytes) as usize)
                        && !s.modifiers.contains(entry)
                    {
                        s.modifiers.push(*entry);
                    }
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        e: zwp_linux_dmabuf_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_linux_dmabuf_v1::Event::Modifier {
            format,
            modifier_hi,
            modifier_lo,
        } = e
        {
            s.modifiers.push((
                format,
                (u64::from(modifier_hi) << 32) | u64::from(modifier_lo),
            ));
        }
    }
}
impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, u64> for State {
    fn event(
        s: &mut Self,
        _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        e: zwlr_screencopy_frame_v1::Event,
        frame: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        if *frame != s.frame {
            return;
        }
        match e {
            Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                s.dma_offer = Some(Spec {
                    format,
                    width,
                    height,
                })
            }
            Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                s.shm_offer = Some(Offer::Shm {
                    spec: Spec {
                        format: shm_fourcc(format),
                        width,
                        height,
                    },
                    stride,
                    format,
                })
            }
            Event::BufferDone => s.offers_done = true,
            Event::Flags {
                flags: WEnum::Value(f),
            } => s.invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert),
            // Receipt time shares Instant's clock across backends and is safe even
            // when the compositor's presentation clock is not CLOCK_MONOTONIC.
            Event::Ready { .. } => {
                s.ready = true;
                s.captured = Instant::now();
            }
            Event::Failed => s.failed = true,
            _ => {}
        }
    }
}
wayland_client::delegate_noop!(State: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(State: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(State: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(State: ignore wl_surface::WlSurface);
wayland_client::delegate_noop!(State: ignore wl_region::WlRegion);
wayland_client::delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
wayland_client::delegate_noop!(State: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
wayland_client::delegate_noop!(State: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
wayland_client::delegate_noop!(State: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);
wayland_client::delegate_noop!(State: ignore wp_viewporter::WpViewporter);
wayland_client::delegate_noop!(State: ignore wp_viewport::WpViewport);

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, usize> for State {
    fn event(
        s: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        e: zwlr_layer_surface_v1::Event,
        i: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match e {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                layer.ack_configure(serial);
                s.configured[*i] = true;
            }
            zwlr_layer_surface_v1::Event::Closed => s.closed = true,
            _ => {}
        }
    }
}
struct Outline {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Outline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The outline's accent colour as ARGB8888 bytes in memory (B, G, R, A).
const OUTLINE_COLOUR: [u8; 4] = [0xf2, 0x8c, 0x8f, 255];
/// Outline thickness in logical pixels.
const OUTLINE_WIDTH: i32 = 2;

/// The outline strips around `region` that fall on `output`, clipped to it
/// and relative to its top-left corner (logical pixels).
fn outline_strips(region: IRect, output: IRect) -> Vec<IRect> {
    let (r, w) = (region, OUTLINE_WIDTH);
    [
        IRect::new(r.x - w, r.y - w, r.w + 2 * w, w),
        IRect::new(r.x - w, r.y + r.h, r.w + 2 * w, w),
        IRect::new(r.x - w, r.y, w, r.h),
        IRect::new(r.x + r.w, r.y, w, r.h),
    ]
    .into_iter()
    .filter_map(|strip| strip.intersect(&output))
    .map(|s| IRect::new(s.x - output.x, s.y - output.y, s.w, s.h))
    .collect()
}

/// A `width`×`height` SHM buffer of the outline colour.
fn solid_buffer(
    shm: &wl_shm::WlShm,
    width: u32,
    height: u32,
    qh: &QueueHandle<State>,
) -> Result<(wl_buffer::WlBuffer, Shm)> {
    let (wl, mut shm) = allocate_shm(shm, width, height, width * 4, wl_shm::Format::Argb8888, qh)?;
    shm.map.as_chunks_mut::<4>().0.fill(OUTLINE_COLOUR);
    Ok((wl, shm))
}

/// One strip of the outline: a layer surface the strip's size.
struct Strip {
    surface: wl_surface::WlSurface,
    layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    viewport: Option<wp_viewport::WpViewport>,
    buffer: (wl_buffer::WlBuffer, Shm),
}

/// Four strip-sized overlay surfaces rather than one transparent surface
/// over the whole output: the compositor blends a few thousand pixels, not
/// a full-screen texture, per frame. Overlay keeps the outline visible over
/// fullscreen windows; on Hyprland any mapped overlay surface still costs
/// direct scanout of that output while it is shown.
fn outline(region: IRect) -> Result<Box<dyn Send>> {
    let (tx, rx) = mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let thread = std::thread::Builder::new()
        .name("quikscope-wlr-outline".into())
        .spawn(move || {
            let result = (|| -> Result<()> {
                let mut c = Client::connect()?;
                // An output-sized region has no outside on that output; drawing on
                // its neighbours would misrepresent which display is being recorded.
                if c.state.outputs.iter().any(|o| o.rect == region) {
                    let _ = tx.send(Ok(()));
                    return Ok(());
                }
                let qh = c.queue.handle();
                let compositor: wl_compositor::WlCompositor = c.globals.bind(&qh, 1..=4, ())?;
                let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 =
                    c.globals.bind(&qh, 1..=4, ())?;
                let viewporter: Option<wp_viewporter::WpViewporter> =
                    c.globals.bind(&qh, 1..=1, ()).ok();
                let mut strips = Vec::new();
                for o in &c.state.outputs {
                    for strip in outline_strips(region, o.rect) {
                        let surface = compositor.create_surface(&qh, ());
                        let empty = compositor.create_region(&qh, ());
                        surface.set_input_region(Some(&empty));
                        empty.destroy();
                        let layer = shell.get_layer_surface(
                            &surface,
                            o.wl.as_ref(),
                            zwlr_layer_shell_v1::Layer::Overlay,
                            "quikscope-outline".into(),
                            &qh,
                            strips.len(),
                        );
                        use zwlr_layer_surface_v1::Anchor;
                        layer.set_anchor(Anchor::Top | Anchor::Left);
                        layer.set_margin(strip.y, 0, 0, strip.x);
                        layer.set_size(strip.w as u32, strip.h as u32);
                        // Placed against the output's edges, not moved aside
                        // for panels' exclusive zones.
                        layer.set_exclusive_zone(-1);
                        layer.set_keyboard_interactivity(
                            zwlr_layer_surface_v1::KeyboardInteractivity::None,
                        );
                        // A buffer of the strip's physical pixels, sized to
                        // the viewport's destination: drawn 1:1 at any
                        // (fractional) scale, so the compositor never
                        // filters a stretched buffer into the recorded
                        // pixels next to the strip.
                        let (viewport, width, height) = match &viewporter {
                            Some(viewporter) => {
                                let viewport = viewporter.get_viewport(&surface, &qh, ());
                                viewport.set_destination(strip.w, strip.h);
                                let (sx, sy) = o.pixel_scale();
                                (
                                    Some(viewport),
                                    physical_span(strip.x, strip.w, sx),
                                    physical_span(strip.y, strip.h, sy),
                                )
                            }
                            None => {
                                // The output's integer scale, which a
                                // fractional one only reduces.
                                let scale = if surface.version() >= 3 {
                                    o.scale.max(1)
                                } else {
                                    1
                                };
                                if scale > 1 {
                                    surface.set_buffer_scale(scale);
                                }
                                (None, strip.w * scale, strip.h * scale)
                            }
                        };
                        let buffer = solid_buffer(
                            &c.state.shm,
                            width.max(1) as u32,
                            height.max(1) as u32,
                            &qh,
                        )?;
                        surface.commit();
                        strips.push(Strip {
                            surface,
                            layer,
                            viewport,
                            buffer,
                        });
                    }
                }
                c.state.configured = vec![false; strips.len()];
                let deadline = Instant::now() + Duration::from_secs(3);
                while c.state.configured.iter().any(|v| !v) {
                    ensure!(Instant::now() < deadline, "outline configure timed out");
                    c.dispatch(Duration::from_millis(50))?;
                }
                for strip in &strips {
                    strip.surface.attach(Some(&strip.buffer.0), 0, 0);
                    strip.surface.damage(0, 0, i32::MAX, i32::MAX);
                    strip.surface.commit();
                }
                c.queue.roundtrip(&mut c.state)?;
                let _ = tx.send(Ok(()));
                while !stopping.load(Ordering::Relaxed) && !c.state.closed {
                    c.dispatch(Duration::from_millis(50))?;
                }
                for strip in strips {
                    strip.layer.destroy();
                    if let Some(viewport) = strip.viewport {
                        viewport.destroy();
                    }
                    strip.surface.destroy();
                    strip.buffer.0.destroy();
                }
                c.queue.roundtrip(&mut c.state)?;
                Ok(())
            })();
            if let Err(e) = result {
                let _ = tx.send(Err(e));
            }
        })?;
    rx.recv().context("outline thread stopped")??;
    Ok(Box::new(Outline {
        stop,
        thread: Some(thread),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shm_formats_map_to_fourccs() {
        assert_eq!(shm_fourcc(wl_shm::Format::Xrgb8888), gpu::XRGB8888);
        assert_eq!(shm_fourcc(wl_shm::Format::Argb8888), gpu::ARGB8888);
        assert_eq!(shm_fourcc(wl_shm::Format::Xbgr8888), gpu::XBGR8888);
        assert_eq!(shm_fourcc(wl_shm::Format::Abgr8888), gpu::ABGR8888);
        // Red and blue swap between the two families.
        assert_eq!(
            gpu::pixel_format(shm_fourcc(wl_shm::Format::Xrgb8888)),
            Some(PixelFormat::Bgrx)
        );
        assert_eq!(
            gpu::pixel_format(shm_fourcc(wl_shm::Format::Xbgr8888)),
            Some(PixelFormat::Rgbx)
        );
        let spec = Spec {
            format: gpu::XBGR8888,
            width: 2560,
            height: 1440,
        };
        assert_eq!(spec.to_string(), "2560x1440 XB24");
    }

    #[test]
    fn outline_strips_surround_the_region_on_its_output() {
        let output = IRect::new(1920, 0, 2560, 1440);
        // Inside: four strips, output-relative, 2 px outside the region.
        assert_eq!(
            outline_strips(IRect::new(2020, 100, 400, 300), output),
            [
                IRect::new(98, 98, 404, 2),
                IRect::new(98, 400, 404, 2),
                IRect::new(98, 100, 2, 300),
                IRect::new(500, 100, 2, 300),
            ]
        );
        // Touching the output's left and top edges: those strips fall on
        // the neighbours, the others are clipped to this output.
        assert_eq!(
            outline_strips(IRect::new(1920, 0, 400, 300), output),
            [IRect::new(0, 300, 402, 2), IRect::new(400, 0, 2, 300)]
        );
        // The left neighbour gets the corner of the bottom strip and the
        // whole left strip.
        assert_eq!(
            outline_strips(IRect::new(1920, 0, 400, 300), IRect::new(0, 0, 1920, 1080)),
            [IRect::new(1918, 300, 2, 2), IRect::new(1918, 0, 2, 300)]
        );
        // Far from the output: nothing.
        assert!(outline_strips(IRect::new(0, 0, 100, 100), output).is_empty());
    }

    #[test]
    fn strip_buffers_cover_their_physical_pixels() {
        // 2560x1440 at 1.5: 1707x960 logical (rounded by the compositor).
        let output = Output {
            rect: IRect::new(0, 0, 1707, 960),
            mode: (2560, 1440),
            scale: 2,
            ..Default::default()
        };
        let (sx, sy) = output.pixel_scale();
        assert!((sx - 1.5).abs() < 1e-3 && (sy - 1.5).abs() < 1e-3);
        // A 2 px strip at 78 covers physical rows 117..120.
        assert_eq!(physical_span(78, 2, sy), 3);
        assert_eq!(physical_span(98, 304, sx), 456);
        // At 1.25 the same strip covers 2 or 3 physical pixels depending
        // on where it starts; the buffer follows the compositor's rounding.
        assert_eq!(physical_span(0, 2, 1.25), 3);
        assert_eq!(physical_span(2, 2, 1.25), 2);
        assert_eq!(physical_span(10, 2, 1.0), 2);
        // A portrait output: the mode is turned.
        let portrait = Output {
            rect: IRect::new(0, 0, 720, 1280),
            mode: (2560, 1440),
            transform: 1,
            ..Default::default()
        };
        assert_eq!(portrait.pixel_scale(), (2.0, 2.0));
        // No mode yet: the integer scale.
        let unknown = Output {
            scale: 2,
            ..Default::default()
        };
        assert_eq!(unknown.pixel_scale(), (2.0, 2.0));
    }
}
