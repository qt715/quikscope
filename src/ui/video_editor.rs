//! The lightweight video editor: playback, trim, mute and export.
//!
//! The video sits letterboxed above a timeline panel (filmstrip, trim
//! handles, playhead); transport controls float at the bottom left and the
//! outputs at the bottom right, matching the annotator's chrome. The source
//! file is only ever read: every output is a new file.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

use super::clips::{self, Clip, Pin};
use super::editor::{EditorServices, tilde};
use super::export_job::{self, Export, Finished, Output, Progress};
use super::playback::{AudioOut, Clock, Filmstrip, Frame, Queue, VideoPlayer};
use super::theme;
use super::toolbar::{self, EDGE, GAP, PAD, Tip, base, button, icon, panel, separator};
use crate::media::export::{ExportOptions, Trim};
use crate::media::{EncoderPreference, MediaInfo, OutputFormat};
use crate::output::OutputFile;

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

/// Sent after "Annotate frame" wrote the frame at `path`, a PNG in the clip
/// cache (nothing lands in the screenshots folder until the annotator
/// saves). It bubbles to the app, where the daemon opens the PNG in the
/// annotator; the editor cannot open annotator windows itself.
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

pub(super) fn format_hint(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Mp4 => "H.264 + Opus",
        OutputFormat::Mkv => "H.264 + Opus",
        OutputFormat::Mov => "QuickTime",
        OutputFormat::Gif => "256 colors",
        OutputFormat::Webp => "Small, lossy",
        OutputFormat::Avif => "AV1, smallest",
        OutputFormat::Jxl => "High quality",
    }
}

pub(super) fn short_label(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Jxl => "JXL",
        f => f.label(),
    }
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

struct Toast {
    message: String,
    error: bool,
    generation: u64,
}

/// The audio output, opened off the UI thread on first play: opening a
/// device can take long enough to freeze the window.
enum Audio {
    Closed,
    Opening,
    Open(AudioOut),
    /// Opening the output device (or the file's audio) failed once; playback
    /// stays silent rather than retrying on every play.
    Failed,
}

/// Rendered bounds of the bottom clusters and the format menu, recorded by
/// tracked [`ScrollHandle`]s like the annotator's chrome probes.
#[derive(Default)]
pub struct Probes {
    pub transport: ScrollHandle,
    pub outputs: ScrollHandle,
    pub menu: ScrollHandle,
}

/// Frames the decoder keeps ahead of the playhead.
const QUEUE: usize = 6;

fn gpu_image(image: crate::raster::Image) -> Arc<RenderImage> {
    super::render_image(image.width, image.height, image.data)
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
    audio: Audio,
    filmstrip: Filmstrip,
    drag: Option<Drag>,
    /// The timeline track as last painted, for hit testing.
    track: Bounds<Pixels>,
    export: Option<Export>,
    toast: Option<Toast>,
    toast_generation: u64,
    pub probes: Probes,
    help_open: bool,
    help_scroll: ScrollHandle,
    /// The source is kept out of clip-cache pruning while it is open.
    _source: Pin,
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
        super::close_natively_through_task(window, cx);
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let video = VideoPlayer::spawn(
            path.clone(),
            Queue {
                depth: QUEUE,
                looping: false,
            },
            gpu_image,
            tx.clone(),
        );
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
            _source: clips::pin(&path),
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
            audio: Audio::Closed,
            filmstrip,
            drag: None,
            track: Bounds::default(),
            export: None,
            toast: None,
            toast_generation: 0,
            probes: Probes::default(),
            help_open: false,
            help_scroll: ScrollHandle::new(),
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
        matches!(self.audio, Audio::Open(_))
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

    pub fn help_open(&self) -> bool {
        self.help_open
    }

    /// Presentation time of the frame on screen.
    pub fn shown_pts(&self) -> Option<Duration> {
        (!self.awaiting).then_some(())?;
        self.shown.as_ref().map(|f| f.pts)
    }

    /// Progress of the running export (0.0–1.0).
    pub fn export_progress(&self) -> Option<f32> {
        self.export.as_ref().map(Export::progress)
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
    /// `save`, `frame`, `help`, `cancel`, or `format:<ext>` while the menu
    /// is open.
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
            "help" => (&self.probes.outputs, 5),
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

    /// Where to look for the frame shown with the playhead at `at`.
    ///
    /// Frame times come from the stream's time base and the playhead from
    /// the frame grid; the two can differ by rounding. Picking the frame
    /// showing a quarter frame later lands on the intended frame either way.
    /// The out point is exclusive (an export ends just before it), so there
    /// the last frame kept shows instead of the first one cut.
    fn pick(&self, at: Duration) -> Duration {
        let at = if at >= self.trim.end {
            self.trim.end.saturating_sub(self.step).max(self.trim.start)
        } else {
            at
        };
        at + self.step / 4
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
            if let Audio::Open(audio) = &self.audio {
                audio.seek(at);
            }
            return;
        }
        let pick = self.pick(at);
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
            self.play(window, cx);
        }
        cx.notify();
    }

    fn play(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.playhead + self.step / 2 >= self.trim.end {
            self.seek(self.trim.start, window);
        }
        // The clock starts now; sound joins when the device is open.
        self.clock = Some(Clock::start(self.playhead));
        self.start_audio(cx);
    }

    fn pause(&mut self) {
        self.clock = None;
        if let Audio::Open(audio) = &self.audio {
            audio.set_playing(false);
        }
    }

    fn start_audio(&mut self, cx: &mut Context<Self>) {
        if self.muted || !self.info.has_audio || self.clock.is_none() {
            return;
        }
        match &self.audio {
            Audio::Open(audio) => {
                // Samples queued before a pause were already handed to the
                // device; restarting at the playhead keeps sound and picture
                // together.
                audio.seek(self.playhead);
                audio.set_playing(true);
            }
            Audio::Opening | Audio::Failed => {}
            Audio::Closed => {
                // Opened on first play, not on open: viewing a clip never
                // wakes the audio device.
                self.audio = Audio::Opening;
                let (path, at) = (self.path.clone(), self.playhead);
                let open = cx.background_spawn(async move { AudioOut::open(&path, at) });
                cx.spawn(async move |this, cx| {
                    let opened = open.await;
                    let _ = this.update(cx, |e, cx| e.audio_opened(opened, cx));
                })
                .detach();
            }
        }
    }

    fn audio_opened(&mut self, opened: anyhow::Result<Option<AudioOut>>, cx: &mut Context<Self>) {
        if !matches!(self.audio, Audio::Opening) {
            // Muted while it opened: muted means no stream at all.
            return;
        }
        self.audio = match opened {
            Ok(Some(audio)) => Audio::Open(audio),
            Ok(None) => Audio::Failed,
            Err(e) => {
                log::warn!("audio playback unavailable: {e:#}");
                Audio::Failed
            }
        };
        if let Audio::Open(audio) = &self.audio {
            audio.seek(self.playhead);
            audio.set_playing(self.clock.is_some());
        }
        cx.notify();
    }

    fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        if !self.info.has_audio {
            return;
        }
        self.muted = !self.muted;
        if self.muted {
            // Muted means no stream at all, not a silent one.
            if !matches!(self.audio, Audio::Failed) {
                self.audio = Audio::Closed;
            }
        } else {
            self.start_audio(cx);
        }
        cx.notify();
    }

    /// Advances playback to the clock before painting.
    fn advance(&mut self, window: &mut Window) {
        let size = self.decode_size(window);
        let Some(clock) = &mut self.clock else {
            if size != self.video.size() {
                // Resized: decode again at the new size where we are.
                self.video.seek(self.pick(self.playhead), size);
                self.awaiting = true;
            }
            return;
        };
        if let Audio::Open(audio) = &self.audio
            && let Some(heard) = audio.position()
        {
            clock.sync(heard);
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
                self.play(window, cx);
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
    /// The export carries on if the window closes (see [`export_job`]).
    fn export(&mut self, output: Output, cx: &mut Context<Self>) {
        if self.export.is_some() {
            self.show_toast("An export is already running".into(), false, cx);
            return;
        }
        let Some(services) = cx.try_global::<EditorServices>().cloned() else {
            self.show_toast("Saving is unavailable".into(), true, cx);
            return;
        };
        if output == Output::Copy && services.clipboard.is_none() {
            self.show_toast("Clipboard is unavailable".into(), true, cx);
            return;
        }
        let reencode = self.needs_encode();
        if !reencode {
            let record = &services.config.record;
            match output {
                Output::Copy => {
                    if let Some(c) = &services.clipboard {
                        c.set_files(std::slice::from_ref(&self.path));
                    }
                    self.show_toast(format!("Copied {}", self.format.label()), false, cx);
                    return;
                }
                // A recording saved unchanged is already where a save puts
                // it; a second, identical file helps nobody.
                Output::Save if same_dir(&self.path, &record.save_dir()) => {
                    self.show_toast(format!("Already saved as {}", tilde(&self.path)), false, cx);
                    self.menu_open = false;
                    return;
                }
                Output::Save => {}
            }
        }
        let record = &services.config.record;
        let format = self.format;
        let save_path = record.new_save_path(SystemTime::now(), format);
        let clips = services.clips.clone();
        // Under the source's name, so a chat app shows a sensible file name.
        let clip_name = format!(
            "{}.{}",
            self.path
                .file_stem()
                .map_or("clip".into(), |s| s.to_string_lossy()),
            format.extension()
        );
        let options = ExportOptions {
            format,
            trim: self.trimmed().then_some(self.trim),
            mute: self.muted || !self.info.has_audio,
            // The rate chosen for animations: a variable-rate recording's
            // average rate understates how fast its frames came.
            fps: (!format.is_video()).then_some(record.animation_fps),
            fragmented: record.mp4_fragmented,
            max_width: (format == OutputFormat::Gif).then_some(GIF_MAX_WIDTH),
            encoder: EncoderPreference::parse(&record.encoder).unwrap_or_default(),
        };
        let src = self.path.clone();
        let work = move |progress: Progress<'_>, cancel: &AtomicBool| {
            // The window (and its pin) may close before this finishes.
            let _source = clips::pin(&src);
            let (_clip, dst) = match output {
                Output::Save => (None, OutputFile::reserve(&save_path)?),
                Output::Copy => {
                    let (clip, path) = Clip::create(&clips, &clip_name)?;
                    (Some(clip), OutputFile::replace(&path)?)
                }
            };
            if reencode {
                crate::media::export::export(&src, dst, &options, None, progress, cancel)
            } else {
                copy_file(&src, dst, progress, cancel)
            }
        };
        let finished = |e: &mut VideoEditor, done: &Finished, cx: &mut Context<VideoEditor>| {
            e.export = None;
            let (message, error) = done.message();
            e.show_toast(message, error, cx);
        };
        match export_job::start(output, format, &services, work, finished, cx) {
            Ok(export) => self.export = Some(export),
            Err(e) => self.show_toast(format!("Export failed: {e:#}"), true, cx),
        }
        self.menu_open = false;
        cx.notify();
    }

    fn cancel_export(&mut self, cx: &mut Context<Self>) {
        if let Some(export) = &self.export {
            // The export thread notices, removes its partial file and
            // reports back; the toast says so then.
            export.cancel();
            cx.notify();
        }
    }

    /// Opens the frame on screen, at full resolution, in the annotator
    /// ([`AnnotateFrame`]). It goes through the clip cache: the screenshots
    /// folder only gets it when the annotator saves.
    fn annotate_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(services) = cx.try_global::<EditorServices>().cloned() else {
            self.show_toast("Annotating is unavailable".into(), true, cx);
            return;
        };
        let at = self.shown.as_ref().map_or(self.playhead, |f| f.pts);
        let name = format!(
            "{}-{}.png",
            self.path
                .file_stem()
                .map_or("frame".into(), |s| s.to_string_lossy()),
            clock_label(at).replace([':', '.'], "-")
        );
        let (src, clips) = (self.path.clone(), services.clips.clone());
        let task = cx.background_spawn(async move {
            // The preview is scaled to the window; decode the frame again at
            // the source size.
            let mut decoder = crate::media::decode::Decoder::open(&src)?;
            decoder.seek(at)?;
            let frame = decoder.next_frame()?.context("No frame at the playhead")?;
            let (_clip, path) = Clip::create(&clips, &name)?;
            frame
                .image
                .save_png(&path, crate::raster::Compression::Fast)?;
            anyhow::Ok(path)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |e, window, cx| match result {
                Ok(path) => {
                    e.show_toast("Opening the frame in the annotator".into(), false, cx);
                    window.dispatch_action(Box::new(AnnotateFrame { path }), cx);
                }
                Err(err) => e.show_toast(format!("Reading the frame failed: {err:#}"), true, cx),
            });
        })
        .detach();
    }

    fn toggle_help(&mut self, cx: &mut Context<Self>) {
        self.help_open = !self.help_open;
        self.menu_open = false;
        cx.notify();
    }

    // ── input ────────────────────────────────────────────────────────────

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let m = ks.modifiers;
        let help_key =
            matches!(ks.key.as_str(), "?" | "f1") || (ks.key == "/" && m.shift && !m.control);
        if self.help_open {
            if help_key || ks.key == "escape" {
                self.toggle_help(cx);
            }
            cx.stop_propagation();
            return;
        }
        if help_key {
            self.toggle_help(cx);
            cx.stop_propagation();
            return;
        }
        if m.control || m.platform {
            match ks.key.as_str() {
                "c" => self.export(Output::Copy, cx),
                "s" => self.export(Output::Save, cx),
                // A running export carries on and reports when done.
                "w" | "q" => super::close_window(window, cx),
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
                super::close_window(window, cx);
                return;
            }
            // Holding Space would otherwise flip play/pause at the key
            // repeat rate.
            "space" if ev.is_held => {}
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
            let progress = export.progress();
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
            .child(
                button("help", "Keyboard shortcuts (? / F1)", self.help_open, true)
                    .text_sm()
                    .on_click(cx.listener(|e, _, _, cx| e.toggle_help(cx)))
                    .child("?"),
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

/// Whether `file` sits directly in `dir`, whichever way either is spelled
/// (symlinks, `..`).
fn same_dir(file: &Path, dir: &Path) -> bool {
    let canonical = |p: &Path| std::fs::canonicalize(p).ok();
    file.parent()
        .and_then(canonical)
        .is_some_and(|parent| Some(parent) == canonical(dir))
}

/// Saves an untouched source as it is: no generation loss, no encoder. A
/// chunked copy rather than `fs::copy`, so progress moves and Cancel works
/// on large recordings; dropping `output` uncommitted removes the partial.
fn copy_file(
    src: &Path,
    output: OutputFile,
    progress: Progress<'_>,
    cancel: &AtomicBool,
) -> anyhow::Result<PathBuf> {
    use std::io::{Read as _, Write as _};

    let mut from = File::open(src).with_context(|| format!("Reading {}", src.display()))?;
    let total = from.metadata().map_or(0, |m| m.len()).max(1);
    let write = || format!("Writing {}", output.path().display());
    let mut to = File::create(output.part()).with_context(write)?;
    let mut buf = vec![0; 1 << 20];
    let mut copied = 0u64;
    loop {
        anyhow::ensure!(!cancel.load(Ordering::Acquire), "Export cancelled");
        let n = from
            .read(&mut buf)
            .with_context(|| format!("Reading {}", src.display()))?;
        if n == 0 {
            break;
        }
        to.write_all(&buf[..n]).with_context(write)?;
        copied += n as u64;
        progress(copied as f32 / total as f32);
    }
    drop(to);
    output.commit()
}

const HELP_PLAYBACK: &[&str] = &[
    "Space: play / pause",
    "← / →: previous / next frame",
    "Shift+← / →: back / forward a second",
    "Home / End: go to the in / out point",
    "Click or drag the timeline: seek",
    "M: mute / unmute",
];

const HELP_TRIM: &[&str] = &[
    "Drag the handles: trim",
    "I / O: set the in / out point at the playhead",
    "F: annotate the frame on screen",
    "Ctrl+C: copy file · Ctrl+S: save to recordings folder",
    "Esc: close menu / cancel export / close editor",
    "Ctrl+W / Ctrl+Q: close (a running export finishes)",
    "? / F1: toggle help",
];

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
                    .tooltip(|_, cx| {
                        cx.new(|_| Tip("Click or drag to seek · drag the handles to trim (I / O)"))
                            .into()
                    })
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
            .when(self.help_open, |d| {
                d.child(
                    // Clicking anywhere closes it, like the format menu.
                    div()
                        .id("help-backdrop")
                        .absolute()
                        .inset_0()
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|e, _, _, cx| e.toggle_help(cx)),
                        ),
                )
                .child(toolbar::help_panel(
                    viewport,
                    &self.help_scroll,
                    [
                        toolbar::help_section("Playback", HELP_PLAYBACK),
                        toolbar::help_section("Trim and output", HELP_TRIM),
                    ],
                ))
            })
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

    #[test]
    fn untouched_copy_reports_progress_and_cancel_leaves_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("clip.mp4");
        let bytes: Vec<u8> = (0..3u32 << 20).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &bytes).unwrap();
        let dst = dir.path().join("out").join("clip.mp4");

        let seen = std::sync::Mutex::new(Vec::new());
        let report = |f: f32| seen.lock().unwrap().push(f);
        let reserved = OutputFile::reserve(&dst).unwrap();
        let saved = copy_file(&src, reserved, &report, &AtomicBool::new(false)).unwrap();
        assert_eq!(std::fs::read(&saved).unwrap(), bytes);
        let seen = seen.into_inner().unwrap();
        assert!(seen.len() >= 3 && seen.is_sorted(), "{seen:?}");
        assert_eq!(seen.last(), Some(&1.0));

        // Cancelled a third of the way: neither the partial file nor the
        // reserved name stays.
        let cancel = AtomicBool::new(false);
        let stop = |f: f32| {
            if f > 0.3 {
                cancel.store(true, Ordering::Release);
            }
        };
        let reserved = OutputFile::reserve(&dst).unwrap();
        let error = copy_file(&src, reserved, &stop, &cancel).unwrap_err();
        assert_eq!(error.to_string(), "Export cancelled");
        let left: Vec<_> = std::fs::read_dir(dst.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, ["clip.mp4"]);
    }
}
