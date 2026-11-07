//! Line-based IPC between `quikscope` commands and the daemon.
//!
//! One request line per connection, one response line back. The format is
//! plain text so the socket can be driven with `socat` too:
//!
//! ```text
//! capture region edit geometry=10,20,300x200   ->  ok 3 300x200
//! open-last                                    ->  ok 3 300x200
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

use crate::geom::IRect;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Capture(Mode, CaptureOptions),
    OpenLast,
    Open(PathBuf),
    /// Open or focus the settings window.
    Settings,
    /// Turn capture notifications on/off (persisted to the config file).
    Notifications(Switch),
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
    Cancelled,
    Error(String),
}

impl Request {
    pub fn parse(line: &str) -> Result<Request, String> {
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
            "open-last" => Request::OpenLast,
            "settings" => Request::Settings,
            "open" => {
                // Paths may contain spaces: take the rest of the line verbatim.
                let path = line.trim_start()["open".len()..].trim();
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
            .and_then(|s| s.split_once('x'))
            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
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
    cmd.arg("daemon")
        .env("QUIKSCOPE_SOCKET", socket)
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
    {
        use std::os::windows::process::CommandExt as _;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no inherited console,
        // and no console is created (CREATE_NO_WINDOW is ignored when detached).
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
            Request::Settings,
            Request::Notifications(Switch::On),
            Request::Notifications(Switch::Off),
            Request::Notifications(Switch::Toggle),
            Request::Notifications(Switch::Status),
            Request::Ping,
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
            Response::Notifications(true),
            Response::Notifications(false),
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
