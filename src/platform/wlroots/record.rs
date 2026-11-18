//! Region screencopy with GBM DMA-BUFs and off-compositor Vulkan detiling.
//! SHM is allowed only without a DMA-BUF offer: compositor readbacks cause judder.
use crate::{
    geom::IRect,
    media::{Frame, VideoSource},
    platform::{RecordTarget, ScreenRecorder},
};
use anyhow::{Context, Result, bail, ensure};
use ffmpeg_next::{self as ffmpeg, ffi};
use memmap2::MmapMut;
use std::{
    fs::File,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool,
        wl_surface,
    },
};
use wayland_protocols::{
    wp::linux_dmabuf::zv1::client::{
        zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
    },
    xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1},
};
use wayland_protocols_wlr::{
    layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1},
    screencopy::v1::client::{zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1},
};

pub struct Recorder;
impl Recorder {
    /// Diagnostic only: force output damage so a static desktop measures the
    /// pipeline's throughput, not how often the user's content changes.
    pub fn open_benchmark(
        &self,
        target: &RecordTarget,
        fps: u32,
        cursor: bool,
    ) -> Result<Box<dyn VideoSource>> {
        open_local(target, fps, cursor, true)
    }
}
struct Source {
    rx: mpsc::Receiver<Result<Frame>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    size: (u32, u32),
}
impl VideoSource for Source {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        match self.rx.recv_timeout(timeout) {
            Ok(f) => f.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(_) => bail!("Wayland recording source stopped"),
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
impl ScreenRecorder for Recorder {
    /// Captures in a helper process (see `helper.rs` for the NVIDIA reason).
    fn open(&self, target: &RecordTarget, fps: u32, cursor: bool) -> Result<Box<dyn VideoSource>> {
        super::helper::open(target, fps, cursor)
    }
    fn outline(&self, region: IRect) -> Result<Box<dyn Send>> {
        outline(region)
    }
}
/// Captures in this process. Only for processes without a GPUI renderer:
/// the capture helper and diagnostics.
pub(super) fn open_local(
    target: &RecordTarget,
    fps: u32,
    cursor: bool,
    force_copy: bool,
) -> Result<Box<dyn VideoSource>> {
    ensure!(fps > 0 && fps <= 240, "recording fps must be in 1..=240");
    ensure!(
        target.region.w > 0 && target.region.h > 0,
        "empty recording region"
    );
    let target = target.clone();
    let (tx, rx) = mpsc::sync_channel(4);
    let (ready, started) = mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let thread = std::thread::Builder::new().name("quikscope-wlr-record".into()).spawn(move || {
            let setup = (|| -> Result<(Client, Frame)> {
                let mut client = Client::connect()?;
                let output = client.state.outputs.iter().position(|o| o.name == target.output).context("recording output disappeared")?;
                ensure!(client.state.outputs[output].transform == 0, "recording rotated/flipped Wayland outputs is not supported");
                client.output = output; client.target = Some(target); client.cursor = cursor; client.force_copy=force_copy;
                let first = client.capture(&stopping)?.context("recording stopped during startup")?;
                Ok((client, first))
            })();
            let (mut client, first) = match setup { Ok(v) => v, Err(e) => { let _ = ready.send(Err(e)); return; } };
            let _ = ready.send(Ok((first.width(), first.height())));
            if tx.send(Ok(first)).is_err() { return; }
            let period = Duration::from_secs_f64(1.0/f64::from(fps));
            let mut next = Instant::now() + period;
            let mut count=0usize;let mut capture_cpu=0.0;
            while !stopping.load(Ordering::Relaxed) {
                std::thread::sleep(next.saturating_duration_since(Instant::now()));
                let cpu_started=thread_cpu();
                let result = client.capture(&stopping);
                capture_cpu+=thread_cpu()-cpu_started;
                match result {
                    Ok(Some(frame)) => { count+=1; if matches!(tx.try_send(Ok(frame)), Err(mpsc::TrySendError::Disconnected(_))) { break; } }
                    Ok(None) => {}
                    Err(e) => { let _ = tx.try_send(Err(e)); break; }
                }
                next = (next + period).max(Instant::now());
            }
            log::info!("Wayland recording: capture-thread CPU/frame={:.3}ms, frames={} (startup and still reference excluded)",capture_cpu*1000.0/count.max(1)as f64,count);
        })?;
    let size = started
        .recv()
        .context("Wayland recorder stopped during startup")??;
    Ok(Box::new(Source {
        rx,
        stop,
        thread: Some(thread),
        size,
    }))
}

fn thread_cpu() -> f64 {
    unsafe {
        let mut ts: libc::timespec = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
        ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
    }
}
// Keep the GBM device alive until the final encoder reference to its BO dies.
#[link(name = "gbm")]
unsafe extern "C" {
    fn gbm_create_device(fd: i32) -> *mut libc::c_void;
    fn gbm_device_destroy(device: *mut libc::c_void);
    fn gbm_bo_create_with_modifiers2(
        device: *mut libc::c_void,
        width: u32,
        height: u32,
        format: u32,
        modifiers: *const u64,
        count: u32,
        flags: u32,
    ) -> *mut libc::c_void;
    fn gbm_bo_create(
        device: *mut libc::c_void,
        width: u32,
        height: u32,
        format: u32,
        flags: u32,
    ) -> *mut libc::c_void;
    fn gbm_bo_destroy(bo: *mut libc::c_void);
    fn gbm_bo_get_fd(bo: *mut libc::c_void) -> i32;
    fn gbm_bo_get_stride(bo: *mut libc::c_void) -> u32;
    fn gbm_bo_get_offset(bo: *mut libc::c_void, plane: i32) -> u32;
    fn gbm_bo_get_modifier(bo: *mut libc::c_void) -> u64;
}
struct Device {
    ptr: *mut libc::c_void,
    _file: File,
    node: String,
}
unsafe impl Send for Device {}
unsafe impl Sync for Device {}
impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            gbm_device_destroy(self.ptr);
        }
    }
}
struct Dma {
    bo: *mut libc::c_void,
    fd: OwnedFd,
    size: usize,
    stride: u32,
    offset: u32,
    modifier: u64,
    _device: Arc<Device>,
    busy: AtomicBool,
}
// Allocations are immutable; only the compositor writes pixels, and the atomic
// lease prevents it from writing while an AVFrame is held by an encoder.
unsafe impl Send for Dma {}
unsafe impl Sync for Dma {}
impl Drop for Dma {
    fn drop(&mut self) {
        unsafe {
            gbm_bo_destroy(self.bo);
        }
    }
}
struct Slot {
    allocation: Arc<Dma>,
    wl: wl_buffer::WlBuffer,
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.wl.destroy();
    }
}
struct Lease {
    descriptor: ffi::AVDRMFrameDescriptor,
    allocation: Arc<Dma>,
    return_to_pool: bool,
}
unsafe extern "C" fn release(opaque: *mut libc::c_void, _: *mut u8) {
    let lease = unsafe { Box::from_raw(opaque.cast::<Lease>()) };
    if lease.return_to_pool {
        lease.allocation.busy.store(false, Ordering::Release);
    }
}
struct Hw(*mut ffi::AVBufferRef);
impl Drop for Hw {
    fn drop(&mut self) {
        unsafe {
            ffi::av_buffer_unref(&mut self.0);
        }
    }
}
#[derive(Clone, Copy, PartialEq, Debug)]
struct Spec {
    format: u32,
    width: u32,
    height: u32,
}
struct Shm {
    wl: wl_buffer::WlBuffer,
    map: MmapMut,
    _file: File,
    stride: u32,
}
impl Drop for Shm {
    fn drop(&mut self) {
        self.wl.destroy();
    }
}
#[derive(Default)]
struct Output {
    wl: Option<wl_output::WlOutput>,
    name: String,
    rect: IRect,
    transform: u32,
    scale: i32,
}
struct State {
    outputs: Vec<Output>,
    node_device: Option<u64>,
    manager: zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    shm: wl_shm::WlShm,
    dma_offer: Option<Spec>,
    shm_offer: Option<(Spec, u32)>,
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
    queue: EventQueue<State>,
    state: State,
    output: usize,
    target: Option<RecordTarget>,
    cursor: bool,
    force_copy: bool,
    device: Option<Arc<Device>>,
    slots: Vec<Slot>,
    spec: Option<Spec>,
    hw: Option<Hw>,
    vulkan: Option<Hw>,
    shm: Option<Shm>,
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
            queue,
            state,
            output: 0,
            target: None,
            cursor: false,
            force_copy: false,
            device: None,
            slots: Vec::new(),
            spec: None,
            hw: None,
            vulkan: None,
            shm: None,
        })
    }
    fn dispatch(&mut self, timeout: Duration) -> Result<()> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        self.queue.flush()?;
        if self.queue.dispatch_pending(&mut self.state)? > 0 {
            return Ok(());
        }
        if let Some(guard) = self.conn.prepare_read() {
            let fd = guard.connection_fd();
            let mut fds = [PollFd::new(&fd, PollFlags::IN)];
            if poll(&mut fds, Some(&Timespec::try_from(timeout)?))? > 0 {
                guard.read()?;
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }
    fn capture(&mut self, stop: &AtomicBool) -> Result<Option<Frame>> {
        self.state.dma_offer = None;
        self.state.shm_offer = None;
        self.state.offers_done = false;
        self.state.ready = false;
        self.state.failed = false;
        self.state.invert = false;
        let target = self.target.as_ref().context("missing recording target")?;
        let r = target.region;
        let o = target.output_rect;
        let proxy = self.state.manager.capture_output_region(
            self.cursor as i32,
            self.state.outputs[self.output]
                .wl
                .as_ref()
                .context("output removed")?,
            r.x - o.x,
            r.y - o.y,
            r.w,
            r.h,
            &self.queue.handle(),
            (),
        );
        let result = self.capture_inner(&proxy, stop);
        proxy.destroy();
        result
    }
    fn capture_inner(
        &mut self,
        proxy: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        stop: &AtomicBool,
    ) -> Result<Option<Frame>> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !self.state.offers_done && !self.state.failed {
            if stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            ensure!(
                Instant::now() < deadline,
                "screencopy buffer offer timed out"
            );
            self.dispatch(Duration::from_millis(50))?;
        }
        ensure!(!self.state.failed, "compositor rejected recording region");
        let mut selected = None;
        if let Some(spec) = self.state.dma_offer {
            ensure!(
                self.state.dmabuf.is_some(),
                "DMA-BUF offered without linux-dmabuf global"
            );
            if self.spec != Some(spec) {
                self.slots.clear();
                self.hw = None;
                self.vulkan = None;
                self.spec = Some(spec);
            }
            if self.device.is_none() {
                self.device = Some(Arc::new(open_device(self.state.node_device)?));
            }
            if self.hw.is_none() {
                self.hw = Some(make_hw(self.device.as_ref().unwrap(), spec)?);
            }
            loop {
                if let Some(i) = self.slots.iter().position(|s| {
                    s.allocation
                        .busy
                        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                }) {
                    selected = Some(i);
                    break;
                }
                if self.slots.len() < 4 {
                    let slot = self.allocate(spec)?;
                    slot.allocation.busy.store(true, Ordering::Release);
                    self.slots.push(slot);
                    selected = Some(self.slots.len() - 1);
                    break;
                }
                if stop.load(Ordering::Relaxed) {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            let buffer = &self.slots[selected.unwrap()].wl;
            if self.force_copy {
                proxy.copy(buffer);
            } else {
                proxy.copy_with_damage(buffer);
            }
        } else {
            let (spec, stride) = self
                .state
                .shm_offer
                .context("compositor offered no supported recording buffer")?;
            ensure!(
                matches!(spec.format, 0 | 1),
                "SHM recording requires ARGB8888/XRGB8888"
            );
            if self.spec != Some(spec) || self.shm.is_none() {
                log::warn!(
                    "Wayland recording: compositor offers no screencopy DMA-BUF; SHM readback can cause game judder"
                );
                self.spec = Some(spec);
                self.shm = Some(allocate_shm(
                    &self.state.shm,
                    spec.width,
                    spec.height,
                    stride,
                    if spec.format == 1 {
                        wl_shm::Format::Xrgb8888
                    } else {
                        wl_shm::Format::Argb8888
                    },
                    &self.queue.handle(),
                )?);
            }
            let buffer = &self.shm.as_ref().unwrap().wl;
            if self.force_copy {
                proxy.copy(buffer);
            } else {
                proxy.copy_with_damage(buffer);
            }
        }
        // copy_with_damage may deliberately wait forever on a static desktop;
        // bounded dispatch keeps dropping the source responsive in that case.
        while !self.state.ready && !self.state.failed {
            if stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            self.dispatch(Duration::from_millis(50))?;
        }
        if self.state.failed || self.state.invert {
            if let Some(i) = selected {
                self.slots[i]
                    .allocation
                    .busy
                    .store(false, Ordering::Release);
            }
            ensure!(
                !self.state.failed,
                "compositor failed DMA-BUF screencopy (not falling back to SHM)"
            );
            bail!("inverted Wayland recording buffers are not supported");
        }
        let spec = self.spec.unwrap();
        if let Some(i) = selected {
            let allocation = self.slots[i].allocation.clone();
            let tiled = allocation.modifier != 0;
            let frame = wrap(
                allocation.clone(),
                spec,
                self.hw.as_ref().unwrap(),
                self.state.captured,
                !tiled,
            )?;
            if tiled {
                if self.vulkan.is_none() {
                    self.vulkan = Some(make_vulkan(self.hw.as_ref().unwrap())?);
                }
                let result = detile(frame, self.vulkan.as_ref().unwrap());
                // FFmpeg's Vulkan execution pools retain completed mapping
                // references until their next submission. Waiting for those
                // callbacks deadlocks a bounded BO pool. CPU transfer completion
                // already proves the GPU is done reading this tiled allocation.
                if result.is_ok() {
                    allocation.busy.store(false, Ordering::Release);
                }
                return result.map(Some);
            }
            return Ok(Some(frame));
        }
        let shm = self.shm.as_ref().unwrap();
        Ok(Some(Frame::from_bgra(
            spec.width,
            spec.height,
            shm.stride as usize,
            &shm.map,
            true,
            self.state.captured,
        )))
    }
    fn allocate(&mut self, spec: Spec) -> Result<Slot> {
        let device = self.device.as_ref().unwrap().clone();
        let modifiers: Vec<u64> = self
            .state
            .modifiers
            .iter()
            .filter(|(f, m)| *f == spec.format && *m != u64::MAX)
            .map(|(_, m)| *m)
            .collect();
        ensure!(
            !modifiers.is_empty(),
            "no compositor DMA-BUF modifiers for fourcc {:#x}",
            spec.format
        );
        // A renderable linear allocation permits direct DRM download. NVIDIA
        // rejects it, so use an advertised tiled modifier and detile off the
        // compositor thread through Vulkan instead of falling back to SHM.
        let mut bo = if modifiers.contains(&0) {
            unsafe {
                gbm_bo_create(
                    device.ptr,
                    spec.width,
                    spec.height,
                    spec.format,
                    (1 << 4) | (1 << 2),
                )
            }
        } else {
            std::ptr::null_mut()
        };
        if bo.is_null() {
            if self.vulkan.is_none() {
                self.vulkan = Some(make_vulkan(self.hw.as_ref().unwrap())?);
            }
            let importable = vulkan_modifiers(self.vulkan.as_ref().unwrap(), &modifiers)?;
            ensure!(
                !importable.is_empty(),
                "no compositor/GBM DMA-BUF modifier can be imported by Vulkan"
            );
            bo = unsafe {
                gbm_bo_create_with_modifiers2(
                    device.ptr,
                    spec.width,
                    spec.height,
                    spec.format,
                    importable.as_ptr(),
                    importable.len() as u32,
                    1 << 2,
                )
            };
        }
        ensure!(
            !bo.is_null(),
            "GBM cannot allocate compositor screencopy format {:#x}: {}",
            spec.format,
            std::io::Error::last_os_error()
        );
        let raw = unsafe { gbm_bo_get_fd(bo) };
        if raw < 0 {
            unsafe {
                gbm_bo_destroy(bo);
            }
            bail!("exporting GBM DMA-BUF failed");
        }
        let size = unsafe { libc::lseek(raw, 0, libc::SEEK_END) };
        if size <= 0 {
            unsafe {
                libc::close(raw);
                gbm_bo_destroy(bo);
            }
            bail!(
                "querying DMA-BUF allocation size: {}",
                std::io::Error::last_os_error()
            );
        }
        let allocation = Arc::new(Dma {
            bo,
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
            size: size as usize,
            stride: unsafe { gbm_bo_get_stride(bo) },
            offset: unsafe { gbm_bo_get_offset(bo, 0) },
            modifier: unsafe { gbm_bo_get_modifier(bo) },
            _device: device.clone(),
            busy: AtomicBool::new(false),
        });
        ensure!(
            modifiers.contains(&allocation.modifier),
            "GBM selected an unadvertised modifier"
        );
        if self.slots.is_empty() {
            log::info!(
                "Wayland recording: DMA-BUF on {}, modifier={:#x}, {:?}",
                device.node,
                allocation.modifier,
                spec
            );
        }
        let params = self
            .state
            .dmabuf
            .as_ref()
            .unwrap()
            .create_params(&self.queue.handle(), ());
        params.add(
            allocation.fd.as_fd(),
            0,
            allocation.offset,
            allocation.stride,
            (allocation.modifier >> 32) as u32,
            allocation.modifier as u32,
        );
        let wl = params.create_immed(
            spec.width as i32,
            spec.height as i32,
            spec.format,
            zwp_linux_buffer_params_v1::Flags::empty(),
            &self.queue.handle(),
            (),
        );
        params.destroy();
        Ok(Slot { allocation, wl })
    }
}
fn open_device(main: Option<u64>) -> Result<Device> {
    // Feedback's main device can be a primary node; GBM and FFmpeg need its
    // matching render node rather than assuming GPU enumeration is stable.
    let mut node = "/dev/dri/renderD128".to_string();
    if let Some(dev) = main
        && let Ok(entries) = std::fs::read_dir(format!(
            "/sys/dev/char/{}:{}/device/drm",
            libc::major(dev),
            libc::minor(dev)
        ))
    {
        for e in entries.flatten() {
            let name = e.file_name();
            if name.as_encoded_bytes().starts_with(b"renderD") {
                node = format!("/dev/dri/{}", name.to_string_lossy());
                break;
            }
        }
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&node)
        .with_context(|| format!("opening compositor render node {node}"))?;
    let ptr = unsafe { gbm_create_device(file.as_raw_fd()) };
    ensure!(!ptr.is_null(), "creating GBM device on {node}");
    Ok(Device {
        ptr,
        _file: file,
        node,
    })
}
fn make_hw(device: &Device, spec: Spec) -> Result<Hw> {
    let sw = match spec.format {
        0x34325258 => ffi::AVPixelFormat::AV_PIX_FMT_BGR0,
        0x34325241 => ffi::AVPixelFormat::AV_PIX_FMT_BGRA,
        0x34324258 => ffi::AVPixelFormat::AV_PIX_FMT_RGB0,
        0x34324241 => ffi::AVPixelFormat::AV_PIX_FMT_RGBA,
        _ => bail!("unsupported recording DRM fourcc {:#x}", spec.format),
    };
    unsafe {
        let node = std::ffi::CString::new(device.node.as_str())?;
        let mut dev = std::ptr::null_mut();
        let rc = ffi::av_hwdevice_ctx_create(
            &mut dev,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
            node.as_ptr(),
            std::ptr::null_mut(),
            0,
        );
        ensure!(rc >= 0, "FFmpeg DRM device: {}", ffmpeg::Error::from(rc));
        let dev = Hw(dev);
        let frames = ffi::av_hwframe_ctx_alloc(dev.0);
        ensure!(!frames.is_null(), "allocating DRM frames context");
        let frames = Hw(frames);
        let ctx = &mut *((*frames.0).data.cast::<ffi::AVHWFramesContext>());
        ctx.format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME;
        ctx.sw_format = sw;
        ctx.width = spec.width as i32;
        ctx.height = spec.height as i32;
        let rc = ffi::av_hwframe_ctx_init(frames.0);
        ensure!(
            rc >= 0,
            "initializing DRM frames: {}",
            ffmpeg::Error::from(rc)
        );
        Ok(frames)
    }
}
fn make_vulkan(drm: &Hw) -> Result<Hw> {
    unsafe {
        let source = &*((*drm.0).data.cast::<ffi::AVHWFramesContext>());
        let mut device = std::ptr::null_mut();
        let rc = ffi::av_hwdevice_ctx_create_derived(
            &mut device,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
            source.device_ref,
            0,
        );
        ensure!(
            rc >= 0,
            "deriving Vulkan from DRM for DMA-BUF detile: {}",
            ffmpeg::Error::from(rc)
        );
        let device = Hw(device);
        let frames = ffi::av_hwframe_ctx_alloc(device.0);
        ensure!(!frames.is_null(), "allocating Vulkan frames context");
        let frames = Hw(frames);
        let ctx = &mut *((*frames.0).data.cast::<ffi::AVHWFramesContext>());
        ctx.format = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN;
        ctx.sw_format = source.sw_format;
        ctx.width = source.width;
        ctx.height = source.height;
        let rc = ffi::av_hwframe_ctx_init(frames.0);
        ensure!(
            rc >= 0,
            "initializing Vulkan detile frames: {}",
            ffmpeg::Error::from(rc)
        );
        log::info!(
            "Wayland recording: Vulkan DMA-BUF detile/download (no compositor SHM readback)"
        );
        Ok(frames)
    }
}
fn detile(frame: Frame, vulkan: &Hw) -> Result<Frame> {
    unsafe {
        let mut mapped = ffmpeg::frame::Video::empty();
        (*mapped.as_mut_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_VULKAN as i32;
        (*mapped.as_mut_ptr()).hw_frames_ctx = ffi::av_buffer_ref(vulkan.0);
        ensure!(
            !(*mapped.as_ptr()).hw_frames_ctx.is_null(),
            "referencing Vulkan frames context"
        );
        let rc = ffi::av_hwframe_map(
            mapped.as_mut_ptr(),
            frame.av().as_ptr(),
            ffi::AV_HWFRAME_MAP_READ as i32,
        );
        ensure!(
            rc >= 0,
            "importing tiled DRM frame into Vulkan: {} (no SHM fallback)",
            ffmpeg::Error::from(rc)
        );
        let mut downloaded = ffmpeg::frame::Video::empty();
        let rc = ffi::av_hwframe_transfer_data(downloaded.as_mut_ptr(), mapped.as_ptr(), 0);
        ensure!(
            rc >= 0,
            "downloading detiled Vulkan frame: {}",
            ffmpeg::Error::from(rc)
        );
        if downloaded.format() == ffmpeg::format::Pixel::BGRA {
            (*downloaded.as_mut_ptr()).format = ffi::AVPixelFormat::AV_PIX_FMT_BGR0 as i32;
        }
        Ok(Frame::from_av(downloaded, frame.captured))
    }
}
fn vulkan_modifiers(frames: &Hw, modifiers: &[u64]) -> Result<Vec<u64>> {
    use ash::vk::{self, Handle};
    unsafe {
        let fc = &*((*frames.0).data.cast::<ffi::AVHWFramesContext>());
        let device = &*((*fc.device_ctx).hwctx.cast::<ffi::AVVulkanDeviceContext>());
        let entry = ash::Entry::load().context("loading Vulkan modifier queries")?;
        let instance = ash::Instance::load(
            entry.static_fn(),
            vk::Instance::from_raw(device.inst as u64),
        );
        let physical = vk::PhysicalDevice::from_raw(device.phys_dev as u64);
        let format = vk::Format::from_raw(*ffi::av_vkfmt_from_pixfmt(fc.sw_format));
        let mut result = Vec::new();
        for &modifier in modifiers {
            let mut drm = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
                .drm_format_modifier(modifier)
                .sharing_mode(vk::SharingMode::EXCLUSIVE);
            let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(format)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_SRC)
                .push_next(&mut external)
                .push_next(&mut drm);
            let mut external_props = vk::ExternalImageFormatProperties::default();
            let mut props = vk::ImageFormatProperties2::default().push_next(&mut external_props);
            if instance
                .get_physical_device_image_format_properties2(physical, &info, &mut props)
                .is_ok()
                && external_props
                    .external_memory_properties
                    .external_memory_features
                    .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
            {
                result.push(modifier);
            }
        }
        Ok(result)
    }
}
fn wrap(
    allocation: Arc<Dma>,
    spec: Spec,
    hw: &Hw,
    captured: Instant,
    return_to_pool: bool,
) -> Result<Frame> {
    unsafe {
        let mut descriptor: ffi::AVDRMFrameDescriptor = std::mem::zeroed();
        descriptor.nb_objects = 1;
        descriptor.objects[0] = ffi::AVDRMObjectDescriptor {
            fd: allocation.fd.as_raw_fd(),
            size: allocation.size,
            format_modifier: allocation.modifier,
        };
        descriptor.nb_layers = 1;
        descriptor.layers[0].format = spec.format;
        descriptor.layers[0].nb_planes = 1;
        descriptor.layers[0].planes[0] = ffi::AVDRMPlaneDescriptor {
            object_index: 0,
            offset: allocation.offset as isize,
            pitch: allocation.stride as isize,
        };
        let lease = Box::into_raw(Box::new(Lease {
            descriptor,
            allocation,
            return_to_pool,
        }));
        let data = std::ptr::addr_of_mut!((*lease).descriptor).cast::<u8>();
        let buffer = ffi::av_buffer_create(
            data,
            std::mem::size_of::<ffi::AVDRMFrameDescriptor>(),
            Some(release),
            lease.cast(),
            0,
        );
        if buffer.is_null() {
            release(lease.cast(), data);
            bail!("allocating DRM AVBufferRef");
        }
        let mut av = ffmpeg::frame::Video::empty();
        let raw = av.as_mut_ptr();
        (*raw).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        (*raw).width = spec.width as i32;
        (*raw).height = spec.height as i32;
        (*raw).data[0] = data;
        (*raw).buf[0] = buffer;
        (*raw).hw_frames_ctx = ffi::av_buffer_ref(hw.0);
        ensure!(
            !(*raw).hw_frames_ctx.is_null(),
            "referencing DRM frames context"
        );
        Ok(Frame::from_av(av, captured))
    }
}
fn allocate_shm(
    shm: &wl_shm::WlShm,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    qh: &QueueHandle<State>,
) -> Result<Shm> {
    let fd = rustix::fs::memfd_create("quikscope-outline", rustix::fs::MemfdFlags::CLOEXEC)?;
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
    Ok(Shm {
        wl,
        map,
        _file: file,
        stride,
    })
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
            wl_output::Event::Mode { width, height, .. } => {
                o.rect.w = width;
                o.rect.h = height;
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
                        .chunks_exact(16)
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
                for bytes in indices.chunks_exact(2) {
                    if let Some(entry) = s
                        .format_table
                        .get(u16::from_ne_bytes(bytes.try_into().unwrap()) as usize)
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
impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        e: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
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
                s.shm_offer = Some((
                    Spec {
                        format: format as u32,
                        width,
                        height,
                    },
                    stride,
                ))
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
                let (globals, _) = registry_queue_init::<State>(&c.conn)?;
                let qh = c.queue.handle();
                let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ())?;
                let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals.bind(&qh, 1..=4, ())?;
                let mut surfaces = Vec::new();
                let mut layers = Vec::new();
                let mut buffers = Vec::new();
                for o in &c.state.outputs {
                    let r = o.rect;
                    let strips = [
                        IRect::new(region.x - 2, region.y - 2, region.w + 4, 2),
                        IRect::new(region.x - 2, region.y + region.h, region.w + 4, 2),
                        IRect::new(region.x - 2, region.y, 2, region.h),
                        IRect::new(region.x + region.w, region.y, 2, region.h),
                    ];
                    if !strips.iter().any(|s| {
                        s.x < r.x + r.w && s.x + s.w > r.x && s.y < r.y + r.h && s.y + s.h > r.y
                    }) {
                        continue;
                    }
                    let i = surfaces.len();
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
                        i,
                    );
                    layer.set_anchor(
                        zwlr_layer_surface_v1::Anchor::Top
                            | zwlr_layer_surface_v1::Anchor::Bottom
                            | zwlr_layer_surface_v1::Anchor::Left
                            | zwlr_layer_surface_v1::Anchor::Right,
                    );
                    layer.set_exclusive_zone(-1);
                    layer.set_keyboard_interactivity(
                        zwlr_layer_surface_v1::KeyboardInteractivity::None,
                    );
                    layer.set_size(r.w as u32, r.h as u32);
                    let mut b = allocate_shm(
                        &c.state.shm,
                        r.w as u32,
                        r.h as u32,
                        r.w as u32 * 4,
                        wl_shm::Format::Argb8888,
                        &qh,
                    )?;
                    for strip in strips {
                        for y in strip.y.max(r.y)..(strip.y + strip.h).min(r.y + r.h) {
                            for x in strip.x.max(r.x)..(strip.x + strip.w).min(r.x + r.w) {
                                let p =
                                    ((y - r.y) as usize * r.w as usize + (x - r.x) as usize) * 4;
                                b.map[p..p + 4].copy_from_slice(&[0xf2, 0x8c, 0x8f, 255]);
                            }
                        }
                    }
                    surface.commit();
                    surfaces.push(surface);
                    layers.push(layer);
                    buffers.push(b);
                }
                c.state.configured = vec![false; surfaces.len()];
                let deadline = Instant::now() + Duration::from_secs(3);
                while c.state.configured.iter().any(|v| !v) {
                    ensure!(Instant::now() < deadline, "outline configure timed out");
                    c.dispatch(Duration::from_millis(50))?;
                }
                for (surface, buffer) in surfaces.iter().zip(&buffers) {
                    surface.attach(Some(&buffer.wl), 0, 0);
                    surface.damage(0, 0, i32::MAX, i32::MAX);
                    surface.commit();
                }
                c.queue.roundtrip(&mut c.state)?;
                let _ = tx.send(Ok(()));
                while !stopping.load(Ordering::Relaxed) && !c.state.closed {
                    c.dispatch(Duration::from_millis(50))?;
                }
                for l in layers {
                    l.destroy();
                }
                for s in surfaces {
                    s.destroy();
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
