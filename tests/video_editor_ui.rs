//! The video editor through GPUI's real input dispatch (offscreen window,
//! headless WGPU renderer): trimming, seeking, formats and exports.
//!
//! Timeline, format and help cases use a path that does not exist, so they
//! run without any media engine. Cases that decode use a 5 s 1280×720
//! H.264/Opus clip made by quikscope's own encoders (`support/fixtures.rs`).
//! Copies go to a temporary clip cache, never ~/.cache.

#![cfg(not(windows))]

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use gpui::{
    AnyWindowHandle, AppContext as _, HeadlessAppContext, KeyDownEvent, Keystroke, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Point, WindowHandle,
    point, px, size,
};
use quikscope::config::Config;
use quikscope::media::{Kind, MediaInfo, OutputFormat};
use quikscope::ui::Assets;
use quikscope::ui::editor::EditorServices;
use quikscope::ui::video_editor::{AnnotateFrame, VideoEditor};

#[path = "support/fixtures.rs"]
mod fixtures;

/// Records files put on the clipboard.
#[derive(Default)]
struct FileClipboard(Mutex<Vec<PathBuf>>);

impl quikscope::platform::Clipboard for FileClipboard {
    fn set_image(
        &self,
        image: Arc<quikscope::raster::Image>,
    ) -> Arc<quikscope::clipboard::PendingPng> {
        quikscope::clipboard::PendingPng::encode(image)
    }

    fn set_text(&self, _: &str) {}

    fn set_files(&self, paths: &[PathBuf]) {
        self.0.lock().unwrap().extend_from_slice(paths);
    }
}

/// Records desktop notifications.
#[derive(Default)]
struct Notes(Mutex<Vec<String>>);

impl quikscope::platform::Notifier for Notes {
    fn shot_captured(
        &self,
        _: u64,
        _: &quikscope::raster::Image,
        _: &str,
        _: &str,
        _: i32,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn message(&self, summary: &str, body: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(format!("{summary} {body}"));
        Ok(())
    }
}

struct Ui {
    cx: HeadlessAppContext,
    window: WindowHandle<VideoEditor>,
    dir: tempfile::TempDir,
    clipboard: Arc<FileClipboard>,
    notes: Arc<Notes>,
    annotated: Arc<Mutex<Vec<PathBuf>>>,
}

impl Ui {
    fn open(path: &Path, info: MediaInfo) -> Ui {
        Self::with_config(path, info, |_| {})
    }

    fn with_config(path: &Path, info: MediaInfo, configure: impl FnOnce(&mut Config)) -> Ui {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            save_dir: dir.path().join("shots").display().to_string(),
            ..Config::default()
        };
        config.record.save_dir = dir.path().join("recordings").display().to_string();
        configure(&mut config);
        let clipboard = Arc::new(FileClipboard::default());
        let notes = Arc::new(Notes::default());
        let annotated = Arc::new(Mutex::new(Vec::new()));
        let text = gpui_platform::current_platform(true).text_system();
        let mut cx = HeadlessAppContext::with_platform(
            text,
            Arc::new(Assets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(quikscope::ui::init);
        let services = EditorServices {
            config: Arc::new(config),
            clipboard: Some(clipboard.clone()),
            pick_color: None,
            notifier: Some(notes.clone()),
            clips: dir.path().join("clips"),
        };
        let sink = annotated.clone();
        cx.update(|cx| {
            cx.set_global(services);
            cx.on_action(move |a: &AnnotateFrame, _| sink.lock().unwrap().push(a.path.clone()));
        });
        let path = path.to_owned();
        let window = cx
            .open_window(size(px(1000.), px(700.)), |window, cx| {
                cx.new(|cx| VideoEditor::new(path, info, window, cx))
            })
            .unwrap();
        let mut ui = Ui {
            cx,
            window,
            dir,
            clipboard,
            notes,
            annotated,
        };
        ui.frame();
        ui
    }

    fn any(&self) -> AnyWindowHandle {
        self.window.into()
    }

    fn frame(&mut self) {
        self.cx
            .update_window(self.any(), |_, window, _| window.refresh())
            .unwrap();
        self.cx.run_until_parked();
        self.cx.capture_screenshot(self.any()).unwrap();
    }

    fn input(&mut self, event: PlatformInput) {
        self.cx
            .update_window(self.any(), |_, window, cx| window.dispatch_event(event, cx))
            .unwrap();
        self.cx.run_until_parked();
    }

    fn key_with(&mut self, key: &str, modifiers: Modifiers) {
        self.input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke: Keystroke {
                modifiers,
                key: key.into(),
                key_char: (key.len() == 1 && !modifiers.control).then(|| key.into()),
            },
            is_held: false,
            prefer_character_input: false,
        }));
        self.frame();
    }

    fn key(&mut self, key: &str) {
        self.key_with(key, Modifiers::default());
    }

    fn shift(&mut self, key: &str) {
        self.key_with(
            key,
            Modifiers {
                shift: true,
                ..Default::default()
            },
        );
    }

    /// `key` as auto-repeat sends it while held down.
    fn held(&mut self, key: &str) {
        self.input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke: Keystroke {
                modifiers: Modifiers::default(),
                key: key.into(),
                key_char: None,
            },
            is_held: true,
            prefer_character_input: false,
        }));
        self.frame();
    }

    fn ctrl(&mut self, key: &str) {
        self.key_with(
            key,
            Modifiers {
                control: true,
                ..Default::default()
            },
        );
    }

    fn mouse(&mut self, kind: &str, at: Point<gpui::Pixels>) {
        let event = match kind {
            "down" => PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position: at,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            }),
            "move" => PlatformInput::MouseMove(MouseMoveEvent {
                position: at,
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            }),
            _ => PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position: at,
                modifiers: Modifiers::default(),
                click_count: 1,
            }),
        };
        self.input(event);
        self.frame();
    }

    fn click(&mut self, at: Point<gpui::Pixels>) {
        self.mouse("down", at);
        self.mouse("up", at);
    }

    fn drag(&mut self, from: Point<gpui::Pixels>, to: Point<gpui::Pixels>) {
        self.mouse("down", from);
        let mid = point((from.x + to.x) / 2.0, from.y);
        self.mouse("move", mid);
        self.mouse("move", to);
        self.mouse("up", to);
    }

    fn click_control(&mut self, id: &str) {
        let at = self
            .editor(|e, _| e.control_bounds(id))
            .unwrap_or_else(|| panic!("no control {id}"))
            .center();
        self.click(at);
    }

    fn editor<R>(
        &mut self,
        f: impl FnOnce(&mut VideoEditor, &mut gpui::Context<VideoEditor>) -> R,
    ) -> R {
        let any = self.any();
        self.cx
            .update_window(any, |view, _, cx| {
                view.downcast::<VideoEditor>()
                    .unwrap()
                    .update(cx, |e, cx| f(e, cx))
            })
            .unwrap()
    }

    /// Window point of media time `t` on the timeline, `dx` to its right.
    fn at(&mut self, t: f64, dx: f32) -> Point<gpui::Pixels> {
        let y = self.editor(|e, _| e.track_bounds().center().y);
        let x = self.editor(|e, _| e.timeline_x(Duration::from_secs_f64(t)));
        point(x + px(dx), y)
    }

    /// Paints until `done` holds (decode and export threads run on their
    /// own clocks).
    fn wait_for(&mut self, what: &str, done: impl Fn(&VideoEditor) -> bool) {
        let start = Instant::now();
        loop {
            self.frame();
            if self.editor(|e, _| done(e)) {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(90),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn trim(&mut self) -> (f64, f64) {
        let t = self.editor(|e, _| e.trim());
        (t.start.as_secs_f64(), t.end.as_secs_f64())
    }

    fn playhead(&mut self) -> f64 {
        self.editor(|e, _| e.playhead().as_secs_f64())
    }

    fn shown(&mut self) -> Option<f64> {
        self.editor(|e, _| e.shown_pts().map(|t| t.as_secs_f64()))
    }

    /// Runs an export started by `start` and returns its toast.
    fn export(&mut self, start: impl FnOnce(&mut Ui)) -> String {
        start(self);
        self.wait_for("the export to finish", |e| e.export_progress().is_none());
        let (message, error) = self
            .editor(|e, _| e.toast().map(|(m, e)| (m.to_owned(), e)))
            .expect("an export ends with a toast");
        assert!(!error, "{message}");
        message
    }

    /// Sends Ctrl+W, which closes the window; nothing is painted after.
    fn close(&mut self) {
        self.input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke: Keystroke {
                modifiers: Modifiers {
                    control: true,
                    ..Default::default()
                },
                key: "w".into(),
                key_char: None,
            },
            is_held: false,
            prefer_character_input: false,
        }));
        assert!(
            self.cx.update_window(self.any(), |_, _, _| ()).is_err(),
            "window still open"
        );
    }

    /// Runs the app (no window left to paint) until `done` holds.
    fn pump_until(&mut self, what: &str, done: impl Fn(&Ui) -> bool) {
        let start = Instant::now();
        while !done(self) {
            assert!(
                start.elapsed() < Duration::from_secs(90),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(10));
            self.cx.run_until_parked();
        }
    }

    fn recordings(&self, ext: &str) -> Vec<PathBuf> {
        let dir = self.dir.path().join("recordings");
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == ext))
                    .collect()
            })
            .unwrap_or_default()
    }
}

const FRAME: f64 = 1.0 / 30.0;

/// Checks the container duration with the `ffprobe` tool too, when it is
/// installed.
#[track_caller]
fn assert_ffprobe_duration(path: &Path, expected: f64) {
    let Ok(out) = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
    else {
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let duration: f64 = text
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("ffprobe {}: {text:?}", path.display()));
    eprintln!("ffprobe {}: {duration} s", path.display());
    assert_close(duration, expected, 0.1);
}

fn close(a: f64, b: f64, tolerance: f64) -> bool {
    (a - b).abs() <= tolerance
}

#[track_caller]
fn assert_close(a: f64, b: f64, tolerance: f64) {
    assert!(
        close(a, b, tolerance),
        "{a} is not within {tolerance} of {b}"
    );
}

fn fake(path: &str, has_audio: bool) -> (PathBuf, MediaInfo) {
    (
        PathBuf::from(path),
        MediaInfo {
            width: 1280,
            height: 720,
            duration: Duration::from_secs(5),
            fps: 30.0,
            frames: Some(150),
            has_audio,
            kind: Kind::Video,
        },
    )
}

/// Makes a 5 s 1280×720 30 fps H.264 + Opus clip of the test pattern with a
/// 440 Hz tone at `amplitude`.
fn make_clip(name: &str, amplitude: f32) -> PathBuf {
    let path = std::env::temp_dir().join(format!("quikscope-{name}-{}.mp4", std::process::id()));
    let length = Duration::from_secs(5);
    let clip = fixtures::Clip {
        width: 1280,
        height: 720,
        fps: 30,
        frames: 150,
        tone: Some(fixtures::Tone {
            frequency: 440.0,
            amplitude,
            start: Duration::ZERO,
            length,
        }),
    };
    fixtures::video(&path, OutputFormat::Mp4, &clip).expect("making the test video");
    path
}

/// A test pattern with a 440 Hz tone.
static FIXTURE: LazyLock<PathBuf> = LazyLock::new(|| make_clip("video-editor", 0.2));
/// The same pattern with silent audio, for cases that open a real audio
/// output: nothing is ever heard.
static SILENT: LazyLock<PathBuf> = LazyLock::new(|| make_clip("video-editor-silent", 0.0));

fn open_clip(clip: &LazyLock<PathBuf>) -> (PathBuf, MediaInfo) {
    let path = LazyLock::force(clip).clone();
    let info = quikscope::media::decode::probe(&path).expect("probing the test video");
    (path, info)
}

fn fixture() -> (PathBuf, MediaInfo) {
    open_clip(&FIXTURE)
}

pub(super) fn trim_handles_and_keys_keep_the_playhead_inside_the_trim() {
    let (path, info) = fake("/nonexistent/clip.mp4", true);
    let mut ui = Ui::open(&path, info);
    assert!(ui.editor(|e, _| e.track_bounds().size.width > px(500.)));
    assert_eq!(ui.trim(), (0.0, 5.0));

    // Dragging the in handle from just left of the track's start.
    let (from, to) = (ui.at(0.0, -5.0), ui.at(1.0, 0.0));
    ui.drag(from, to);
    let (start, end) = ui.trim();
    assert_close(start, 1.0, 1e-6);
    assert_eq!(end, 5.0);
    assert_close(ui.playhead(), 1.0, 1e-6);

    // The out handle, from just right of the end.
    let (from, to) = (ui.at(5.0, 5.0), ui.at(4.0, 0.0));
    ui.drag(from, to);
    assert_close(ui.trim().1, 4.0, 1e-6);
    assert_close(ui.playhead(), 4.0, 1e-6);

    // The in handle stops a frame before the out handle.
    let (from, to) = (ui.at(1.0, -5.0), ui.at(4.9, 0.0));
    ui.drag(from, to);
    assert_close(ui.trim().0, 4.0 - FRAME, 1e-6);
    let (from, to) = (ui.at(4.0 - FRAME, -5.0), ui.at(0.0, -40.0));
    ui.drag(from, to);
    assert_eq!(ui.trim().0, 0.0);

    // Clicking past the out point seeks to it, no further.
    let past = ui.at(4.6, 0.0);
    ui.click(past);
    assert_close(ui.playhead(), 4.0, 1e-6);

    // Frame and second steps, clamped to the trim.
    ui.key("home");
    assert_eq!(ui.playhead(), 0.0);
    ui.key("left");
    assert_eq!(ui.playhead(), 0.0);
    ui.key("right");
    assert_close(ui.playhead(), FRAME, 1e-6);
    ui.shift("right");
    assert_close(ui.playhead(), 1.0 + FRAME, 1e-6);
    ui.key("left");
    assert_close(ui.playhead(), 1.0, 1e-6);
    ui.key("end");
    assert_close(ui.playhead(), 4.0, 1e-6);
    ui.shift("right");
    assert_close(ui.playhead(), 4.0, 1e-6);

    // I and O at the playhead, keeping a frame between them.
    ui.key("i");
    assert_close(ui.trim().0, 4.0 - FRAME, 1e-6);
    ui.key("home");
    ui.shift("left");
    assert_close(ui.playhead(), 4.0 - FRAME, 1e-6);
    let (from, to) = (ui.at(4.0 - FRAME, -5.0), ui.at(0.0, -40.0));
    ui.drag(from, to);
    assert_eq!(ui.trim().0, 0.0);
    assert_eq!(ui.playhead(), 0.0);
    ui.shift("right");
    ui.shift("right");
    ui.key("o");
    assert_close(ui.trim().1, 2.0, 1e-6);
    ui.key("i");
    assert_close(ui.trim().0, 2.0 - FRAME, 1e-6);
    let playhead = ui.playhead();
    let (start, end) = ui.trim();
    assert!(start <= playhead && playhead <= end);
}

pub(super) fn format_menu_selects_formats_and_closes_on_escape_or_outside() {
    let (path, info) = fake("/nonexistent/clip.mp4", true);
    let mut ui = Ui::open(&path, info.clone());
    assert_eq!(ui.editor(|e, _| e.format()), OutputFormat::Mp4);

    ui.click_control("format");
    assert!(ui.editor(|e, _| e.menu_open()));
    ui.frame();
    ui.click_control("format:gif");
    assert!(!ui.editor(|e, _| e.menu_open()));
    assert_eq!(ui.editor(|e, _| e.format()), OutputFormat::Gif);

    for format in OutputFormat::VIDEO.iter().chain(&OutputFormat::ANIMATION) {
        ui.click_control("format");
        ui.click_control(&format!("format:{}", format.extension()));
        assert_eq!(ui.editor(|e, _| e.format()), *format);
    }

    // Escape closes the menu, not the window.
    ui.click_control("format");
    ui.key("escape");
    assert!(!ui.editor(|e, _| e.menu_open()));
    // A click outside closes it without seeking the timeline under it.
    ui.click_control("format");
    let playhead = ui.playhead();
    let on_track = ui.at(3.0, 0.0);
    ui.click(on_track);
    assert!(!ui.editor(|e, _| e.menu_open()));
    assert_eq!(ui.playhead(), playhead);
    assert_eq!(ui.editor(|e, _| e.format()), OutputFormat::Jxl);

    // Defaults: the source's own container, else the configured video format.
    let mkv = Ui::open(Path::new("/nonexistent/clip.mkv"), info.clone()).editor(|e, _| e.format());
    assert_eq!(mkv, OutputFormat::Mkv);
    let webm = Ui::with_config(Path::new("/nonexistent/clip.webm"), info, |c| {
        c.record.video_format = "mov".into();
    })
    .editor(|e, _| e.format());
    assert_eq!(webm, OutputFormat::Mov);
}

pub(super) fn mute_needs_audio_and_toggles_with_m() {
    let (path, info) = fake("/nonexistent/clip.mp4", true);
    let mut ui = Ui::open(&path, info);
    assert!(ui.editor(|e, _| e.control_bounds("mute")).is_some());
    ui.key("m");
    assert!(ui.editor(|e, _| e.is_muted()));
    ui.click_control("mute");
    assert!(!ui.editor(|e, _| e.is_muted()));

    let (path, info) = fake("/nonexistent/silent.mp4", false);
    let mut ui = Ui::open(&path, info);
    assert!(ui.editor(|e, _| e.control_bounds("mute")).is_none());
    ui.key("m");
    assert!(!ui.editor(|e, _| e.is_muted()));
}

pub(super) fn real_video_seeks_steps_and_plays() {
    let (path, info) = fixture();
    assert_eq!((info.width, info.height), (1280, 720));
    assert!(info.has_audio);
    let mut ui = Ui::open(&path, info);
    ui.wait_for("the first frame", |e| e.shown_pts().is_some());
    assert_eq!(ui.shown(), Some(0.0));

    let target = ui.at(2.5, 0.0);
    ui.click(target);
    assert_close(ui.playhead(), 2.5, 1e-6);
    ui.wait_for("the frame at 2.5 s", |e| {
        e.shown_pts()
            .is_some_and(|t| close(t.as_secs_f64(), 2.5, FRAME / 2.0))
    });
    ui.key("right");
    ui.wait_for("the next frame", |e| {
        e.shown_pts()
            .is_some_and(|t| close(t.as_secs_f64(), 2.5 + FRAME, FRAME / 2.0))
    });
    ui.shift("left");
    ui.wait_for("the frame a second earlier", |e| {
        e.shown_pts()
            .is_some_and(|t| close(t.as_secs_f64(), 1.5 + FRAME, FRAME / 2.0))
    });

    // Trim to 1.0–3.0 and look at it.
    ui.key("home");
    ui.shift("right");
    ui.key("i");
    ui.shift("right");
    ui.shift("right");
    ui.key("o");
    assert_eq!(ui.trim(), (1.0, 3.0));
    let mid = ui.at(2.0, 0.0);
    ui.click(mid);
    ui.wait_for("the frame at 2 s", |e| {
        e.shown_pts()
            .is_some_and(|t| close(t.as_secs_f64(), 2.0, FRAME / 2.0))
    });
    ui.frame();

    // At the out handle the last kept frame shows: an export ends before
    // the out point, so the frame there is cut.
    let (from, to) = (ui.at(3.0, 5.0), ui.at(2.5, 0.0));
    ui.drag(from, to);
    assert_close(ui.trim().1, 2.5, 1e-6);
    ui.wait_for("the last kept frame", |e| {
        e.shown_pts()
            .is_some_and(|t| close(t.as_secs_f64(), 2.5 - FRAME, FRAME / 2.0))
    });
    let (from, to) = (ui.at(2.5, 5.0), ui.at(3.0, 0.0));
    ui.drag(from, to);
    assert_close(ui.trim().1, 3.0, 1e-6);
    ui.click(mid);

    // Muted, so the test never makes a sound; playback stops at the out point.
    ui.key("m");
    ui.key("space");
    assert!(ui.editor(|e, _| e.is_playing()));
    // Holding Space repeats the key; it must not flip play/pause each time.
    ui.held("space");
    ui.held("space");
    assert!(ui.editor(|e, _| e.is_playing()));
    ui.wait_for("playback to reach the out point", |e| !e.is_playing());
    assert_close(ui.playhead(), 3.0, 1e-6);
    ui.wait_for("the frames before the out point", |e| {
        e.shown_pts()
            .is_some_and(|t| t.as_secs_f64() > 3.0 - 1.5 * FRAME)
    });
    assert_close(ui.shown().unwrap(), 3.0 - FRAME, FRAME / 2.0);
    // Playing again starts from the in point.
    ui.key("space");
    assert!(ui.editor(|e, _| e.is_playing()));
    assert!(ui.playhead() < 1.2);
    ui.key("space");
    assert!(!ui.editor(|e, _| e.is_playing()));
}

pub(super) fn real_video_exports_trimmed_gif_and_mp4() {
    let (path, info) = fixture();
    let before = std::fs::metadata(&path).unwrap();
    let mut ui = Ui::open(&path, info);
    ui.wait_for("the first frame", |e| e.shown_pts().is_some());
    ui.shift("right");
    ui.key("i");
    ui.shift("right");
    ui.shift("right");
    ui.key("o");
    assert_eq!(ui.trim(), (1.0, 3.0));

    ui.click_control("format");
    ui.click_control("format:gif");
    let message = ui.export(|ui| {
        ui.ctrl("s");
        ui.frame();
    });
    assert!(message.starts_with("Saved"), "{message}");
    let gif = ui.recordings("gif");
    assert_eq!(gif.len(), 1, "{gif:?}");
    let probed = quikscope::media::decode::probe(&gif[0]).unwrap();
    assert_eq!(probed.kind, Kind::Animation);
    assert_eq!(probed.width, 1280);
    assert_close(probed.duration.as_secs_f64(), 2.0, 0.1);
    assert_ffprobe_duration(&gif[0], 2.0);

    ui.click_control("format");
    ui.click_control("format:mp4");
    let message = ui.export(|ui| ui.ctrl("s"));
    assert!(message.starts_with("Saved"), "{message}");
    let mp4 = ui.recordings("mp4");
    assert_eq!(mp4.len(), 1, "{mp4:?}");
    let probed = quikscope::media::decode::probe(&mp4[0]).unwrap();
    assert_eq!((probed.width, probed.height), (1280, 720));
    assert!(probed.has_audio);
    assert_close(probed.duration.as_secs_f64(), 2.0, 0.1);
    assert_ffprobe_duration(&mp4[0], 2.0);

    // Copy, muted: a silent file on the clipboard, in a clip directory of
    // its own under the source's name.
    ui.key("m");
    let message = ui.export(|ui| ui.ctrl("c"));
    assert_eq!(message, "Copied MP4");
    let copied = ui.clipboard.0.lock().unwrap().clone();
    assert_eq!(copied.len(), 1);
    assert_eq!(copied[0].file_name(), path.file_name());
    assert_eq!(
        copied[0].parent().unwrap().parent().unwrap(),
        ui.dir.path().join("clips")
    );
    let probed = quikscope::media::decode::probe(&copied[0]).unwrap();
    assert!(!probed.has_audio);
    assert_close(probed.duration.as_secs_f64(), 2.0, 0.1);
    assert_ffprobe_duration(&copied[0], 2.0);

    let after = std::fs::metadata(&path).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
}

pub(super) fn export_can_be_cancelled_and_leaves_no_file() {
    let (path, info) = fixture();
    let mut ui = Ui::open(&path, info);
    ui.wait_for("the first frame", |e| e.shown_pts().is_some());
    ui.click_control("format");
    ui.click_control("format:gif");
    ui.ctrl("s");
    assert!(ui.editor(|e, _| e.export_progress()).is_some());
    ui.click_control("cancel");
    ui.wait_for("the export to stop", |e| e.export_progress().is_none());
    let message = ui.editor(|e, _| e.toast().map(|(m, _)| m.to_owned()));
    assert_eq!(message.as_deref(), Some("Export cancelled"));
    let dir = ui.dir.path().join("recordings");
    let left: Vec<_> = std::fs::read_dir(&dir)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");
}

/// Saving an untouched video copies it as it is, byte for byte; saving it
/// untouched again from the recordings folder makes no duplicate.
pub(super) fn untouched_save_copies_the_source_exactly() {
    let (path, info) = fixture();
    let mut ui = Ui::open(&path, info.clone());
    let message = ui.export(|ui| ui.ctrl("s"));
    assert!(message.starts_with("Saved"), "{message}");
    let mp4 = ui.recordings("mp4");
    assert_eq!(mp4.len(), 1, "{mp4:?}");
    assert_eq!(
        std::fs::read(&mp4[0]).unwrap(),
        std::fs::read(&path).unwrap()
    );

    let recordings = ui.dir.path().join("recordings");
    let mut saved = Ui::with_config(&mp4[0], info, |config| {
        config.record.save_dir = recordings.display().to_string();
    });
    let message = saved.export(|ui| ui.ctrl("s"));
    assert!(message.starts_with("Already saved as"), "{message}");
    assert_eq!(ui.recordings("mp4"), mp4, "no duplicate");
}

/// Save as writes the chosen file in the format its name gives (or the
/// selected one, added to a name without an extension); Ctrl+S then updates
/// that file rather than adding one to the recordings folder.
pub(super) fn save_as_writes_the_chosen_file_and_ctrl_s_updates_it() {
    let (path, info) = fixture();
    let mut ui = Ui::open(&path, info);
    let chosen = ui.dir.path().join("elsewhere");
    std::fs::create_dir(&chosen).unwrap();
    let clip = chosen.join("clip");
    let message = ui.export(|ui| ui.editor(|e, cx| e.save_as_chosen(clip.clone(), cx)));
    assert!(message.ends_with("clip.mp4"), "{message}");
    let mp4 = chosen.join("clip.mp4");
    assert_eq!(
        std::fs::read(&mp4).unwrap(),
        std::fs::read(&path).unwrap(),
        "untouched: copied as it is"
    );

    ui.key("m");
    let message = ui.export(|ui| ui.ctrl("s"));
    assert!(message.ends_with("clip.mp4"), "{message}");
    assert!(!quikscope::media::decode::probe(&mp4).unwrap().has_audio);
    assert!(ui.recordings("mp4").is_empty());

    let mkv = chosen.join("clip.MKV");
    ui.export(|ui| ui.editor(|e, cx| e.save_as_chosen(mkv.clone(), cx)));
    assert_eq!(ui.editor(|e, _| e.format()), OutputFormat::Mkv);
    let probed = quikscope::media::decode::probe(&mkv).unwrap();
    assert_eq!(probed.kind, Kind::Video);
    assert!(!probed.has_audio);
}

/// Closing the window mid-export lets the export finish; a notification
/// says so, since no window is left for a toast.
pub(super) fn closing_the_window_lets_the_export_finish() {
    let (path, info) = fixture();
    let mut ui = Ui::open(&path, info);
    ui.click_control("format");
    ui.click_control("format:gif");
    ui.ctrl("s");
    assert!(ui.editor(|e, _| e.export_progress()).is_some());
    ui.close();
    ui.pump_until("the export to finish", |ui| {
        !ui.notes.0.lock().unwrap().is_empty()
    });
    let notes = ui.notes.0.lock().unwrap().clone();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].starts_with("Saved "), "{notes:?}");
    let gif = ui.recordings("gif");
    assert_eq!(gif.len(), 1, "{gif:?}");
    assert!(notes[0].contains(gif[0].file_name().unwrap().to_str().unwrap()));
}

/// "Annotate frame" opens the frame through the clip cache: nothing lands
/// in the screenshots folder unless the annotator saves.
pub(super) fn annotate_frame_opens_a_full_size_png_from_the_clip_cache() {
    let (path, info) = fixture();
    let mut ui = Ui::open(&path, info);
    ui.wait_for("the first frame", |e| e.shown_pts().is_some());
    let target = ui.at(2.5, 0.0);
    ui.click(target);
    ui.wait_for("the frame at 2.5 s", |e| {
        e.shown_pts()
            .is_some_and(|t| close(t.as_secs_f64(), 2.5, FRAME / 2.0))
    });
    ui.key("f");
    ui.wait_for("the frame to be written", |e| {
        e.toast()
            .is_some_and(|(m, _)| m.starts_with("Opening the frame"))
    });
    let annotated = ui.annotated.lock().unwrap().clone();
    assert_eq!(annotated.len(), 1);
    assert!(annotated[0].starts_with(ui.dir.path().join("clips")));
    assert!(!ui.dir.path().join("shots").exists());
    let image = quikscope::raster::Image::load(&annotated[0]).unwrap();
    assert_eq!((image.width, image.height), (1280, 720));
}

/// `?`, F1 and the help button open the shortcut list; Escape, the same
/// keys or a click anywhere close it, without acting on what is under it.
pub(super) fn shortcut_help_opens_and_closes_without_side_effects() {
    let (path, info) = fake("/nonexistent/clip.mp4", true);
    let mut ui = Ui::open(&path, info);
    let open = |ui: &mut Ui| ui.editor(|e, _| e.help_open());
    ui.shift("/");
    assert!(open(&mut ui));
    // Keys go to the help, not the editor: no trim point is set.
    ui.key("i");
    assert_eq!(ui.trim(), (0.0, 5.0));
    ui.key("escape");
    assert!(!open(&mut ui));
    ui.key("f1");
    assert!(open(&mut ui));
    ui.key("f1");
    assert!(!open(&mut ui));
    ui.click_control("help");
    assert!(open(&mut ui));
    let playhead = ui.playhead();
    let on_track = ui.at(3.0, 0.0);
    ui.click(on_track);
    assert!(!open(&mut ui));
    assert_eq!(ui.playhead(), playhead);
}

pub(super) fn audible_playback_opens_output_and_mute_closes_it() {
    let (path, info) = open_clip(&SILENT);
    assert!(info.has_audio);
    let mut ui = Ui::open(&path, info);
    ui.wait_for("the first frame", |e| e.shown_pts().is_some());
    // Viewing alone never opens the audio device.
    assert!(!ui.editor(|e, _| e.has_audio_output()));
    ui.shift("right");
    ui.key("o");
    ui.key("home");
    // The clock starts at once; the device opens off the UI thread.
    ui.key("space");
    assert!(ui.editor(|e, _| e.is_playing()));
    ui.wait_for("the audio output or the out point", |e| {
        e.has_audio_output() || !e.is_playing()
    });
    if !ui.editor(|e, _| e.has_audio_output()) {
        eprintln!("no audio output device: playback continues silently");
    }
    ui.wait_for("playback to reach the out point", |e| !e.is_playing());
    assert_close(ui.playhead(), 1.0, 1e-6);
    ui.wait_for("the frames before 1 s", |e| {
        e.shown_pts()
            .is_some_and(|t| t.as_secs_f64() > 1.0 - 1.5 * FRAME)
    });
    assert_close(ui.shown().unwrap(), 1.0 - FRAME, FRAME / 2.0);
    ui.key("m");
    assert!(!ui.editor(|e, _| e.has_audio_output()));
    ui.key("space");
    assert!(ui.editor(|e, _| e.is_playing() && !e.has_audio_output()));
    ui.key("space");
}
