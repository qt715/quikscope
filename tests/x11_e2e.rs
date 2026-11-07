//! Real daemon/client integration on a private Xvfb, never the caller's DISPLAY.
//! Set QUIKSCOPE_E2E=1 to require Xvfb, or QUIKSCOPE_XVFB to its executable.

use std::io::{BufRead as _, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use quikscope::raster::Image;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::randr::{ConnectionExt as _, MonitorInfo};
use x11rb::protocol::xproto::*;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

const W: u16 = 1024;
const H: u16 = 768;
const TIMEOUT: Duration = Duration::from_secs(30);

struct Session {
    dir: tempfile::TempDir,
    display: String,
    children: Vec<Child>,
    conn: RustConnection,
    root: Window,
    scale: u32,
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn executable(name: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|dir| dir.join(name).is_file())
}

impl Session {
    fn start() -> Option<Self> {
        Self::start_size("1024x768x24")
    }

    fn start_size(size: &str) -> Option<Self> {
        let xvfb = std::env::var("QUIKSCOPE_XVFB").unwrap_or_else(|_| "Xvfb".into());
        if !PathBuf::from(&xvfb).is_file() && !executable(&xvfb) {
            assert!(
                !std::env::var("QUIKSCOPE_E2E").is_ok_and(|v| v == "1"),
                "QUIKSCOPE_E2E=1 but Xvfb is absent"
            );
            eprintln!("skipping X11 e2e: install Xvfb or set QUIKSCOPE_XVFB");
            return None;
        }
        let dir = tempfile::Builder::new()
            .prefix("quikscope-x11-e2e-")
            .tempdir()
            .unwrap();
        for sub in ["home", "config/quikscope", "cache", "data", "run", "shots"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            dir.path().join("run"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("config/quikscope/config.toml"),
            format!(
                "save_dir = {:?}\n",
                dir.path().join("shots").to_str().unwrap()
            ),
        )
        .unwrap();
        // The server itself allocates an unused display. No guessed :99, real
        // Xauthority, global environment mutation, or connection to the host X.
        let mut server = Command::new(xvfb)
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                size,
                "-nolisten",
                "tcp",
                "-ac",
                "-noreset",
                "-extension",
                "BIG-REQUESTS",
            ])
            .env("HOME", dir.path().join("home"))
            .env_remove("DISPLAY")
            .env_remove("XAUTHORITY")
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(dir.path().join("xvfb.log")).unwrap())
            .spawn()
            .expect("start private Xvfb");
        let stdout = server.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = tx.send(result);
        });
        let display = match rx.recv_timeout(TIMEOUT) {
            Ok(Ok(line)) if line.trim().parse::<u32>().is_ok() => format!(":{}", line.trim()),
            result => {
                let _ = server.kill();
                let _ = server.wait();
                panic!(
                    "Xvfb -displayfd failed: {result:?}; {}",
                    std::fs::read_to_string(dir.path().join("xvfb.log")).unwrap()
                );
            }
        };
        let (conn, screen) = x11rb::connect(Some(&display)).unwrap();
        let root = conn.setup().roots[screen].root;
        assert_eq!(conn.setup().roots[screen].root_depth, 24);
        Some(Self {
            dir,
            display,
            children: vec![server],
            conn,
            root,
            scale: 1,
        })
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_quikscope"));
        for key in [
            "WAYLAND_DISPLAY",
            "WAYLAND_SOCKET",
            "XAUTHORITY",
            "HYPRLAND_INSTANCE_SIGNATURE",
            "SWAYSOCK",
        ] {
            cmd.env_remove(key);
        }
        for (key, sub) in [
            ("HOME", "home"),
            ("XDG_RUNTIME_DIR", "run"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_DATA_HOME", "data"),
        ] {
            cmd.env(key, self.dir.path().join(sub));
        }
        cmd.env("DISPLAY", &self.display)
            .env(
                "QUIKSCOPE_SOCKET",
                self.dir.path().join("run/quikscope.sock"),
            )
            .env("QUIKSCOPE_BACKEND", "x11")
            .env("GPUI_X11_SCALE_FACTOR", self.scale.to_string())
            .env("XDG_SESSION_TYPE", "x11")
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", self.dir.path().join("run/no-bus").display()),
            )
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env("RUST_LOG", "info");
        cmd
    }

    fn start_daemon(&mut self) {
        let child = self
            .command()
            .arg("daemon")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(self.dir.path().join("daemon.log")).unwrap())
            .spawn()
            .unwrap();
        self.children.push(child);
        wait_for("private daemon socket", || {
            if let Some(status) = self.children.last_mut().unwrap().try_wait().unwrap() {
                panic!(
                    "daemon exited {status}: {}",
                    std::fs::read_to_string(self.dir.path().join("daemon.log")).unwrap()
                );
            }
            self.dir.path().join("run/quikscope.sock").exists()
        });
        assert!(self.run(&["ping"]).status.success());
    }

    fn spawn(&self, args: &[&str]) -> Child {
        self.command()
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn finish(&self, mut child: Child) -> Output {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = child.kill();
        let out = child.wait_with_output().unwrap();
        panic!(
            "client timed out: {out:?}; daemon: {}",
            std::fs::read_to_string(self.dir.path().join("daemon.log")).unwrap()
        );
    }

    fn run(&self, args: &[&str]) -> Output {
        self.finish(self.spawn(args))
    }

    fn saved(&self, out: Output) -> Image {
        assert!(out.status.success(), "{out:?}");
        let path = PathBuf::from(String::from_utf8(out.stdout).unwrap().trim());
        assert!(path.starts_with(self.dir.path().join("shots")), "{path:?}");
        Image::decode(&std::fs::read(path).unwrap()).unwrap()
    }

    fn atom(&self, name: &[u8]) -> Atom {
        self.conn
            .intern_atom(false, name)
            .unwrap()
            .reply()
            .unwrap()
            .atom
    }

    fn pattern(&self) -> Image {
        let mut image = Image {
            width: u32::from(W),
            height: u32::from(H),
            data: Vec::with_capacity(usize::from(W) * usize::from(H) * 4),
        };
        // Deterministic noise resists PNG compression so clipboard ownership
        // must use INCR, rather than only exercising single-property replies.
        let mut state = 0x8a53_1b97u32;
        for _ in 0..u32::from(W) * u32::from(H) {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            image.data.extend_from_slice(&[
                state as u8,
                (state >> 8) as u8,
                (state >> 16) as u8,
                255,
            ]);
        }
        let pixmap = self.conn.generate_id().unwrap();
        let gc = self.conn.generate_id().unwrap();
        self.conn
            .create_pixmap(24, pixmap, self.root, W, H)
            .unwrap()
            .check()
            .unwrap();
        self.conn
            .create_gc(gc, pixmap, &CreateGCAux::new())
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(
            self.conn.setup().image_byte_order,
            ImageOrder::LSB_FIRST,
            "Xvfb pixel packing"
        );
        let stride = usize::from(W) * 4;
        for (row, bytes) in image.data.chunks(stride * 16).enumerate() {
            self.conn
                .put_image(
                    ImageFormat::Z_PIXMAP,
                    pixmap,
                    gc,
                    W,
                    (bytes.len() / stride) as u16,
                    0,
                    (row * 16) as i16,
                    0,
                    24,
                    bytes,
                )
                .unwrap()
                .check()
                .unwrap();
        }
        self.conn
            .change_window_attributes(
                self.root,
                &ChangeWindowAttributesAux::new().background_pixmap(pixmap),
            )
            .unwrap()
            .check()
            .unwrap();
        self.conn
            .clear_area(false, self.root, 0, 0, W, H)
            .unwrap()
            .check()
            .unwrap();
        self.conn.free_gc(gc).unwrap();
        self.conn.free_pixmap(pixmap).unwrap();
        self.conn.get_input_focus().unwrap().reply().unwrap();
        image
    }

    fn overlay(&self) {
        wait_for("mapped region overlay", || {
            self.conn
                .query_tree(self.root)
                .unwrap()
                .reply()
                .unwrap()
                .children
                .into_iter()
                .any(|window| {
                    self.conn
                        .get_window_attributes(window)
                        .unwrap()
                        .reply()
                        .is_ok_and(|a| a.map_state == MapState::VIEWABLE)
                        && self
                            .conn
                            .get_geometry(window)
                            .unwrap()
                            .reply()
                            .is_ok_and(|g| g.width >= W / 2 && g.height == H)
                })
        });
        // Mapping precedes input grabs; a roundtrip alone does not imply that
        // the daemon has processed its MapNotify and installed the handlers.
        std::thread::sleep(Duration::from_millis(150));
    }

    fn input(&self, kind: u8, detail: u8, x: i16, y: i16) {
        self.conn
            .xtest_fake_input(kind, detail, CURRENT_TIME, self.root, x, y, 0)
            .unwrap()
            .check()
            .unwrap();
        self.conn.flush().unwrap();
    }

    fn key(&self, keysym: u32) {
        let setup = self.conn.setup();
        let mapping = self
            .conn
            .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)
            .unwrap()
            .reply()
            .unwrap();
        let index = mapping
            .keysyms
            .chunks(usize::from(mapping.keysyms_per_keycode))
            .position(|symbols| symbols.contains(&keysym))
            .expect("Xvfb key mapping");
        let keycode = setup.min_keycode + index as u8;
        self.input(KEY_PRESS_EVENT, keycode, 0, 0);
        self.input(KEY_RELEASE_EVENT, keycode, 0, 0);
    }

    fn event(&self) -> Event {
        let mut result = None;
        wait_for("X11 selection event", || {
            result = self.conn.poll_for_event().unwrap();
            result.is_some()
        });
        result.unwrap()
    }

    fn clipboard(&self) -> (Vec<u8>, bool) {
        let window = self.conn.generate_id().unwrap();
        self.conn
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                self.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .unwrap()
            .check()
            .unwrap();
        let property = self.atom(b"QUIKSCOPE_TEST_PASTE");
        let png = self.atom(b"image/png");
        self.conn
            .convert_selection(window, self.atom(b"CLIPBOARD"), png, property, CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        loop {
            if let Event::SelectionNotify(event) = self.event() {
                assert_ne!(event.property, NONE, "owner refused image/png");
                break;
            }
        }
        let initial = self
            .conn
            .get_property(false, window, property, AtomEnum::ANY, 0, u32::MAX)
            .unwrap()
            .reply()
            .unwrap();
        let incremental = initial.type_ == self.atom(b"INCR");
        let mut bytes = initial.value;
        if incremental {
            bytes.clear();
            self.conn
                .delete_property(window, property)
                .unwrap()
                .check()
                .unwrap();
            loop {
                if !matches!(self.event(), Event::PropertyNotify(e) if e.window == window && e.atom == property && e.state == Property::NEW_VALUE)
                {
                    continue;
                }
                let chunk = self
                    .conn
                    .get_property(true, window, property, AtomEnum::ANY, 0, u32::MAX)
                    .unwrap()
                    .reply()
                    .unwrap();
                if chunk.type_ == NONE {
                    continue;
                }
                assert_eq!(chunk.type_, png);
                assert_eq!(chunk.format, 8);
                if chunk.value.is_empty() {
                    break;
                }
                bytes.extend_from_slice(&chunk.value);
            }
        } else {
            assert_eq!(initial.type_, png);
        }
        self.conn.destroy_window(window).unwrap().check().unwrap();
        (bytes, incremental)
    }

    fn split_monitors(&self) -> bool {
        let Ok(version) = self.conn.randr_query_version(1, 5).unwrap().reply() else {
            return false;
        };
        if (version.major_version, version.minor_version) < (1, 5) {
            return false;
        }
        for (index, name) in [b"QS-LEFT".as_slice(), b"QS-RIGHT".as_slice()]
            .into_iter()
            .enumerate()
        {
            let monitor = MonitorInfo {
                name: self.atom(name),
                primary: index == 0,
                automatic: false,
                x: (index as i16) * (W / 2) as i16,
                y: 0,
                width: W / 2,
                height: H,
                width_in_millimeters: 135,
                height_in_millimeters: 203,
                outputs: vec![],
            };
            if self
                .conn
                .randr_set_monitor(self.root, monitor)
                .unwrap()
                .check()
                .is_err()
            {
                return false;
            }
        }
        let monitors = self
            .conn
            .randr_get_monitors(self.root, false)
            .unwrap()
            .reply()
            .unwrap();
        [b"QS-LEFT".as_slice(), b"QS-RIGHT".as_slice()]
            .into_iter()
            .all(|name| monitors.monitors.iter().any(|m| m.name == self.atom(name)))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for child in self.children.iter_mut().rev() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn crop(image: &Image, x: u32, y: u32, width: u32, height: u32) -> Image {
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for row in y..y + height {
        let start = ((row * image.width + x) * 4) as usize;
        data.extend_from_slice(&image.data[start..start + (width * 4) as usize]);
    }
    Image {
        width,
        height,
        data,
    }
}

#[test]
fn screen_geometry_and_incr_clipboard_are_pixel_exact() {
    let Some(mut s) = Session::start() else {
        return;
    };
    let expected = s.pattern();
    s.start_daemon();
    let image = s.saved(s.run(&["capture", "screen", "--save", "--no-notify"]));
    assert_eq!(image, expected, "all screen pixels");
    let (png, incremental) = s.clipboard();
    assert!(
        png.len() > usize::from(s.conn.setup().maximum_request_length) * 4,
        "test must exceed the core X request limit"
    );
    assert!(incremental, "large PNG must exercise the INCR handshake");
    assert_eq!(Image::decode(&png).unwrap(), expected);
    let region = s.saved(s.run(&["capture", "-g", "73,51 237x139", "--save", "--no-notify"]));
    assert_eq!(region, crop(&expected, 73, 51, 237, 139));
    let (png, _) = s.clipboard();
    assert_eq!(Image::decode(&png).unwrap(), region);
}

#[test]
fn xtest_drag_escape_and_enter() {
    let Some(mut s) = Session::start() else {
        return;
    };
    s.scale = 2;
    let expected = s.pattern();
    s.start_daemon();
    let child = s.spawn(&["capture", "region", "--save", "--no-notify"]);
    s.overlay();
    s.input(MOTION_NOTIFY_EVENT, 0, 80, 60);
    s.input(BUTTON_PRESS_EVENT, 1, 80, 60);
    s.input(MOTION_NOTIFY_EVENT, 0, 390, 260);
    s.input(BUTTON_RELEASE_EVENT, 1, 390, 260);
    let region = s.saved(s.finish(child));
    assert_eq!(region, crop(&expected, 80, 60, 310, 200));
    assert_eq!(Image::decode(&s.clipboard().0).unwrap(), region);

    let child = s.spawn(&["capture", "region", "--no-notify"]);
    s.overlay();
    s.key(0xff1b); // Escape
    let out = s.finish(child);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cancelled"),
        "{out:?}"
    );

    // Xvfb has no compositing manager: a live request must clearly fall back
    // to the same frozen selection instead of putting black over the root.
    let child = s.spawn(&["capture", "region", "--no-freeze", "--save", "--no-notify"]);
    s.overlay();
    s.key(0xff0d); // Return: whole focused monitor
    assert_eq!(s.saved(s.finish(child)), expected);
    let log = std::fs::read_to_string(s.dir.path().join("daemon.log")).unwrap();
    assert!(log.contains("X11 has no compositing manager"), "{log}");
    assert!(log.contains("falling back to frozen mode"), "{log}");
}

#[test]
fn randr_monitor_layout_composes_and_selects_named_output() {
    let Some(mut s) = Session::start() else {
        return;
    };
    if !s.split_monitors() {
        eprintln!("skipping multi-monitor case: Xvfb cannot create RandR 1.5 monitors");
        return;
    }
    let expected = s.pattern();
    s.start_daemon();
    assert_eq!(
        s.saved(s.run(&["capture", "screen", "--save", "--no-notify"])),
        expected
    );
    let right = s.saved(s.run(&[
        "capture",
        "output",
        "-o",
        "QS-RIGHT",
        "--save",
        "--no-notify",
    ]));
    assert_eq!(
        right,
        crop(
            &expected,
            u32::from(W / 2),
            0,
            u32::from(W / 2),
            u32::from(H)
        )
    );
    let seam = s.saved(s.run(&["capture", "-g", "480,51 100x139", "--save", "--no-notify"]));
    assert_eq!(seam, crop(&expected, 480, 51, 100, 139));
}

#[test]
fn capture_latency_2560x1440() {
    let Some(s) = Session::start_size("2560x1440x24") else {
        return;
    };
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "capture_latency_worker", "--nocapture"])
        .env("DISPLAY", &s.display)
        .env_remove("XAUTHORITY")
        .env("QUIKSCOPE_X11_LATENCY_WORKER", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    eprintln!("{}", String::from_utf8_lossy(&out.stdout));
}

#[test]
fn capture_latency_worker() {
    if std::env::var("QUIKSCOPE_X11_LATENCY_WORKER").as_deref() != Ok("1") {
        return;
    }
    use quikscope::platform::{Capture as _, x11::ScreenCapture};
    let shm = ScreenCapture::spawn().unwrap();
    let plain = ScreenCapture::spawn_without_shm().unwrap();
    let reference = shm.capture(None, false).unwrap();
    assert_eq!(reference.outputs[0].image.width, 2560);
    assert_eq!(reference.outputs[0].image.height, 1440);
    assert_eq!(
        reference.outputs[0].image,
        plain.capture(None, false).unwrap().outputs[0].image
    );
    for (label, capture) in [("MIT-SHM", shm), ("GetImage", plain)] {
        let mut times = Vec::new();
        for index in 0..35 {
            let started = Instant::now();
            let snapshot = capture.capture(None, false).unwrap();
            std::hint::black_box(snapshot);
            if index >= 5 {
                times.push(started.elapsed().as_secs_f64() * 1000.);
            }
        }
        times.sort_by(f64::total_cmp);
        println!(
            "{label} 2560x1440 capture median {:.3} ms, p95 {:.3} ms (30 warm captures)",
            times[15], times[28]
        );
    }
}

#[test]
fn portal_uses_xwayland_clipboard_without_data_control() {
    let Some(mut s) = Session::start() else {
        return;
    };
    let mut worker = Command::new(std::env::current_exe().unwrap());
    worker
        .args(["--exact", "portal_clipboard_worker", "--nocapture"])
        .envs(
            s.command()
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .env("QUIKSCOPE_BACKEND", "portal")
        .env("QUIKSCOPE_PORTAL_CLIPBOARD_WORKER", "1")
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}/absent-bus", s.dir.path().display()),
        )
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("XAUTHORITY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = worker.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    s.children.push(child);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line == "clipboard-ready" {
                let _ = tx.send(());
            }
        }
    });
    rx.recv_timeout(TIMEOUT)
        .expect("portal clipboard fallback worker ready");
    let (png, _) = s.clipboard();
    assert_eq!(
        Image::decode(&png).unwrap(),
        Image::filled(128, 80, [13, 47, 193, 255])
    );
    let child = s.children.last_mut().unwrap();
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}

#[test]
fn portal_clipboard_worker() {
    if std::env::var("QUIKSCOPE_PORTAL_CLIPBOARD_WORKER").as_deref() != Ok("1") {
        return;
    }
    use std::io::Write as _;
    use std::sync::Arc;
    let platform = quikscope::platform::connect(Arc::new(|_| {}));
    assert_eq!(platform.name, "portal");
    let clipboard = platform.clipboard.expect("Xwayland clipboard fallback");
    let _png = clipboard.set_image(Arc::new(Image::filled(128, 80, [13, 47, 193, 255])));
    println!("clipboard-ready");
    std::io::stdout().flush().unwrap();
    let mut done = String::new();
    std::io::stdin().read_line(&mut done).unwrap();
}
