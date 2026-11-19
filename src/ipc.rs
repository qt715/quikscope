//! Line-based IPC between `quikscope` commands and the daemon.
//!
//! One request line per connection, one response line back. The format is
//! plain text so the socket can be driven with `socat` too:
//!
//! ```text
//! capture region edit geometry=10,20,300x200   ->  ok 3 300x200
//! open-last                                    ->  ok 3 300x200
//! pick                                         ->  ok color #0067ed
//! record video region                          ->  ok recording video 1280x720
//! record video                                 ->  ok recorded 4 1280x720 5120 /path/rec.mp4
//! record status                                ->  ok record video 5120 running | ok record saving | ok record idle
//! version                                      ->  ok version 0.3.0
//! ping                                         ->  ok pong
//! ```

use std::fmt;
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
#[cfg(windows)]
use uds_windows::UnixStream;

use anyhow::{Context as _, Result, bail};

use crate::document::Rgba;
use crate::geom::IRect;
use crate::media::{AudioDevice, Kind, OutputFormat};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Freeze the screen and select a rectangle or a window with the pointer.
    Region,
    /// The focused output (Hyprland) or `output=NAME`.
    Output,
    /// The active window (Hyprland only).
    Window,
    /// Every output, composed in the compositor layout.
    Screen,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Region => "region",
            Mode::Output => "output",
            Mode::Window => "window",
            Mode::Screen => "screen",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        Some(match s {
            "region" => Mode::Region,
            "output" | "monitor" => Mode::Output,
            "window" | "active" => Mode::Window,
            "screen" | "all" | "fullscreen" => Mode::Screen,
            _ => return None,
        })
    }
}

/// Per-request overrides of the config file. `None` means "use the config".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptureOptions {
    pub edit: Option<bool>,
    pub notify: Option<bool>,
    pub clipboard: Option<bool>,
    pub cursor: Option<bool>,
    /// Freeze before region selection, or select over the live desktop.
    pub freeze: Option<bool>,
    /// Also write the capture to the screenshots directory immediately.
    pub save: bool,
    pub output: Option<String>,
    /// Skip interactive selection and capture this logical rectangle.
    pub geometry: Option<IRect>,
    /// Extract the text in the capture and copy that instead of the image.
    pub text: bool,
}

/// Change to the capture-notification setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    On,
    Off,
    Toggle,
    /// Report without changing.
    Status,
}

impl Switch {
    pub fn parse(s: &str) -> Option<Switch> {
        Some(match s {
            "on" => Switch::On,
            "off" => Switch::Off,
            "toggle" => Switch::Toggle,
            "status" => Switch::Status,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Switch::On => "on",
            Switch::Off => "off",
            Switch::Toggle => "toggle",
            Switch::Status => "status",
        }
    }

    /// The new value given the current one.
    pub fn apply(self, current: bool) -> bool {
        match self {
            Switch::On => true,
            Switch::Off => false,
            Switch::Toggle => !current,
            Switch::Status => current,
        }
    }
}

/// A recording request; `None` fields use the `[record]` configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordOptions {
    pub kind: Kind,
    /// `Region` selects interactively unless `geometry` is given; `Output`
    /// and `Screen` record a whole output; `Window` the active window's rectangle.
    pub mode: Mode,
    pub fps: Option<u32>,
    pub format: Option<OutputFormat>,
    /// Audio device ids; `Some(vec![])` records no audio.
    pub audio: Option<Vec<String>>,
    pub cursor: Option<bool>,
    pub edit: Option<bool>,
    pub notify: Option<bool>,
    pub clipboard: Option<bool>,
    pub output: Option<String>,
    pub geometry: Option<IRect>,
}

impl RecordOptions {
    pub fn new(kind: Kind) -> RecordOptions {
        RecordOptions {
            kind,
            mode: Mode::Region,
            fps: None,
            format: None,
            audio: None,
            cursor: None,
            edit: None,
            notify: None,
            clipboard: None,
            output: None,
            geometry: None,
        }
    }

    /// Options for `record NAME`: `video`, `animation` (`anim`) and `gif`.
    /// `gif` also fixes the format; the other animation names use
    /// `record.animation_format`.
    pub fn named(name: &str) -> Option<RecordOptions> {
        let mut options = RecordOptions::new(Kind::parse(name)?);
        if name == "gif" {
            options.format = Some(OutputFormat::Gif);
        }
        Some(options)
    }
}

/// Controls the running recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordAction {
    Stop,
    Pause,
    Resume,
    TogglePause,
    /// Stop and delete what was recorded.
    Cancel,
    Status,
}

impl RecordAction {
    pub fn parse(s: &str) -> Option<RecordAction> {
        Some(match s {
            "stop" => RecordAction::Stop,
            "pause" => RecordAction::Pause,
            "resume" => RecordAction::Resume,
            "toggle-pause" => RecordAction::TogglePause,
            "cancel" => RecordAction::Cancel,
            "status" => RecordAction::Status,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RecordAction::Stop => "stop",
            RecordAction::Pause => "pause",
            RecordAction::Resume => "resume",
            RecordAction::TogglePause => "toggle-pause",
            RecordAction::Cancel => "cancel",
            RecordAction::Status => "status",
        }
    }
}

/// The running recording, as reported by `record status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordState {
    pub kind: Kind,
    pub elapsed_ms: u64,
    pub paused: bool,
}

/// The recording slot as `record status` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordStatus {
    /// Nothing recorded or being saved (also while a recording is starting).
    Idle,
    Recording(RecordState),
    /// A stopped or discarded recording is still being written or torn
    /// down; a new recording cannot start yet.
    Saving,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Capture(Mode, CaptureOptions),
    OpenLast,
    /// Open an image in the annotator, an animation in the annotator's
    /// animation mode, or a video in the video editor.
    Open(PathBuf),
    /// Open or focus the settings window.
    Settings,
    /// Turn capture notifications on/off (persisted to the config file).
    Notifications(Switch),
    /// Pick a color from anywhere on the desktop and show it in the color
    /// window.
    Pick,
    /// Start a recording, or stop the running one (so one key toggles).
    Record(RecordOptions),
    RecordControl(RecordAction),
    /// List recordable audio devices.
    AudioDevices,
    /// The daemon's version, so a client can tell an older daemon apart.
    Version,
    Ping,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    /// Capture or open succeeded: shot id, pixel size, and the saved path if any.
    Shot {
        id: u64,
        width: u32,
        height: u32,
        saved: Option<PathBuf>,
    },
    /// Text extracted from shot `id` (`capture … text`); empty when the
    /// capture had no readable text.
    Text {
        id: u64,
        text: String,
    },
    Pong,
    /// Capture notifications are now on (`true`) or off.
    Notifications(bool),
    /// The color picked with [`Request::Pick`].
    Color(Rgba),
    /// A recording started with this frame size.
    Recording {
        kind: Kind,
        width: u32,
        height: u32,
    },
    /// A recording finished and was saved to `path`.
    Recorded {
        id: u64,
        width: u32,
        height: u32,
        duration_ms: u64,
        path: PathBuf,
    },
    /// `record status` (and pause/resume/cancel): the recording slot.
    RecordStatus(RecordStatus),
    AudioDevices(Vec<AudioDevice>),
    /// The daemon's `CARGO_PKG_VERSION`.
    Version(String),
    Cancelled,
    Error(String),
}

impl Request {
    pub fn parse(line: &str) -> Result<Request, String> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        if line.contains('\n') {
            return Err("newlines are not supported in requests".into());
        }
        let mut words = line.split_whitespace();
        let cmd = words.next().ok_or("empty request")?;
        let req = match cmd {
            "capture" => {
                let mode = words.next().ok_or("capture needs a mode")?;
                let mode =
                    Mode::parse(mode).ok_or_else(|| format!("unknown capture mode {mode:?}"))?;
                let mut o = CaptureOptions::default();
                for w in words.by_ref() {
                    match w.split_once('=') {
                        Some(("output", v)) => o.output = Some(v.to_string()),
                        Some(("geometry", v)) => o.geometry = Some(v.parse()?),
                        Some(_) => return Err(format!("unknown capture option {w:?}")),
                        None => {
                            let (flag, value) = match w.strip_prefix("no-") {
                                Some(f) => (f, false),
                                None => (w, true),
                            };
                            match flag {
                                "edit" => o.edit = Some(value),
                                "notify" => o.notify = Some(value),
                                "clipboard" => o.clipboard = Some(value),
                                "cursor" => o.cursor = Some(value),
                                "freeze" => o.freeze = Some(value),
                                "save" if value => o.save = true,
                                "text" if value => o.text = true,
                                _ => return Err(format!("unknown capture option {w:?}")),
                            }
                        }
                    }
                }
                return Ok(Request::Capture(mode, o));
            }
            "record" => {
                let first = words
                    .next()
                    .ok_or("record needs video, animation or an action")?;
                if let Some(action) = RecordAction::parse(first) {
                    Request::RecordControl(action)
                } else {
                    let mut o = RecordOptions::named(first)
                        .ok_or_else(|| format!("unknown recording kind {first:?}"))?;
                    let kind = o.kind;
                    for w in words.by_ref() {
                        if let Some(mode) = Mode::parse(w) {
                            o.mode = mode;
                            continue;
                        }
                        match w.split_once('=') {
                            Some(("output", v)) => o.output = Some(v.to_string()),
                            Some(("geometry", v)) => o.geometry = Some(v.parse()?),
                            Some(("fps", v)) => {
                                o.fps = Some(
                                    v.parse()
                                        .ok()
                                        .filter(|fps| (1..=240).contains(fps))
                                        .ok_or_else(|| format!("fps {v:?} is not 1–240"))?,
                                )
                            }
                            Some(("format", v)) => {
                                let format = OutputFormat::parse(v)
                                    .ok_or_else(|| format!("unknown format {v:?}"))?;
                                if format.kind() != kind {
                                    return Err(format!(
                                        "{} is not a {} format",
                                        format.label(),
                                        kind.as_str()
                                    ));
                                }
                                o.format = Some(format);
                            }
                            Some(("audio", v)) => {
                                o.audio = Some(
                                    v.split(',')
                                        .filter(|d| !d.is_empty())
                                        .map(String::from)
                                        .collect(),
                                )
                            }
                            Some(_) => return Err(format!("unknown record option {w:?}")),
                            None => {
                                let (flag, value) = match w.strip_prefix("no-") {
                                    Some(f) => (f, false),
                                    None => (w, true),
                                };
                                match flag {
                                    "edit" => o.edit = Some(value),
                                    "notify" => o.notify = Some(value),
                                    "clipboard" => o.clipboard = Some(value),
                                    "cursor" => o.cursor = Some(value),
                                    "audio" if !value => o.audio = Some(Vec::new()),
                                    _ => return Err(format!("unknown record option {w:?}")),
                                }
                            }
                        }
                    }
                    return Ok(Request::Record(o));
                }
            }
            "audio-devices" => Request::AudioDevices,
            "open-last" => Request::OpenLast,
            "pick" => Request::Pick,
            "settings" => Request::Settings,
            "open" => {
                // Paths may contain spaces: take the rest of the line verbatim.
                let path = line.trim_start()["open".len()..]
                    .strip_prefix(' ')
                    .ok_or("open needs a space before its path")?;
                if path.is_empty() {
                    return Err("open needs a path".into());
                }
                return Ok(Request::Open(PathBuf::from(path)));
            }
            "notifications" | "notify" => {
                let s = words
                    .next()
                    .ok_or("notifications needs on, off, toggle or status")?;
                Request::Notifications(
                    Switch::parse(s)
                        .ok_or_else(|| format!("unknown notifications setting {s:?}"))?,
                )
            }
            "version" => Request::Version,
            "ping" => Request::Ping,
            "quit" => Request::Quit,
            other => return Err(format!("unknown command {other:?}")),
        };
        if let Some(extra) = words.next() {
            return Err(format!("unexpected argument {extra:?}"));
        }
        Ok(req)
    }
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Request::Capture(mode, o) => {
                write!(f, "capture {}", mode.as_str())?;
                for (name, v) in [
                    ("edit", o.edit),
                    ("notify", o.notify),
                    ("clipboard", o.clipboard),
                    ("cursor", o.cursor),
                    ("freeze", o.freeze),
                ] {
                    match v {
                        Some(true) => write!(f, " {name}")?,
                        Some(false) => write!(f, " no-{name}")?,
                        None => {}
                    }
                }
                if o.save {
                    write!(f, " save")?;
                }
                if o.text {
                    write!(f, " text")?;
                }
                if let Some(out) = &o.output {
                    write!(f, " output={out}")?;
                }
                if let Some(g) = &o.geometry {
                    write!(f, " geometry={g}")?;
                }
                Ok(())
            }
            Request::OpenLast => write!(f, "open-last"),
            Request::Open(p) => write!(f, "open {}", p.display()),
            Request::Settings => write!(f, "settings"),
            Request::Notifications(s) => write!(f, "notifications {}", s.as_str()),
            Request::Pick => write!(f, "pick"),
            Request::Record(o) => {
                write!(f, "record {} {}", o.kind.as_str(), o.mode.as_str())?;
                for (name, v) in [
                    ("edit", o.edit),
                    ("notify", o.notify),
                    ("clipboard", o.clipboard),
                    ("cursor", o.cursor),
                ] {
                    match v {
                        Some(true) => write!(f, " {name}")?,
                        Some(false) => write!(f, " no-{name}")?,
                        None => {}
                    }
                }
                if let Some(fps) = o.fps {
                    write!(f, " fps={fps}")?;
                }
                if let Some(format) = o.format {
                    write!(f, " format={format}")?;
                }
                match &o.audio {
                    Some(devices) if devices.is_empty() => write!(f, " no-audio")?,
                    Some(devices) => write!(f, " audio={}", devices.join(","))?,
                    None => {}
                }
                if let Some(out) = &o.output {
                    write!(f, " output={out}")?;
                }
                if let Some(g) = &o.geometry {
                    write!(f, " geometry={g}")?;
                }
                Ok(())
            }
            Request::RecordControl(action) => write!(f, "record {}", action.as_str()),
            Request::AudioDevices => write!(f, "audio-devices"),
            Request::Version => write!(f, "version"),
            Request::Ping => write!(f, "ping"),
            Request::Quit => write!(f, "quit"),
        }
    }
}

impl Response {
    pub fn parse(line: &str) -> Result<Response, String> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line == "ok pong" {
            return Ok(Response::Pong);
        }
        if line == "cancelled" {
            return Ok(Response::Cancelled);
        }
        if let Some(state) = line.strip_prefix("ok notifications ") {
            return match state {
                "on" => Ok(Response::Notifications(true)),
                "off" => Ok(Response::Notifications(false)),
                _ => Err(format!("malformed response {line:?}")),
            };
        }
        if let Some(hex) = line.strip_prefix("ok color ") {
            return crate::color::parse_hex(hex)
                .map(Response::Color)
                .ok_or_else(|| format!("malformed color in {line:?}"));
        }
        if let Some(rest) = line.strip_prefix("ok text ") {
            let (id, text) = rest.split_once(' ').unwrap_or((rest, ""));
            let id = id
                .parse()
                .map_err(|_| format!("malformed shot id in {line:?}"))?;
            return Ok(Response::Text {
                id,
                text: unescape(text),
            });
        }
        if let Some(msg) = line.strip_prefix("err ") {
            return Ok(Response::Error(msg.to_string()));
        }
        if let Some(json) = line.strip_prefix("ok audio ") {
            return serde_json::from_str(json)
                .map(Response::AudioDevices)
                .map_err(|e| format!("malformed audio device list: {e}"));
        }
        if let Some(version) = line.strip_prefix("ok version ") {
            return Ok(Response::Version(version.to_string()));
        }
        if let Some(rest) = line.strip_prefix("ok record ") {
            match rest {
                "idle" => return Ok(Response::RecordStatus(RecordStatus::Idle)),
                "saving" => return Ok(Response::RecordStatus(RecordStatus::Saving)),
                _ => {}
            }
            let malformed = || format!("malformed record status {line:?}");
            let mut parts = rest.split(' ');
            let kind = parts.next().and_then(Kind::parse).ok_or_else(malformed)?;
            let elapsed_ms = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(malformed)?;
            let paused = match parts.next() {
                Some("paused") => true,
                Some("running") => false,
                _ => return Err(malformed()),
            };
            return Ok(Response::RecordStatus(RecordStatus::Recording(
                RecordState {
                    kind,
                    elapsed_ms,
                    paused,
                },
            )));
        }
        if let Some(rest) = line.strip_prefix("ok recording ") {
            let malformed = || format!("malformed recording response {line:?}");
            let (kind, size) = rest.split_once(' ').ok_or_else(malformed)?;
            let kind = Kind::parse(kind).ok_or_else(malformed)?;
            let (width, height) = parse_size(size).ok_or_else(malformed)?;
            return Ok(Response::Recording {
                kind,
                width,
                height,
            });
        }
        if let Some(rest) = line.strip_prefix("ok recorded ") {
            let malformed = || format!("malformed recorded response {line:?}");
            let mut parts = rest.splitn(4, ' ');
            let id = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(malformed)?;
            let (width, height) = parts.next().and_then(parse_size).ok_or_else(malformed)?;
            let duration_ms = parts
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(malformed)?;
            let path = parts
                .next()
                .filter(|p| !p.is_empty())
                .ok_or_else(malformed)?;
            return Ok(Response::Recorded {
                id,
                width,
                height,
                duration_ms,
                path: PathBuf::from(path),
            });
        }
        let rest = line
            .strip_prefix("ok ")
            .ok_or_else(|| format!("malformed response {line:?}"))?;
        let mut parts = rest.splitn(3, ' ');
        let id = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("malformed shot id in {line:?}"))?;
        let (width, height) = parts
            .next()
            .and_then(parse_size)
            .ok_or_else(|| format!("malformed size in {line:?}"))?;
        let saved = parts.next().map(PathBuf::from);
        Ok(Response::Shot {
            id,
            width,
            height,
            saved,
        })
    }
}

/// `WIDTHxHEIGHT`.
fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// Keeps multi-line text on one protocol line: `\` and line breaks become
/// `\\`, `\n` and `\r`.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

impl fmt::Display for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Response::Shot {
                id,
                width,
                height,
                saved,
            } => {
                write!(f, "ok {id} {width}x{height}")?;
                if let Some(p) = saved {
                    write!(f, " {}", p.display())?;
                }
                Ok(())
            }
            Response::Text { id, text } => write!(f, "ok text {id} {}", escape(text)),
            Response::Pong => write!(f, "ok pong"),
            Response::Notifications(on) => {
                write!(f, "ok notifications {}", if *on { "on" } else { "off" })
            }
            Response::Color(c) => write!(f, "ok color {}", crate::color::to_hex(*c)),
            Response::Recording {
                kind,
                width,
                height,
            } => write!(f, "ok recording {} {width}x{height}", kind.as_str()),
            Response::Recorded {
                id,
                width,
                height,
                duration_ms,
                path,
            } => write!(
                f,
                "ok recorded {id} {width}x{height} {duration_ms} {}",
                path.display()
            ),
            Response::RecordStatus(RecordStatus::Idle) => write!(f, "ok record idle"),
            Response::RecordStatus(RecordStatus::Saving) => write!(f, "ok record saving"),
            Response::RecordStatus(RecordStatus::Recording(s)) => write!(
                f,
                "ok record {} {} {}",
                s.kind.as_str(),
                s.elapsed_ms,
                if s.paused { "paused" } else { "running" }
            ),
            Response::Version(version) => write!(f, "ok version {version}"),
            Response::AudioDevices(devices) => write!(
                f,
                "ok audio {}",
                serde_json::to_string(devices).expect("plain strings serialize")
            ),
            Response::Cancelled => write!(f, "cancelled"),
            // Newlines would split the reply into several protocol lines.
            Response::Error(msg) => write!(f, "err {}", msg.replace('\n', " ")),
        }
    }
}

/// Sends one request and waits for its response. `timeout` bounds the
/// whole exchange; interactive captures pass `None` because selection waits
/// on the user.
pub fn send(socket: &Path, req: &Request, timeout: Option<Duration>) -> Result<Response> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to {}", socket.display()))?;
    exchange(stream, req, timeout)
}

/// Like [`send`], but starts the daemon first when nothing is listening.
pub fn send_or_spawn(socket: &Path, req: &Request, timeout: Option<Duration>) -> Result<Response> {
    // No directory, no daemon (it creates the directory when it binds).
    // Checked first because Windows AF_UNIX reports a missing directory as
    // WSAENETDOWN, not NotFound, which would fail every command on a fresh
    // profile instead of starting the daemon.
    if socket.parent().is_some_and(|dir| !dir.exists()) {
        return exchange(spawn_daemon(socket)?, req, timeout);
    }
    let stream = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            spawn_daemon(socket)?
        }
        Err(e) => return Err(e).with_context(|| format!("connecting to {}", socket.display())),
    };
    exchange(stream, req, timeout)
}

fn exchange(mut stream: UnixStream, req: &Request, timeout: Option<Duration>) -> Result<Response> {
    if let Request::Open(path) = req {
        let path = path.to_str().context("IPC paths must be UTF-8")?;
        anyhow::ensure!(!path.contains('\n'), "IPC paths cannot contain newlines");
    }
    #[cfg(windows)]
    crate::platform::windows::allow_foreground();
    stream.set_read_timeout(timeout)?;
    writeln!(stream, "{req}")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .context("reading daemon response")?;
    if line.is_empty() {
        bail!("daemon closed the connection without replying");
    }
    Response::parse(&line).map_err(anyhow::Error::msg)
}

/// Starts a detached daemon and returns the first successful connection to it.
fn spawn_daemon(socket: &Path) -> Result<UnixStream> {
    #[cfg(unix)]
    use std::os::unix::process::CommandExt as _;
    let exe = std::env::current_exe().context("locating the quikscope executable")?;
    let log = crate::config::cache_dir().join("daemon.log");
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let log_file = std::fs::File::create(&log).ok();
    let mut cmd = std::process::Command::new(exe);
    // Detaching on Windows changes CWD; retain the client's socket location.
    cmd.arg("daemon")
        .env("QUIKSCOPE_SOCKET", std::path::absolute(socket)?)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    match log_file {
        Some(f) => cmd.stderr(f),
        None => cmd.stderr(std::process::Stdio::null()),
    };
    // Detach into its own session so closing the launching terminal (or the
    // compositor's exec helper exiting) does not take the daemon down.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    #[cfg(windows)]
    configure_windows_daemon(&mut cmd);
    cmd.spawn().context("starting the daemon")?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(stream) = UnixStream::connect(socket) {
            return Ok(stream);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    bail!("the daemon did not start within 5s; see {}", log.display())
}

#[cfg(windows)]
fn configure_windows_daemon(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt as _;
    // Never inherit an untrusted download directory as the DLL search CWD.
    cmd.current_dir(crate::config::cache_dir());
    // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no inherited console.
    cmd.creation_flags(0x0000_0008 | 0x0000_0200);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn detached_windows_daemon_does_not_inherit_cwd() {
        let mut cmd = std::process::Command::new("quikscope.exe");
        configure_windows_daemon(&mut cmd);
        assert_eq!(
            cmd.get_current_dir(),
            Some(crate::config::cache_dir().as_path())
        );
    }

    #[test]
    fn open_preserves_trailing_spaces_and_rejects_newlines() {
        assert_eq!(
            Request::parse("open /tmp/shot.png  \n"),
            Ok(Request::Open(PathBuf::from("/tmp/shot.png  ")))
        );
        assert!(Request::parse("open /tmp/a\nb").is_err());
    }

    #[test]
    fn capture_requests_round_trip() {
        let cases = [
            Request::Capture(Mode::Region, CaptureOptions::default()),
            Request::Capture(
                Mode::Output,
                CaptureOptions {
                    edit: Some(true),
                    notify: Some(false),
                    clipboard: Some(false),
                    cursor: Some(true),
                    freeze: Some(false),
                    save: true,
                    output: Some("DP-1".into()),
                    geometry: Some(IRect::new(-10, 5, 300, 200)),
                    text: true,
                },
            ),
            Request::OpenLast,
            Request::Open(PathBuf::from("/tmp/with space/shot.png")),
            Request::Open(PathBuf::from("/tmp/ leading and trailing ")),
            Request::Settings,
            Request::Notifications(Switch::On),
            Request::Notifications(Switch::Off),
            Request::Notifications(Switch::Toggle),
            Request::Notifications(Switch::Status),
            Request::Ping,
            Request::Pick,
            Request::Version,
            Request::Quit,
        ];
        for req in cases {
            assert_eq!(Request::parse(&req.to_string()), Ok(req.clone()), "{req}");
        }
    }

    #[test]
    fn freeze_overrides_round_trip_and_preserve_config_default() {
        for (flags, expected) in [
            ("", None),
            (" freeze", Some(true)),
            (" no-freeze", Some(false)),
            (" freeze no-freeze", Some(false)),
            (" no-freeze freeze", Some(true)),
        ] {
            let req = Request::parse(&format!("capture region{flags}")).unwrap();
            let Request::Capture(_, options) = &req else {
                unreachable!()
            };
            assert_eq!(options.freeze, expected, "{flags}");
            assert_eq!(Request::parse(&req.to_string()), Ok(req));
        }
    }

    #[test]
    fn capture_mode_aliases() {
        assert_eq!(
            Request::parse("capture fullscreen"),
            Ok(Request::Capture(Mode::Screen, CaptureOptions::default()))
        );
        assert_eq!(
            Request::parse("capture active"),
            Ok(Request::Capture(Mode::Window, CaptureOptions::default()))
        );
    }

    #[test]
    fn rejects_malformed_requests() {
        for bad in [
            "",
            "capture",
            "capture banana",
            "capture region wobble",
            "capture region geometry=1,2",
            "capture region no-save",
            "capture region freeze=false",
            "capture region no-no-freeze",
            "notifications",
            "notifications maybe",
            "notifications on off",
            "ping extra",
            "settings extra",
            "open",
            "launch",
        ] {
            assert!(Request::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn responses_round_trip() {
        let cases = [
            Response::Shot {
                id: 4,
                width: 2560,
                height: 1440,
                saved: None,
            },
            Response::Shot {
                id: 5,
                width: 1,
                height: 2,
                saved: Some(PathBuf::from("/home/u/Pictures/a b.png")),
            },
            Response::Pong,
            Response::Cancelled,
            Response::Error("no outputs".into()),
            Response::Version("0.3.0".into()),
            Response::RecordStatus(RecordStatus::Idle),
            Response::RecordStatus(RecordStatus::Saving),
            Response::RecordStatus(RecordStatus::Recording(RecordState {
                kind: Kind::Animation,
                elapsed_ms: 5120,
                paused: true,
            })),
            Response::Notifications(true),
            Response::Notifications(false),
            Response::Color(Rgba {
                r: 0,
                g: 0x67,
                b: 0xed,
                a: 255,
            }),
            Response::Color(Rgba {
                r: 1,
                g: 2,
                b: 3,
                a: 0x80,
            }),
            Response::Text {
                id: 7,
                text: "日本語\nline two\\n not a break\r\n".into(),
            },
            Response::Text {
                id: 8,
                text: String::new(),
            },
        ];
        for r in cases {
            assert_eq!(Response::parse(&format!("{r}\n")), Ok(r.clone()));
        }
    }

    #[test]
    fn record_status_lines_are_stable() {
        // Scripts parse these; an older client reading `saving` fails loudly
        // instead of misreporting idle.
        assert_eq!(
            Response::RecordStatus(RecordStatus::Saving).to_string(),
            "ok record saving"
        );
        assert_eq!(
            Response::RecordStatus(RecordStatus::Recording(RecordState {
                kind: Kind::Video,
                elapsed_ms: 1500,
                paused: false,
            }))
            .to_string(),
            "ok record video 1500 running"
        );
        assert_eq!(
            Response::Version(env!("CARGO_PKG_VERSION").into()).to_string(),
            format!("ok version {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn record_gif_fixes_the_format_but_other_animation_names_do_not() {
        let Ok(Request::Record(gif)) = Request::parse("record gif region") else {
            panic!()
        };
        assert_eq!(
            (gif.kind, gif.format),
            (Kind::Animation, Some(OutputFormat::Gif))
        );
        assert_eq!(
            Request::parse(&Request::Record(gif.clone()).to_string()),
            Ok(Request::Record(gif))
        );
        for name in ["animation", "anim"] {
            let Ok(Request::Record(o)) = Request::parse(&format!("record {name}")) else {
                panic!()
            };
            assert_eq!((o.kind, o.format), (Kind::Animation, None), "{name}");
        }
        assert!(
            Request::parse("record gif format=webp").is_ok_and(|r| matches!(
                r,
                Request::Record(RecordOptions {
                    format: Some(OutputFormat::Webp),
                    ..
                })
            ))
        );
    }

    #[test]
    fn text_responses_stay_on_one_line() {
        let r = Response::Text {
            id: 1,
            text: "a\nb".into(),
        };
        assert_eq!(r.to_string(), "ok text 1 a\\nb");
        assert_eq!(
            Request::parse("capture region text no-notify"),
            Ok(Request::Capture(
                Mode::Region,
                CaptureOptions {
                    text: true,
                    notify: Some(false),
                    ..Default::default()
                }
            ))
        );
        assert!(Request::parse("capture region no-text").is_err());
    }

    #[test]
    fn error_messages_stay_on_one_line() {
        let r = Response::Error("line one\nline two".into());
        assert_eq!(r.to_string(), "err line one line two");
    }

    #[test]
    fn send_reports_daemon_hangup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        #[cfg(unix)]
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        #[cfg(windows)]
        let listener = uds_windows::UnixListener::bind(&path).unwrap();
        let t = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&s).read_line(&mut line).unwrap();
            line
        });
        let err = send(&path, &Request::Ping, Some(Duration::from_secs(5))).unwrap_err();
        assert!(err.to_string().contains("without replying"), "{err}");
        assert_eq!(t.join().unwrap(), "ping\n");
    }
}
