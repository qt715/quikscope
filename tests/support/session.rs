//! A private headless desktop for end-to-end tests: a wlroots compositor
//! (labwc or sway) with the pixman renderer, a private D-Bus session with a
//! fake notification server, and a `moment` daemon — all in a temporary
//! directory, never touching the developer's real session.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zbus::zvariant::OwnedValue;

pub const MOMENT: &str = env!("CARGO_BIN_EXE_moment");
pub const HELPER: &str = env!("CARGO_BIN_EXE_moment-e2e-helper");

/// Default headless output size of wlroots.
pub const OUTPUT_W: u32 = 1280;
pub const OUTPUT_H: u32 = 720;

#[derive(Clone, Debug)]
pub struct Notified {
    pub id: u32,
    pub summary: String,
    pub body: String,
    pub actions: Vec<String>,
    pub hints: Vec<String>,
}

/// A running `moment-e2e-helper ime`; see [`Session::input_method`].
pub struct InputMethod {
    child: Child,
    lines: std::sync::mpsc::Receiver<String>,
}

impl InputMethod {
    /// The next activation report (`active` / `inactive`), if one comes
    /// within `timeout`.
    pub fn next(&self, timeout: Duration) -> Option<String> {
        self.lines.recv_timeout(timeout).ok()
    }

    /// Sends one step: `pre=TEXT` or `commit=TEXT`.
    pub fn step(&mut self, step: &str) {
        use std::io::Write as _;
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{step}").unwrap();
        stdin.flush().unwrap();
        std::thread::sleep(Duration::from_millis(150));
    }

    /// Closes stdin and checks the helper exited cleanly.
    pub fn stop(mut self) {
        drop(self.child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "input method helper failed: {status}");
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        panic!("input method helper did not exit");
    }
}

impl Drop for InputMethod {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Default)]
struct FakeNotifications {
    log: Arc<Mutex<Vec<Notified>>>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl FakeNotifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &mut self,
        _app_name: String,
        _replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        _expire_timeout: i32,
    ) -> u32 {
        let mut log = self.log.lock().unwrap();
        let id = log.len() as u32 + 1;
        log.push(Notified {
            id,
            summary,
            body,
            actions,
            hints: hints.into_keys().collect(),
        });
        id
    }

    fn get_capabilities(&self) -> Vec<String> {
        vec!["actions".into(), "body".into()]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "fake".into(),
            "moment-tests".into(),
            "1".into(),
            "1.2".into(),
        )
    }

    fn close_notification(&self, _id: u32) {}
}

pub struct Session {
    pub dir: tempfile::TempDir,
    env: Vec<(String, String)>,
    children: Vec<Child>,
    daemon: Option<Child>,
    bus: zbus::blocking::Connection,
    notifications: Arc<Mutex<Vec<Notified>>>,
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

fn wait_for(what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out after {timeout:?} waiting for {what}");
}

impl Session {
    /// Starts a session, or returns `None` (test skipped) when no headless
    /// compositor is installed and `MOMENT_E2E=1` is not set.
    pub fn start() -> Option<Session> {
        Self::start_with(1, |_| {})
    }

    /// Like [`Session::start`] with `outputs` headless outputs (laid out left
    /// to right); `before_pattern` runs after the compositor is up but before
    /// the test wallpaper is drawn (to reconfigure outputs).
    pub fn start_with(outputs: usize, before_pattern: impl FnOnce(&Session)) -> Option<Session> {
        let required = std::env::var("MOMENT_E2E").is_ok_and(|v| v == "1");
        let compositor = std::env::var("MOMENT_E2E_COMPOSITOR").ok().or_else(|| {
            ["labwc", "sway"]
                .into_iter()
                .find(|c| which(c))
                .map(String::from)
        });
        let Some(compositor) = compositor.filter(|c| which(c)) else {
            assert!(
                !required,
                "MOMENT_E2E=1 but neither labwc nor sway is installed"
            );
            eprintln!("skipping: no headless compositor (install labwc or sway)");
            return None;
        };
        assert!(
            which("dbus-daemon"),
            "dbus-daemon is required for e2e tests"
        );

        let dir = tempfile::Builder::new()
            .prefix("moment-e2e")
            .tempdir()
            .unwrap();
        let root = dir.path();
        for sub in ["run", "config/moment", "cache", "home", "shots"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let run = root.join("run");
        std::fs::set_permissions(&run, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        std::fs::write(
            root.join("config/moment/config.toml"),
            format!("save_dir = \"{}\"\n", root.join("shots").display()),
        )
        .unwrap();

        let bus_addr = format!("unix:path={}", run.join("bus").display());
        // No service directories: nothing on the developer's machine gets
        // auto-started on this private bus.
        let bus_conf = root.join("bus.conf");
        std::fs::write(
            &bus_conf,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>{bus_addr}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#
            ),
        )
        .unwrap();

        let env: Vec<(String, String)> = vec![
            ("XDG_RUNTIME_DIR".into(), run.display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                root.join("config").display().to_string(),
            ),
            (
                "XDG_CACHE_HOME".into(),
                root.join("cache").display().to_string(),
            ),
            ("HOME".into(), root.join("home").display().to_string()),
            ("WAYLAND_DISPLAY".into(), "wayland-0".into()),
            ("DBUS_SESSION_BUS_ADDRESS".into(), bus_addr.clone()),
            (
                "MOMENT_SOCKET".into(),
                run.join("moment.sock").display().to_string(),
            ),
            ("MOMENT_LOG".into(), "moment=debug,warn".into()),
            ("WLR_BACKENDS".into(), "headless".into()),
            ("WLR_RENDERER".into(), "pixman".into()),
            ("WLR_LIBINPUT_NO_DEVICES".into(), "1".into()),
            ("WLR_HEADLESS_OUTPUTS".into(), outputs.to_string()),
            // Software rendering everywhere: deterministic and GPU-free.
            ("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()),
        ];

        let mut children = Vec::new();
        let mut cmd = Command::new("dbus-daemon");
        cmd.arg(format!("--config-file={}", bus_conf.display()))
            .arg("--nofork")
            .arg("--nopidfile");
        children.push(spawn_quiet(cmd, &env, None));
        wait_for("the private D-Bus", Duration::from_secs(5), || {
            run.join("bus").exists()
        });

        let comp_log = std::fs::File::create(root.join("compositor.log")).unwrap();
        let cmd = match compositor.as_str() {
            "sway" => {
                let conf = root.join("sway.conf");
                std::fs::write(&conf, "xwayland disable\n").unwrap();
                let mut c = Command::new("sway");
                c.arg("-c").arg(&conf);
                c
            }
            _ => {
                // labwc always starts Xwayland, whose socket lives in the
                // global /tmp/.X11-unix; a private /tmp keeps it there.
                if which("bwrap") && bwrap_works() {
                    let mut c = Command::new("bwrap");
                    // `--unshare-net` also hides Xwayland's abstract X socket.
                    c.args([
                        "--dev-bind",
                        "/",
                        "/",
                        "--unshare-net",
                        "--tmpfs",
                        "/tmp",
                        "--bind",
                    ])
                    .arg(root)
                    .arg(root)
                    .args(["--die-with-parent", "--", "labwc"]);
                    c
                } else {
                    Command::new("labwc")
                }
            }
        };
        // A compositor started with WAYLAND_DISPLAY set would try to nest.
        let mut child = spawn_quiet(cmd, &env, Some(comp_log));
        let wayland = run.join("wayland-0");
        wait_for("the compositor", Duration::from_secs(10), || {
            if let Ok(Some(status)) = child.try_wait() {
                panic!("compositor exited early: {status}");
            }
            wayland.exists()
        });
        children.push(child);

        let log: Arc<Mutex<Vec<Notified>>> = Arc::default();
        let bus = zbus::blocking::connection::Builder::address(bus_addr.as_str())
            .unwrap()
            .name("org.freedesktop.Notifications")
            .unwrap()
            .serve_at(
                "/org/freedesktop/Notifications",
                FakeNotifications { log: log.clone() },
            )
            .unwrap()
            .build()
            .unwrap();

        let mut session = Session {
            dir,
            env,
            children,
            daemon: None,
            bus,
            notifications: log,
        };
        before_pattern(&session);
        session.start_pattern();
        Some(session)
    }

    fn start_pattern(&mut self) {
        let mut cmd = Command::new(HELPER);
        cmd.arg("pattern").stdout(Stdio::piped());
        let mut child = spawn_quiet(cmd, &self.env, None);
        let mut out = child.stdout.take().unwrap();
        let mut buf = [0u8; 6];
        out.read_exact(&mut buf).expect("pattern helper starts");
        assert_eq!(&buf, b"ready\n");
        self.children.push(child);
        // Let the compositor present a frame with the wallpaper.
        std::thread::sleep(Duration::from_millis(150));
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn shots_dir(&self) -> PathBuf {
        self.root().join("shots")
    }

    pub fn cache_file(&self) -> PathBuf {
        self.root().join("cache/moment/last.raw")
    }

    pub fn socket(&self) -> PathBuf {
        self.root().join("run/moment.sock")
    }

    fn daemon_log_path(&self) -> PathBuf {
        self.root().join("daemon.log")
    }

    pub fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        for var in [
            "DISPLAY",
            "HYPRLAND_INSTANCE_SIGNATURE",
            "SWAYSOCK",
            "XDG_SESSION_TYPE",
        ] {
            cmd.env_remove(var);
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd
    }

    pub fn start_daemon(&mut self) {
        let log = std::fs::File::create(self.daemon_log_path()).unwrap();
        let mut cmd = self.command(MOMENT);
        cmd.arg("daemon").stdout(Stdio::null()).stderr(log);
        self.daemon = Some(cmd.spawn().unwrap());
        self.wait_log("daemon ready", Duration::from_secs(20));
    }

    pub fn daemon_log(&self) -> String {
        std::fs::read_to_string(self.daemon_log_path()).unwrap_or_default()
    }

    /// Waits until the daemon log contains `needle` `count` times in total.
    pub fn wait_log_count(&self, needle: &str, count: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.daemon_log().matches(needle).count() >= count {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "timed out waiting for {needle:?} x{count} in the daemon log:\n{}",
            self.daemon_log()
        );
    }

    pub fn wait_log(&self, needle: &str, timeout: Duration) {
        self.wait_log_count(needle, 1, timeout);
    }

    pub fn moment(&self, args: &[&str]) -> Output {
        let out = self.command(MOMENT).args(args).output().unwrap();
        eprintln!(
            "$ moment {} -> {}\n{}{}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    pub fn spawn_moment(&self, args: &[&str]) -> Child {
        self.command(MOMENT)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Waits for a client spawned with [`Session::spawn_moment`], killing it
    /// (and failing) if it outlives `timeout`.
    pub fn finish(&self, mut child: Child, timeout: Duration) -> Output {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return child.wait_with_output().unwrap();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let out = child.wait_with_output().unwrap();
        panic!(
            "client did not finish within {timeout:?}; stderr: {}\ndaemon log:\n{}",
            String::from_utf8_lossy(&out.stderr),
            self.daemon_log()
        );
    }

    /// Waits until the `n`th selection overlay is on screen and has the
    /// pointer, so synthetic input lands on it rather than the wallpaper.
    pub fn wait_overlay(&self, n: usize) {
        self.wait_log_count("selection overlays opened", n, Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(300));
        self.helper(&["move", "1", "1"]);
    }

    pub fn helper(&self, args: &[&str]) -> Output {
        let out = self.command(HELPER).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "helper {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    pub fn has(&self, bin: &str) -> bool {
        which(bin)
    }

    /// Presses key chords on the virtual keyboard, e.g. `["r", "ctrl+s"]`.
    pub fn keys(&self, chords: &[&str]) {
        let mut args = vec!["key"];
        args.extend_from_slice(chords);
        self.helper(&args);
        std::thread::sleep(Duration::from_millis(80));
    }

    pub fn type_text(&self, text: &str) {
        self.helper(&["type", text]);
        std::thread::sleep(Duration::from_millis(80));
    }

    /// Starts a resident input method, like fcitx5 running in the session.
    pub fn input_method(&self) -> InputMethod {
        let mut child = self
            .command(HELPER)
            .arg("ime")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead as _;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let im = InputMethod { child, lines: rx };
        assert_eq!(im.next(Duration::from_secs(5)).as_deref(), Some("ready"));
        im
    }

    pub fn toplevels(&self) -> Vec<String> {
        String::from_utf8(self.helper(&["toplevels"]).stdout)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    pub fn wait_for_window(&self, app_id: &str) {
        wait_for("an editor window", Duration::from_secs(10), || {
            self.toplevels().iter().any(|l| l.starts_with(app_id))
        });
    }

    pub fn notifications(&self) -> Vec<Notified> {
        self.notifications.lock().unwrap().clone()
    }

    pub fn wait_notification(&self) -> Notified {
        wait_for("a notification", Duration::from_secs(5), || {
            !self.notifications().is_empty()
        });
        self.notifications().last().cloned().unwrap()
    }

    /// Simulates the user clicking a notification.
    pub fn click_notification(&self, id: u32, action: &str) {
        self.bus
            .emit_signal(
                None::<()>,
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "ActionInvoked",
                &(id, action),
            )
            .unwrap();
    }

    pub fn stop_daemon(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            let _ = self.moment(&["quit"]);
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = d.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = d.kill();
            let _ = d.wait();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            let _ = d.kill();
            let _ = d.wait();
        }
        // A daemon auto-started by the client is not our child; stop it by socket.
        let _ = self.command(MOMENT).arg("quit").output();
        for c in self.children.iter_mut().rev() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if std::env::var_os("MOMENT_E2E_KEEP").is_some() {
            let kept = self.dir.path().to_path_buf();
            eprintln!("keeping {}", kept.display());
            std::mem::forget(std::mem::replace(
                &mut self.dir,
                tempfile::tempdir().unwrap(),
            ));
        }
    }
}

fn spawn_quiet(mut cmd: Command, env: &[(String, String)], log: Option<std::fs::File>) -> Child {
    let is_compositor = log.is_some();
    for (k, v) in env {
        if !(is_compositor && k == "WAYLAND_DISPLAY") {
            cmd.env(k, v);
        }
    }
    cmd.env_remove("DISPLAY")
        .env_remove("HYPRLAND_INSTANCE_SIGNATURE");
    if is_compositor {
        cmd.env_remove("WAYLAND_DISPLAY");
    }
    cmd.stdin(Stdio::null());
    if cmd.get_program() != HELPER {
        cmd.stdout(Stdio::null());
    }
    match log {
        Some(f) => cmd.stderr(f),
        None => cmd.stderr(Stdio::null()),
    };
    cmd.spawn()
        .unwrap_or_else(|e| panic!("spawning {:?}: {e}", cmd.get_program()))
}

fn bwrap_works() -> bool {
    Command::new("bwrap")
        .args([
            "--dev-bind",
            "/",
            "/",
            "--unshare-net",
            "--tmpfs",
            "/tmp",
            "true",
        ])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The helper's wallpaper colour at output pixel (x, y), BGRA.
pub fn pattern_pixel(x: u32, y: u32) -> [u8; 4] {
    let b = if x < 64 && y < 64 { 255 } else { 0 };
    [b, (y % 256) as u8, (x % 256) as u8, 255]
}

pub fn decode_png(path: &Path) -> moment::raster::Image {
    moment::raster::Image::load(path).unwrap_or_else(|e| panic!("{}: {e:#}", path.display()))
}
