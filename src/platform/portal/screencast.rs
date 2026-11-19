//! ScreenCast portal transport and a deliberately CPU-only PipeWire consumer.
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use pipewire::{self as pw, properties::properties, spa};
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::{DEST, PATH, TOKEN};
use crate::geom::IRect;
use crate::media::{Frame, PixelFormat, Pixels, VideoSource};
use crate::platform::{RecordTarget, ScreenRecorder};

const INTERFACE: &str = "org.freedesktop.portal.ScreenCast";
type Results = HashMap<String, OwnedValue>;
type Streams = Vec<(u32, HashMap<String, OwnedValue>)>;

pub struct ScreenCast {
    connection: Connection,
    /// Restore tokens by output name (see [`ScreenCast::start`]).
    token_path: PathBuf,
    // A restore token is single-use; concurrent opens must not spend the same one.
    opening: std::sync::Mutex<()>,
}

impl ScreenCast {
    pub fn connect() -> Result<Self> {
        let state = std::env::var_os("XDG_STATE_HOME")
            .filter(|p| std::path::Path::new(p).is_absolute())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
            .context("no state directory for ScreenCast restore tokens")?;
        Ok(Self {
            connection: Connection::session().context("connecting to ScreenCast portal bus")?,
            token_path: state.join("quikscope/screencast-tokens.json"),
            opening: std::sync::Mutex::new(()),
        })
    }

    /// Starts a session sharing `target`'s output. GNOME and KDE restore a
    /// token's sources without a dialog, so tokens are kept per output: one
    /// token for every output would pin all recordings to the first monitor
    /// ever chosen. A restored session that shares another monitor (the
    /// layout changed, or the token predates a different choice) is closed
    /// and the token dropped, so a second attempt shows the dialog.
    fn start(&self, target: &RecordTarget, cursor: bool) -> Result<(Session, u32, OwnedFd)> {
        let _lock = self.opening.lock().unwrap();
        let mut tokens = load_tokens(&self.token_path)?;
        let restore = tokens.get(&target.output).cloned();
        let started = match self.attempt(target, cursor, restore.as_deref()) {
            Err(error) if restore.is_some() && error.is::<WrongOutput>() => {
                log::info!(
                    "ScreenCast permission restored for {} shares another monitor; asking again",
                    target.output
                );
                tokens.remove(&target.output);
                save_tokens(&self.token_path, &tokens)?;
                self.attempt(target, cursor, None)?
            }
            result => result?,
        };
        if let Some(token) = started.token {
            tokens.insert(target.output.clone(), token);
            save_tokens(&self.token_path, &tokens)?;
        }
        Ok((started.session, started.node, started.fd))
    }

    /// One session: CreateSession, SelectSources (restoring `restore`),
    /// Start, and the stream of `target`'s output. On failure the session is
    /// closed before returning.
    fn attempt(
        &self,
        target: &RecordTarget,
        cursor: bool,
        restore: Option<&str>,
    ) -> Result<Started> {
        let proxy = Proxy::new(&self.connection, DEST, PATH, INTERFACE)?;
        let session_token = token();
        let options =
            HashMap::from([("session_handle_token", Value::from(session_token.as_str()))]);
        let mut created = request(&self.connection, &proxy, "CreateSession", &(options,))?;
        let handle = created
            .remove("session_handle")
            .context("ScreenCast CreateSession omitted session_handle")?;
        let path = session_path(&handle)?;
        let session = Session {
            connection: self.connection.clone(),
            path,
        };
        let mut options = HashMap::from([
            ("types", Value::from(1u32)),
            ("multiple", Value::from(true)),
            ("cursor_mode", Value::from(if cursor { 2u32 } else { 1u32 })),
            ("persist_mode", Value::from(2u32)),
        ]);
        if let Some(restore) = restore {
            options.insert("restore_token", Value::from(restore));
        }
        request(
            &self.connection,
            &proxy,
            "SelectSources",
            &(&session.path, options),
        )?;
        let mut started = request(
            &self.connection,
            &proxy,
            "Start",
            &(&session.path, "", HashMap::<&str, Value<'_>>::new()),
        )?;
        let token = started
            .remove("restore_token")
            .map(String::try_from)
            .transpose()?;
        let streams: Streams = started
            .remove("streams")
            .context("ScreenCast Start omitted streams")?
            .try_into()?;
        // Checked before the new token is kept: it would restore the same
        // wrong selection.
        let node = choose_stream(&streams, target.output_rect)?;
        let fd: zbus::zvariant::OwnedFd = proxy.call(
            "OpenPipeWireRemote",
            &(&session.path, HashMap::<&str, Value<'_>>::new()),
        )?;
        Ok(Started {
            session,
            node,
            fd: fd.into(),
            token,
        })
    }
}

/// A started ScreenCast session and the stream of the requested output.
struct Started {
    session: Session,
    node: u32,
    fd: OwnedFd,
    /// The token restoring this session's sources, if the portal issued one.
    token: Option<String>,
}

/// The portal shared streams, but none of the requested output.
#[derive(Debug)]
struct WrongOutput;

impl std::fmt::Display for WrongOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "portal did not share the selected output; select that monitor in the permission dialog",
        )
    }
}

impl std::error::Error for WrongOutput {}

fn token() -> String {
    format!(
        "quikscope_{}_{}",
        std::process::id(),
        TOKEN.fetch_add(1, Ordering::Relaxed)
    )
}

/// The session object of a CreateSession response. The portal spec types
/// `session_handle` as a string (`s`), which is what GNOME's and KDE's
/// portals send; an object path (`o`) is accepted as well.
fn session_path(handle: &Value<'_>) -> Result<OwnedObjectPath> {
    match handle {
        Value::Str(path) => Ok(OwnedObjectPath::try_from(path.as_str())
            .context("ScreenCast session_handle is not an object path")?),
        Value::ObjectPath(path) => Ok(path.clone().into()),
        other => bail!(
            "ScreenCast session_handle has type {}",
            other.value_signature()
        ),
    }
}

// Each call subscribes before invoking the method: some backends reply immediately.
fn request<B: serde::Serialize + zbus::zvariant::DynamicType>(
    connection: &Connection,
    proxy: &Proxy<'_>,
    method: &str,
    body: &B,
) -> Result<Results> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(DEST)?
        .interface("org.freedesktop.portal.Request")?
        .member("Response")?
        .build();
    let responses = MessageIterator::for_match_rule(rule, connection, Some(32))?;
    let handle: OwnedObjectPath = proxy
        .call(method, body)
        .with_context(|| format!("ScreenCast {method}"))?;
    for response in responses {
        let response = response?;
        if response.header().path().map(|p| p.as_str()) != Some(handle.as_str()) {
            continue;
        }
        let (code, results): (u32, Results) = response.body().deserialize()?;
        return match code {
            0 => Ok(results),
            1 => Err(anyhow!("ScreenCast {method} cancelled by user")),
            _ => Err(anyhow!(
                "ScreenCast {method} failed (portal response {code})"
            )),
        };
    }
    bail!("ScreenCast portal disconnected during {method}")
}

/// Restore tokens by output name; none when the file is missing. A damaged
/// file costs a permission dialog, not the recording.
fn load_tokens(path: &std::path::Path) -> Result<HashMap<String, String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            log::warn!("ignoring damaged {}: {error}", path.display());
            HashMap::new()
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(e).context("reading ScreenCast restore tokens"),
    }
}

fn save_tokens(path: &std::path::Path, tokens: &HashMap<String, String>) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let parent = path.parent().context("invalid ScreenCast token path")?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("{}.tmp", self::token()));
    let result = (|| {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(tokens)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result.context("persisting ScreenCast restore tokens")
}

fn choose_stream(streams: &Streams, output: IRect) -> Result<u32> {
    for (node, properties) in streams {
        let position = properties
            .get("position")
            .and_then(|v| <(i32, i32)>::try_from(&**v).ok());
        let size = properties
            .get("size")
            .and_then(|v| <(i32, i32)>::try_from(&**v).ok());
        if position == Some((output.x, output.y)) && size == Some((output.w, output.h)) {
            return Ok(*node);
        }
    }
    // A single stream without geometry is common on GNOME: it is what the
    // user picked in the dialog for this output, or what a token of this
    // output restores (tokens are per output). Multiple unknown streams
    // cannot safely be mapped to a user-selected global rectangle.
    if let [(node, properties)] = streams.as_slice()
        && (!properties.contains_key("position") || !properties.contains_key("size"))
    {
        return Ok(*node);
    }
    Err(WrongOutput.into())
}

struct Session {
    connection: Connection,
    path: OwnedObjectPath,
}
impl Drop for Session {
    fn drop(&mut self) {
        if let Ok(proxy) = Proxy::new(
            &self.connection,
            DEST,
            self.path.as_str(),
            "org.freedesktop.portal.Session",
        ) {
            let _ = proxy.call::<_, _, ()>("Close", &());
        }
    }
}

impl ScreenRecorder for ScreenCast {
    fn open(&self, target: &RecordTarget, fps: u32, cursor: bool) -> Result<Box<dyn VideoSource>> {
        ensure!(fps > 0 && fps <= 1000, "invalid ScreenCast frame rate");
        ensure!(
            !target.output_rect.is_empty()
                && !target.region.is_empty()
                && target.region.intersect(&target.output_rect) == Some(target.region),
            "recording region must be inside its output"
        );
        let (session, node, fd) = self.start(target, cursor)?;
        let control = Arc::new(Control::default());
        let (frames_tx, frames) = mpsc::sync_channel(2);
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let thread_control = control.clone();
        let target = target.clone();
        let worker = std::thread::Builder::new()
            .name("quikscope-portal-video".into())
            .spawn(move || {
                if let Err(error) = consume(
                    fd,
                    node,
                    target,
                    fps,
                    &thread_control,
                    &ready_tx,
                    &frames_tx,
                ) {
                    let text = format!("{error:#}");
                    let _ = ready_tx.try_send(Err(anyhow!(text.clone())));
                    let _ = frames_tx.try_send(Err(anyhow!(text)));
                }
            })?;
        let mut source = Source {
            session: Some(session),
            control,
            worker: Some(worker),
            frames,
            size: (0, 0),
            epoch: 0,
        };
        source.size = ready
            .recv_timeout(Duration::from_secs(15))
            .context("waiting for portal video format")??;
        Ok(Box::new(source))
    }

    fn outline(&self, _region: IRect) -> Result<Box<dyn Send>> {
        // GNOME has no click-through overlay protocol. Even on KDE, a portal
        // does not promise that an overlay can be excluded from its stream;
        // prefer the desktop's sharing indicator over contaminating the video.
        Ok(Box::new(()))
    }
}

/// State the PipeWire thread shares with its [`Source`].
#[derive(Default)]
struct Control {
    stop: AtomicBool,
    paused: AtomicBool,
    /// Number of resumes; frames carry the value they were captured under.
    epoch: AtomicU64,
}

/// Cropped pixels from the PipeWire thread; turned into a [`Frame`] on the
/// session's thread.
struct Captured {
    epoch: u64,
    pixels: Pooled,
    width: u32,
    height: u32,
    format: PixelFormat,
    at: Instant,
}

/// Frame pixels lent to the pipeline; dropping them hands the buffer back
/// to the PipeWire thread, so a recording does not allocate (and fault in)
/// a frame-sized buffer per frame.
struct Pooled {
    pixels: Vec<u8>,
    pool: mpsc::Sender<Vec<u8>>,
}

impl AsRef<[u8]> for Pooled {
    fn as_ref(&self) -> &[u8] {
        &self.pixels
    }
}

impl Drop for Pooled {
    fn drop(&mut self) {
        // After the recording ended the buffer is simply freed.
        let _ = self.pool.send(std::mem::take(&mut self.pixels));
    }
}

struct Source {
    session: Option<Session>,
    control: Arc<Control>,
    worker: Option<JoinHandle<()>>,
    frames: mpsc::Receiver<Result<Captured>>,
    size: (u32, u32),
    epoch: u64,
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
                Ok(Ok(c)) if c.epoch != self.epoch => {}
                Ok(Ok(c)) => {
                    return Ok(Some(Frame::from_pixels(
                        c.width,
                        c.height,
                        c.width as usize * 4,
                        c.format,
                        Pixels::Shared(Box::new(c.pixels)),
                        c.at,
                    )));
                }
                Ok(Err(e)) => return Err(e),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(None),
                Err(_) => bail!("portal video stream disconnected"),
            }
        }
    }
    fn set_paused(&mut self, paused: bool) {
        if !paused {
            // Frames tagged before this point are pre-pause pictures.
            self.epoch = self.control.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        }
        self.control.paused.store(paused, Ordering::Relaxed);
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.session.take();
    }
}

fn crop(target: &RecordTarget, width: u32, height: u32) -> Result<(usize, usize, u32, u32)> {
    let output = target.output_rect;
    let region = target.region;
    let scale_x = width as f64 / output.w as f64;
    let scale_y = height as f64 / output.h as f64;
    let x = ((region.x - output.x) as f64 * scale_x).round() as usize;
    let y = ((region.y - output.y) as f64 * scale_y).round() as usize;
    let right = ((region.right() - output.x) as f64 * scale_x).round() as usize;
    let bottom = ((region.bottom() - output.y) as f64 * scale_y).round() as usize;
    ensure!(
        right > x && bottom > y && right <= width as usize && bottom <= height as usize,
        "invalid portal pixel crop"
    );
    Ok((x, y, (right - x) as u32, (bottom - y) as u32))
}

/// Packs the `w`×`h` pixels at (`x`, `y`) of rows `stride` bytes apart
/// tightly into `pixels`; `false` when `bytes` ends before the last of them.
fn crop_rows(
    bytes: &[u8],
    stride: usize,
    (x, y, w, h): (usize, usize, u32, u32),
    pixels: &mut Vec<u8>,
) -> bool {
    let row = w as usize * 4;
    if h == 0 || stride < x * 4 + row {
        return false;
    }
    let end = (y + h as usize - 1) * stride + x * 4 + row;
    if bytes.len() < end {
        return false;
    }
    pixels.clear();
    pixels.reserve(row * h as usize);
    for line in bytes[y * stride..end].chunks(stride) {
        pixels.extend_from_slice(&line[x * 4..x * 4 + row]);
    }
    true
}

/// Picks frames from a stream that runs at the compositor's rate so they
/// average `fps`: each accepted frame books the next slot one period later,
/// and a frame up to half a period early still takes its slot, so jitter
/// around the slot never drops frames that are on time.
struct Throttle {
    period: Duration,
    next: Option<Instant>,
}

impl Throttle {
    fn new(fps: u32) -> Throttle {
        Throttle {
            period: Duration::from_secs_f64(1.0 / f64::from(fps)),
            next: None,
        }
    }

    fn accept(&mut self, now: Instant) -> bool {
        if self.next.is_some_and(|next| now + self.period / 2 < next) {
            return false;
        }
        // More than a period behind (nothing changed on screen for a while):
        // start a new schedule instead of accepting a burst.
        let slot = self
            .next
            .filter(|&next| next + self.period > now)
            .unwrap_or(now);
        self.next = Some(slot + self.period);
        true
    }

    fn restart(&mut self) {
        self.next = None;
    }
}

fn consume(
    fd: OwnedFd,
    node: u32,
    target: RecordTarget,
    fps: u32,
    control: &Arc<Control>,
    ready: &mpsc::SyncSender<Result<(u32, u32)>>,
    frames: &mpsc::SyncSender<Result<Captured>>,
) -> Result<()> {
    use spa::param::{
        ParamType,
        format::{FormatProperties, MediaSubtype, MediaType},
        video::{VideoFormat, VideoInfoRaw},
    };
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_fd_rc(fd, None)?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "quikscope-screen",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video", *pw::keys::MEDIA_CATEGORY => "Capture", *pw::keys::MEDIA_ROLE => "Screen"
        },
    )?;
    struct Data {
        format: VideoInfoRaw,
        pixel: Option<PixelFormat>,
        size: Option<(u32, u32)>,
        error: Option<String>,
        throttle: Throttle,
        epoch: u64,
    }
    let error = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    let state_error = error.clone();
    let callback_error = error.clone();
    let ready = ready.clone();
    // Buffers of frames the pipeline dropped, reused for the next ones.
    let (returns, pool) = mpsc::channel::<Vec<u8>>();
    let frames = frames.clone();
    let format_target = target.clone();
    let process_control = control.clone();
    let _listener = stream
        .add_local_listener_with_user_data(Data {
            format: VideoInfoRaw::default(),
            pixel: None,
            size: None,
            error: None,
            throttle: Throttle::new(fps),
            epoch: 0,
        })
        .state_changed(move |_, _, old, state| match state {
            pw::stream::StreamState::Error(message) => {
                *state_error.borrow_mut() = Some(message.to_string());
            }
            pw::stream::StreamState::Unconnected
                if !matches!(old, pw::stream::StreamState::Unconnected) =>
            {
                *state_error.borrow_mut() = Some("portal video stream disconnected".into());
            }
            _ => {}
        })
        .param_changed(move |_, data, id, param| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else {
                return;
            };
            let result = (|| -> Result<()> {
                data.format
                    .parse(param)
                    .map_err(|e| anyhow!("invalid PipeWire format: {e:?}"))?;
                // A monitor has no transparency: alpha is not image content.
                data.pixel = Some(match data.format.format() {
                    VideoFormat::BGRx | VideoFormat::BGRA => PixelFormat::Bgrx,
                    VideoFormat::RGBx | VideoFormat::RGBA => PixelFormat::Rgbx,
                    other => bail!("portal negotiated unsupported video format {other:?}"),
                });
                let size = data.format.size();
                let (_, _, w, h) = crop(&format_target, size.width, size.height)?;
                if let Some(old) = data.size {
                    ensure!(old == (w, h), "portal output resized while recording");
                } else {
                    data.size = Some((w, h));
                    let _ = ready.try_send(Ok((w, h)));
                }
                Ok(())
            })();
            if let Err(e) = result {
                data.error = Some(format!("{e:#}"));
            }
        })
        .process(move |stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            if let Some(e) = data.error.take() {
                *callback_error.borrow_mut() = Some(e);
                return;
            }
            if process_control.paused.load(Ordering::Relaxed) {
                return;
            }
            let epoch = process_control.epoch.load(Ordering::Relaxed);
            if epoch != data.epoch {
                data.epoch = epoch;
                data.throttle.restart();
            }
            let (Some(format), Some(plane)) = (data.pixel, buffer.datas_mut().first_mut()) else {
                return;
            };
            let size = data.format.size();
            let region = match crop(&target, size.width, size.height) {
                Ok(region) => region,
                Err(e) => {
                    *callback_error.borrow_mut() = Some(format!("{e:#}"));
                    return;
                }
            };
            let chunk = plane.chunk();
            let (offset, len) = (chunk.offset() as usize, chunk.size() as usize);
            // Metadata-only updates (a moved cursor) come as empty buffers,
            // and a corrupted buffer is one bad frame: neither ends the
            // recording.
            if len == 0 || chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED) {
                return;
            }
            let Ok(stride) = usize::try_from(chunk.stride()) else {
                return;
            };
            let Some(bytes) = plane.data() else {
                *callback_error.borrow_mut() =
                    Some("portal did not provide mapped CPU video buffers".into());
                return;
            };
            let now = Instant::now();
            if !data.throttle.accept(now) {
                return;
            }
            let mut pixels = Pooled {
                pixels: pool.try_recv().unwrap_or_default(),
                pool: returns.clone(),
            };
            if !bytes
                .get(offset..)
                .and_then(|b| b.get(..len))
                .is_some_and(|b| crop_rows(b, stride, region, &mut pixels.pixels))
            {
                log::debug!("portal video: skipped a short buffer ({len} bytes, stride {stride})");
                return;
            }
            let (width, height) = (region.2, region.3);
            // A full queue means the encoder is behind: drop this frame.
            let _ = frames.try_send(Ok(Captured {
                epoch,
                pixels,
                width,
                height,
                format,
                at: now,
            }));
        })
        .register()?;
    // No modifier property and no DMA-BUF data type: negotiate mapped MemFd /
    // MemPtr buffers. Portal backends do the GPU readback, so arbitrary
    // desktop DMA-BUF modifiers never need importing here.
    let object = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        )
    );
    let serialized = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&serialized).context("invalid video format pod")?];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    let mut active = true;
    while !control.stop.load(Ordering::Relaxed) {
        // An inactive stream makes the compositor stop producing frames (no
        // copies while paused); compositors send a new frame when a stream
        // starts streaming again, so a resume never shows a stale picture.
        let paused = control.paused.load(Ordering::Relaxed);
        if paused == active {
            stream.set_active(!paused)?;
            active = !paused;
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(50)));
        if let Some(error) = error.borrow_mut().take() {
            bail!("PipeWire: {error}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead as _;
    use std::sync::Mutex;

    struct Fake {
        calls: Arc<Mutex<Vec<String>>>,
        restored: Arc<Mutex<Vec<Option<String>>>>,
        remote: Option<(u32, PathBuf)>,
        /// Monitors whose streams carry geometry; none: a single stream
        /// without geometry, as GNOME may send.
        monitors: Vec<(String, IRect)>,
        /// The monitor the user picks in the permission dialog.
        pick: Arc<Mutex<String>>,
        /// The source SelectSources chose for the next Start.
        selected: Mutex<String>,
    }

    impl Fake {
        fn new(
            calls: &Arc<Mutex<Vec<String>>>,
            restored: &Arc<Mutex<Vec<Option<String>>>>,
        ) -> Self {
            Fake {
                calls: calls.clone(),
                restored: restored.clone(),
                remote: None,
                monitors: Vec::new(),
                pick: Arc::new(Mutex::new("monitor".into())),
                selected: Mutex::new(String::new()),
            }
        }
    }

    async fn respond(
        connection: &zbus::Connection,
        header: zbus::message::Header<'_>,
        results: Results,
    ) -> OwnedObjectPath {
        let path: OwnedObjectPath = format!(
            "/org/freedesktop/portal/desktop/request/quikscope/r{}",
            TOKEN.fetch_add(1, Ordering::Relaxed)
        )
        .try_into()
        .unwrap();
        connection
            .emit_signal(
                header.sender().map(|s| s.as_str()),
                path.as_str(),
                "org.freedesktop.portal.Request",
                "Response",
                &(0u32, results),
            )
            .await
            .unwrap();
        path
    }

    #[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
    impl Fake {
        async fn create_session(
            &self,
            options: Results,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(connection)] connection: &zbus::Connection,
        ) -> OwnedObjectPath {
            assert!(options.contains_key("session_handle_token"));
            self.calls.lock().unwrap().push("create".into());
            respond(
                connection,
                header,
                // The portal spec types session_handle as a string.
                HashMap::from([(
                    "session_handle".into(),
                    Value::from("/org/freedesktop/portal/desktop/session/quikscope/test")
                        .try_into()
                        .unwrap(),
                )]),
            )
            .await
        }
        async fn select_sources(
            &self,
            _session: OwnedObjectPath,
            options: Results,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(connection)] connection: &zbus::Connection,
        ) -> OwnedObjectPath {
            assert_eq!(u32::try_from(options.get("types").unwrap()).unwrap(), 1);
            assert_eq!(
                u32::try_from(options.get("persist_mode").unwrap()).unwrap(),
                2
            );
            assert_eq!(
                u32::try_from(options.get("cursor_mode").unwrap()).unwrap(),
                2
            );
            self.calls.lock().unwrap().push("select".into());
            let token = options
                .get("restore_token")
                .map(|v| <&str>::try_from(v).unwrap().to_owned());
            // Like GNOME and KDE: a valid token restores its sources
            // silently; anything else shows the dialog.
            let restored = token
                .as_deref()
                .and_then(|t| t.strip_prefix("permission-"))
                .map(str::to_owned);
            *self.selected.lock().unwrap() = match restored {
                Some(source) => source,
                None => {
                    self.calls.lock().unwrap().push("dialog".into());
                    self.pick.lock().unwrap().clone()
                }
            };
            self.restored.lock().unwrap().push(token);
            respond(connection, header, HashMap::new()).await
        }
        async fn start(
            &self,
            _session: OwnedObjectPath,
            _parent: String,
            _options: Results,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(connection)] connection: &zbus::Connection,
        ) -> OwnedObjectPath {
            self.calls.lock().unwrap().push("start".into());
            let node = self.remote.as_ref().map_or(42, |r| r.0);
            let selected = self.selected.lock().unwrap().clone();
            let mut properties = HashMap::<String, OwnedValue>::new();
            if let Some((_, rect)) = self.monitors.iter().find(|(name, _)| *name == selected) {
                properties.insert(
                    "position".into(),
                    Value::from((rect.x, rect.y)).try_into().unwrap(),
                );
                properties.insert(
                    "size".into(),
                    Value::from((rect.w, rect.h)).try_into().unwrap(),
                );
            }
            let streams = vec![(node, properties)];
            respond(
                connection,
                header,
                HashMap::from([
                    ("streams".into(), Value::from(streams).try_into().unwrap()),
                    (
                        "restore_token".into(),
                        Value::from(format!("permission-{selected}"))
                            .try_into()
                            .unwrap(),
                    ),
                ]),
            )
            .await
        }
        fn open_pipe_wire_remote(
            &self,
            _session: OwnedObjectPath,
            _options: Results,
        ) -> zbus::zvariant::OwnedFd {
            self.calls.lock().unwrap().push("remote".into());
            if let Some((_, path)) = &self.remote {
                return OwnedFd::from(std::os::unix::net::UnixStream::connect(path).unwrap())
                    .into();
            }
            // The transport-only test still exercises UNIX_FD transfer.
            let (socket, _) = std::os::unix::net::UnixStream::pair().unwrap();
            OwnedFd::from(socket).into()
        }
    }

    struct FakeSession(Arc<Mutex<Vec<String>>>);
    #[zbus::interface(name = "org.freedesktop.portal.Session")]
    impl FakeSession {
        fn close(&self) {
            self.0.lock().unwrap().push("close".into());
        }
    }

    struct Bus(std::process::Child);
    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A private bus serving `fake`, and a recorder keeping its tokens in
    /// `directory`.
    fn serve(
        fake: Fake,
        directory: &std::path::Path,
    ) -> (Bus, zbus::blocking::Connection, ScreenCast) {
        let mut child = std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut address = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let bus = Bus(child);
        let calls = fake.calls.clone();
        let service = zbus::blocking::connection::Builder::address(address.trim())
            .unwrap()
            .name(DEST)
            .unwrap()
            .serve_at(PATH, fake)
            .unwrap()
            .serve_at(
                "/org/freedesktop/portal/desktop/session/quikscope/test",
                FakeSession(calls),
            )
            .unwrap()
            .build()
            .unwrap();
        let recorder = ScreenCast {
            connection: zbus::blocking::connection::Builder::address(address.trim())
                .unwrap()
                .build()
                .unwrap(),
            token_path: directory.join("state/screencast-tokens.json"),
            opening: Mutex::new(()),
        };
        (bus, service, recorder)
    }

    #[test]
    fn dbus_flow_restores_token_and_closes_session() {
        let directory = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let restored = Arc::new(Mutex::new(Vec::new()));
        let (_bus, _service, recorder) = serve(Fake::new(&calls, &restored), directory.path());
        let target = RecordTarget {
            output: "monitor".into(),
            output_rect: IRect::new(0, 0, 800, 600),
            region: IRect::new(20, 30, 200, 100),
        };
        for _ in 0..2 {
            let (session, node, _fd) = recorder.start(&target, true).unwrap();
            assert_eq!(node, 42);
            drop(session);
        }
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "create", "select", "dialog", "start", "remote", "close", "create", "select",
                "start", "remote", "close"
            ]
        );
        assert_eq!(
            *restored.lock().unwrap(),
            [None, Some("permission-monitor".into())]
        );
        assert_eq!(
            load_tokens(&recorder.token_path).unwrap(),
            HashMap::from([("monitor".into(), "permission-monitor".into())])
        );
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&recorder.token_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    /// GNOME/KDE restore a token's monitor without asking: recording another
    /// monitor must neither reuse it nor fail without ever showing a dialog.
    #[test]
    fn restore_tokens_are_per_output_and_a_wrong_restore_asks_again() {
        let directory = tempfile::tempdir().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let restored = Arc::new(Mutex::new(Vec::new()));
        let left = IRect::new(0, 0, 800, 600);
        let right = IRect::new(800, 0, 1024, 768);
        let pick = Arc::new(Mutex::new("left".to_owned()));
        let fake = Fake {
            monitors: vec![("left".into(), left), ("right".into(), right)],
            pick: pick.clone(),
            ..Fake::new(&calls, &restored)
        };
        let (_bus, _service, recorder) = serve(fake, directory.path());
        let target = |output: &str, rect: IRect| RecordTarget {
            output: output.into(),
            output_rect: rect,
            region: IRect::new(rect.x + 10, rect.y + 10, 100, 100),
        };
        let record = |target: &RecordTarget| {
            calls.lock().unwrap().clear();
            restored.lock().unwrap().clear();
            recorder
                .start(target, true)
                .map(|(session, ..)| drop(session))
        };
        let calls = || calls.lock().unwrap().clone();
        let tokens = || load_tokens(&recorder.token_path).unwrap();

        record(&target("left", left)).unwrap();
        assert!(calls().contains(&"dialog".into()));
        // Another output has no token yet: the dialog appears.
        *pick.lock().unwrap() = "right".into();
        record(&target("right", right)).unwrap();
        assert!(calls().contains(&"dialog".into()));
        assert_eq!(*restored.lock().unwrap(), [None]);
        assert_eq!(
            tokens(),
            HashMap::from([
                ("left".into(), "permission-left".into()),
                ("right".into(), "permission-right".into()),
            ])
        );
        // Each output restores its own monitor silently.
        record(&target("left", left)).unwrap();
        assert!(!calls().contains(&"dialog".into()));
        assert_eq!(*restored.lock().unwrap(), [Some("permission-left".into())]);

        // A token restoring another monitor (the layout changed): the
        // restored session closes and the dialog is shown.
        let mut stale = tokens();
        stale.insert("right".into(), "permission-left".into());
        save_tokens(&recorder.token_path, &stale).unwrap();
        record(&target("right", right)).unwrap();
        assert_eq!(
            calls(),
            [
                "create", "select", "start", "close", "create", "select", "dialog", "start",
                "remote", "close"
            ]
        );
        assert_eq!(tokens()["right"], "permission-right");

        // The user picks the wrong monitor in the dialog as well: the
        // recording fails and no token is left to restore that choice.
        save_tokens(&recorder.token_path, &stale).unwrap();
        *pick.lock().unwrap() = "left".into();
        let error = record(&target("right", right)).unwrap_err();
        assert!(error.is::<WrongOutput>(), "{error:#}");
        assert!(!tokens().contains_key("right"));
        assert_eq!(tokens()["left"], "permission-left");
        // Without a token, nothing is retried.
        assert!(
            record(&target("right", right))
                .unwrap_err()
                .is::<WrongOutput>()
        );
        assert_eq!(calls().iter().filter(|c| *c == "create").count(), 1);
    }

    #[test]
    fn damaged_token_file_only_costs_a_dialog() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("tokens.json");
        assert!(load_tokens(&path).unwrap().is_empty());
        std::fs::write(&path, "screencast-token-from-a-build").unwrap();
        assert!(load_tokens(&path).unwrap().is_empty());
    }

    #[test]
    fn selects_geometry_and_scales_crop() {
        let output = IRect::new(-800, 100, 800, 600);
        let properties = HashMap::from([
            (
                "position".into(),
                Value::from((-800i32, 100i32)).try_into().unwrap(),
            ),
            (
                "size".into(),
                Value::from((800i32, 600i32)).try_into().unwrap(),
            ),
        ]);
        let streams = vec![(1, HashMap::new()), (2, properties)];
        assert_eq!(choose_stream(&streams, output).unwrap(), 2);
        assert!(choose_stream(&streams, IRect::new(0, 0, 100, 100)).is_err());
        let target = RecordTarget {
            output: "monitor".into(),
            output_rect: output,
            region: IRect::new(-780, 130, 200, 100),
        };
        assert_eq!(crop(&target, 1600, 1200).unwrap(), (40, 60, 400, 200));
    }

    #[test]
    fn session_handle_is_a_string() {
        let path = "/org/freedesktop/portal/desktop/session/1_42/quikscope_1";
        assert_eq!(session_path(&Value::from(path)).unwrap().as_str(), path);
        let object = zbus::zvariant::ObjectPath::try_from(path).unwrap();
        assert_eq!(session_path(&Value::from(object)).unwrap().as_str(), path);
        assert!(session_path(&Value::from("not a path")).is_err());
        assert!(session_path(&Value::from(7u32)).is_err());
    }

    #[test]
    fn crops_rows_tightly() {
        // 3x3 pixels, rows padded to 16 bytes; take the 2x2 block at (1, 1).
        let bytes: Vec<u8> = (0..48).collect();
        // A reused buffer of a larger earlier frame keeps none of its bytes.
        let mut pixels = vec![0xaa; 64];
        assert!(crop_rows(&bytes, 16, (1, 1, 2, 2), &mut pixels));
        assert_eq!(pixels.len(), 16);
        assert_eq!(pixels[..8], bytes[20..28]);
        assert_eq!(pixels[8..], bytes[36..44]);
        // The last row only needs to reach the crop, not the full stride.
        assert!(crop_rows(&bytes[..44], 16, (1, 1, 2, 2), &mut pixels));
        assert!(!crop_rows(&bytes[..43], 16, (1, 1, 2, 2), &mut pixels));
        assert!(!crop_rows(&bytes, 8, (1, 1, 2, 2), &mut pixels));
        assert!(!crop_rows(&bytes, 16, (0, 0, 2, 0), &mut pixels));
    }

    #[test]
    fn dropped_frames_return_their_buffer() {
        let (returns, pool) = mpsc::channel();
        let pixels = Pooled {
            pixels: (0..16).collect(),
            pool: returns.clone(),
        };
        let frame = Frame::from_pixels(
            2,
            2,
            8,
            PixelFormat::Bgrx,
            Pixels::Shared(Box::new(pixels)),
            Instant::now(),
        );
        assert!(pool.try_recv().is_err());
        drop(frame);
        assert_eq!(pool.try_recv().unwrap(), (0..16).collect::<Vec<u8>>());
        // Once the recording ended, a late frame is simply freed.
        drop(pool);
        drop(Pooled {
            pixels: vec![0; 4],
            pool: returns,
        });
    }

    #[test]
    fn throttle_keeps_on_time_frames() {
        let start = Instant::now();
        let count = |fps: u32, source_hz: f64, jitter: f64| {
            let mut throttle = Throttle::new(fps);
            (0..(source_hz as usize * 10))
                .filter(|&i| {
                    // Alternate early and late arrivals around the source's ticks.
                    let offset = if i % 2 == 0 { jitter } else { -jitter };
                    let t = (i as f64 / source_hz + offset).max(0.0);
                    throttle.accept(start + Duration::from_secs_f64(t))
                })
                .count()
        };
        // 10 s of a jittery 60 Hz stream at 60 fps keeps every frame (a
        // "since the last frame" check dropped about half of them).
        assert_eq!(count(60, 60.0, 0.002), 600);
        // A 144 Hz stream is thinned to 60 fps on average.
        assert!((595..=605).contains(&count(60, 144.0, 0.0)));
        assert!((295..=305).contains(&count(30, 60.0, 0.001)));
        // After a pause in the stream the schedule restarts, no burst.
        let mut throttle = Throttle::new(10);
        assert!(throttle.accept(start));
        assert!(throttle.accept(start + Duration::from_secs(5)));
        assert!(!throttle.accept(start + Duration::from_millis(5010)));
        throttle.restart();
        assert!(throttle.accept(start + Duration::from_millis(5020)));
    }

    /// Requires pipewire, WirePlumber, pw-dump and GStreamer's PipeWire plugin.
    /// Every process has a private runtime and bus; no real monitor is captured.
    #[test]
    #[ignore = "requires private PipeWire/GStreamer services"]
    fn private_pipewire_video() {
        use std::process::{Command, Stdio};
        let runtime = tempfile::tempdir().unwrap();
        let spawn = |name: &str, args: &[&str], address: Option<&str>| {
            let mut command = Command::new(name);
            command
                .args(args)
                .env("PIPEWIRE_RUNTIME_DIR", runtime.path())
                .env("XDG_RUNTIME_DIR", runtime.path())
                // WirePlumber stores default devices under the state dir.
                .env("XDG_STATE_HOME", runtime.path().join("state"))
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Some(address) = address {
                command.env("DBUS_SESSION_BUS_ADDRESS", address);
            }
            Bus(command.spawn().unwrap())
        };
        let mut bus = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut address = String::new();
        std::io::BufReader::new(bus.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let _bus = Bus(bus);
        let address = address.trim();
        let _server = spawn("pipewire", &[], Some(address));
        let deadline = Instant::now() + Duration::from_secs(10);
        let socket = runtime.path().join("pipewire-0");
        while !socket.exists() {
            assert!(
                Instant::now() < deadline,
                "private PipeWire failed to start"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _manager = spawn("wireplumber", &[], Some(address));
        let _producer = spawn(
            "gst-launch-1.0",
            &[
                "-q",
                "videotestsrc",
                "is-live=true",
                "pattern=solid-color",
                "foreground-color=0xff123456",
                "!",
                "video/x-raw,format=BGRx,width=320,height=240,framerate=30/1",
                "!",
                "pipewiresink",
                "mode=provide",
                "stream-properties=props,node.name=quikscope-test-source,media.class=Video/Source",
            ],
            Some(address),
        );
        let node = loop {
            let dump = Command::new("pw-dump")
                .env("PIPEWIRE_RUNTIME_DIR", runtime.path())
                .env("XDG_RUNTIME_DIR", runtime.path())
                .output()
                .unwrap();
            if let Ok(objects) = serde_json::from_slice::<Vec<serde_json::Value>>(&dump.stdout)
                && let Some(node) = objects
                    .iter()
                    .find(|v| v["info"]["props"]["node.name"] == "quikscope-test-source")
            {
                break node["id"].as_u64().unwrap() as u32;
            }
            assert!(
                Instant::now() < deadline,
                "private video producer failed to start"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let _service = zbus::blocking::connection::Builder::address(address)
            .unwrap()
            .name(DEST)
            .unwrap()
            .serve_at(
                PATH,
                Fake {
                    remote: Some((node, socket)),
                    ..Fake::new(&calls, &Arc::new(Mutex::new(Vec::new())))
                },
            )
            .unwrap()
            .serve_at(
                "/org/freedesktop/portal/desktop/session/quikscope/test",
                FakeSession(calls.clone()),
            )
            .unwrap()
            .build()
            .unwrap();
        let recorder = ScreenCast {
            connection: zbus::blocking::connection::Builder::address(address)
                .unwrap()
                .build()
                .unwrap(),
            token_path: runtime.path().join("state/screencast-tokens.json"),
            opening: Mutex::new(()),
        };
        let target = RecordTarget {
            output: "test".into(),
            output_rect: IRect::new(0, 0, 320, 240),
            region: IRect::new(20, 30, 200, 100),
        };
        let mut source = recorder.open(&target, 10, true).unwrap();
        assert_eq!(source.size(), (200, 100));
        let frame = source
            .next_frame(Duration::from_secs(5))
            .unwrap()
            .expect("test source frame");
        assert_eq!((frame.width(), frame.height()), (200, 100));
        drop(source);
        assert_eq!(calls.lock().unwrap().last().unwrap(), "close");
        // GStreamer's pipewiresink needs a moment before it negotiates with
        // a new consumer (real portals create a node per session).
        std::thread::sleep(Duration::from_secs(2));

        // The consumer itself: pixels, frame rate, pause.
        let (session, node, fd) = recorder.start(&target, true).unwrap();
        let control = Arc::new(Control::default());
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let (frames_tx, frames) = mpsc::sync_channel(2);
        let worker = {
            let (control, target) = (control.clone(), target.clone());
            std::thread::spawn(move || {
                consume(fd, node, target, 10, &control, &ready_tx, &frames_tx).unwrap()
            })
        };
        assert_eq!(
            ready.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(),
            (200, 100)
        );
        let next = || {
            frames
                .recv_timeout(Duration::from_secs(5))
                .expect("test source frame")
                .unwrap()
        };
        let first = next();
        let mut last = first.at;
        for _ in 0..4 {
            let c = next();
            assert_eq!((c.width, c.height, c.format), (200, 100, PixelFormat::Bgrx));
            assert_eq!(c.pixels.pixels.len(), 200 * 100 * 4);
            // videotestsrc foreground-color 0xff123456 in BGRx byte order.
            assert_eq!(&c.pixels.pixels[..3], &[0x56, 0x34, 0x12]);
            assert_eq!(c.epoch, 0);
            last = c.at;
        }
        // 10 fps from a 30 Hz producer: about 100 ms apart on average.
        let spacing = last.duration_since(first.at) / 4;
        assert!(
            (Duration::from_millis(80)..Duration::from_millis(120)).contains(&spacing),
            "{spacing:?}"
        );
        control.paused.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(300));
        while frames.try_recv().is_ok() {}
        assert!(frames.recv_timeout(Duration::from_millis(500)).is_err());
        control.epoch.store(1, Ordering::Relaxed);
        control.paused.store(false, Ordering::Relaxed);
        assert_eq!(next().epoch, 1);
        control.stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        drop(session);
    }
}
