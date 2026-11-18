//! Wayland recording in a child process (`quikscope __record-helper`).
//!
//! With NVIDIA's driver (620.70.06, RTX 5090) a process that also runs
//! GPUI's Vulkan renderer imported some DMA-BUF sizes (1279–1280 pixels wide,
//! about 720 tall) into FFmpeg's Vulkan device as all-zero images, so whole
//! recordings came out black; the same capture in a process without GPUI was
//! correct for every size, and neither delaying the import, fresh fd numbers
//! nor creating the Vulkan device early changed that. The helper owns the
//! screencopy connection, GBM buffers and Vulkan detiling, and hands BGR0
//! frames to the daemon through a shared-memory ring without copying them
//! again on the daemon side.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use ffmpeg_next::{self as ffmpeg, ffi};
use memmap2::MmapMut;

use crate::geom::IRect;
use crate::media::{Frame, VideoSource};
use crate::platform::RecordTarget;

/// Frames in flight between the helper and the encoder. A full ring drops
/// the newest capture rather than stalling the compositor connection.
const SLOTS: u32 = 6;
/// The helper's end of the socket pair.
const HELPER_FD: RawFd = 3;
/// Command-line verb of the helper process.
pub const VERB: &str = "__record-helper";

const MSG_HEADER: u32 = 1;
const MSG_FRAME: u32 = 2;
const MSG_ERROR: u32 = 3;
const MSG_RELEASE: u32 = 4;

/// One fixed-size message. `HEADER`: `value` = width << 32 | height, the
/// ring's memfd attached. `FRAME`: `slot` filled, `value` = CLOCK_MONOTONIC
/// nanoseconds of capture. `ERROR`: `value` = length of UTF-8 text that
/// follows. `RELEASE` (daemon → helper): `slot` is free again.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Msg {
    kind: u32,
    slot: u32,
    value: u64,
}

impl Msg {
    fn bytes(&self) -> [u8; 16] {
        let mut out = [0; 16];
        out[..4].copy_from_slice(&self.kind.to_ne_bytes());
        out[4..8].copy_from_slice(&self.slot.to_ne_bytes());
        out[8..].copy_from_slice(&self.value.to_ne_bytes());
        out
    }

    fn parse(bytes: &[u8; 16]) -> Msg {
        Msg {
            kind: u32::from_ne_bytes(bytes[..4].try_into().unwrap()),
            slot: u32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
            value: u64::from_ne_bytes(bytes[8..].try_into().unwrap()),
        }
    }
}

fn monotonic_ns() -> u64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// The `Instant` of a CLOCK_MONOTONIC reading (Rust's `Instant` uses the
/// same clock on Linux, but cannot be built from a raw value).
fn instant_of(ns: u64) -> Instant {
    let (now, now_ns) = (Instant::now(), monotonic_ns());
    now.checked_sub(Duration::from_nanos(now_ns.saturating_sub(ns)))
        .unwrap_or(now)
}

fn send_msg(socket: &UnixStream, msg: Msg, fd: Option<RawFd>) -> std::io::Result<()> {
    let bytes = msg.bytes();
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let mut control = [0u8; 64];
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    if let Some(fd) = fd {
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
        header.msg_control = control.as_mut_ptr().cast();
        header.msg_controllen = space as _;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&header);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
        }
    }
    let sent = unsafe { libc::sendmsg(socket.as_raw_fd(), &header, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if sent as usize != bytes.len() {
        return Err(std::io::Error::other("short message"));
    }
    Ok(())
}

/// Reads one message, and the fd attached to it if any. `Ok(None)` at EOF.
fn recv_msg(socket: &UnixStream) -> std::io::Result<Option<(Msg, Option<OwnedFd>)>> {
    let mut bytes = [0u8; 16];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut control = [0u8; 64];
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = control.len() as _;
    let received =
        unsafe { libc::recvmsg(socket.as_raw_fd(), &mut header, libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if received == 0 {
        return Ok(None);
    }
    if (received as usize) < bytes.len() {
        // A message is written in one sendmsg; finish a split read.
        (&*socket).read_exact(&mut bytes[received as usize..])?;
    }
    let mut fd = None;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        if !cmsg.is_null()
            && (*cmsg).cmsg_level == libc::SOL_SOCKET
            && (*cmsg).cmsg_type == libc::SCM_RIGHTS
        {
            let raw = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>());
            fd = Some(OwnedFd::from_raw_fd(raw));
        }
    }
    Ok(Some((Msg::parse(&bytes), fd)))
}

/// Daemon side: the helper process and the frames it delivers.
struct Remote {
    child: Child,
    socket: UnixStream,
    rx: mpsc::Receiver<Result<Frame>>,
    reader: Option<std::thread::JoinHandle<()>>,
    size: (u32, u32),
}

impl VideoSource for Remote {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        match self.rx.recv_timeout(timeout) {
            Ok(frame) => frame.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("the Wayland capture helper stopped (see the daemon log)")
            }
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        // The helper exits when its socket closes; the kill covers a
        // helper stuck in a compositor round trip.
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Returns a ring slot to the helper when the encoder drops its frame.
struct Lease {
    socket: UnixStream,
    slot: u32,
    _ring: Arc<MmapMut>,
}

unsafe extern "C" fn release(opaque: *mut libc::c_void, _: *mut u8) {
    let lease = unsafe { Box::from_raw(opaque.cast::<Lease>()) };
    let _ = send_msg(
        &lease.socket,
        Msg {
            kind: MSG_RELEASE,
            slot: lease.slot,
            value: 0,
        },
        None,
    );
}

/// Starts `quikscope __record-helper` for `target` and waits for its first
/// frame size.
pub fn open(target: &RecordTarget, fps: u32, cursor: bool) -> Result<Box<dyn VideoSource>> {
    let (socket, theirs) = UnixStream::pair().context("creating the capture socket pair")?;
    let r = target.region;
    let o = target.output_rect;
    let mut command = Command::new(std::env::current_exe().context("locating quikscope")?);
    command
        .arg(VERB)
        .arg(&target.output)
        .args([o.x, o.y, o.w, o.h, r.x, r.y, r.w, r.h].map(|v| v.to_string()))
        .arg(fps.to_string())
        .arg(if cursor { "1" } else { "0" })
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    let raw = theirs.as_raw_fd();
    // SAFETY: dup2 is async-signal-safe; the new fd 3 does not inherit
    // CLOEXEC, every other descriptor of ours does.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(raw, HELPER_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .context("starting the Wayland capture helper")?;
    drop(theirs);

    let (header, fd) = match recv_msg(&socket)? {
        Some((msg, fd)) if msg.kind == MSG_HEADER => (msg, fd),
        Some((msg, _)) if msg.kind == MSG_ERROR => {
            let mut text = vec![0; msg.value.min(64 * 1024) as usize];
            (&socket).read_exact(&mut text)?;
            bail!("{}", String::from_utf8_lossy(&text));
        }
        _ => bail!("the Wayland capture helper exited during start-up (see the daemon log)"),
    };
    let fd = fd.context("capture helper sent no frame memory")?;
    let (width, height) = ((header.value >> 32) as u32, header.value as u32);
    let slot_len = width as usize * height as usize * 4;
    let ring = Arc::new(unsafe {
        MmapMut::map_mut(&std::fs::File::from(fd)).context("mapping the capture ring")?
    });
    ensure!(
        ring.len() >= slot_len * SLOTS as usize,
        "capture ring is smaller than announced"
    );

    let (tx, rx) = mpsc::sync_channel(SLOTS as usize);
    let reader_socket = socket.try_clone()?;
    let reader = std::thread::Builder::new()
        .name("quikscope-wlr-frames".into())
        .spawn(move || {
            loop {
                let msg = match recv_msg(&reader_socket) {
                    Ok(Some((msg, _))) => msg,
                    Ok(None) | Err(_) => return,
                };
                let frame = match msg.kind {
                    MSG_FRAME if msg.slot < SLOTS => {
                        wrap(&ring, &reader_socket, msg.slot, width, height, msg.value)
                    }
                    MSG_ERROR => {
                        let mut text = vec![0; msg.value.min(64 * 1024) as usize];
                        if (&reader_socket).read_exact(&mut text).is_err() {
                            return;
                        }
                        Err(anyhow::anyhow!("{}", String::from_utf8_lossy(&text)))
                    }
                    _ => Err(anyhow::anyhow!("malformed capture helper message")),
                };
                let failed = frame.is_err();
                if tx.send(frame).is_err() || failed {
                    return;
                }
            }
        })?;
    Ok(Box::new(Remote {
        child,
        socket,
        rx,
        reader: Some(reader),
        size: (width, height),
    }))
}

/// A frame over ring slot `slot`, returned to the helper when dropped.
fn wrap(
    ring: &Arc<MmapMut>,
    socket: &UnixStream,
    slot: u32,
    width: u32,
    height: u32,
    captured_ns: u64,
) -> Result<Frame> {
    let len = width as usize * height as usize * 4;
    let lease = Box::new(Lease {
        socket: socket.try_clone()?,
        slot,
        _ring: ring.clone(),
    });
    unsafe {
        let data = ring.as_ptr().add(slot as usize * len) as *mut u8;
        let buffer = ffi::av_buffer_create(
            data,
            len,
            Some(release),
            Box::into_raw(lease).cast(),
            ffi::AV_BUFFER_FLAG_READONLY,
        );
        ensure!(!buffer.is_null(), "wrapping a captured frame");
        let mut av = ffmpeg::frame::Video::empty();
        let raw = av.as_mut_ptr();
        (*raw).format = ffi::AVPixelFormat::AV_PIX_FMT_BGR0 as i32;
        (*raw).width = width as i32;
        (*raw).height = height as i32;
        (*raw).buf[0] = buffer;
        (*raw).data[0] = data;
        (*raw).linesize[0] = width as i32 * 4;
        Ok(Frame::from_av(av, instant_of(captured_ns)))
    }
}

/// Helper side: `quikscope __record-helper OUTPUT OX OY OW OH X Y W H FPS CURSOR`.
pub fn helper_main(args: &[String]) -> Result<()> {
    // SAFETY: the daemon put our end of the socket pair at fd 3.
    let socket = unsafe { UnixStream::from_raw_fd(HELPER_FD) };
    let result = run_helper(&socket, args);
    if let Err(e) = &result {
        let text = format!("{e:#}");
        let _ = send_msg(
            &socket,
            Msg {
                kind: MSG_ERROR,
                slot: 0,
                value: text.len() as u64,
            },
            None,
        )
        .and_then(|()| (&socket).write_all(text.as_bytes()));
    }
    result
}

fn run_helper(socket: &UnixStream, args: &[String]) -> Result<()> {
    let [output, rest @ ..] = args else {
        bail!("usage: {VERB} OUTPUT OX OY OW OH X Y W H FPS CURSOR");
    };
    let numbers: Vec<i64> = rest
        .iter()
        .map(|a| {
            a.parse()
                .with_context(|| format!("bad helper argument {a:?}"))
        })
        .collect::<Result<_>>()?;
    let [ox, oy, ow, oh, x, y, w, h, fps, cursor] = numbers[..] else {
        bail!("usage: {VERB} OUTPUT OX OY OW OH X Y W H FPS CURSOR");
    };
    let target = RecordTarget {
        output: output.clone(),
        output_rect: IRect::new(ox as i32, oy as i32, ow as i32, oh as i32),
        region: IRect::new(x as i32, y as i32, w as i32, h as i32),
    };
    crate::media::init();
    let mut source = super::record::open_local(&target, fps as u32, cursor != 0, false)?;
    let (width, height) = source.size();
    let slot_len = width as usize * height as usize * 4;

    let ring_fd = unsafe { libc::memfd_create(c"quikscope-frames".as_ptr(), libc::MFD_CLOEXEC) };
    ensure!(
        ring_fd >= 0,
        "creating frame memory: {}",
        std::io::Error::last_os_error()
    );
    let ring_file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(ring_fd) });
    ring_file.set_len((slot_len * SLOTS as usize) as u64)?;
    let mut ring = unsafe { MmapMut::map_mut(&ring_file)? };
    send_msg(
        socket,
        Msg {
            kind: MSG_HEADER,
            slot: 0,
            value: (u64::from(width) << 32) | u64::from(height),
        },
        Some(ring_file.as_raw_fd()),
    )?;

    // Releases arrive on their own thread so a slow encoder never blocks
    // capture; EOF (the daemon stopped or died) ends the helper.
    let stop = Arc::new(AtomicBool::new(false));
    let (free_tx, free_rx) = mpsc::channel::<u32>();
    {
        let (socket, stop) = (socket.try_clone()?, stop.clone());
        std::thread::spawn(move || {
            loop {
                match recv_msg(&socket) {
                    Ok(Some((msg, _))) if msg.kind == MSG_RELEASE => {
                        if free_tx.send(msg.slot).is_err() {
                            return;
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => {
                        stop.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            }
        });
    }
    let mut free: Vec<u32> = (0..SLOTS).collect();
    let mut dropped = 0usize;
    while !stop.load(Ordering::Relaxed) {
        let Some(frame) = source.next_frame(Duration::from_millis(100))? else {
            continue;
        };
        free.extend(free_rx.try_iter());
        let Some(slot) = free.pop() else {
            dropped += 1;
            continue;
        };
        let captured = monotonic_ns().saturating_sub(frame.captured.elapsed().as_nanos() as u64);
        let cpu = to_cpu(frame)?;
        let dst = &mut ring[slot as usize * slot_len..(slot as usize + 1) * slot_len];
        let (stride, data) = (cpu.stride(0), cpu.data(0));
        let row = width as usize * 4;
        for (y, chunk) in dst.chunks_exact_mut(row).enumerate() {
            chunk.copy_from_slice(&data[y * stride..y * stride + row]);
        }
        if send_msg(
            socket,
            Msg {
                kind: MSG_FRAME,
                slot,
                value: captured,
            },
            None,
        )
        .is_err()
        {
            break;
        }
    }
    if dropped > 0 {
        log::info!("Wayland capture helper: {dropped} frames dropped while the encoder caught up");
    }
    Ok(())
}

/// BGR0/BGRA pixels in CPU memory (downloading linear DRM frames).
fn to_cpu(frame: Frame) -> Result<ffmpeg::frame::Video> {
    let av = frame.into_av();
    if unsafe { (*av.as_ptr()).hw_frames_ctx.is_null() } {
        return Ok(av);
    }
    let mut cpu = ffmpeg::frame::Video::empty();
    let rc = unsafe { ffi::av_hwframe_transfer_data(cpu.as_mut_ptr(), av.as_ptr(), 0) };
    ensure!(
        rc >= 0,
        "downloading a DRM frame: {}",
        ffmpeg::Error::from(rc)
    );
    Ok(cpu)
}
