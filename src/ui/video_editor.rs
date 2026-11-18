//! The lightweight video editor: playback, trim, mute and export.
//!
//! The video sits letterboxed above a timeline panel (filmstrip, trim
//! handles, playhead); transport controls float at the bottom left and the
//! outputs at the bottom right, matching the annotator's chrome. The source
//! file is only ever read: every output is a new file.

mod playback;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, Bounds, Context, CursorStyle, DispatchPhase, FocusHandle, Focusable,
    FontWeight, HitboxBehavior, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Render, RenderImage,
    ScrollHandle, Size, StatefulInteractiveElement as _, Styled as _, Task, Window, canvas, div,
    fill, point, prelude::FluentBuilder as _, px, size,
};

use self::playback::{AudioOut, Clock, Filmstrip, Frame, VideoPlayer};
use super::editor::{EditorServices, tilde};
use super::theme;
use super::toolbar::{self, EDGE, GAP, PAD, base, button, icon, panel, separator};
use crate::media::export::{ExportOptions, Trim};
use crate::media::{EncoderPreference, MediaInfo, OutputFormat};

/// Every icon the video editor references (checked against the asset table).
pub const ICONS: [&str; 9] = [
    "play",
    "pause",
    "volume-2",
    "volume-x",
    "copy",
    "save",
    "chevron-down",
    "image-plus",
    "x",
];

/// Sent after "Annotate frame" saved the frame at `path` as a PNG. It
/// bubbles to the app, where the daemon can open the PNG in the annotator;
/// the editor cannot open annotator windows itself.
#[derive(Clone, Debug, PartialEq, gpui::Action)]
#[action(namespace = video_editor, no_json)]
pub struct AnnotateFrame {
    pub path: PathBuf,
}

/// Timeline panel height: a 36 px track inside 7 px padding and the border.
const TIMELINE_H: f32 = 52.0;
const TIMELINE_PAD: f32 = 7.0;
/// Trim handles sit just outside the kept range, so the track's ends are
/// inset by one handle and both handles stay grabbable at the clip's ends.
const HANDLE_W: f32 = 10.0;
/// Below the timeline: the bottom clusters and a gap.
const TIMELINE_BOTTOM: f32 = EDGE + toolbar::BAR + 8.0;
/// Space under the video: timeline, clusters and the margins between them.
const VIDEO_BOTTOM: f32 = TIMELINE_BOTTOM + TIMELINE_H + toolbar::CLEARANCE;
const MIN_W: f32 = 640.0;
const MIN_H: f32 = 400.0;
/// Thumbnails decoded for the filmstrip; tiles pick the nearest one.
const FILMSTRIP: usize = 24;
const FORMAT_W: f32 = 64.0;
const SAVE_W: f32 = 72.0;
const PROGRESS_W: f32 = 120.0;
/// GIF's palette and LZW make large frames huge; screen recordings of a
/// whole 4K monitor would otherwise produce GIFs no chat accepts. The
/// modern animation formats compress well enough to keep the source size.
const GIF_MAX_WIDTH: u32 = 1280;

/// Logical window size for a video of `info` on a screen of `screen`: the
/// video at most at one logical pixel per video pixel, within 80% of the
/// screen, plus the timeline and controls.
pub fn window_size(info: &MediaInfo, screen: Size<Pixels>) -> Size<Pixels> {
    let (sw, sh) = (f32::from(screen.width), f32::from(screen.height));
    let (chrome_w, chrome_h) = (2.0 * EDGE, EDGE + VIDEO_BOTTOM);
    let (vw, vh) = (info.width.max(1) as f32, info.height.max(1) as f32);
    let scale = ((sw * 0.8 - chrome_w) / vw)
        .min((sh * 0.8 - chrome_h) / vh)
        .clamp(0.0, 1.0);
    size(
        px((vw * scale + chrome_w).max(MIN_W).min(sw).round()),
        px((vh * scale + chrome_h).max(MIN_H).min(sh).round()),
    )
}

/// Where the video is painted in a `viewport`: as large as fits above the
/// timeline, centered, at most one logical pixel per video pixel (the size
/// [`window_size`] opens the window for; HiDPI screens scale it up like any
/// video player rather than showing it postage-stamp sized).
fn video_rect(viewport: Size<Pixels>, info: &MediaInfo, scale_factor: f32) -> Bounds<Pixels> {
    let (w, h) = (
        f32::from(viewport.width) - 2.0 * EDGE,
        f32::from(viewport.height) - EDGE - VIDEO_BOTTOM,
    );
    let (vw, vh) = (info.width.max(1) as f32, info.height.max(1) as f32);
    let zoom = (w / vw).min(h / vh).clamp(0.0, 1.0);
    // Whole device pixels keep the frame sharp.
    let snap = |v: f32| (v * scale_factor).round() / scale_factor;
    let (dw, dh) = (snap(vw * zoom), snap(vh * zoom));
    Bounds::new(
        point(
            px(snap(EDGE + (w - dw) / 2.0)),
            px(snap(EDGE + (h - dh) / 2.0)),
        ),
        size(px(dw), px(dh)),
    )
}

/// `m:ss.cc`, or `h:mm:ss.cc` from an hour on.
pub fn clock_label(t: Duration) -> String {
    let cs = t.as_millis() / 10;
    let (h, m, s, c) = (cs / 360_000, cs / 6000 % 60, cs / 100 % 60, cs % 100);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}.{c:02}")
    } else {
        format!("{m}:{s:02}.{c:02}")
    }
}

fn format_hint(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Mp4 => "H.264 + AAC",
        OutputFormat::Mkv => "H.264 + Opus",
        OutputFormat::Mov => "QuickTime",
        OutputFormat::Gif => "256 colors",
        OutputFormat::Webp => "Small, lossy",
        OutputFormat::Avif => "AV1, smallest",
        OutputFormat::Jxl => "High quality",
    }
}

fn short_label(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Jxl => "JXL",
        f => f.label(),
    }
}

/// What a running export ends in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    /// Put on the clipboard as a file.
    Copy,
    /// Written into the recordings folder.
    Save,
}

/// Which part of the timeline a drag moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grip {
    In,
    Out,
    Playhead,
}

struct Drag {
    grip: Grip,
    /// Playback paused for the drag and resumes when it ends.
    resume: bool,
}

struct Export {
    output: Output,
    format: OutputFormat,
    /// f32 bits, 0.0–1.0.
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
}

impl Drop for Export {
    fn drop(&mut self) {
        // Closing the window mid-export stops it and removes the partial file.
        self.cancel.store(true, Ordering::Release);
    }
}

struct Toast {
    message: String,
    error: bool,
    generation: u64,
}

/// Rendered bounds of the bottom clusters and the format menu, recorded by
/// tracked [`ScrollHandle`]s like the annotator's chrome probes.
#[derive(Default)]
pub struct Probes {
    pub transport: ScrollHandle,
    pub outputs: ScrollHandle,
    pub menu: ScrollHandle,
}

/// One open video. Config and clipboard come from the
/// [`super::editor::EditorServices`] global, like the annotator.
pub struct VideoEditor {
    path: PathBuf,
    info: MediaInfo,
    /// Length the timeline spans (never zero).
    duration: Duration,
    focus: FocusHandle,
    /// One source frame: arrow keys step by it and trim points snap to it.
    step: Duration,
    trim: Trim,
    playhead: Duration,
    /// Set while playing.
    clock: Option<Clock>,
    muted: bool,
    format: OutputFormat,
    menu_open: bool,
    video: VideoPlayer,
    /// A decoder seek is running; its first frame replaces what is shown.
    awaiting: bool,
    shown: Option<Frame>,
    /// The frame texture last painted, released when another replaces it.
    painted: Option<Arc<RenderImage>>,
    decode_error: Option<String>,
    audio: Option<AudioOut>,
    /// Opening the output device (or the file's audio) failed once; playback
    /// stays silent rather than retrying (and stalling) on every play.
    audio_failed: bool,
    filmstrip: Filmstrip,
    drag: Option<Drag>,
    /// The timeline track as last painted, for hit testing.
    track: Bounds<Pixels>,
    export: Option<Export>,
    toast: Option<Toast>,
    toast_generation: u64,
    pub probes: Probes,
    _wake: Task<()>,
}

impl VideoEditor {
    pub fn new(
        path: PathBuf,
        info: MediaInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> VideoEditor {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let video = VideoPlayer::spawn(path.clone(), tx.clone());
        // Device pixels of the 36 px track at the window's scale.
        let thumb_h = (36.0 * window.scale_factor()).round() as u32;
        let filmstrip = Filmstrip::spawn(path.clone(), &info, thumb_h, FILMSTRIP, tx);
        let wake = cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                while rx.try_recv().is_ok() {}
                if this.update(cx, |e, cx| e.decoded(cx)).is_err() {
                    break;
                }
            }
        });
        let config_format = cx
            .try_global::<EditorServices>()
            .map(|s| s.config.record.video_format())
            .unwrap_or(OutputFormat::Mp4);
        let format = OutputFormat::from_path(&path)
            .filter(|f| f.is_video())
            .unwrap_or(config_format);
        let step = Duration::from_secs_f64(1.0 / if info.fps > 0.0 { info.fps } else { 30.0 });
        let duration = info.duration.max(step);
        let editor = VideoEditor {
            path,
            duration,
            focus,
            step,
            trim: Trim {
                start: Duration::ZERO,
                end: duration,
            },
            playhead: Duration::ZERO,
            clock: None,
            muted: false,
            format,
            menu_open: false,
            video,
            awaiting: true,
            shown: None,
            painted: None,
            decode_error: None,
            audio: None,
            audio_failed: false,
            filmstrip,
            drag: None,
            track: Bounds::default(),
            export: None,
            toast: None,
            toast_generation: 0,
            probes: Probes::default(),
            _wake: wake,
            info,
        };
        let size = editor.decode_size(window);
        editor.video.seek(Duration::ZERO, size);
        editor
    }

    // ── state, for the window and tests ─────────────────────────────────

    pub fn trim(&self) -> Trim {
        self.trim
    }

    pub fn playhead(&self) -> Duration {
        self.playhead
    }

    pub fn is_playing(&self) -> bool {
        self.clock.is_some()
    }

    /// An audio output stream is open (playing or paused).
    pub fn has_audio_output(&self) -> bool {
        self.audio.is_some()
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    pub fn format(&self) -> OutputFormat {
        self.format
    }

    pub fn menu_open(&self) -> bool {
        self.menu_open
    }

    /// Presentation time of the frame on screen.
    pub fn shown_pts(&self) -> Option<Duration> {
        (!self.awaiting).then_some(())?;
        self.shown.as_ref().map(|f| f.pts)
    }

    /// Progress of the running export (0.0–1.0).
    pub fn export_progress(&self) -> Option<f32> {
        self.export
            .as_ref()
            .map(|e| f32::from_bits(e.progress.load(Ordering::Relaxed)))
    }

    pub fn toast(&self) -> Option<(&str, bool)> {
        self.toast.as_ref().map(|t| (t.message.as_str(), t.error))
    }

    /// The timeline track as last painted.
    pub fn track_bounds(&self) -> Bounds<Pixels> {
        self.track
    }

    /// Window x of media time `t` on the timeline.
    pub fn timeline_x(&self, t: Duration) -> Pixels {
        let inner = self.track_inner();
        let f = t.as_secs_f64() / self.duration.as_secs_f64();
        inner.left() + inner.size.width * f.clamp(0.0, 1.0) as f32
    }

    /// Painted bounds of a control: `play`, `mute`, `copy`, `format`,
    /// `save`, `frame`, `cancel`, or `format:<ext>` while the menu is open.
    pub fn control_bounds(&self, id: &str) -> Option<Bounds<Pixels>> {
        if let Some(ext) = id.strip_prefix("format:") {
            let format = OutputFormat::parse(ext)?;
            // Headers precede each group in the menu.
            let index = match OutputFormat::VIDEO.iter().position(|f| *f == format) {
                Some(i) => 1 + i,
                None => {
                    2 + OutputFormat::VIDEO.len()
                        + OutputFormat::ANIMATION.iter().position(|f| *f == format)?
                }
            };
            return self.probes.menu.bounds_for_item(index);
        }
        let (handle, index) = match id {
            "play" => (&self.probes.transport, 0),
            "mute" => (&self.probes.transport, 2),
            "copy" | "cancel" => (&self.probes.outputs, if id == "copy" { 0 } else { 2 }),
            "format" => (&self.probes.outputs, 1),
            "save" => (&self.probes.outputs, 2),
            "frame" => (&self.probes.outputs, 4),
            _ => return None,
        };
        if (id == "cancel") != self.export.is_some() || (id == "mute" && !self.info.has_audio) {
            return None;
        }
        handle.bounds_for_item(index)
    }

    fn trimmed(&self) -> bool {
        self.trim.start > Duration::ZERO || self.trim.end < self.duration
    }

    /// Frame pixels for the video area: what the window shows it at, never
    /// more than the source.
    fn decode_size(&self, window: &Window) -> (u32, u32) {
        let sf = window.scale_factor();
        let rect = video_rect(window.viewport_size(), &self.info, sf);
        let w = (f32::from(rect.size.width) * sf).round().max(2.0) as u32;
        let h = (f32::from(rect.size.height) * sf).round().max(2.0) as u32;
        (
            w.min(self.info.width.max(2)),
            h.min(self.info.height.max(2)),
        )
    }

    /// `t` on the source's frame grid, inside the clip.
    fn snap(&self, t: Duration) -> Duration {
        let rate = if self.info.fps > 0.0 {
            self.info.fps
        } else {
            30.0
        };
        let frames = (t.as_secs_f64() * rate).round();
        // Rounded to whole nanoseconds: truncation would put frame 30 of a
        // 30 fps clip a nanosecond before 1 s.
        Duration::from_nanos((frames / rate * 1e9).round() as u64).min(self.duration)
    }

    fn track_inner(&self) -> Bounds<Pixels> {
        let mut inner = self.track;
        inner.origin.x += px(HANDLE_W);
        inner.size.width = (inner.size.width - px(2.0 * HANDLE_W)).max(px(1.));
        inner
    }

    fn time_at(&self, x: Pixels) -> Duration {
        let inner = self.track_inner();
        let f = f32::from(x - inner.left()) / f32::from(inner.size.width);
        self.snap(self.duration.mul_f64(f.clamp(0.0, 1.0) as f64))
    }

    // ── playback ─────────────────────────────────────────────────────────

    /// The decode threads produced something.
    fn decoded(&mut self, cx: &mut Context<Self>) {
        if self.decode_error.is_none()
            && let Some(error) = self.video.error()
        {
            log::error!("playing {}: {error}", self.path.display());
            self.decode_error = Some(error);
        }
        if self.clock.is_none() && self.awaiting {
            if let Some(frame) = self.video.take_first() {
                self.shown = Some(frame);
                self.awaiting = false;
            } else if self.video.ended() {
                self.awaiting = false;
            }
        }
        cx.notify();
    }

    /// Moves playback to `at` (clamped to the trim), keeping play/pause.
    fn seek(&mut self, at: Duration, window: &Window) {
        let at = at.clamp(self.trim.start, self.trim.end);
        self.playhead = at;
        let size = self.decode_size(window);
        if let Some(clock) = &mut self.clock {
            *clock = Clock::start(at);
            self.video.seek(at, size);
            self.awaiting = true;
            if let Some(audio) = &self.audio {
                audio.seek(at);
            }
            return;
        }
        // Frame times come from the stream's time base and the playhead from
        // the frame grid; the two can differ by rounding. Picking the frame
        // showing a quarter frame later lands on the intended frame either
        // way.
        let pick = at + self.step / 4;
        let settled = !self.awaiting && self.video.size() == size;
        match &self.shown {
            Some(f) if settled && f.covers(pick) => {}
            Some(f) if settled && f.pts <= pick && self.video.covers(pick, size) => {
                if let Some(next) = self.video.take_due(pick) {
                    self.shown = Some(next);
                }
            }
            _ => {
                self.video.seek(pick, size);
                self.awaiting = true;
            }
        }
    }

    fn toggle_play(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.clock.is_some() {
            self.pause();
        } else {
            self.play(window);
        }
        cx.notify();
    }

    fn play(&mut self, window: &Window) {
        if self.playhead + self.step / 2 >= self.trim.end {
            self.seek(self.trim.start, window);
        }
        self.clock = Some(Clock::start(self.playhead));
        self.start_audio();
    }

    fn pause(&mut self) {
        self.clock = None;
        if let Some(audio) = &self.audio {
            audio.set_playing(false);
        }
    }

    fn start_audio(&mut self) {
        if self.muted || !self.info.has_audio || self.audio_failed || self.clock.is_none() {
            return;
        }
        if self.audio.is_none() {
            // Opened on first play, not on open: viewing a clip never wakes
            // the audio device.
            match AudioOut::open(&self.path, self.playhead) {
                Ok(Some(audio)) => self.audio = Some(audio),
                Ok(None) => {
                    self.audio_failed = true;
                    return;
                }
                Err(e) => {
                    log::warn!("audio playback unavailable: {e:#}");
                    self.audio_failed = true;
                    return;
                }
            }
        }
        if let Some(audio) = &self.audio {
            // Samples queued before a pause were already handed to the
            // device; restarting at the playhead keeps sound and picture
            // together.
            audio.seek(self.playhead);
            audio.set_playing(true);
        }
    }

    fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        if !self.info.has_audio {
            return;
        }
        self.muted = !self.muted;
        if self.muted {
            // Muted means no stream at all, not a silent one.
            self.audio = None;
        } else {
            self.start_audio();
        }
        cx.notify();
    }

    /// Advances playback to the clock before painting.
    fn advance(&mut self, window: &mut Window) {
        let size = self.decode_size(window);
        let Some(clock) = &mut self.clock else {
            if size != self.video.size() {
                // Resized: decode again at the new size where we are.
                self.video.seek(self.playhead + self.step / 4, size);
                self.awaiting = true;
            }
            return;
        };
        if let Some(audio) = self.audio.as_ref().and_then(AudioOut::position) {
            clock.sync(audio);
        }
        let now = clock.now();
        if size != self.video.size() {
            self.video.seek(now, size);
            self.awaiting = true;
        }
        if now >= self.trim.end {
            self.pause();
            self.playhead = self.trim.end;
            self.seek(self.trim.end, window);
            return;
        }
        if let Some(frame) = self.video.take_due(now) {
            self.shown = Some(frame);
            self.awaiting = false;
        }
        self.playhead = now.max(self.trim.start);
        if self.video.ended()
            && self
                .shown
                .as_ref()
                .is_none_or(|f| now >= f.pts + f.duration)
        {
            self.pause();
            return;
        }
        window.request_animation_frame();
    }

    fn step_frames(&mut self, frames: i32, window: &Window, cx: &mut Context<Self>) {
        self.pause();
        let delta = self.step * frames.unsigned_abs();
        let at = if frames < 0 {
            self.playhead.saturating_sub(delta)
        } else {
            self.playhead + delta
        };
        self.seek(self.snap(at), window);
        cx.notify();
    }

    fn jump(&mut self, seconds: i32, window: &Window, cx: &mut Context<Self>) {
        let delta = Duration::from_secs(seconds.unsigned_abs() as u64);
        let at = if seconds < 0 {
            self.playhead.saturating_sub(delta)
        } else {
            self.playhead + delta
        };
        self.seek(self.snap(at), window);
        cx.notify();
    }

    // ── trimming ─────────────────────────────────────────────────────────

    /// Sets the in point, at least one frame before the out point.
    fn set_in(&mut self, at: Duration, window: &Window, cx: &mut Context<Self>) {
        self.trim.start = at.min(self.trim.end.saturating_sub(self.step));
        if self.playhead < self.trim.start {
            self.seek(self.trim.start, window);
        }
        cx.notify();
    }

    /// Sets the out point, at least one frame after the in point.
    fn set_out(&mut self, at: Duration, window: &Window, cx: &mut Context<Self>) {
        self.trim.end = at.max(self.trim.start + self.step).min(self.duration);
        if self.playhead > self.trim.end {
            self.seek(self.trim.end, window);
        }
        cx.notify();
    }

    fn timeline_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.menu_open = false;
        let x = ev.position.x;
        let (in_x, out_x) = (
            self.timeline_x(self.trim.start),
            self.timeline_x(self.trim.end),
        );
        let on_in = x >= in_x - px(HANDLE_W + 2.0) && x <= in_x + px(4.);
        let on_out = x >= out_x - px(4.) && x <= out_x + px(HANDLE_W + 2.0);
        let grip = match (on_in, on_out) {
            (true, true) if (x - in_x).abs() <= (x - out_x).abs() => Grip::In,
            (true, true) => Grip::Out,
            (true, false) => Grip::In,
            (false, true) => Grip::Out,
            (false, false) => Grip::Playhead,
        };
        let resume = self.clock.is_some();
        self.pause();
        self.drag = Some(Drag { grip, resume });
        self.drag_to(x, window, cx);
    }

    fn drag_to(&mut self, x: Pixels, window: &Window, cx: &mut Context<Self>) {
        let Some(drag) = &self.drag else { return };
        let t = self.time_at(x);
        // The playhead follows a trim handle so you see the frame you cut at.
        match drag.grip {
            Grip::In => {
                self.set_in(t, window, cx);
                self.seek(self.trim.start, window);
            }
            Grip::Out => {
                self.set_out(t, window, cx);
                self.seek(self.trim.end, window);
            }
            Grip::Playhead => self.seek(t, window),
        }
        cx.notify();
    }

    fn mouse_move(&mut self, ev: &MouseMoveEvent, window: &Window, cx: &mut Context<Self>) {
        if self.drag.is_some() {
            if ev.pressed_button == Some(MouseButton::Left) {
                self.drag_to(ev.position.x, window, cx);
            } else {
                self.end_drag(window, cx);
            }
        }
    }

    fn end_drag(&mut self, window: &Window, cx: &mut Context<Self>) {
        if let Some(drag) = self.drag.take() {
            if drag.resume {
                self.play(window);
            }
            cx.notify();
        }
    }

    // ── outputs ──────────────────────────────────────────────────────────

    fn set_format(&mut self, format: OutputFormat, cx: &mut Context<Self>) {
        self.format = format;
        self.menu_open = false;
        cx.notify();
    }

    fn toggle_menu(&mut self, cx: &mut Context<Self>) {
        self.menu_open = !self.menu_open;
        cx.notify();
    }

    fn show_toast(&mut self, message: String, error: bool, cx: &mut Context<Self>) {
        self.toast_generation += 1;
        let generation = self.toast_generation;
        self.toast = Some(Toast {
            message,
            error,
            generation,
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(if error { 6000 } else { 2500 }))
                .await;
            let _ = this.update(cx, |e, cx| {
                if e.toast.as_ref().is_some_and(|t| t.generation == generation) {
                    e.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Whether the output differs from the source file at all; when it does
    /// not, copy and save use the file as it is instead of re-encoding.
    fn needs_encode(&self) -> bool {
        self.trimmed()
            || (self.muted && self.info.has_audio)
            || OutputFormat::from_path(&self.path) != Some(self.format)
    }

    /// Ctrl+C / Ctrl+S: exports the trimmed range in the chosen format, then
    /// puts the file on the clipboard or leaves it in the recordings folder.
    fn export(&mut self, output: Output, cx: &mut Context<Self>) {
        if self.export.is_some() {
            self.show_toast("An export is already running".into(), false, cx);
            return;
        }
        let Some(services) = cx.try_global::<EditorServices>().cloned() else {
            self.show_toast("Saving is unavailable".into(), true, cx);
            return;
        };
        let clipboard = services.clipboard.clone();
        if output == Output::Copy && clipboard.is_none() {
            self.show_toast("Clipboard is unavailable".into(), true, cx);
            return;
        }
        if output == Output::Copy && !self.needs_encode() {
            if let Some(c) = &clipboard {
                c.set_files(std::slice::from_ref(&self.path));
            }
            self.show_toast(format!("Copied {}", self.format.label()), false, cx);
            return;
        }
        let record = &services.config.record;
        let format = self.format;
        let dst = match output {
            Output::Save => record.new_save_path(SystemTime::now(), format),
            Output::Copy => clip_path(&self.path, format),
        };
        let source_fps = self.info.fps.round().max(1.0) as u32;
        let options = ExportOptions {
            format,
            trim: self.trimmed().then_some(self.trim),
            mute: self.muted || !self.info.has_audio,
            fps: (!format.is_video()).then(|| record.animation_fps.min(source_fps)),
            max_width: (format == OutputFormat::Gif).then_some(GIF_MAX_WIDTH),
            encoder: EncoderPreference::parse(&record.encoder).unwrap_or_default(),
        };
        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        // Progress repaints and the result arrive over one channel, so the
        // window wakes when something changed rather than on a timer.
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Option<anyhow::Result<PathBuf>>>();
        let src = self.path.clone();
        let reencode = self.needs_encode();
        let (p, c) = (progress.clone(), cancel.clone());
        // A dedicated thread: an export runs for minutes and would otherwise
        // hold one of GPUI's few background workers.
        let spawned = std::thread::Builder::new()
            .name("quikscope-export".into())
            .spawn(move || {
                let result = if reencode {
                    let report = |f: f32| {
                        // A repaint per whole percent, not per frame.
                        let before = f32::from_bits(p.swap(f.to_bits(), Ordering::Relaxed));
                        if (f * 100.0) as u32 != (before * 100.0) as u32 {
                            let _ = tx.unbounded_send(None);
                        }
                    };
                    create_parent(&dst).and_then(|()| {
                        crate::media::export::export(&src, &dst, &options, None, &report, &c)
                    })
                } else {
                    copy_file(&src, &dst)
                };
                let _ = tx.unbounded_send(Some(result.map(|()| dst)));
            });
        if let Err(e) = spawned {
            self.show_toast(format!("Export failed: {e}"), true, cx);
            return;
        }
        self.export = Some(Export {
            output,
            format,
            progress,
            cancel,
        });
        self.menu_open = false;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = loop {
                match rx.next().await {
                    Some(Some(result)) => break result,
                    Some(None) => {
                        if this.update(cx, |_, cx| cx.notify()).is_err() {
                            return;
                        }
                    }
                    None => break Err(anyhow::anyhow!("the export thread stopped")),
                }
            };
            let _ = this.update(cx, |e, cx| e.finish_export(result, clipboard, cx));
        })
        .detach();
    }

    fn finish_export(
        &mut self,
        result: anyhow::Result<PathBuf>,
        clipboard: Option<Arc<dyn crate::platform::Clipboard>>,
        cx: &mut Context<Self>,
    ) {
        let Some(export) = self.export.take() else {
            return;
        };
        let cancelled = export.cancel.load(Ordering::Acquire);
        match result {
            Ok(path) => {
                log::info!("exported {}", path.display());
                match export.output {
                    Output::Save => self.show_toast(format!("Saved {}", tilde(&path)), false, cx),
                    Output::Copy => {
                        if let Some(c) = clipboard {
                            c.set_files(&[path]);
                        }
                        self.show_toast(format!("Copied {}", export.format.label()), false, cx);
                    }
                }
            }
            Err(_) if cancelled => self.show_toast("Export cancelled".into(), false, cx),
            Err(e) => {
                log::error!("export of {} failed: {e:#}", self.path.display());
                self.show_toast(format!("Export failed: {e:#}"), true, cx);
            }
        }
    }

    fn cancel_export(&mut self, cx: &mut Context<Self>) {
        if let Some(export) = &self.export {
            // The export thread notices, removes its partial file and
            // reports back; the toast says so then.
            export.cancel.store(true, Ordering::Release);
            cx.notify();
        }
    }

    /// Saves the frame on screen, at full resolution, as a screenshot PNG
    /// and offers it to the annotator ([`AnnotateFrame`]).
    fn annotate_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(services) = cx.try_global::<EditorServices>().cloned() else {
            self.show_toast("Saving is unavailable".into(), true, cx);
            return;
        };
        let at = self.shown.as_ref().map_or(self.playhead, |f| f.pts);
        let path = services.config.new_save_path(SystemTime::now());
        let src = self.path.clone();
        let task = cx.background_spawn(async move {
            // The preview is scaled to the window; decode the frame again at
            // the source size.
            let mut decoder = crate::media::decode::Decoder::open(&src)?;
            decoder.seek(at)?;
            let frame = decoder.next_frame()?.context("No frame at the playhead")?;
            frame
                .image
                .save_png(&path, crate::raster::Compression::Balanced)?;
            anyhow::Ok(path)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |e, window, cx| match result {
                Ok(path) => {
                    e.show_toast(format!("Saved frame {}", tilde(&path)), false, cx);
                    window.dispatch_action(Box::new(AnnotateFrame { path }), cx);
                }
                Err(err) => e.show_toast(format!("Saving the frame failed: {err:#}"), true, cx),
            });
        })
        .detach();
    }

    // ── input ────────────────────────────────────────────────────────────

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let m = ks.modifiers;
        if m.control || m.platform {
            match ks.key.as_str() {
                "c" => self.export(Output::Copy, cx),
                "s" => self.export(Output::Save, cx),
                "w" | "q" => window.remove_window(),
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        match ks.key.as_str() {
            "escape" if self.menu_open => self.menu_open = false,
            "escape" if self.export.is_some() => self.cancel_export(cx),
            "escape" if self.drag.is_some() => self.end_drag(window, cx),
            "escape" => {
                window.remove_window();
                return;
            }
            "space" => self.toggle_play(window, cx),
            "left" if m.shift => self.jump(-1, window, cx),
            "right" if m.shift => self.jump(1, window, cx),
            "left" => self.step_frames(-1, window, cx),
            "right" => self.step_frames(1, window, cx),
            "home" => self.seek(self.trim.start, window),
            "end" => self.seek(self.trim.end, window),
            "i" => self.set_in(self.playhead, window, cx),
            "o" => self.set_out(self.playhead, window, cx),
            "m" => self.toggle_mute(cx),
            "f" => self.annotate_frame(window, cx),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    // ── painting ─────────────────────────────────────────────────────────

    fn paint_video(&mut self, bounds: Bounds<Pixels>, window: &mut Window) {
        let rect = video_rect(window.viewport_size(), &self.info, window.scale_factor());
        window.paint_quad(fill(rect, gpui::rgb(0x0b0b0d)));
        let Some(frame) = &self.shown else { return };
        if let Some(old) = &self.painted
            && !Arc::ptr_eq(old, &frame.image)
        {
            // One texture per decoded frame: release each as it is replaced
            // or the atlas grows for the whole playback.
            let _ = window.drop_image(old.clone());
        }
        self.painted = Some(frame.image.clone());
        if let Err(e) = window.paint_image(bounds, rect, (0.).into(), frame.image.clone(), 0, false)
        {
            log::error!("painting a video frame failed: {e:#}");
        }
    }

    fn paint_track(
        &mut self,
        bounds: Bounds<Pixels>,
        hitbox: &gpui::Hitbox,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.track = bounds;
        let inner = self.track_inner();
        window.paint_quad(fill(bounds, gpui::rgb(0x18181c)).corner_radii(px(4.)));

        // Filmstrip: tiles at the thumbnails' aspect, each showing the
        // thumbnail nearest its middle.
        let tile_w = f32::from(inner.size.height) * self.info.width.max(1) as f32
            / self.info.height.max(1) as f32;
        if tile_w >= 1.0 {
            let tiles = (f32::from(inner.size.width) / tile_w).ceil() as usize;
            for i in 0..tiles {
                let x = inner.left() + px(i as f32 * tile_w);
                let mid = (i as f32 + 0.5) * tile_w / f32::from(inner.size.width);
                let Some(thumb) = self
                    .filmstrip
                    .nearest(self.duration.mul_f64(mid.min(1.0) as f64))
                else {
                    continue;
                };
                let tile = Bounds::new(point(x, inner.top()), size(px(tile_w), inner.size.height));
                let _ = window.paint_image(inner, tile, (0.).into(), thumb, 0, false);
            }
        }

        // Everything outside the trim is dimmed.
        let (in_x, out_x) = (
            self.timeline_x(self.trim.start),
            self.timeline_x(self.trim.end),
        );
        let dim = gpui::rgba(0x121214c8);
        if in_x > inner.left() {
            window.paint_quad(fill(
                Bounds::from_corners(inner.origin, point(in_x, inner.bottom())),
                dim,
            ));
        }
        if out_x < inner.right() {
            window.paint_quad(fill(
                Bounds::from_corners(point(out_x, inner.top()), inner.bottom_right()),
                dim,
            ));
        }

        // The kept range: an accent frame with a handle at each end.
        let accent = if self.drag.as_ref().is_some_and(|d| d.grip != Grip::Playhead) {
            theme::accent_hover()
        } else {
            theme::accent()
        };
        let edge = px(2.);
        window.paint_quad(fill(
            Bounds::from_corners(point(in_x, bounds.top()), point(out_x, bounds.top() + edge)),
            accent,
        ));
        window.paint_quad(fill(
            Bounds::from_corners(
                point(in_x, bounds.bottom() - edge),
                point(out_x, bounds.bottom()),
            ),
            accent,
        ));
        for (left, corners) in [
            (in_x - px(HANDLE_W), (px(4.), px(0.))),
            (out_x, (px(0.), px(4.))),
        ] {
            let handle = Bounds::new(
                point(left, bounds.top()),
                size(px(HANDLE_W), bounds.size.height),
            );
            window.paint_quad(fill(handle, accent).corner_radii(gpui::Corners {
                top_left: corners.0,
                bottom_left: corners.0,
                top_right: corners.1,
                bottom_right: corners.1,
            }));
            let grip = Bounds::new(
                point(left + px(HANDLE_W / 2.0 - 1.0), bounds.center().y - px(7.)),
                size(px(2.), px(14.)),
            );
            window.paint_quad(fill(grip, theme::on_accent()).corner_radii(px(1.)));
        }

        // Playhead: a line over the track with a knob above it.
        let x = self.timeline_x(self.playhead);
        let line = Bounds::new(
            point(x - px(1.), bounds.top() - px(3.)),
            size(px(2.), bounds.size.height + px(6.)),
        );
        window.paint_quad(fill(line, theme::text()).corner_radii(px(1.)));
        window.paint_quad(
            fill(
                Bounds::new(
                    point(x - px(5.), bounds.top() - px(5.)),
                    size(px(10.), px(6.)),
                ),
                theme::text(),
            )
            .corner_radii(px(2.)),
        );

        let mouse = window.mouse_position().x;
        let near = |hx: Pixels| (mouse - hx).abs() <= px(HANDLE_W);
        let cursor = match &self.drag {
            Some(d) if d.grip != Grip::Playhead => CursorStyle::ResizeLeftRight,
            None if hitbox.is_hovered(window)
                && (near(in_x - px(HANDLE_W / 2.0)) || near(out_x + px(HANDLE_W / 2.0))) =>
            {
                CursorStyle::ResizeLeftRight
            }
            _ => CursorStyle::PointingHand,
        };
        window.set_cursor_style(cursor, hitbox);

        if self.drag.is_some() {
            let editor = cx.entity();
            window.on_mouse_event({
                let editor = editor.clone();
                move |ev: &MouseMoveEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble {
                        editor.update(cx, |e, cx| e.mouse_move(ev, window, cx));
                    }
                }
            });
            window.on_mouse_event(move |_: &MouseUpEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    editor.update(cx, |e, cx| e.end_drag(window, cx));
                }
            });
        }
    }

    fn transport(&self, narrow: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let playing = self.clock.is_some();
        let time = format!(
            "{} / {}",
            clock_label(self.playhead),
            clock_label(self.duration)
        );
        let trim_len = self.trim.end - self.trim.start;
        panel("transport", &self.probes.transport)
            .absolute()
            .left(px(EDGE))
            .bottom(px(EDGE))
            .gap(px(GAP))
            .p(px(PAD))
            .child(
                button(
                    "play",
                    if playing {
                        "Pause (Space)"
                    } else {
                        "Play (Space)"
                    },
                    false,
                    true,
                )
                .on_click(cx.listener(|e, _, window, cx| e.toggle_play(window, cx)))
                .child(icon(if playing { "pause" } else { "play" }, theme::text())),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .px_1p5()
                    .text_xs()
                    .text_color(theme::text())
                    .child(time),
            )
            .when(self.info.has_audio, |d| {
                d.child(
                    button(
                        "mute",
                        if self.muted { "Unmute (M)" } else { "Mute (M)" },
                        self.muted,
                        true,
                    )
                    .on_click(cx.listener(|e, _, _, cx| e.toggle_mute(cx)))
                    .child(icon(
                        if self.muted { "volume-x" } else { "volume-2" },
                        if self.muted {
                            theme::accent()
                        } else {
                            theme::text()
                        },
                    )),
                )
            })
            .when(self.trimmed() && !narrow, |d| {
                d.child(separator()).child(
                    div()
                        .flex_shrink_0()
                        .px_1p5()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(format!("Trim {:.2} s", trim_len.as_secs_f64())),
                )
            })
    }

    fn outputs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cluster = panel("outputs", &self.probes.outputs)
            .absolute()
            .right(px(EDGE))
            .bottom(px(EDGE))
            .gap(px(GAP))
            .p(px(PAD));
        if let Some(export) = &self.export {
            let progress = f32::from_bits(export.progress.load(Ordering::Relaxed)).clamp(0.0, 1.0);
            let verb = match export.output {
                Output::Copy => "Copying",
                Output::Save => "Saving",
            };
            return cluster
                .child(
                    div()
                        .flex_shrink_0()
                        .px_1p5()
                        .text_xs()
                        .text_color(theme::text())
                        .child(format!("{verb} {}", short_label(export.format))),
                )
                .child(
                    div()
                        .id("export-progress")
                        .flex_shrink_0()
                        .w(px(PROGRESS_W))
                        .h(px(6.))
                        .rounded_full()
                        .bg(theme::hover())
                        .child(
                            div()
                                .h_full()
                                .rounded_full()
                                .bg(theme::accent())
                                .w(px(PROGRESS_W * progress)),
                        ),
                )
                .child(
                    button("cancel", "Cancel export (Esc)", false, true)
                        .on_click(cx.listener(|e, _, _, cx| e.cancel_export(cx)))
                        .child(icon("x", theme::text())),
                );
        }
        cluster
            .child(
                button("copy", "Copy file (Ctrl+C)", false, true)
                    .on_click(cx.listener(|e, _, _, cx| e.export(Output::Copy, cx)))
                    .child(icon("copy", theme::text())),
            )
            .child(
                base("format", "Output format")
                    .w(px(FORMAT_W))
                    .gap_1()
                    .text_sm()
                    .text_color(theme::text())
                    .when(self.menu_open, |d| d.bg(theme::accent_bg()))
                    .when(!self.menu_open, |d| d.hover(|s| s.bg(theme::hover())))
                    .on_click(cx.listener(|e, _, _, cx| e.toggle_menu(cx)))
                    .child(short_label(self.format))
                    .child(icon("chevron-down", theme::muted())),
            )
            .child(
                // The one primary action, as in the annotator.
                base("save", "Save to recordings folder (Ctrl+S)")
                    .w(px(SAVE_W))
                    .gap_1p5()
                    .bg(theme::accent())
                    .hover(|s| s.bg(theme::accent_hover()))
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::on_accent())
                    .on_click(cx.listener(|e, _, _, cx| e.export(Output::Save, cx)))
                    .child(icon("save", theme::on_accent()))
                    .child("Save"),
            )
            .child(separator())
            .child(
                button("frame", "Annotate frame (F)", false, true)
                    .on_click(cx.listener(|e, _, window, cx| e.annotate_frame(window, cx)))
                    .child(icon("image-plus", theme::text())),
            )
    }

    fn format_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let header = |title: &'static str| {
            div()
                .px_2()
                .pt_1p5()
                .pb_0p5()
                .text_xs()
                .text_color(theme::muted())
                .child(title)
        };
        let item = |format: OutputFormat| {
            let active = self.format == format;
            div()
                .id(gpui::ElementId::Name(
                    format!("format-{}", format.extension()).into(),
                ))
                .flex()
                .items_center()
                .justify_between()
                .gap_4()
                .h(px(28.))
                .px_2()
                .rounded_md()
                .text_sm()
                .when(active, |d| {
                    d.bg(theme::accent_bg()).text_color(theme::accent())
                })
                .when(!active, |d| {
                    d.text_color(theme::text()).hover(|s| s.bg(theme::hover()))
                })
                .on_click(cx.listener(move |e, _, _, cx| e.set_format(format, cx)))
                .child(format.label())
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(format_hint(format)),
                )
        };
        let menu = div()
            .id("format-menu")
            .track_scroll(&self.probes.menu)
            .occlude()
            .absolute()
            .right(px(EDGE))
            .bottom(px(EDGE + toolbar::BAR + 6.0))
            .w(px(220.))
            .p(px(PAD))
            .flex()
            .flex_col()
            .rounded_lg()
            .bg(theme::panel())
            .border_1()
            .border_color(theme::panel_border())
            .shadow_lg()
            .child(header("Video"))
            .children(OutputFormat::VIDEO.map(item))
            .child(header("Animation · no sound"))
            .children(OutputFormat::ANIMATION.map(item));
        // A backdrop over the window: clicking anywhere else closes the menu
        // without also pressing whatever is under the pointer.
        div()
            .absolute()
            .inset_0()
            .child(
                div()
                    .id("format-backdrop")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|e, _, _, cx| {
                            e.menu_open = false;
                            cx.notify();
                        }),
                    ),
            )
            .child(menu)
    }

    fn toast_element(&self) -> Option<impl IntoElement> {
        let t = self.toast.as_ref()?;
        Some(
            div()
                .absolute()
                .bottom(px(VIDEO_BOTTOM + 4.0))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("toast")
                        .occlude()
                        .px_3()
                        .py_1p5()
                        .rounded_lg()
                        .bg(theme::panel())
                        .border_1()
                        .border_color(if t.error {
                            gpui::rgb(0xe03131)
                        } else {
                            theme::panel_border()
                        })
                        .max_w(px(560.))
                        .mx_4()
                        .text_sm()
                        .text_color(theme::text())
                        .child(t.message.clone()),
                ),
        )
    }
}

/// Where a copied clip is written: the cache, under the source's name so a
/// chat app shows a sensible file name. Clips older than a day are removed
/// first; the clipboard only needs the newest.
fn clip_path(src: &Path, format: OutputFormat) -> PathBuf {
    let dir = crate::config::cache_dir().join("clips");
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let day = Duration::from_secs(24 * 3600);
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > day);
            if old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clip".into());
    let path = dir.join(format!("{stem}.{}", format.extension()));
    if path == src {
        dir.join(format!("{stem}-clip.{}", format.extension()))
    } else {
        path
    }
}

fn create_parent(path: &Path) -> anyhow::Result<()> {
    let dir = path.parent().context("save path has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

/// Saves an untouched source as it is: no generation loss, no encoder.
fn copy_file(src: &Path, dst: &Path) -> anyhow::Result<()> {
    create_parent(dst)?;
    let part = dst.with_extension(format!(
        "{}.part",
        dst.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    std::fs::copy(src, &part).with_context(|| format!("writing {}", part.display()))?;
    std::fs::rename(&part, dst).with_context(|| format!("writing {}", dst.display()))
}

impl Focusable for VideoEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for VideoEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.advance(window);
        let editor = cx.entity();
        let viewport = window.viewport_size();
        let narrow = viewport.width < px(MIN_W);
        let rect = video_rect(viewport, &self.info, window.scale_factor());
        let error = self.decode_error.clone().map(|e| {
            div()
                .absolute()
                .left(rect.left())
                .top(rect.top())
                .w(rect.size.width)
                .h(rect.size.height)
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_1()
                .p_4()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme::text())
                        .child("This video can't be played"),
                )
                .child(div().text_xs().text_color(gpui::rgb(0xff9999)).child(e))
        });
        let track_editor = editor.clone();
        div()
            .id("video-editor")
            .key_context("VideoEditor")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .bg(theme::canvas_bg())
            .font_family(super::UI_FONT)
            .text_color(theme::text())
            .on_key_down(cx.listener(Self::key_down))
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, (), window, cx| {
                        editor.update(cx, |e, _| e.paint_video(bounds, window));
                    },
                )
                .absolute()
                .size_full(),
            )
            .children(error)
            .child(
                div()
                    .id("timeline")
                    .occlude()
                    .absolute()
                    .left(px(EDGE))
                    .right(px(EDGE))
                    .bottom(px(TIMELINE_BOTTOM))
                    .h(px(TIMELINE_H))
                    .p(px(TIMELINE_PAD))
                    .rounded_lg()
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::panel_border())
                    .shadow_lg()
                    .child(
                        div()
                            .size_full()
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::timeline_down))
                            .child(
                                canvas(
                                    |bounds, window, _| {
                                        window.insert_hitbox(bounds, HitboxBehavior::Normal)
                                    },
                                    move |bounds, hitbox, window, cx| {
                                        track_editor.update(cx, |e, cx| {
                                            e.paint_track(bounds, &hitbox, window, cx)
                                        });
                                    },
                                )
                                .size_full(),
                            ),
                    ),
            )
            .child(self.transport(narrow, cx))
            .child(self.outputs(cx))
            .children(self.toast_element())
            .when(self.menu_open, |d| d.child(self.format_menu(cx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(w: u32, h: u32) -> MediaInfo {
        MediaInfo {
            width: w,
            height: h,
            duration: Duration::from_secs(5),
            fps: 30.0,
            frames: Some(150),
            has_audio: true,
            kind: crate::media::Kind::Video,
        }
    }

    #[test]
    fn window_fits_the_video_at_most_at_actual_size() {
        let s = window_size(&info(1280, 720), size(px(1920.), px(1080.)));
        // 80% of 1080 leaves less than 720 px for the video: it shrinks.
        assert!(s.height <= px(1080. * 0.8 + 1.), "{s:?}");
        let small = window_size(&info(320, 200), size(px(1920.), px(1080.)));
        assert_eq!(small.width, px(MIN_W));
        let huge = window_size(&info(7680, 4320), size(px(1280.), px(720.)));
        assert!(huge.width <= px(1280.) && huge.height <= px(720.));
    }

    #[test]
    fn video_rect_letterboxes_and_never_magnifies() {
        let r = video_rect(size(px(1000.), px(700.)), &info(1920, 1080), 1.0);
        let ratio = f32::from(r.size.width) / f32::from(r.size.height);
        assert!((ratio - 16.0 / 9.0).abs() < 0.02, "{r:?}");
        assert!(r.left() >= px(EDGE) && r.right() <= px(1000. - EDGE));
        assert!(r.bottom() <= px(700. - VIDEO_BOTTOM));
        let tiny = video_rect(size(px(1000.), px(700.)), &info(100, 50), 2.0);
        assert_eq!(tiny.size, size(px(100.), px(50.)));
    }

    #[test]
    fn clock_labels() {
        assert_eq!(clock_label(Duration::from_millis(1234)), "0:01.23");
        assert_eq!(clock_label(Duration::from_secs(61)), "1:01.00");
        assert_eq!(clock_label(Duration::from_secs(3723)), "1:02:03.00");
    }
}
