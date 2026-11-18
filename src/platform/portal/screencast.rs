//! ScreenCast portal transport and a deliberately CPU-only PipeWire consumer.
use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use pipewire::{self as pw, properties::properties, spa};
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::{DEST, PATH, TOKEN};
use crate::geom::IRect;
use crate::media::{Frame, VideoSource};
use crate::platform::{RecordTarget, ScreenRecorder};

const INTERFACE: &str = "org.freedesktop.portal.ScreenCast";
type Results = HashMap<String, OwnedValue>;
type Streams = Vec<(u32, HashMap<String, OwnedValue>)>;

pub struct ScreenCast {
    connection: Connection,
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
            .context("no state directory for ScreenCast restore token")?;
        Ok(Self {
            connection: Connection::session().context("connecting to ScreenCast portal bus")?,
            token_path: state.join("quikscope/screencast-token"),
            opening: std::sync::Mutex::new(()),
        })
    }

    fn start(&self, target: &RecordTarget, cursor: bool) -> Result<(Session, u32, OwnedFd)> {
        let _lock = self.opening.lock().unwrap();
        let proxy = Proxy::new(&self.connection, DEST, PATH, INTERFACE)?;
        let session_token = token();
        let options = HashMap::from([
            ("session_handle_token", Value::from(session_token.as_str())),
        ]);
        let mut created = request(&self.connection, &proxy, "CreateSession", &(options,))?;
        let path: OwnedObjectPath = created.remove("session_handle")
            .context("ScreenCast CreateSession omitted session_handle")?.try_into()?;
        let session = Session { connection: self.connection.clone(), path };
        let restore = match std::fs::read_to_string(&self.token_path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("reading ScreenCast restore token"),
        };
        let mut options = HashMap::from([
            ("types", Value::from(1u32)),
            ("multiple", Value::from(true)),
            ("cursor_mode", Value::from(if cursor { 2u32 } else { 1u32 })),
            ("persist_mode", Value::from(2u32)),
        ]);
        if let Some(restore) = restore.as_deref() {
            options.insert("restore_token", Value::from(restore));
        }
        request(&self.connection, &proxy, "SelectSources", &(&session.path, options))?;
        let mut started = request(&self.connection, &proxy, "Start", &(&session.path, "", HashMap::<&str, Value<'_>>::new()))?;
        if let Some(value) = started.remove("restore_token") {
            let restore: String = value.try_into()?;
            save_token(&self.token_path, &restore)?;
        }
        let streams: Streams = started.remove("streams").context("ScreenCast Start omitted streams")?.try_into()?;
        let node = choose_stream(&streams, target.output_rect)?;
        let fd: zbus::zvariant::OwnedFd = proxy.call("OpenPipeWireRemote", &(&session.path, HashMap::<&str, Value<'_>>::new()))?;
        Ok((session, node, fd.into()))
    }
}

fn token() -> String {
    format!("quikscope_{}_{}", std::process::id(), TOKEN.fetch_add(1, Ordering::Relaxed))
}

// Each call subscribes before invoking the method: some backends reply immediately.
fn request<B: serde::Serialize + zbus::zvariant::DynamicType>(
    connection: &Connection, proxy: &Proxy<'_>, method: &str, body: &B,
) -> Result<Results> {
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal)
        .sender(DEST)?.interface("org.freedesktop.portal.Request")?.member("Response")?.build();
    let responses = MessageIterator::for_match_rule(rule, connection, Some(32))?;
    let handle: OwnedObjectPath = proxy.call(method, body).with_context(|| format!("ScreenCast {method}"))?;
    for response in responses {
        let response = response?;
        if response.header().path().map(|p| p.as_str()) != Some(handle.as_str()) { continue; }
        let (code, results): (u32, Results) = response.body().deserialize()?;
        return match code {
            0 => Ok(results),
            1 => Err(anyhow!("ScreenCast {method} cancelled by user")),
            _ => Err(anyhow!("ScreenCast {method} failed (portal response {code})")),
        };
    }
    bail!("ScreenCast portal disconnected during {method}")
}

fn save_token(path: &std::path::Path, token: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let parent = path.parent().context("invalid ScreenCast token path")?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("{}.tmp", self::token()));
    let result = (|| {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() { let _ = std::fs::remove_file(temporary); }
    result.context("persisting ScreenCast restore token")
}

fn choose_stream(streams: &Streams, output: IRect) -> Result<u32> {
    for (node, properties) in streams {
        let position = properties.get("position").and_then(|v| <(i32, i32)>::try_from(&**v).ok());
        let size = properties.get("size").and_then(|v| <(i32, i32)>::try_from(&**v).ok());
        if position == Some((output.x, output.y)) && size == Some((output.w, output.h)) { return Ok(*node); }
    }
    // A single stream without geometry is common on GNOME. Multiple unknown
    // streams cannot safely be mapped to a user-selected global rectangle.
    if let [(node, properties)] = streams.as_slice()
        && (!properties.contains_key("position") || !properties.contains_key("size")) {
        return Ok(*node);
    }
    bail!("portal did not share the selected output; select that monitor in the permission dialog")
}

struct Session { connection: Connection, path: OwnedObjectPath }
impl Drop for Session {
    fn drop(&mut self) {
        if let Ok(proxy) = Proxy::new(&self.connection, DEST, self.path.as_str(), "org.freedesktop.portal.Session") {
            let _ = proxy.call::<_, _, ()>("Close", &());
        }
    }
}

impl ScreenRecorder for ScreenCast {
    fn open(&self, target: &RecordTarget, fps: u32, cursor: bool) -> Result<Box<dyn VideoSource>> {
        ensure!(fps > 0 && fps <= 1000, "invalid ScreenCast frame rate");
        ensure!(!target.output_rect.is_empty() && !target.region.is_empty()
            && target.region.intersect(&target.output_rect) == Some(target.region), "recording region must be inside its output");
        let (session, node, fd) = self.start(target, cursor)?;
        let stop = Arc::new(AtomicBool::new(false));
        let (frames_tx, frames) = mpsc::sync_channel(2);
        let (ready_tx, ready) = mpsc::sync_channel(1);
        let thread_stop = stop.clone();
        let target = target.clone();
        let worker = std::thread::Builder::new().name("quikscope-portal-video".into()).spawn(move || {
            if let Err(error) = consume(fd, node, target, fps, thread_stop, &ready_tx, &frames_tx) {
                let text = format!("{error:#}");
                let _ = ready_tx.try_send(Err(anyhow!(text.clone())));
                let _ = frames_tx.try_send(Err(anyhow!(text)));
            }
        })?;
        let mut source = Source { session: Some(session), stop, worker: Some(worker), frames, size: (0, 0) };
        source.size = ready.recv_timeout(Duration::from_secs(15)).context("waiting for portal video format")??;
        Ok(Box::new(source))
    }

    fn outline(&self, _region: IRect) -> Result<Box<dyn Send>> {
        // GNOME has no click-through overlay protocol. Even on KDE, a portal
        // does not promise that an overlay can be excluded from its stream;
        // prefer the desktop's sharing indicator over contaminating the video.
        Ok(Box::new(()))
    }
}

struct Source {
    session: Option<Session>, stop: Arc<AtomicBool>, worker: Option<JoinHandle<()>>,
    frames: mpsc::Receiver<Result<Frame>>, size: (u32, u32),
}
impl VideoSource for Source {
    fn size(&self) -> (u32, u32) { self.size }
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>> {
        match self.frames.recv_timeout(timeout) {
            Ok(frame) => frame.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(_) => bail!("portal video stream disconnected"),
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() { let _ = worker.join(); }
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
    ensure!(right > x && bottom > y && right <= width as usize && bottom <= height as usize, "invalid portal pixel crop");
    Ok((x, y, (right - x) as u32, (bottom - y) as u32))
}

fn consume(fd: OwnedFd, node: u32, target: RecordTarget, fps: u32, stop: Arc<AtomicBool>,
    ready: &mpsc::SyncSender<Result<(u32, u32)>>, frames: &mpsc::SyncSender<Result<Frame>>) -> Result<()> {
    use spa::param::{ParamType, format::{FormatProperties, MediaType, MediaSubtype}, video::{VideoFormat, VideoInfoRaw}};
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_fd_rc(fd, None)?;
    let stream = pw::stream::StreamBox::new(&core, "quikscope-screen", properties! {
        *pw::keys::MEDIA_TYPE => "Video", *pw::keys::MEDIA_CATEGORY => "Capture", *pw::keys::MEDIA_ROLE => "Screen"
    })?;
    struct Data { format: VideoInfoRaw, last: Option<Instant>, size: Option<(u32, u32)>, error: Option<String> }
    let error = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    let state_error = error.clone();
    let callback_error = error.clone();
    let ready = ready.clone();
    let frames = frames.clone();
    let format_target = target.clone();
    let _listener = stream.add_local_listener_with_user_data(Data { format: VideoInfoRaw::default(), last: None, size: None, error: None })
        .state_changed(move |_, _, old, state| {
            match state {
                pw::stream::StreamState::Error(message) => {
                    *state_error.borrow_mut() = Some(message.to_string());
                }
                pw::stream::StreamState::Unconnected
                    if !matches!(old, pw::stream::StreamState::Unconnected) =>
                {
                    *state_error.borrow_mut() = Some("portal video stream disconnected".into());
                }
                _ => {}
            }
        })
        .param_changed(move |_, data, id, param| {
            if id != ParamType::Format.as_raw() { return; }
            let Some(param) = param else { return; };
            let result = (|| -> Result<()> {
                data.format.parse(param).map_err(|e| anyhow!("invalid PipeWire format: {e:?}"))?;
                ensure!(data.format.format() == VideoFormat::BGRx, "portal negotiated a non-BGRx format");
                let size = data.format.size();
                let (_, _, w, h) = crop(&format_target, size.width, size.height)?;
                if let Some(old) = data.size { ensure!(old == (w, h), "portal output resized while recording"); }
                else { data.size = Some((w, h)); let _ = ready.try_send(Ok((w, h))); }
                Ok(())
            })();
            if let Err(e) = result { data.error = Some(format!("{e:#}")); }
        })
        .process(move |stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return; };
            if let Some(e) = data.error.take() { *callback_error.borrow_mut() = Some(e); return; }
            let now = Instant::now();
            if data.last.is_some_and(|last| now.duration_since(last) < Duration::from_secs_f64(1.0 / fps as f64)) { return; }
            let Some(plane) = buffer.datas_mut().first_mut() else { return; };
            let size = data.format.size();
            let result = (|| -> Result<Frame> {
                let (x, y, w, h) = crop(&target, size.width, size.height)?;
                let chunk = plane.chunk();
                ensure!(!chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED), "corrupt PipeWire video buffer");
                let stride = usize::try_from(chunk.stride()).context("negative PipeWire stride")?;
                ensure!(stride >= size.width as usize * 4, "invalid PipeWire stride");
                let offset = chunk.offset() as usize;
                let available = chunk.size() as usize;
                let start = y * stride + x * 4;
                let end = start + (h as usize - 1) * stride + w as usize * 4;
                let bytes = plane.data().context("portal did not provide mapped CPU video buffers")?;
                ensure!(end <= available && offset.checked_add(end).is_some_and(|end| end <= bytes.len()), "short PipeWire video buffer");
                Ok(Frame::from_bgra(w, h, stride, &bytes[offset + start..offset + end], true, now))
            })();
            match result {
                Ok(frame) => { if frames.try_send(Ok(frame)).is_ok() { data.last = Some(now); } }
                Err(e) => { *callback_error.borrow_mut() = Some(format!("{e:#}")); }
            }
        }).register()?;
    // No modifier property and no DMA-BUF data type: negotiate mapped MemFd /
    // MemPtr BGRx instead. Portal backends own GPU readback; importing arbitrary
    // desktop DMA-BUF modifiers into FFmpeg is deliberately not assumed here.
    let object = spa::pod::object!(spa::utils::SpaTypes::ObjectParamFormat, ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRx));
    let serialized = spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(object))?.0.into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&serialized).context("invalid video format pod")?];
    stream.connect(spa::utils::Direction::Input, Some(node), pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS, &mut params)?;
    while !stop.load(Ordering::Relaxed) {
        mainloop.loop_().iterate(pw::loop_::Timeout::Finite(Duration::from_millis(50)));
        if let Some(error) = error.borrow_mut().take() { bail!("PipeWire: {error}"); }
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
    }

    async fn respond(connection: &zbus::Connection, header: zbus::message::Header<'_>, results: Results) -> OwnedObjectPath {
        let path: OwnedObjectPath = format!("/org/freedesktop/portal/desktop/request/quikscope/r{}", TOKEN.fetch_add(1, Ordering::Relaxed)).try_into().unwrap();
        connection.emit_signal(header.sender().map(|s| s.as_str()), path.as_str(), "org.freedesktop.portal.Request", "Response", &(0u32, results)).await.unwrap();
        path
    }

    #[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
    impl Fake {
        async fn create_session(&self, options: Results, #[zbus(header)] header: zbus::message::Header<'_>, #[zbus(connection)] connection: &zbus::Connection) -> OwnedObjectPath {
            assert!(options.contains_key("session_handle_token"));
            self.calls.lock().unwrap().push("create".into());
            respond(connection, header, HashMap::from([("session_handle".into(), Value::from(OwnedObjectPath::try_from("/org/freedesktop/portal/desktop/session/quikscope/test").unwrap()).try_into().unwrap())])).await
        }
        async fn select_sources(&self, _session: OwnedObjectPath, options: Results, #[zbus(header)] header: zbus::message::Header<'_>, #[zbus(connection)] connection: &zbus::Connection) -> OwnedObjectPath {
            assert_eq!(u32::try_from(options.get("types").unwrap()).unwrap(), 1);
            assert_eq!(u32::try_from(options.get("persist_mode").unwrap()).unwrap(), 2);
            assert_eq!(u32::try_from(options.get("cursor_mode").unwrap()).unwrap(), 2);
            self.calls.lock().unwrap().push("select".into());
            self.restored.lock().unwrap().push(options.get("restore_token").map(|v| <&str>::try_from(v).unwrap().to_owned()));
            respond(connection, header, HashMap::new()).await
        }
        async fn start(&self, _session: OwnedObjectPath, _parent: String, _options: Results, #[zbus(header)] header: zbus::message::Header<'_>, #[zbus(connection)] connection: &zbus::Connection) -> OwnedObjectPath {
            self.calls.lock().unwrap().push("start".into());
            let node = self.remote.as_ref().map_or(42, |r| r.0);
            let streams = vec![(node, HashMap::<String, OwnedValue>::new())];
            respond(connection, header, HashMap::from([
                ("streams".into(), Value::from(streams).try_into().unwrap()),
                ("restore_token".into(), Value::from("restored-permission").try_into().unwrap()),
            ])).await
        }
        fn open_pipe_wire_remote(&self, _session: OwnedObjectPath, _options: Results) -> zbus::zvariant::OwnedFd {
            self.calls.lock().unwrap().push("remote".into());
            if let Some((_, path)) = &self.remote {
                return OwnedFd::from(std::os::unix::net::UnixStream::connect(path).unwrap()).into();
            }
            // The transport-only test still exercises UNIX_FD transfer.
            let (socket, _) = std::os::unix::net::UnixStream::pair().unwrap();
            OwnedFd::from(socket).into()
        }
    }

    struct FakeSession(Arc<Mutex<Vec<String>>>);
    #[zbus::interface(name = "org.freedesktop.portal.Session")]
    impl FakeSession {
        fn close(&self) { self.0.lock().unwrap().push("close".into()); }
    }

    struct Bus(std::process::Child);
    impl Drop for Bus {
        fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
    }

    #[test]
    fn dbus_flow_restores_token_and_closes_session() {
        let directory = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("dbus-daemon").args(["--session", "--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped()).spawn().unwrap();
        let mut address = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut address).unwrap();
        let _bus = Bus(child);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let restored = Arc::new(Mutex::new(Vec::new()));
        let _service = zbus::blocking::connection::Builder::address(address.trim()).unwrap().name(DEST).unwrap()
            .serve_at(PATH, Fake { calls: calls.clone(), restored: restored.clone(), remote: None }).unwrap()
            .serve_at("/org/freedesktop/portal/desktop/session/quikscope/test", FakeSession(calls.clone())).unwrap().build().unwrap();
        let recorder = ScreenCast {
            connection: zbus::blocking::connection::Builder::address(address.trim()).unwrap().build().unwrap(),
            token_path: directory.path().join("state/screencast-token"),
            opening: Mutex::new(()),
        };
        let target = RecordTarget { output: "monitor".into(), output_rect: IRect::new(0, 0, 800, 600), region: IRect::new(20, 30, 200, 100) };
        for _ in 0..2 {
            let (session, node, _fd) = recorder.start(&target, true).unwrap();
            assert_eq!(node, 42);
            drop(session);
        }
        assert_eq!(*calls.lock().unwrap(), ["create", "select", "start", "remote", "close", "create", "select", "start", "remote", "close"]);
        assert_eq!(*restored.lock().unwrap(), [None, Some("restored-permission".into())]);
        assert_eq!(std::fs::read_to_string(&recorder.token_path).unwrap(), "restored-permission");
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(std::fs::metadata(&recorder.token_path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn selects_geometry_and_scales_crop() {
        let output = IRect::new(-800, 100, 800, 600);
        let properties = HashMap::from([
            ("position".into(), Value::from((-800i32, 100i32)).try_into().unwrap()),
            ("size".into(), Value::from((800i32, 600i32)).try_into().unwrap()),
        ]);
        let streams = vec![(1, HashMap::new()), (2, properties)];
        assert_eq!(choose_stream(&streams, output).unwrap(), 2);
        assert!(choose_stream(&streams, IRect::new(0, 0, 100, 100)).is_err());
        let target = RecordTarget { output: "monitor".into(), output_rect: output, region: IRect::new(-780, 130, 200, 100) };
        assert_eq!(crop(&target, 1600, 1200).unwrap(), (40, 60, 400, 200));
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
            command.args(args).env("PIPEWIRE_RUNTIME_DIR", runtime.path()).env("XDG_RUNTIME_DIR", runtime.path())
                .stdout(Stdio::null()).stderr(Stdio::null());
            if let Some(address) = address { command.env("DBUS_SESSION_BUS_ADDRESS", address); }
            Bus(command.spawn().unwrap())
        };
        let mut bus = Command::new("dbus-daemon").args(["--session", "--nofork", "--print-address=1"]).stdout(Stdio::piped()).spawn().unwrap();
        let mut address = String::new();
        std::io::BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address).unwrap();
        let _bus = Bus(bus);
        let address = address.trim();
        let _server = spawn("pipewire", &[], Some(address));
        let deadline = Instant::now() + Duration::from_secs(10);
        let socket = runtime.path().join("pipewire-0");
        while !socket.exists() {
            assert!(Instant::now() < deadline, "private PipeWire failed to start");
            std::thread::sleep(Duration::from_millis(20));
        }
        let _manager = spawn("wireplumber", &[], Some(address));
        let _producer = spawn("gst-launch-1.0", &["-q", "videotestsrc", "is-live=true", "pattern=solid-color", "foreground-color=0xff123456",
            "!", "video/x-raw,format=BGRx,width=320,height=240,framerate=30/1", "!", "pipewiresink", "mode=provide",
            "stream-properties=props,node.name=quikscope-test-source,media.class=Video/Source"], Some(address));
        let node = loop {
            let dump = Command::new("pw-dump").env("PIPEWIRE_RUNTIME_DIR", runtime.path()).env("XDG_RUNTIME_DIR", runtime.path()).output().unwrap();
            if let Ok(objects) = serde_json::from_slice::<Vec<serde_json::Value>>(&dump.stdout)
                && let Some(node) = objects.iter().find(|v| v["info"]["props"]["node.name"] == "quikscope-test-source") {
                break node["id"].as_u64().unwrap() as u32;
            }
            assert!(Instant::now() < deadline, "private video producer failed to start");
            std::thread::sleep(Duration::from_millis(20));
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let _service = zbus::blocking::connection::Builder::address(address).unwrap().name(DEST).unwrap()
            .serve_at(PATH, Fake { calls: calls.clone(), restored: Arc::new(Mutex::new(Vec::new())), remote: Some((node, socket)) }).unwrap()
            .serve_at("/org/freedesktop/portal/desktop/session/quikscope/test", FakeSession(calls.clone())).unwrap().build().unwrap();
        let recorder = ScreenCast {
            connection: zbus::blocking::connection::Builder::address(address).unwrap().build().unwrap(),
            token_path: runtime.path().join("state/screencast-token"), opening: Mutex::new(()),
        };
        let target = RecordTarget { output: "test".into(), output_rect: IRect::new(0, 0, 320, 240), region: IRect::new(20, 30, 200, 100) };
        let mut source = recorder.open(&target, 10, true).unwrap();
        assert_eq!(source.size(), (200, 100));
        let mut previous = None;
        for _ in 0..3 {
            let frame = source.next_frame(Duration::from_secs(5)).unwrap().expect("test source frame");
            assert_eq!((frame.width(), frame.height()), (200, 100));
            assert_eq!(&frame.av().data(0)[..3], &[0x56, 0x34, 0x12]);
            if let Some(previous) = previous { assert!(frame.captured.duration_since(previous) >= Duration::from_millis(100)); }
            previous = Some(frame.captured);
        }
        drop(source);
        assert_eq!(calls.lock().unwrap().last().unwrap(), "close");
    }
}
