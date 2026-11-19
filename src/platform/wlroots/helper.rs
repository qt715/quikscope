//! Wayland recording in a child process (`quikscope __record-helper`).
//!
//! With NVIDIA's driver (620.70.06, RTX 5090) a process that also runs
//! GPUI's Vulkan renderer imported some DMA-BUF sizes (1279–1280 pixels wide,
//! about 720 tall) into a second Vulkan device as all-zero images, so whole
//! recordings came out black; the same capture in a process without GPUI was
//! correct for every size, and neither delaying the import, fresh fd numbers
//! nor creating the Vulkan device early changed that. The helper owns the
//! screencopy connection, GBM buffers and Vulkan readback, and hands frames
//! to the daemon through a shared-memory ring that the daemon's frames
//! borrow without copying.
//!
//! Pipelining: the helper's main thread only talks Wayland. It requests the
//! next frame as soon as the previous one is copied (no earlier than the
//! frame rate allows), while a readback thread moves finished copies into
//! the ring, so compositor copy and GPU→CPU readback overlap.

use std::io::{Read, Write};
use std::os::fd::{AsFd as _, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use memmap2::{Mmap, MmapMut};

use super::record::{Buffers, Capturer};
use crate::geom::IRect;
use crate::media::{Frame, PixelFormat, Pixels, VideoSource};
use crate::platform::RecordTarget;

/// Frames in flight between the helper and the encoder. A full ring drops
/// the newest capture rather than stalling the compositor connection.
const SLOTS: u32 = 6;
/// The helper's end of the socket pair.
const HELPER_FD: RawFd = 3;
/// Command-line verb of the helper process.
pub const VERB: &str = "__record-helper";
/// How long the helper may take to deliver its first frame.
const HANDSHAKE: Duration = Duration::from_secs(5);

const MSG_HEADER: u32 = 1;
const MSG_FRAME: u32 = 2;
const MSG_ERROR: u32 = 3;
const MSG_RELEASE: u32 = 4;
const MSG_PAUSE: u32 = 5;
const MSG_RESUME: u32 = 6;

/// One fixed-size message.
/// - `HEADER` (helper → daemon, once): `slot` = pixel format code,
///   `value` = width << 32 | height; the ring's memfd attached.
/// - `FRAME`: `slot` filled, `value` = CLOCK_MONOTONIC nanoseconds of
///   capture, `epoch` = the resume it was captured after.
/// - `ERROR`: `value` = length of UTF-8 text that follows.
/// - `RELEASE` (daemon → helper): `slot` is free again.
/// - `PAUSE` / `RESUME` (daemon → helper): stop / restart capturing;
///   `RESUME` carries the new `epoch`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Msg {
    kind: u32,
    slot: u32,
    value: u64,
    epoch: u64,
}

const MSG_LEN: usize = 24;

impl Msg {
    fn bytes(&self) -> [u8; MSG_LEN] {
        let mut out = [0; MSG_LEN];
        out[..4].copy_from_slice(&self.kind.to_ne_bytes());
        out[4..8].copy_from_slice(&self.slot.to_ne_bytes());
        out[8..16].copy_from_slice(&self.value.to_ne_bytes());
        out[16..].copy_from_slice(&self.epoch.to_ne_bytes());
        out
    }

    fn parse(bytes: &[u8; MSG_LEN]) -> Msg {
        Msg {
            kind: u32::from_ne_bytes(bytes[..4].try_into().unwrap()),
            slot: u32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
            value: u64::from_ne_bytes(bytes[8..16].try_into().unwrap()),
            epoch: u64::from_ne_bytes(bytes[16..].try_into().unwrap()),
        }
    }

    fn new(kind: u32) -> Msg {
        Msg {
            kind,
            ..Msg::default()
        }
    }
}

fn format_code(format: PixelFormat) -> u32 {
    match format {
        PixelFormat::Bgra => 0,
        PixelFormat::Bgrx => 1,
        PixelFormat::Rgba => 2,
        PixelFormat::Rgbx => 3,
    }
}

fn format_of(code: u32) -> Option<PixelFormat> {
    [
        PixelFormat::Bgra,
        PixelFormat::Bgrx,
        PixelFormat::Rgba,
        PixelFormat::Rgbx,
    ]
    .get(code as usize)
    .copied()
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
    let mut bytes = [0u8; MSG_LEN];
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
    // Take ownership of an attached fd before anything can fail.
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
    if (received as usize) < bytes.len() {
        // A message is written in one sendmsg; finish a split read.
        (&*socket).read_exact(&mut bytes[received as usize..])?;
    }
    Ok(Some((Msg::parse(&bytes), fd)))
}

fn read_error(socket: &UnixStream, msg: Msg) -> anyhow::Error {
    let mut text = vec![0; msg.value.min(64 * 1024) as usize];
    match (&*socket).read_exact(&mut text) {
        Ok(()) => anyhow::anyhow!("{}", String::from_utf8_lossy(&text)),
        Err(e) => anyhow::Error::new(e).context("reading the capture helper's error"),
    }
}

/// The helper process, killed and reaped however its owner goes away.
struct Helper(Child);

impl Drop for Helper {
    fn drop(&mut self) {
        // The helper exits by itself once its socket closes (logging what it
        // dropped); a helper stuck in a compositor or GPU call is killed.
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if !matches!(self.0.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What the daemon-side reader passes to `next_frame`.
type Delivery = Result<(u64, Frame)>;

/// Daemon side: the helper process and the frames it delivers.
struct Remote {
    socket: Arc<UnixStream>,
    rx: mpsc::Receiver<Delivery>,
    reader: Option<std::thread::JoinHandle<()>>,
    size: (u32, u32),
    /// Frames captured before the latest resume carry an older epoch.
    epoch: u64,
    // Last, so the socket is shut down before the helper is waited for.
    _helper: Helper,
}

impl VideoSource for Remote {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        let deadline = Instant::now() + timeout;
        loop {
            match self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                // Captured before the last resume: dropping it frees the slot.
                Ok(Ok((epoch, _))) if epoch != self.epoch => {}
                Ok(frame) => return frame.map(|(_, frame)| Some(frame)),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("the Wayland capture helper stopped (see the daemon log)")
                }
            }
        }
    }

    fn set_paused(&mut self, paused: bool) {
        let msg = if paused {
            Msg::new(MSG_PAUSE)
        } else {
            self.epoch += 1;
            Msg {
                epoch: self.epoch,
                ..Msg::new(MSG_RESUME)
            }
        };
        // A dead helper is reported by the next `next_frame`.
        let _ = send_msg(&self.socket, msg, None);
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Ring slot `slot` lent to a frame; dropping it hands the slot back.
struct Lease {
    ring: Arc<Mmap>,
    socket: Arc<UnixStream>,
    slot: u32,
    len: usize,
}

impl AsRef<[u8]> for Lease {
    fn as_ref(&self) -> &[u8] {
        let start = self.slot as usize * self.len;
        &self.ring[start..start + self.len]
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = send_msg(
            &self.socket,
            Msg {
                slot: self.slot,
                ..Msg::new(MSG_RELEASE)
            },
            None,
        );
    }
}

/// Starts `quikscope __record-helper` for `target` and waits for its first
/// frame. `force_copy` (diagnostics) makes every copy a full one.
pub(super) fn spawn(
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
    let (socket, theirs) = UnixStream::pair().context("creating the capture socket pair")?;
    let r = target.region;
    let o = target.output_rect;
    // /proc/self/exe is this process's binary even after a package upgrade
    // replaced the file at its path (current_exe() would name the new one).
    let mut command = Command::new("/proc/self/exe");
    command
        .arg0("quikscope")
        .arg(VERB)
        .arg(&target.output)
        .args([o.x, o.y, o.w, o.h, r.x, r.y, r.w, r.h].map(|v| v.to_string()))
        .arg(fps.to_string())
        .arg(if cursor { "1" } else { "0" })
        .arg(if force_copy { "1" } else { "0" })
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
    let helper = Helper(
        command
            .spawn()
            .context("starting the Wayland capture helper")?,
    );
    drop(theirs);
    let socket = Arc::new(socket);
    let (format, width, height, ring) = match handshake(&socket) {
        Ok(header) => header,
        Err(e) => {
            // Unblocks a helper that is waiting to send, before the guard
            // waits for it to exit.
            let _ = socket.shutdown(std::net::Shutdown::Both);
            return Err(e);
        }
    };
    let len = width as usize * height as usize * 4;
    let (tx, rx) = mpsc::sync_channel(SLOTS as usize);
    let reader_socket = socket.clone();
    let reader = std::thread::Builder::new()
        .name("quikscope-wlr-frames".into())
        .spawn(move || {
            loop {
                let msg = match recv_msg(&reader_socket) {
                    Ok(Some((msg, _))) => msg,
                    Ok(None) | Err(_) => return,
                };
                let delivery = match msg.kind {
                    MSG_FRAME if msg.slot < SLOTS => {
                        let lease = Lease {
                            ring: ring.clone(),
                            socket: reader_socket.clone(),
                            slot: msg.slot,
                            len,
                        };
                        Ok((
                            msg.epoch,
                            Frame::from_pixels(
                                width,
                                height,
                                width as usize * 4,
                                format,
                                Pixels::Shared(Box::new(lease)),
                                instant_of(msg.value),
                            ),
                        ))
                    }
                    MSG_ERROR => Err(read_error(&reader_socket, msg)),
                    _ => Err(anyhow::anyhow!("malformed capture helper message")),
                };
                let failed = delivery.is_err();
                if tx.send(delivery).is_err() || failed {
                    return;
                }
            }
        })?;
    Ok(Box::new(Remote {
        socket,
        rx,
        reader: Some(reader),
        size: (width, height),
        epoch: 0,
        _helper: helper,
    }))
}

/// Reads the helper's header within [`HANDSHAKE`] and maps its ring.
fn handshake(socket: &UnixStream) -> Result<(PixelFormat, u32, u32, Arc<Mmap>)> {
    socket.set_read_timeout(Some(HANDSHAKE))?;
    let (header, fd) = match recv_msg(socket) {
        Ok(Some((msg, fd))) if msg.kind == MSG_HEADER => (msg, fd),
        Ok(Some((msg, _))) if msg.kind == MSG_ERROR => return Err(read_error(socket, msg)),
        Ok(_) => bail!("the Wayland capture helper exited during start-up (see the daemon log)"),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            bail!(
                "the Wayland capture helper delivered no frame within {} s",
                HANDSHAKE.as_secs()
            )
        }
        Err(e) => return Err(e).context("starting the Wayland capture helper"),
    };
    socket.set_read_timeout(None)?;
    let fd = fd.context("the capture helper sent no frame memory")?;
    let format = format_of(header.slot).context("the capture helper sent an unknown format")?;
    let (width, height) = ((header.value >> 32) as u32, header.value as u32);
    ensure!(
        width > 0 && height > 0,
        "the capture helper sent an empty size"
    );
    let ring =
        unsafe { Mmap::map(&std::fs::File::from(fd)) }.context("mapping the capture ring")?;
    ensure!(
        ring.len() >= width as usize * height as usize * 4 * SLOTS as usize,
        "capture ring is smaller than announced"
    );
    Ok((format, width, height, Arc::new(ring)))
}

/// Helper side: `quikscope __record-helper OUTPUT OX OY OW OH X Y W H FPS CURSOR FORCE`.
pub fn helper_main(args: &[String]) -> Result<()> {
    // SAFETY: the daemon put our end of the socket pair at fd 3.
    let socket = unsafe { UnixStream::from_raw_fd(HELPER_FD) };
    let result = run_helper(&socket, args);
    if let Err(e) = &result {
        let text = format!("{e:#}");
        let _ = send_msg(
            &socket,
            Msg {
                value: text.len() as u64,
                ..Msg::new(MSG_ERROR)
            },
            None,
        )
        .and_then(|()| (&socket).write_all(text.as_bytes()));
    }
    result
}

/// Spaces compositor copy requests one frame period apart. A request goes
/// out once the previous frame is copied and the period since the previous
/// request's slot has passed; slots keep their phase, so a request delayed
/// by a late frame does not push the following ones back. After a long wait
/// (a damage copy on a static screen) the schedule restarts from that
/// request instead of catching up with a burst.
struct Pacer {
    period: Duration,
    due: Instant,
}

impl Pacer {
    fn new(fps: u32, now: Instant) -> Pacer {
        Pacer {
            period: Duration::from_secs_f64(1.0 / f64::from(fps)),
            due: now,
        }
    }

    fn due(&self) -> Instant {
        self.due
    }

    /// A request went out at `now` (not before `due`).
    fn issued(&mut self, now: Instant) {
        self.due += self.period;
        if self.due < now {
            self.due = now + self.period;
        }
    }

    fn restart(&mut self, now: Instant) {
        self.due = now;
    }
}

fn run_helper(socket: &UnixStream, args: &[String]) -> Result<()> {
    const USAGE: &str = "usage: __record-helper OUTPUT OX OY OW OH X Y W H FPS CURSOR FORCE";
    let [output, rest @ ..] = args else {
        bail!(USAGE);
    };
    let numbers: Vec<i64> = rest
        .iter()
        .map(|a| {
            a.parse()
                .with_context(|| format!("bad helper argument {a:?}"))
        })
        .collect::<Result<_>>()?;
    let [ox, oy, ow, oh, x, y, w, h, fps, cursor, force] = numbers[..] else {
        bail!(USAGE);
    };
    let target = RecordTarget {
        output: output.clone(),
        output_rect: IRect::new(ox as i32, oy as i32, ow as i32, oh as i32),
        region: IRect::new(x as i32, y as i32, w as i32, h as i32),
    };
    ensure!((1..=240).contains(&fps), "recording fps must be in 1..=240");
    let mut capturer = Capturer::connect(&target, cursor != 0, force != 0)?;
    let wake = rustix::event::eventfd(
        0,
        rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
    )?;
    let wake = Arc::new(wake);
    let (done_tx, done_rx) = mpsc::channel::<Result<usize>>();
    let mut readback: Option<Readback> = None;
    let mut pacer = Pacer::new(fps as u32, Instant::now());
    let (mut paused, mut epoch) = (false, 0);
    let result = (|| -> Result<()> {
        loop {
            let now = Instant::now();
            let idle = !paused && capturer.can_request();
            let timeout = if idle && now >= pacer.due() {
                capturer.request();
                pacer.issued(now);
                // The compositor's events (or the daemon) wake us up.
                Duration::from_millis(100)
            } else if idle {
                pacer.due().saturating_duration_since(now)
            } else {
                Duration::from_millis(100)
            };
            let [from_daemon, woken] = capturer.wait(
                [socket.as_fd(), wake.as_fd()],
                timeout.min(Duration::from_millis(100)),
            )?;
            if from_daemon {
                let Some((msg, _)) = recv_msg(socket)? else {
                    return Ok(());
                };
                match msg.kind {
                    MSG_RELEASE if msg.slot < SLOTS => {
                        if let Some(readback) = &readback {
                            let _ = readback.free.send(msg.slot);
                        }
                    }
                    MSG_PAUSE => {
                        paused = true;
                        capturer.pause();
                    }
                    MSG_RESUME => {
                        paused = false;
                        epoch = msg.epoch;
                        pacer.restart(Instant::now());
                    }
                    _ => bail!("malformed message from the daemon"),
                }
            }
            if woken {
                let _ = rustix::io::read(&*wake, &mut [0u8; 8]);
            }
            for done in done_rx.try_iter() {
                capturer.release(done?);
            }
            if let Some(ready) = capturer.progress()? {
                let readback = match &mut readback {
                    Some(readback) => readback,
                    None => {
                        let buffers = capturer.buffers().context("frame without buffers")?;
                        readback.insert(Readback::start(
                            socket,
                            buffers.clone(),
                            done_tx.clone(),
                            wake.clone(),
                        )?)
                    }
                };
                let captured =
                    monotonic_ns().saturating_sub(ready.captured.elapsed().as_nanos() as u64);
                let job = Job {
                    buffer: ready.buffer,
                    captured,
                    epoch,
                    inverted: ready.inverted,
                };
                // The readback thread ended: the daemon went away, or it
                // failed and its error is waiting in `done_rx`.
                if readback.jobs.send(job).is_err() {
                    return Ok(());
                }
            }
        }
    })();
    if let Some(readback) = readback {
        drop(readback.jobs);
        let _ = readback.thread.join();
    }
    // A readback error explains a closed channel better than the latter.
    if let Some(Err(e)) = done_rx.try_iter().find(Result::is_err) {
        return Err(e);
    }
    result
}

/// A finished compositor copy to move into the ring.
struct Job {
    buffer: usize,
    captured: u64,
    epoch: u64,
    inverted: bool,
}

/// The readback thread and its inputs.
struct Readback {
    jobs: mpsc::Sender<Job>,
    free: mpsc::Sender<u32>,
    thread: std::thread::JoinHandle<()>,
}

impl Readback {
    /// Creates the ring, announces it to the daemon and starts the thread.
    fn start(
        socket: &UnixStream,
        buffers: Arc<Buffers>,
        done: mpsc::Sender<Result<usize>>,
        wake: Arc<OwnedFd>,
    ) -> Result<Readback> {
        let spec = buffers.spec;
        let len = spec.width as usize * spec.height as usize * 4;
        let ring_fd =
            rustix::fs::memfd_create("quikscope-frames", rustix::fs::MemfdFlags::CLOEXEC)?;
        let ring_file = std::fs::File::from(ring_fd);
        ring_file.set_len((len * SLOTS as usize) as u64)?;
        let mut ring = unsafe { MmapMut::map_mut(&ring_file)? };
        send_msg(
            socket,
            Msg {
                slot: format_code(buffers.format),
                value: (u64::from(spec.width) << 32) | u64::from(spec.height),
                ..Msg::new(MSG_HEADER)
            },
            Some(ring_file.as_raw_fd()),
        )?;
        let socket = socket.try_clone()?;
        let (jobs, job_rx) = mpsc::channel::<Job>();
        let (free, free_rx) = mpsc::channel::<u32>();
        let thread = std::thread::Builder::new()
            .name("quikscope-wlr-readback".into())
            .spawn(move || {
                let mut available: Vec<u32> = (0..SLOTS).rev().collect();
                let mut dropped = 0usize;
                let row = spec.width as usize * 4;
                let finish = |result: Result<usize>| {
                    let _ = done.send(result);
                    let _ = rustix::io::write(&*wake, &1u64.to_ne_bytes());
                };
                for job in job_rx {
                    available.extend(free_rx.try_iter());
                    let Some(slot) = available.pop() else {
                        dropped += 1;
                        finish(Ok(job.buffer));
                        continue;
                    };
                    let dst = &mut ring[slot as usize * len..][..len];
                    if let Err(e) = buffers.read(job.buffer, |src, stride| {
                        copy_rows(src, stride, dst, row, job.inverted)
                    }) {
                        finish(Err(e));
                        break;
                    }
                    let frame = Msg {
                        slot,
                        value: job.captured,
                        epoch: job.epoch,
                        ..Msg::new(MSG_FRAME)
                    };
                    // The daemon closing its end is how a recording stops.
                    if send_msg(&socket, frame, None).is_err() {
                        break;
                    }
                    finish(Ok(job.buffer));
                }
                if dropped > 0 {
                    log::info!(
                        "Wayland capture helper: {dropped} frames dropped while the encoder caught up"
                    );
                }
            })?;
        Ok(Readback { jobs, free, thread })
    }
}

/// Copies `dst.len() / row` rows of `row` bytes from `src` (rows `stride`
/// apart, bottom-up when `inverted`) into the tightly packed `dst`.
fn copy_rows(src: &[u8], stride: usize, dst: &mut [u8], row: usize, inverted: bool) {
    let rows = dst.len() / row;
    if stride == row && !inverted {
        dst.copy_from_slice(&src[..dst.len()]);
        return;
    }
    for (y, out) in dst.chunks_exact_mut(row).enumerate() {
        let from = if inverted { rows - 1 - y } else { y };
        out.copy_from_slice(&src[from * stride..][..row]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let msg = Msg {
            kind: MSG_FRAME,
            slot: 5,
            value: u64::MAX - 7,
            epoch: 3,
        };
        assert_eq!(Msg::parse(&msg.bytes()), msg);
        for format in [
            PixelFormat::Bgra,
            PixelFormat::Bgrx,
            PixelFormat::Rgba,
            PixelFormat::Rgbx,
        ] {
            assert_eq!(format_of(format_code(format)), Some(format));
        }
        assert_eq!(format_of(4), None);
    }

    #[test]
    fn socket_carries_messages_and_fds() {
        let (a, b) = UnixStream::pair().unwrap();
        let file = tempfile::tempfile().unwrap();
        let msg = Msg {
            slot: 1,
            value: 640 << 32 | 480,
            ..Msg::new(MSG_HEADER)
        };
        send_msg(&a, msg, Some(file.as_raw_fd())).unwrap();
        send_msg(&a, Msg::new(MSG_PAUSE), None).unwrap();
        let (got, fd) = recv_msg(&b).unwrap().unwrap();
        assert_eq!(got, msg);
        assert!(fd.is_some());
        let (got, fd) = recv_msg(&b).unwrap().unwrap();
        assert_eq!(got.kind, MSG_PAUSE);
        assert!(fd.is_none());
        drop(a);
        assert!(recv_msg(&b).unwrap().is_none());
    }

    #[test]
    fn copies_rows_with_padding_and_inversion() {
        // Two rows of two pixels with 4 bytes of padding per row.
        let src: Vec<u8> = (0..24).collect();
        let mut dst = [0u8; 16];
        copy_rows(&src, 12, &mut dst, 8, false);
        assert_eq!(dst[..8], src[..8]);
        assert_eq!(dst[8..], src[12..20]);
        copy_rows(&src, 12, &mut dst, 8, true);
        assert_eq!(dst[..8], src[12..20]);
        assert_eq!(dst[8..], src[..8]);
        let mut tight = [0u8; 16];
        copy_rows(&src[..16], 8, &mut tight, 8, false);
        assert_eq!(tight[..], src[..16]);
    }

    #[test]
    fn pacer_keeps_phase_and_restarts_after_stalls() {
        let start = Instant::now();
        let ms = Duration::from_millis;
        let mut pacer = Pacer::new(50, start);
        assert_eq!(pacer.due(), start);
        pacer.issued(start);
        assert_eq!(pacer.due(), start + ms(20));
        // A late request (the previous frame came late) keeps the slots.
        pacer.issued(start + ms(27));
        assert_eq!(pacer.due(), start + ms(40));
        // More than a period late: restart one period after this request.
        pacer.issued(start + ms(500));
        assert_eq!(pacer.due(), start + ms(520));
        pacer.restart(start + ms(900));
        assert_eq!(pacer.due(), start + ms(900));
    }

    #[test]
    fn leases_lend_their_slot_and_hand_it_back() {
        let (daemon, helper) = UnixStream::pair().unwrap();
        let file = tempfile::tempfile().unwrap();
        file.set_len(4 * 8).unwrap();
        let mut map = unsafe { MmapMut::map_mut(&file) }.unwrap();
        map[8..16].copy_from_slice(b"slotone!");
        let ring = Arc::new(map.make_read_only().unwrap());
        let socket = Arc::new(daemon);
        let lease = Lease {
            ring,
            socket,
            slot: 1,
            len: 8,
        };
        let frame = Frame::from_pixels(
            2,
            1,
            8,
            PixelFormat::Rgbx,
            Pixels::Shared(Box::new(lease)),
            Instant::now(),
        );
        assert_eq!((frame.width(), frame.height()), (2, 1));
        drop(frame);
        let (msg, _) = recv_msg(&helper).unwrap().unwrap();
        assert_eq!((msg.kind, msg.slot), (MSG_RELEASE, 1));
    }
}
