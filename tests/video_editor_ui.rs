//! The video editor through GPUI's real input dispatch (offscreen window,
//! headless WGPU renderer): trimming, seeking, formats and exports.
//!
//! Timeline and format cases use a path that does not exist, so they run
//! without any media engine. Cases that decode use a 5 s 1280×720 H.264/AAC
//! clip made by the `ffmpeg` command-line tool; they skip when it is not
//! installed, and export cases skip when FFmpeg lacks the encoder.

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

struct Ui {
    cx: HeadlessAppContext,
    window: WindowHandle<VideoEditor>,
    dir: tempfile::TempDir,
    clipboard: Arc<FileClipboard>,
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

    /// Runs an export started by `start`. `None` when FFmpeg has no encoder
    /// for the format (the case then skips).
    fn export(&mut self, start: impl FnOnce(&mut Ui)) -> Option<String> {
        start(self);
        self.wait_for("the export to finish", |e| e.export_progress().is_none());
        let (message, error) = self
            .editor(|e, _| e.toast().map(|(m, e)| (m.to_owned(), e)))
            .expect("an export ends with a toast");
        if error && message.to_lowercase().contains("encoder") {
            eprintln!("skipping: {message}");
            return None;
        }
        assert!(!error, "{message}");
        Some(message)
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
/// installed (it ships with the `ffmpeg` tool that made the fixture).
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

/// Makes a 5 s 1280×720 30 fps H.264 + AAC clip with `audio` (an FFmpeg
/// source filter) as its sound; `None` without the `ffmpeg` tool or any
/// H.264 encoder (distribution builds ship libx264 or only OpenH264).
fn make_clip(name: &str, audio: &str) -> Option<PathBuf> {
    let path = std::env::temp_dir().join(format!("quikscope-{name}-{}.mp4", std::process::id()));
    let video = "testsrc2=size=1280x720:rate=30:duration=5";
    let made = ["libx264", "libopenh264"].into_iter().any(|encoder| {
        std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "quiet", "-y"])
            .args(["-f", "lavfi", "-i", video, "-f", "lavfi", "-i", audio])
            .args([
                "-c:v", encoder, "-pix_fmt", "yuv420p", "-c:a", "aac", "-t", "5",
            ])
            .arg(&path)
            .status()
            .is_ok_and(|s| s.success())
    });
    if !made {
        eprintln!("skipping: the ffmpeg command-line tool could not make a test video");
    }
    made.then_some(path)
}

/// A test pattern with a 440 Hz tone.
static FIXTURE: LazyLock<Option<PathBuf>> =
    LazyLock::new(|| make_clip("video-editor", "sine=frequency=440:sample_rate=48000"));
/// The same pattern with silent audio, for cases that open a real audio
/// output: nothing is ever heard.
static SILENT: LazyLock<Option<PathBuf>> =
    LazyLock::new(|| make_clip("video-editor-silent", "anullsrc=r=48000:cl=stereo"));

fn open_clip(clip: &LazyLock<Option<PathBuf>>) -> Option<(PathBuf, MediaInfo)> {
    let path = LazyLock::force(clip).clone()?;
    let info = quikscope::media::decode::probe(&path).expect("probing the test video");
    Some((path, info))
}

fn fixture() -> Option<(PathBuf, MediaInfo)> {
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
    let Some((path, info)) = fixture() else {
        return;
    };
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

    // Muted, so the test never makes a sound; playback stops at the out point.
    ui.key("m");
    ui.key("space");
    assert!(ui.editor(|e, _| e.is_playing()));
    ui.wait_for("playback to reach the out point", |e| !e.is_playing());
    assert_close(ui.playhead(), 3.0, 1e-6);
    ui.wait_for("the last trimmed frame", |e| {
        e.shown_pts().is_some_and(|t| t.as_secs_f64() > 2.9)
    });
    // Playing again starts from the in point.
    ui.key("space");
    assert!(ui.editor(|e, _| e.is_playing()));
    assert!(ui.playhead() < 1.2);
    ui.key("space");
    assert!(!ui.editor(|e, _| e.is_playing()));
}

pub(super) fn real_video_exports_trimmed_gif_and_mp4() {
    let Some((path, info)) = fixture() else {
        return;
    };
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
    let Some(message) = ui.export(|ui| {
        ui.ctrl("s");
        ui.frame();
    }) else {
        return;
    };
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
    let Some(message) = ui.export(|ui| ui.ctrl("s")) else {
        return;
    };
    assert!(message.starts_with("Saved"), "{message}");
    let mp4 = ui.recordings("mp4");
    assert_eq!(mp4.len(), 1, "{mp4:?}");
    let probed = quikscope::media::decode::probe(&mp4[0]).unwrap();
    assert_eq!((probed.width, probed.height), (1280, 720));
    assert!(probed.has_audio);
    assert_close(probed.duration.as_secs_f64(), 2.0, 0.1);
    assert_ffprobe_duration(&mp4[0], 2.0);

    // Copy, muted: a silent file on the clipboard.
    ui.key("m");
    let Some(message) = ui.export(|ui| ui.ctrl("c")) else {
        return;
    };
    assert_eq!(message, "Copied MP4");
    let copied = ui.clipboard.0.lock().unwrap().clone();
    assert_eq!(copied.len(), 1);
    let probed = quikscope::media::decode::probe(&copied[0]).unwrap();
    assert!(!probed.has_audio);
    assert_close(probed.duration.as_secs_f64(), 2.0, 0.1);
    assert_ffprobe_duration(&copied[0], 2.0);
    let _ = std::fs::remove_file(&copied[0]);

    let after = std::fs::metadata(&path).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
}

pub(super) fn export_can_be_cancelled_and_leaves_no_file() {
    let Some((path, info)) = fixture() else {
        return;
    };
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

pub(super) fn annotate_frame_saves_full_size_png_and_dispatches_action() {
    let Some((path, info)) = fixture() else {
        return;
    };
    let mut ui = Ui::open(&path, info);
    ui.wait_for("the first frame", |e| e.shown_pts().is_some());
    let target = ui.at(2.5, 0.0);
    ui.click(target);
    ui.wait_for("the frame at 2.5 s", |e| {
        e.shown_pts()
            .is_some_and(|t| close(t.as_secs_f64(), 2.5, FRAME / 2.0))
    });
    ui.key("f");
    ui.wait_for("the frame to be saved", |e| {
        e.toast().is_some_and(|(m, _)| m.starts_with("Saved frame"))
    });
    let annotated = ui.annotated.lock().unwrap().clone();
    assert_eq!(annotated.len(), 1);
    assert!(annotated[0].starts_with(ui.dir.path().join("shots")));
    let image = quikscope::raster::Image::load(&annotated[0]).unwrap();
    assert_eq!((image.width, image.height), (1280, 720));
}

pub(super) fn audible_playback_opens_output_and_mute_closes_it() {
    let Some((path, info)) = open_clip(&SILENT) else {
        return;
    };
    assert!(info.has_audio);
    let mut ui = Ui::open(&path, info);
    ui.wait_for("the first frame", |e| e.shown_pts().is_some());
    // Viewing alone never opens the audio device.
    assert!(!ui.editor(|e, _| e.has_audio_output()));
    ui.shift("right");
    ui.key("o");
    ui.key("home");
    ui.key("space");
    if !ui.editor(|e, _| e.has_audio_output()) {
        eprintln!("no audio output device: playback continues silently");
    }
    ui.wait_for("playback to reach the out point", |e| !e.is_playing());
    assert_close(ui.playhead(), 1.0, 1e-6);
    ui.wait_for("the last frame before 1 s", |e| {
        e.shown_pts().is_some_and(|t| t.as_secs_f64() > 0.9)
    });
    ui.key("m");
    assert!(!ui.editor(|e, _| e.has_audio_output()));
    ui.key("space");
    assert!(ui.editor(|e, _| e.is_playing() && !e.has_audio_output()));
    ui.key("space");
}
