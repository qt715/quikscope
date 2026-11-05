//! The annotation editor window.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui::{
    App, AppContext as _, Bounds, Context, CursorStyle, DispatchPhase, Entity, FocusHandle,
    Focusable, Global, HitboxBehavior, InteractiveElement as _, IntoElement, KeyDownEvent,
    KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    Pixels, Point, Render, ScrollEvent, Size, StatefulInteractiveElement as _, Styled as _, Window,
    canvas, div, fill, outline, point, px,
};

use super::motion::{Transition, View};
use super::{PatchCache, Shot, ViewTransform, hsla, paint_prims, paint_tiles, theme, toolbar};
use crate::clipboard::Clipboard;
use crate::config::Config;
use crate::document::{Document, PALETTE, Rgba, Shape, ShapeId, ShapeKind, Style};
use crate::geom::{Pt, RectF};
use crate::raster::Compression;

/// Services the editor needs from the daemon.
#[derive(Clone)]
pub struct EditorServices {
    pub config: Arc<Config>,
    pub clipboard: Option<Clipboard>,
}

impl Global for EditorServices {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Select,
    Hand,
    Rect,
    Ellipse,
    Arrow,
    Line,
    Pen,
    Text,
    Highlighter,
    Step,
    Pixelate,
    Crop,
}

impl Tool {
    /// Toolbar order; digit shortcuts follow it (1–9, 0).
    pub const ALL: [Tool; 12] = [
        Tool::Select,
        Tool::Hand,
        Tool::Rect,
        Tool::Ellipse,
        Tool::Arrow,
        Tool::Line,
        Tool::Pen,
        Tool::Text,
        Tool::Highlighter,
        Tool::Step,
        Tool::Pixelate,
        Tool::Crop,
    ];

    pub fn from_key(key: &str) -> Option<Tool> {
        Some(match key {
            "v" | "1" => Tool::Select,
            "h" => Tool::Hand,
            "r" | "2" => Tool::Rect,
            "o" | "e" | "3" => Tool::Ellipse,
            "a" | "4" => Tool::Arrow,
            "l" | "5" => Tool::Line,
            "p" | "d" | "6" => Tool::Pen,
            "t" | "7" => Tool::Text,
            "m" | "8" => Tool::Highlighter,
            "n" | "9" => Tool::Step,
            "x" | "b" | "0" => Tool::Pixelate,
            "c" => Tool::Crop,
            _ => return None,
        })
    }

    /// The digit shortcut for this tool, if it has one.
    pub fn digit(self) -> Option<char> {
        "1234567890"
            .chars()
            .find(|d| Tool::from_key(d.encode_utf8(&mut [0; 4])) == Some(self))
    }

    pub fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select (V)",
            Tool::Hand => "Pan (H)",
            Tool::Rect => "Rectangle (R)",
            Tool::Ellipse => "Ellipse (O)",
            Tool::Arrow => "Arrow (A)",
            Tool::Line => "Line (L)",
            Tool::Pen => "Pen (P)",
            Tool::Text => "Text (T)",
            Tool::Highlighter => "Highlighter (M)",
            Tool::Step => "Step number (N)",
            Tool::Pixelate => "Pixelate (X)",
            Tool::Crop => "Crop (C)",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Tool::Select => "mouse-pointer-2",
            Tool::Hand => "hand",
            Tool::Rect => "square",
            Tool::Ellipse => "circle",
            Tool::Arrow => "move-up-right",
            Tool::Line => "minus",
            Tool::Pen => "pencil",
            Tool::Text => "type",
            Tool::Highlighter => "highlighter",
            Tool::Step => "list-ordered",
            Tool::Pixelate => "grid-3x3",
            Tool::Crop => "crop",
        }
    }

    fn cursor(self) -> CursorStyle {
        match self {
            Tool::Select => CursorStyle::Arrow,
            Tool::Hand => CursorStyle::OpenHand,
            Tool::Text => CursorStyle::IBeam,
            _ => CursorStyle::Crosshair,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Drag {
    None,
    Pan {
        mouse: Point<Pixels>,
        pan: Point<Pixels>,
    },
    Draw {
        start: Pt,
    },
    Move {
        id: ShapeId,
        last: Pt,
        moved: bool,
    },
    Crop {
        start: Pt,
    },
}

#[derive(Clone, Debug)]
pub struct TextEdit {
    /// The shape being edited, when editing existing text.
    pub id: Option<ShapeId>,
    pub origin: Pt,
    pub text: String,
    /// Caret position in chars.
    pub cursor: usize,
    pub style: Style,
}

struct Toast {
    message: String,
    error: bool,
    generation: u64,
}

pub struct Editor {
    pub shot: Arc<Shot>,
    pub doc: Entity<Document>,
    focus: FocusHandle,
    pub tool: Tool,
    pub style: Style,
    /// Where the view is heading; [`Editor::current_view`] is what is shown.
    view: View,
    transition: Option<Transition>,
    /// Refit on resize until the user zooms or pans by hand.
    auto_fit: bool,
    /// Canvas size the current fit was computed for.
    fitted_for: Size<Pixels>,
    canvas: Bounds<Pixels>,
    drag: Drag,
    draft: Option<Shape>,
    crop_draft: Option<RectF>,
    pub selected: Option<ShapeId>,
    pub text: Option<TextEdit>,
    space_held: bool,
    patches: PatchCache,
    toast: Option<Toast>,
    toast_generation: u64,
    /// Where the last Ctrl+S wrote, so repeated saves update one file.
    saved: Option<(PathBuf, u64)>,
    /// Set until the first frame is painted, for latency logging.
    opened: Option<Instant>,
}

const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 32.0;
const TOOLBAR_SPACE: f32 = 64.0;
const MARGIN: f32 = 24.0;
/// Wheel/touchpad zoom steps arrive in bursts; each retargets a short
/// transition so continuous zooming stays fluid.
const WHEEL_ZOOM: Duration = Duration::from_millis(160);
/// Discrete jumps (fit, 100%, Ctrl+=/-) travel further and get more time.
const STEP_ZOOM: Duration = Duration::from_millis(260);

impl Editor {
    pub fn new(
        shot: Arc<Shot>,
        doc: Entity<Document>,
        window: &mut Window,
        cx: &mut Context<Editor>,
    ) -> Editor {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        cx.observe(&doc, |_, _, cx| cx.notify()).detach();
        let style = cx
            .try_global::<EditorServices>()
            .map(|s| Style {
                color: crate::config::parse_hex_color(&s.config.editor.color)
                    .map(Rgba::rgb)
                    .unwrap_or(PALETTE[0]),
                size: s.config.editor.stroke.min(2),
                fill: false,
            })
            .unwrap_or_default();
        Editor {
            shot,
            doc,
            focus,
            tool: Tool::Arrow,
            style,
            view: View {
                zoom: 1.0,
                pan: point(px(0.), px(0.)),
            },
            transition: None,
            auto_fit: true,
            fitted_for: Size::default(),
            canvas: Bounds::default(),
            drag: Drag::None,
            draft: None,
            crop_draft: None,
            selected: None,
            text: None,
            space_held: false,
            patches: PatchCache::default(),
            toast: None,
            toast_generation: 0,
            saved: None,
            opened: Some(Instant::now()),
        }
    }

    /// The view on screen, mid-transition or settled.
    pub fn current_view(&self) -> View {
        match self
            .transition
            .and_then(|t| Some((t, t.progress(Instant::now())?)))
        {
            Some((t, e)) => View::lerp(t.from, self.view, e),
            None => self.view,
        }
    }

    /// Window-space transform for what is on screen now.
    pub fn transform(&self) -> ViewTransform {
        let v = self.current_view();
        ViewTransform {
            origin: self.canvas.origin + v.pan,
            zoom: v.zoom,
        }
    }

    pub fn zoom(&self) -> f32 {
        self.current_view().zoom
    }

    /// Jumps to `view` with no transition (drag-panning, resizing).
    fn set_view(&mut self, view: View) {
        self.view = view;
        self.transition = None;
    }

    /// Eases from what is on screen now to `view`.
    fn animate_to(&mut self, view: View, duration: Duration) {
        let from = self.current_view();
        self.view = view;
        self.transition = Some(Transition {
            from,
            start: Instant::now(),
            duration,
        });
    }

    fn fitted_view(&self, window: &Window) -> Option<View> {
        let (w, h) = (self.shot.image.width as f32, self.shot.image.height as f32);
        let cw = f32::from(self.canvas.size.width) - 2.0 * MARGIN;
        let ch = f32::from(self.canvas.size.height) - TOOLBAR_SPACE - MARGIN;
        if cw <= 0.0 || ch <= 0.0 || w <= 0.0 || h <= 0.0 {
            return None;
        }
        let native = 1.0 / window.scale_factor();
        let zoom = (cw / w).min(ch / h).min(native).clamp(MIN_ZOOM, MAX_ZOOM);
        Some(View {
            zoom,
            pan: point(
                px((f32::from(self.canvas.size.width) - w * zoom) / 2.0),
                px(TOOLBAR_SPACE + (ch - h * zoom) / 2.0),
            ),
        })
    }

    /// Fits the image into the canvas, never magnifying past one image
    /// pixel per device pixel, and keeps it fitted across window resizes.
    pub fn fit(&mut self, window: &Window) {
        if let Some(v) = self.fitted_view(window) {
            self.animate_to(v, STEP_ZOOM);
            self.auto_fit = true;
            self.fitted_for = self.canvas.size;
        }
    }

    pub fn actual_size(&mut self, window: &Window) {
        let center = self.canvas.center();
        self.zoom_around(1.0 / window.scale_factor(), center, STEP_ZOOM);
    }

    /// Zooms to `zoom`, keeping the image point under `anchor` (window
    /// coordinates) where it is on screen now.
    pub fn zoom_around(&mut self, zoom: f32, anchor: Point<Pixels>, duration: Duration) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let img = self.transform().to_image(anchor);
        let rel = anchor - self.canvas.origin;
        let target = View {
            zoom,
            pan: point(rel.x - px(img.x * zoom), rel.y - px(img.y * zoom)),
        };
        self.animate_to(target, duration);
        self.auto_fit = false;
    }

    pub fn set_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        self.commit_text(cx);
        self.tool = tool;
        if tool != Tool::Select {
            self.selected = None;
        }
        self.crop_draft = None;
        cx.notify();
    }

    /// Applies a style change to the tool and to the selected or edited shape.
    pub fn set_style(&mut self, style: Style, cx: &mut Context<Self>) {
        self.style = style;
        if let Some(t) = &mut self.text {
            t.style = style;
        } else if let Some(id) = self.selected {
            self.doc.update(cx, |d, _| {
                if let Some(s) = d.shape(id).cloned()
                    && s.style != style
                {
                    d.replace(s.with_style(style));
                }
            });
        }
        cx.notify();
    }

    pub fn set_color(&mut self, color: Rgba, cx: &mut Context<Self>) {
        self.set_style(
            Style {
                color,
                ..self.style
            },
            cx,
        );
    }

    pub fn set_size(&mut self, size: u8, cx: &mut Context<Self>) {
        self.set_style(
            Style {
                size: size.min(2),
                ..self.style
            },
            cx,
        );
    }

    pub fn toggle_fill(&mut self, cx: &mut Context<Self>) {
        self.set_style(
            Style {
                fill: !self.style.fill,
                ..self.style
            },
            cx,
        );
    }

    pub fn undo(&mut self, cx: &mut Context<Self>) {
        self.text = None;
        self.selected = None;
        self.doc.update(cx, |d, _| d.undo());
        cx.notify();
    }

    pub fn redo(&mut self, cx: &mut Context<Self>) {
        self.text = None;
        self.selected = None;
        self.doc.update(cx, |d, _| d.redo());
        cx.notify();
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.selected.take() {
            self.doc.update(cx, |d, _| d.remove(id));
            cx.notify();
        }
    }

    fn nudge(&mut self, dx: f32, dy: f32, cx: &mut Context<Self>) {
        if let Some(id) = self.selected {
            self.doc.update(cx, |d, _| {
                if let Some(s) = d.shape(id).cloned() {
                    d.replace(s.translated(dx, dy));
                }
            });
        }
    }

    // ── text editing ────────────────────────────────────────────────────

    fn begin_text(&mut self, origin: Pt, existing: Option<ShapeId>, cx: &mut Context<Self>) {
        self.commit_text(cx);
        let (text, style, origin) = existing
            .and_then(|id| {
                let doc = self.doc.read(cx);
                let s = doc.shape(id)?;
                match &s.kind {
                    ShapeKind::Text { origin, text } => Some((text.clone(), s.style, *origin)),
                    _ => None,
                }
            })
            .unwrap_or((String::new(), self.style, origin));
        if existing.is_some() {
            self.style = style;
        }
        self.text = Some(TextEdit {
            id: existing,
            cursor: text.chars().count(),
            origin,
            text,
            style,
        });
        self.selected = None;
        cx.notify();
    }

    /// Writes the text being edited into the document (one undo step).
    pub fn commit_text(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.text.take() else {
            return;
        };
        self.doc.update(cx, |d, _| {
            let kind = ShapeKind::Text {
                origin: edit.origin,
                text: edit.text.clone(),
            };
            match edit.id {
                Some(id) => {
                    let shape = Shape::new(id, kind, edit.style, d.scale);
                    if shape.is_degenerate() {
                        d.remove(id);
                    } else if d.shape(id).map(|s| s.as_ref()) != Some(&shape) {
                        d.replace(shape);
                    }
                }
                None => {
                    let shape = d.make_shape(kind, edit.style);
                    if !shape.is_degenerate() {
                        d.add(shape);
                    }
                }
            }
        });
        cx.notify();
    }

    /// Returns true when the key was consumed by the text being edited.
    fn text_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(t) = &mut self.text else {
            return false;
        };
        let ks = &ev.keystroke;
        if ks.modifiers.control || ks.modifiers.platform || ks.modifiers.alt {
            return false;
        }
        let byte_at =
            |s: &str, chars: usize| s.char_indices().nth(chars).map_or(s.len(), |(i, _)| i);
        match ks.key.as_str() {
            "escape" => {
                self.commit_text(cx);
                return true;
            }
            "enter" => {
                let at = byte_at(&t.text, t.cursor);
                t.text.insert(at, '\n');
                t.cursor += 1;
            }
            "backspace" => {
                if t.cursor > 0 {
                    let at = byte_at(&t.text, t.cursor - 1);
                    t.text.remove(at);
                    t.cursor -= 1;
                }
            }
            "delete" => {
                if t.cursor < t.text.chars().count() {
                    let at = byte_at(&t.text, t.cursor);
                    t.text.remove(at);
                }
            }
            "left" => t.cursor = t.cursor.saturating_sub(1),
            "right" => t.cursor = (t.cursor + 1).min(t.text.chars().count()),
            "home" => {
                let before: Vec<char> = t.text.chars().take(t.cursor).collect();
                let line_start = before.iter().rposition(|c| *c == '\n').map_or(0, |i| i + 1);
                t.cursor = line_start;
            }
            "end" => {
                let rest = t.text.chars().skip(t.cursor).position(|c| c == '\n');
                t.cursor = match rest {
                    Some(n) => t.cursor + n,
                    None => t.text.chars().count(),
                };
            }
            _ => match &ks.key_char {
                Some(s) if !s.is_empty() && !s.chars().any(char::is_control) => {
                    let at = byte_at(&t.text, t.cursor);
                    t.text.insert_str(at, s);
                    t.cursor += s.chars().count();
                }
                _ => return false,
            },
        }
        cx.notify();
        true
    }

    // ── input ───────────────────────────────────────────────────────────

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.text_key(ev, cx) {
            cx.stop_propagation();
            return;
        }
        let ks = &ev.keystroke;
        let m = ks.modifiers;
        let key = ks.key.as_str();
        if m.control || m.platform {
            match (key, m.shift) {
                ("s", shift) => self.save(shift, window, cx),
                ("c", _) => self.copy(cx),
                ("z", false) => self.undo(cx),
                ("z", true) | ("y", _) => self.redo(cx),
                ("w", _) | ("q", _) => window.remove_window(),
                ("0", _) => {
                    self.fit(window);
                    cx.notify();
                }
                ("1", _) => {
                    self.actual_size(window);
                    cx.notify();
                }
                ("=", _) | ("+", _) => {
                    self.zoom_around(self.view.zoom * 1.25, self.canvas.center(), STEP_ZOOM);
                    cx.notify();
                }
                ("-", _) => {
                    self.zoom_around(self.view.zoom / 1.25, self.canvas.center(), STEP_ZOOM);
                    cx.notify();
                }
                _ => return,
            }
            cx.stop_propagation();
            return;
        }
        let step = if m.shift { 10.0 } else { 1.0 };
        match key {
            "escape" => {
                if self.drag != Drag::None || self.draft.is_some() || self.crop_draft.is_some() {
                    self.drag = Drag::None;
                    self.draft = None;
                    self.crop_draft = None;
                } else if self.selected.is_some() {
                    self.selected = None;
                } else {
                    window.remove_window();
                    return;
                }
            }
            "delete" | "backspace" => self.delete_selected(cx),
            "space" => self.space_held = true,
            "left" => self.nudge(-step, 0.0, cx),
            "right" => self.nudge(step, 0.0, cx),
            "up" => self.nudge(0.0, -step, cx),
            "down" => self.nudge(0.0, step, cx),
            "f" => self.toggle_fill(cx),
            "[" => self.set_size(self.style.size.saturating_sub(1), cx),
            "]" => self.set_size(self.style.size + 1, cx),
            "enter" => {
                // Enter on a selected text shape starts editing it.
                let id = self.selected;
                let is_text = id.is_some_and(|id| {
                    matches!(
                        self.doc.read(cx).shape(id).map(|s| &s.kind),
                        Some(ShapeKind::Text { .. })
                    )
                });
                if is_text {
                    self.begin_text(Pt::default(), id, cx);
                }
            }
            k if !m.shift => match Tool::from_key(k) {
                Some(tool) => self.set_tool(tool, cx),
                None => return,
            },
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn key_up(&mut self, ev: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key == "space" {
            self.space_held = false;
            cx.notify();
        }
    }

    fn start_pan(&mut self, mouse: Point<Pixels>) {
        // Grabbing the canvas stops any transition where it is.
        let now = self.current_view();
        self.set_view(now);
        self.auto_fit = false;
        self.drag = Drag::Pan {
            mouse,
            pan: now.pan,
        };
    }

    fn mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        if ev.button == MouseButton::Middle || (self.space_held && ev.button == MouseButton::Left) {
            self.start_pan(ev.position);
            cx.notify();
            return;
        }
        if ev.button != MouseButton::Left {
            return;
        }
        let p = self.transform().to_image(ev.position);
        let tolerance = 6.0 / self.zoom();

        if self.text.is_some() {
            // A click away from the text being edited finishes it.
            self.commit_text(cx);
            if self.tool != Tool::Text {
                return;
            }
        }

        match self.tool {
            Tool::Select => {
                let hit = self.doc.read(cx).hit_test(p, tolerance);
                match hit {
                    Some(id) => {
                        let is_text = matches!(
                            self.doc.read(cx).shape(id).map(|s| &s.kind),
                            Some(ShapeKind::Text { .. })
                        );
                        if ev.click_count >= 2 && is_text {
                            self.begin_text(p, Some(id), cx);
                            return;
                        }
                        self.selected = Some(id);
                        if let Some(s) = self.doc.read(cx).shape(id) {
                            self.style = s.style;
                        }
                        self.drag = Drag::Move {
                            id,
                            last: p,
                            moved: false,
                        };
                    }
                    None => {
                        self.selected = None;
                        self.start_pan(ev.position);
                    }
                }
            }
            Tool::Hand => self.start_pan(ev.position),
            Tool::Text => {
                let hit = self.doc.read(cx).hit_test(p, tolerance);
                let text_hit = hit.filter(|id| {
                    matches!(
                        self.doc.read(cx).shape(*id).map(|s| &s.kind),
                        Some(ShapeKind::Text { .. })
                    )
                });
                // Clicks place the middle of the first line at the pointer.
                let size = crate::document::TEXT_SIZES[self.style.size.min(2) as usize]
                    * self.doc.read(cx).scale;
                let origin = p.offset(0.0, -size * crate::text::LINE_HEIGHT / 2.0);
                self.begin_text(origin, text_hit, cx);
            }
            Tool::Step => {
                let style = self.style;
                self.doc.update(cx, |d, _| {
                    let number = d.next_step_number();
                    let s = d.make_shape(ShapeKind::Step { center: p, number }, style);
                    d.add(s);
                });
            }
            Tool::Crop => {
                self.drag = Drag::Crop { start: p };
                self.crop_draft = Some(RectF::from_points(p, p));
            }
            tool => {
                let kind = match tool {
                    Tool::Pen => ShapeKind::Pen { points: vec![p] },
                    Tool::Highlighter => ShapeKind::Highlighter { points: vec![p] },
                    Tool::Line => ShapeKind::Line { a: p, b: p },
                    Tool::Arrow => ShapeKind::Arrow { a: p, b: p },
                    Tool::Rect => ShapeKind::Rect { a: p, b: p },
                    Tool::Ellipse => ShapeKind::Ellipse { a: p, b: p },
                    Tool::Pixelate => ShapeKind::Pixelate { a: p, b: p },
                    _ => unreachable!("handled above"),
                };
                let style = self.style;
                self.draft = Some(self.doc.update(cx, |d, _| d.make_shape(kind, style)));
                self.drag = Drag::Draw { start: p };
            }
        }
        cx.notify();
    }

    fn mouse_move(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        let p = self.transform().to_image(ev.position);
        match self.drag.clone() {
            Drag::None => return,
            Drag::Pan { mouse, pan } => {
                let zoom = self.view.zoom;
                self.set_view(View {
                    zoom,
                    pan: pan + (ev.position - mouse),
                });
            }
            Drag::Draw { start } => {
                let Some(draft) = &self.draft else { return };
                let shift = ev.modifiers.shift;
                let kind = match &draft.kind {
                    ShapeKind::Pen { points } | ShapeKind::Highlighter { points } => {
                        // Skip sub-pixel moves: they add vertices, not detail.
                        if points.last().is_some_and(|l| l.dist(p) * self.zoom() < 1.5) {
                            return;
                        }
                        let mut points = points.clone();
                        points.push(p);
                        match draft.kind {
                            ShapeKind::Pen { .. } => ShapeKind::Pen { points },
                            _ => ShapeKind::Highlighter { points },
                        }
                    }
                    ShapeKind::Line { .. } => ShapeKind::Line {
                        a: start,
                        b: if shift { snap_angle(start, p) } else { p },
                    },
                    ShapeKind::Arrow { .. } => ShapeKind::Arrow {
                        a: start,
                        b: if shift { snap_angle(start, p) } else { p },
                    },
                    ShapeKind::Rect { .. } => ShapeKind::Rect {
                        a: start,
                        b: if shift { square(start, p) } else { p },
                    },
                    ShapeKind::Ellipse { .. } => ShapeKind::Ellipse {
                        a: start,
                        b: if shift { square(start, p) } else { p },
                    },
                    ShapeKind::Pixelate { .. } => ShapeKind::Pixelate { a: start, b: p },
                    _ => return,
                };
                self.draft = Some(Shape::new(draft.id, kind, draft.style, draft.scale));
            }
            Drag::Move { id, last, moved } => {
                let (dx, dy) = (p.x - last.x, p.y - last.y);
                if dx == 0.0 && dy == 0.0 {
                    return;
                }
                self.doc.update(cx, |d, _| {
                    if !moved {
                        d.checkpoint();
                    }
                    if let Some(s) = d.shape(id).cloned() {
                        d.replace_live(s.translated(dx, dy));
                    }
                });
                self.drag = Drag::Move {
                    id,
                    last: p,
                    moved: true,
                };
            }
            Drag::Crop { start } => {
                let (w, h) = (self.shot.image.width as f32, self.shot.image.height as f32);
                let clamp = |q: Pt| Pt::new(q.x.clamp(0.0, w), q.y.clamp(0.0, h));
                self.crop_draft = Some(RectF::from_points(clamp(start), clamp(p)));
            }
        }
        cx.notify();
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, cx: &mut Context<Self>) {
        match std::mem::replace(&mut self.drag, Drag::None) {
            Drag::Draw { .. } => {
                if let Some(draft) = self.draft.take()
                    && !draft.is_degenerate()
                {
                    self.doc.update(cx, |d, _| d.add(draft));
                }
            }
            Drag::Crop { .. } => {
                let crop = self.crop_draft.take();
                // A click without a drag clears the crop.
                let zoom = self.zoom();
                let crop = crop.filter(|c| c.w * zoom >= 4.0 && c.h * zoom >= 4.0);
                self.doc.update(cx, |d, _| d.set_crop(crop));
            }
            Drag::None => return,
            Drag::Pan { .. } | Drag::Move { .. } => {}
        }
        cx.notify();
    }

    fn scroll(&mut self, ev: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = ev.delta.pixel_delta(px(40.));
        if ev.modifiers.control {
            let factor = (1.0 + f32::from(delta.y) / 400.0).clamp(0.5, 2.0);
            // Relative to the target, so a burst of wheel steps accumulates
            // instead of restarting from wherever the transition is.
            self.zoom_around(self.view.zoom * factor, ev.position, WHEEL_ZOOM);
        } else {
            let delta = if ev.modifiers.shift {
                point(delta.y, px(0.))
            } else {
                delta
            };
            let now = self.current_view();
            self.set_view(View {
                zoom: now.zoom,
                pan: now.pan + delta,
            });
            self.auto_fit = false;
        }
        cx.notify();
    }

    // ── output ──────────────────────────────────────────────────────────

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

    /// Ctrl+S: renders and writes a PNG into the screenshots directory.
    /// Repeated saves overwrite the same file; Ctrl+Shift+S always makes a
    /// new one.
    pub fn save(&mut self, new_file: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_text(cx);
        let Some(services) = cx.try_global::<EditorServices>().cloned() else {
            self.show_toast("Saving is unavailable".into(), true, cx);
            return;
        };
        let (job, revision) = {
            let d = self.doc.read(cx);
            (d.snapshot(), d.revision())
        };
        let path = match &self.saved {
            Some((p, _)) if !new_file => p.clone(),
            _ => services.config.new_save_path(SystemTime::now()),
        };
        let close = services.config.editor.close_on_save;
        let task = cx.background_spawn(async move {
            let image = crate::export::render(&job);
            image.save_png(&path, Compression::Balanced).map(|_| path)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |e, window, cx| match result {
                Ok(path) => {
                    log::info!("saved {}", path.display());
                    e.show_toast(format!("Saved {}", tilde(&path)), false, cx);
                    e.saved = Some((path, revision));
                    if close {
                        window.remove_window();
                    }
                }
                Err(err) => e.show_toast(format!("Save failed: {err:#}"), true, cx),
            });
        })
        .detach();
    }

    /// Ctrl+C: puts the annotated image on the clipboard.
    pub fn copy(&mut self, cx: &mut Context<Self>) {
        self.commit_text(cx);
        let Some(clipboard) = cx
            .try_global::<EditorServices>()
            .and_then(|s| s.clipboard.clone())
        else {
            self.show_toast("Clipboard is unavailable".into(), true, cx);
            return;
        };
        let job = self.doc.read(cx).snapshot();
        let task = cx.background_spawn(async move { Arc::new(crate::export::render(&job)) });
        cx.spawn(async move |this, cx| {
            let image = task.await;
            clipboard.set_image(image);
            let _ = this.update(cx, |e, cx| {
                e.show_toast("Copied to clipboard".into(), false, cx)
            });
        })
        .detach();
    }

    pub fn saved_path(&self) -> Option<&PathBuf> {
        self.saved.as_ref().map(|(p, _)| p)
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        let rev = self.doc.read(cx).revision();
        match &self.saved {
            Some((_, saved)) => *saved != rev,
            None => !self.doc.read(cx).is_pristine(),
        }
    }

    // ── painting ────────────────────────────────────────────────────────

    fn paint(
        &mut self,
        bounds: Bounds<Pixels>,
        hitbox: &gpui::Hitbox,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.canvas = bounds;
        if let Some(opened) = self.opened.take() {
            log::debug!(
                "editor first frame {:.1}ms after creation",
                opened.elapsed().as_secs_f64() * 1e3
            );
        }
        // Fit on the first frame and whenever the window is resized, until
        // the user takes over the view. No transition: the window itself is
        // already changing size.
        if self.auto_fit
            && bounds.size != self.fitted_for
            && let Some(v) = self.fitted_view(window)
        {
            self.set_view(v);
            self.fitted_for = bounds.size;
        }
        if let Some(t) = self.transition {
            if t.progress(Instant::now()).is_some() {
                window.request_animation_frame();
            } else {
                self.transition = None;
            }
        }
        let xf = self.transform();
        let zoom = xf.zoom;
        let doc = self.doc.read(cx);
        let image_rect = RectF::new(
            0.0,
            0.0,
            self.shot.image.width as f32,
            self.shot.image.height as f32,
        );

        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            // Drop shadow-ish frame so the image edge reads against the canvas.
            let frame = xf.rect_to_screen(image_rect.inflate(1.0 / zoom));
            window.paint_quad(fill(frame, gpui::rgba(0xffffff18)));
            paint_tiles(window, self.shot.tiles(), xf, bounds);

            let editing = self.text.as_ref().and_then(|t| t.id);
            for shape in doc.shapes() {
                if Some(shape.id) == editing {
                    continue;
                }
                paint_prims(
                    window,
                    shape.prims(&doc.base),
                    xf,
                    bounds,
                    &mut self.patches,
                );
            }
            if let Some(draft) = &self.draft {
                paint_prims(
                    window,
                    draft.prims(&doc.base),
                    xf,
                    bounds,
                    &mut self.patches,
                );
            }

            if let Some(t) = &self.text {
                let shape = Shape::new(
                    0,
                    ShapeKind::Text {
                        origin: t.origin,
                        text: t.text.clone(),
                    },
                    t.style,
                    doc.scale,
                );
                paint_prims(
                    window,
                    shape.prims(&doc.base),
                    xf,
                    bounds,
                    &mut self.patches,
                );
                let layout = shape.text_layout().unwrap();
                let caret = layout.caret(&t.text, t.cursor);
                let top = xf.to_screen(t.origin.offset(caret.x, caret.y));
                let caret_rect = Bounds {
                    origin: top,
                    size: gpui::size(px(2.), px(layout.line_height * zoom)),
                };
                window.paint_quad(fill(caret_rect, hsla(t.style.color)));
                let frame = RectF::new(
                    t.origin.x,
                    t.origin.y,
                    layout.width.max(layout.size * 0.5),
                    layout.height(),
                )
                .inflate(4.0 / zoom);
                let mut o = outline(
                    xf.rect_to_screen(frame),
                    theme::selection(),
                    gpui::BorderStyle::Dashed,
                );
                o.border_widths = px(1.).into();
                window.paint_quad(o);
            }

            if let Some(id) = self.selected
                && let Some(s) = doc.shape(id)
            {
                let r = xf.rect_to_screen(s.bounds());
                let r = Bounds {
                    origin: r.origin - point(px(4.), px(4.)),
                    size: r.size + gpui::size(px(8.), px(8.)),
                };
                let mut o = outline(r, theme::selection(), gpui::BorderStyle::Dashed);
                o.border_widths = px(1.5).into();
                o.corner_radii = px(3.).into();
                window.paint_quad(o);
            }

            let crop = self.crop_draft.or(doc.crop());
            if let Some(c) = crop {
                paint_crop(window, xf.rect_to_screen(image_rect), xf.rect_to_screen(c));
            }
        });
        self.patches.end_frame(window);

        let cursor = match (&self.drag, self.space_held) {
            (Drag::Pan { .. }, _) => CursorStyle::ClosedHand,
            (_, true) => CursorStyle::OpenHand,
            _ => self.tool.cursor(),
        };
        window.set_cursor_style(cursor, hitbox);

        let editor = cx.entity();
        window.on_mouse_event({
            let editor = editor.clone();
            move |ev: &MouseMoveEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble {
                    editor.update(cx, |e, cx| e.mouse_move(ev, cx));
                }
            }
        });
        window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                editor.update(cx, |e, cx| e.mouse_up(ev, cx));
            }
        });
    }
}

fn paint_crop(window: &mut Window, image: Bounds<Pixels>, crop: Bounds<Pixels>) {
    let dim = gpui::rgba(0x0000_00a0);
    let (l, t, r, b) = (image.left(), image.top(), image.right(), image.bottom());
    let (cl, ct, cr, cb) = (crop.left(), crop.top(), crop.right(), crop.bottom());
    for rect in [
        Bounds::from_corners(point(l, t), point(r, ct)),
        Bounds::from_corners(point(l, cb), point(r, b)),
        Bounds::from_corners(point(l, ct), point(cl, cb)),
        Bounds::from_corners(point(cr, ct), point(r, cb)),
    ] {
        if rect.size.width > px(0.) && rect.size.height > px(0.) {
            window.paint_quad(fill(rect, dim));
        }
    }
    let mut o = outline(crop, gpui::white(), gpui::BorderStyle::Solid);
    o.border_widths = px(1.).into();
    window.paint_quad(o);
}

/// Snaps `p` so the segment from `start` is a multiple of 15°.
fn snap_angle(start: Pt, p: Pt) -> Pt {
    let (dx, dy) = (p.x - start.x, p.y - start.y);
    let len = dx.hypot(dy);
    let step = 15f32.to_radians();
    let angle = (dy.atan2(dx) / step).round() * step;
    Pt::new(start.x + len * angle.cos(), start.y + len * angle.sin())
}

/// Constrains the rectangle from `start` to `p` to a square.
fn square(start: Pt, p: Pt) -> Pt {
    let (dx, dy) = (p.x - start.x, p.y - start.y);
    let side = dx.abs().max(dy.abs());
    Pt::new(start.x + side * dx.signum(), start.y + side * dy.signum())
}

/// `~/…` for paths under `$HOME`, for toasts.
pub fn tilde(path: &std::path::Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        None => path.display().to_string(),
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = cx.entity();
        let zoom_label = format!("{:.0}%", self.view.zoom * 100.0);
        let toast = self.toast.as_ref().map(|t| {
            div()
                .absolute()
                .bottom_4()
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
                        .text_sm()
                        .text_color(theme::text())
                        .child(t.message.clone()),
                )
        });

        div()
            .id("editor")
            .key_context("Editor")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .bg(theme::canvas_bg())
            .font_family(super::UI_FONT)
            .text_color(theme::text())
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(Self::key_up))
            .child(
                div()
                    .absolute()
                    .size_full()
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
                    .on_mouse_down(MouseButton::Middle, cx.listener(Self::mouse_down))
                    .on_scroll(cx.listener(Self::scroll))
                    .child(
                        canvas(
                            |bounds, window, _| {
                                window.insert_hitbox(bounds, HitboxBehavior::Normal)
                            },
                            move |bounds, hitbox, window, cx| {
                                editor.update(cx, |e, cx| e.paint(bounds, &hitbox, window, cx));
                            },
                        )
                        .size_full(),
                    ),
            )
            .child(toolbar::toolbar(self, cx))
            .child(
                div().absolute().bottom_4().left_4().child(
                    div()
                        .id("zoom")
                        .occlude()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(theme::panel())
                        .text_xs()
                        .text_color(theme::muted())
                        .hover(|s| s.bg(theme::hover()))
                        .on_click(cx.listener(|e, _, window, cx| {
                            e.fit(window);
                            cx.notify();
                        }))
                        .child(zoom_label),
                ),
            )
            .children(toast)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digit_and_letter_shortcuts_cover_every_tool() {
        for tool in Tool::ALL {
            let found = ["v", "h", "r", "o", "a", "l", "p", "t", "m", "n", "x", "c"]
                .iter()
                .any(|k| Tool::from_key(k) == Some(tool));
            assert!(found, "{tool:?} has no letter shortcut");
        }
        assert_eq!(Tool::from_key("q"), None);
    }

    #[test]
    fn angle_snapping_uses_fifteen_degree_steps() {
        let s = Pt::new(0.0, 0.0);
        let p = snap_angle(s, Pt::new(100.0, 3.0));
        assert!((p.y).abs() < 1e-3 && (p.x - 100.045).abs() < 0.01);
        let p = snap_angle(s, Pt::new(50.0, 52.0));
        assert!((p.x - p.y).abs() < 1e-3, "snaps to 45°");
    }

    #[test]
    fn square_constraint_keeps_drag_direction() {
        assert_eq!(
            square(Pt::new(10.0, 10.0), Pt::new(0.0, 40.0)),
            Pt::new(-20.0, 40.0)
        );
    }

    #[test]
    fn tilde_shortens_home_paths() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(tilde(&home.join("Pictures/a.png")), "~/Pictures/a.png");
        assert_eq!(tilde(std::path::Path::new("/etc/x")), "/etc/x");
    }
}
