//! The annotation editor window.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use gpui::{
    App, AppContext as _, Bounds, Context, CursorStyle, DispatchPhase, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, Global, HitboxBehavior, InteractiveElement as _,
    IntoElement, KeyDownEvent, KeyUpEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, Pixels, Point, Render, ScrollEvent, Size,
    StatefulInteractiveElement as _, Styled as _, UTF16Selection, Window, canvas, div, fill,
    outline, point, px,
};

use super::color_picker::{ColorPicker, PickerEvent};
use super::edit_buffer::EditBuffer;
use super::font_menu::{self, FontMenu, FontMenuEvent, next_size};
use super::motion::{Transition, View};
use super::zoom;
use super::{PatchCache, Shot, ViewTransform, hsla, paint_prims, paint_tiles, theme, toolbar};
use crate::config::Config;
use crate::document::{Document, PALETTE, Rgba, Shape, ShapeId, ShapeKind, Style};
use crate::fonts::FontSpec;
use crate::geom::{IRect, Pt, RectF};
use crate::raster::Compression;
use crate::text::TextLayout;

/// Services the editor needs from the daemon.
#[derive(Clone)]
pub struct EditorServices {
    pub config: Arc<Config>,
    pub clipboard: Option<Arc<dyn crate::platform::Clipboard>>,
}

impl Global for EditorServices {}

/// The tool most recently chosen in any editor; `quickscope open-last` opens
/// with it so you can carry on where you left off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LastTool(pub Tool);

impl Global for LastTool {}

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
    /// Text, caret and IME pre-edit.
    pub buf: EditBuffer,
    pub style: Style,
    pub font: FontSpec,
}

impl TextEdit {
    pub fn kind(&self) -> ShapeKind {
        ShapeKind::Text {
            origin: self.origin,
            text: self.buf.text.clone(),
            font: self.font.clone(),
        }
    }
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
    /// Refit on resize, and when drawings grow the canvas, until the user
    /// zooms or pans by hand.
    auto_fit: bool,
    /// Window canvas size and document canvas the current fit was for.
    fitted_for: (Size<Pixels>, IRect),
    /// Ctrl+scroll accumulated into zoom steps.
    wheel: zoom::Wheel,
    /// A text extraction is running.
    reading_text: bool,
    /// Lines the last extraction read (image pixels) and when, for the
    /// highlight that fades over [`TEXT_FLASH`].
    text_flash: Option<(Vec<RectF>, Instant)>,
    canvas: Bounds<Pixels>,
    drag: Drag,
    draft: Option<Shape>,
    crop_draft: Option<RectF>,
    pub selected: Option<ShapeId>,
    pub text: Option<TextEdit>,
    space_held: bool,
    /// With the select tool: the item a press would pick up (open-hand
    /// cursor and a faint outline).
    hover: Option<ShapeId>,
    patches: PatchCache,
    toast: Option<Toast>,
    toast_generation: u64,
    /// Where the last Ctrl+S wrote, so repeated saves update one file.
    saved: Option<(PathBuf, u64)>,
    /// Set until the first frame is painted, for latency logging.
    opened: Option<Instant>,
    /// Font for new text; follows the last font chosen.
    pub font: FontSpec,
    font_menu: Entity<FontMenu>,
    color_picker: Entity<ColorPicker>,
    /// Window x of the toolbar button the picker hangs from, when open.
    picker_at: Option<Pixels>,
    /// Shape whose color a picker drag is changing: the drag's first change
    /// records undo, the rest replace it in place.
    color_session: Option<ShapeId>,
}

const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 32.0;
const TOOLBAR_SPACE: f32 = 64.0;
const MARGIN: f32 = 24.0;
/// Wheel/touchpad zoom steps arrive in bursts; each retargets a short
/// transition so continuous zooming stays fluid.
const WHEEL_ZOOM: Duration = Duration::from_millis(160);
/// How long the lines found by text extraction stay highlighted.
const TEXT_FLASH: Duration = Duration::from_millis(1600);
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
        let (style, font) = cx
            .try_global::<EditorServices>()
            .map(|s| {
                let e = &s.config.editor;
                let style = Style {
                    color: crate::color::parse_hex(&e.color).unwrap_or(PALETTE[0]),
                    size: e.stroke.min(2),
                    fill: false,
                };
                (style, e.font())
            })
            .unwrap_or_default();
        let font_menu = cx.new(|cx| FontMenu::new(font.clone(), window, cx));
        let color_picker = cx.new(|cx| ColorPicker::new(style.color, window, cx));
        cx.subscribe_in(&font_menu, window, Self::on_font_menu)
            .detach();
        cx.subscribe_in(&color_picker, window, Self::on_picker)
            .detach();
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
            fitted_for: Default::default(),
            wheel: zoom::Wheel::default(),
            reading_text: false,
            text_flash: None,
            canvas: Bounds::default(),
            drag: Drag::None,
            draft: None,
            crop_draft: None,
            selected: None,
            text: None,
            space_held: false,
            hover: None,
            patches: PatchCache::default(),
            toast: None,
            toast_generation: 0,
            saved: None,
            opened: Some(Instant::now()),
            font,
            font_menu,
            color_picker,
            picker_at: None,
            color_session: None,
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

    /// The view that fits `area` (image pixels) into the window, at the
    /// largest zoom level that fits and never past actual pixels.
    fn fitted_view(&self, area: IRect, window: &Window) -> Option<View> {
        let (w, h) = (area.w as f32, area.h as f32);
        let cw = f32::from(self.canvas.size.width) - 2.0 * MARGIN;
        let ch = f32::from(self.canvas.size.height) - TOOLBAR_SPACE - MARGIN;
        if cw <= 0.0 || ch <= 0.0 || w <= 0.0 || h <= 0.0 {
            return None;
        }
        let sf = window.scale_factor();
        let device = ((cw / w).min(ch / h) * sf).min(1.0);
        let zoom = (zoom::snap_down(device) / sf).clamp(MIN_ZOOM, MAX_ZOOM);
        Some(View {
            zoom,
            pan: point(
                px((f32::from(self.canvas.size.width) - w * zoom) / 2.0 - area.x as f32 * zoom),
                px(TOOLBAR_SPACE + (ch - h * zoom) / 2.0 - area.y as f32 * zoom),
            ),
        })
    }

    /// Fits the screenshot and everything drawn around it into the window,
    /// never magnifying past actual pixels, and keeps it fitted across
    /// window resizes and drawings that grow the canvas.
    pub fn fit(&mut self, window: &Window, cx: &App) {
        let area = self.doc.read(cx).canvas();
        if let Some(v) = self.fitted_view(area, window) {
            self.animate_to(v, STEP_ZOOM);
            self.auto_fit = true;
            self.fitted_for = (self.canvas.size, area);
        }
    }

    pub fn actual_size(&mut self, window: &Window) {
        let center = self.canvas.center();
        self.zoom_around(1.0 / window.scale_factor(), center, STEP_ZOOM);
    }

    /// Moves `steps` zoom levels (negative: out) from where the view is
    /// heading, so quick repeats chain, around `anchor`.
    fn zoom_steps(
        &mut self,
        steps: i32,
        anchor: Point<Pixels>,
        duration: Duration,
        window: &Window,
    ) {
        let sf = window.scale_factor();
        let target = zoom::step_by(self.view.zoom * sf, steps) / sf;
        if (target - self.view.zoom).abs() > f32::EPSILON {
            self.zoom_around(target, anchor, duration);
        }
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
        cx.set_global(LastTool(tool));
        if tool != Tool::Select {
            self.selected = None;
        }
        self.crop_draft = None;
        self.hover = None;
        cx.notify();
    }

    /// The item a select-tool press at `p` picks up: the topmost item under
    /// the pointer, otherwise the selected item anywhere inside its box, so
    /// once selected, hollow shapes and thin strokes drag from inside while
    /// items lying within them stay directly clickable.
    fn grab_target(&self, p: Pt, cx: &App) -> Option<ShapeId> {
        let doc = self.doc.read(cx);
        let tolerance = 6.0 / self.zoom();
        doc.hit_test(p, tolerance).or_else(|| {
            self.selected.filter(|id| {
                doc.shape(*id)
                    .is_some_and(|s| s.bounds().inflate(tolerance).contains(p))
            })
        })
    }

    /// Applies a style change to the tool and to the selected or edited shape.
    pub fn set_style(&mut self, style: Style, cx: &mut Context<Self>) {
        self.apply_style(style, false, cx);
    }

    /// Like [`Editor::set_style`]. While `live` (a picker drag), only the
    /// first change records undo; later ones, and the final non-live one,
    /// update that step in place.
    fn apply_style(&mut self, style: Style, live: bool, cx: &mut Context<Self>) {
        self.style = style;
        if let Some(t) = &mut self.text {
            t.style = style;
        } else if let Some(id) = self.selected {
            let in_session = self.color_session == Some(id);
            self.doc.update(cx, |d, _| {
                if let Some(s) = d.shape(id).cloned()
                    && s.style != style
                {
                    if in_session {
                        d.replace_live(s.with_style(style));
                    } else {
                        d.replace(s.with_style(style));
                    }
                }
            });
            self.color_session = live.then_some(id);
        }
        cx.notify();
    }

    fn on_picker(
        &mut self,
        _: &Entity<ColorPicker>,
        ev: &PickerEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let PickerEvent::Changed { color, live } = *ev;
        self.apply_style(
            Style {
                color,
                ..self.style
            },
            live,
            cx,
        );
    }

    pub fn picker_open(&self) -> bool {
        self.picker_at.is_some()
    }

    /// Opens the color picker under the toolbar button at window x `at`, or
    /// closes it.
    pub fn toggle_picker(&mut self, at: Pixels, cx: &mut Context<Self>) {
        self.picker_at = match self.picker_at {
            Some(_) => None,
            None => Some(at),
        };
        cx.notify();
    }

    /// The font and image-space bounds of the text being edited or the
    /// selected text shape — what the font menu applies to.
    pub fn font_target(&self, cx: &App) -> Option<(FontSpec, RectF)> {
        if let Some(t) = &self.text {
            let doc = self.doc.read(cx);
            let shape = Shape::new(0, t.kind(), t.style, doc.scale);
            return Some((t.font.clone(), shape.bounds()));
        }
        let s = self.doc.read(cx).shape(self.selected?)?;
        match &s.kind {
            ShapeKind::Text { font, .. } => Some((font.clone(), s.bounds())),
            _ => None,
        }
    }

    /// Sets the font for new text and for the edited or selected text.
    pub fn set_font(&mut self, font: FontSpec, cx: &mut Context<Self>) {
        self.font = font.clone();
        if let Some(t) = &mut self.text {
            t.font = font;
        } else if let Some(id) = self.selected {
            self.doc.update(cx, |d, _| {
                let Some(s) = d.shape(id).cloned() else {
                    return;
                };
                if let ShapeKind::Text {
                    origin,
                    text,
                    font: old,
                } = &s.kind
                    && *old != font
                {
                    let kind = ShapeKind::Text {
                        origin: *origin,
                        text: text.clone(),
                        font,
                    };
                    d.replace(Shape::new(id, kind, s.style, s.scale));
                }
            });
        }
        cx.notify();
    }

    /// Changes the target's font with `f` (keyboard shortcuts). Returns
    /// false when there is no text to apply it to.
    fn update_font(
        &mut self,
        f: impl FnOnce(&FontSpec) -> FontSpec,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((font, _)) = self.font_target(cx) else {
            return false;
        };
        self.set_font(f(&font), cx);
        true
    }

    fn on_font_menu(
        &mut self,
        _: &Entity<FontMenu>,
        ev: &FontMenuEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            FontMenuEvent::Changed(font) => self.set_font(font.clone(), cx),
            // Back to the canvas so typing continues into the text.
            FontMenuEvent::Done => window.focus(&self.focus, cx),
        }
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
        let (text, style, origin, font) = existing
            .and_then(|id| {
                let doc = self.doc.read(cx);
                let s = doc.shape(id)?;
                match &s.kind {
                    ShapeKind::Text { origin, text, font } => {
                        Some((text.clone(), s.style, *origin, font.clone()))
                    }
                    _ => None,
                }
            })
            .unwrap_or((String::new(), self.style, origin, self.font.clone()));
        if existing.is_some() {
            self.style = style;
            self.font = font.clone();
        }
        self.text = Some(TextEdit {
            id: existing,
            origin,
            buf: EditBuffer::new(text),
            style,
            font,
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
            let kind = edit.kind();
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
    /// Text itself arrives here only from plain key presses; input methods
    /// deliver it through the [`EntityInputHandler`] impl below.
    fn text_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(t) = &mut self.text else {
            return false;
        };
        let ks = &ev.keystroke;
        if ks.modifiers.control || ks.modifiers.platform || ks.modifiers.alt {
            return false;
        }
        let b = &mut t.buf;
        // Arrow keys move visually: on a right-to-left line, "left" goes
        // towards the end of the text.
        let rtl = || {
            let line = b.text.split('\n').nth(b.line()).unwrap_or("");
            unicode_bidi::BidiInfo::new(line, None)
                .paragraphs
                .first()
                .is_some_and(|p| p.level.is_rtl())
        };
        match ks.key.as_str() {
            "escape" => {
                self.commit_text(cx);
                return true;
            }
            "enter" => b.insert("\n"),
            "backspace" => b.backspace(),
            "delete" => b.delete(),
            "left" => {
                let rtl = rtl();
                b.step(rtl)
            }
            "right" => {
                let rtl = rtl();
                b.step(!rtl)
            }
            "home" => b.home(),
            "end" => b.end(),
            _ => match &ks.key_char {
                Some(s) if !s.is_empty() && !s.chars().any(char::is_control) => b.insert(s),
                _ => return false,
            },
        }
        window.notify_text_input_changed(&self.focus);
        cx.notify();
        true
    }

    // ── input ───────────────────────────────────────────────────────────

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key == "escape" && self.picker_at.is_some() {
            self.picker_at = None;
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.text_key(ev, window, cx) {
            cx.stop_propagation();
            return;
        }
        let ks = &ev.keystroke;
        let m = ks.modifiers;
        let key = ks.key.as_str();
        if m.control || m.platform {
            match (key, m.shift) {
                // Text formatting, for the edited or selected text.
                ("b", false) if self.update_font(FontSpec::toggle_bold, cx) => {}
                ("i", false)
                    if self.update_font(
                        |f| FontSpec {
                            italic: !f.italic,
                            ..f.clone()
                        },
                        cx,
                    ) => {}
                ("." | ">", true)
                    if self.update_font(|f| f.with_size(next_size(f.size, true)), cx) => {}
                ("," | "<", true)
                    if self.update_font(|f| f.with_size(next_size(f.size, false)), cx) => {}
                ("s", shift) => self.save(shift, window, cx),
                ("c", false) => self.copy(cx),
                ("c", true) => self.copy_text(cx),
                ("z", false) => self.undo(cx),
                ("z", true) | ("y", _) => self.redo(cx),
                ("w", _) | ("q", _) => window.remove_window(),
                ("0", _) => {
                    self.fit(window, cx);
                    cx.notify();
                }
                ("1", _) => {
                    self.actual_size(window);
                    cx.notify();
                }
                ("=", _) | ("+", _) => {
                    self.zoom_steps(1, self.canvas.center(), STEP_ZOOM, window);
                    cx.notify();
                }
                ("-", _) => {
                    self.zoom_steps(-1, self.canvas.center(), STEP_ZOOM, window);
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
        // A click on the canvas dismisses the color picker.
        self.picker_at = None;
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
                let is_text = hit.is_some_and(|id| {
                    matches!(
                        self.doc.read(cx).shape(id).map(|s| &s.kind),
                        Some(ShapeKind::Text { .. })
                    )
                });
                if ev.click_count >= 2 && is_text {
                    self.begin_text(p, hit, cx);
                    return;
                }
                match self.grab_target(p, cx) {
                    Some(id) => {
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
                let size = self.font.size * self.doc.read(cx).scale;
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
            Drag::None => {
                let hover = (self.tool == Tool::Select && !self.space_held)
                    .then(|| self.grab_target(p, cx))
                    .flatten();
                if hover != self.hover {
                    self.hover = hover;
                    cx.notify();
                }
                return;
            }
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
                let c = self.doc.read(cx).canvas();
                let (x0, y0) = (c.x as f32, c.y as f32);
                let (x1, y1) = (c.right() as f32, c.bottom() as f32);
                let clamp = |q: Pt| Pt::new(q.x.clamp(x0, x1), q.y.clamp(y0, y1));
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

    fn scroll(&mut self, ev: &ScrollEvent, window: &mut Window, cx: &mut Context<Self>) {
        let delta = ev.delta.pixel_delta(px(40.));
        if ev.modifiers.control {
            // One zoom level per wheel notch; touchpad pixels accumulate.
            // Steps chain from the target, so a quick burst of notches keeps
            // easing instead of restarting from wherever the transition is.
            let steps = self.wheel.feed(f32::from(delta.y));
            if steps != 0 {
                self.zoom_steps(steps, ev.position, WHEEL_ZOOM, window);
            }
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

    /// Ctrl+Shift+C: reads the screenshot's text (inside the crop, if any;
    /// annotations are not read) and copies it, briefly highlighting the
    /// lines that were read.
    pub fn copy_text(&mut self, cx: &mut Context<Self>) {
        self.commit_text(cx);
        if self.reading_text {
            return;
        }
        let services = cx.try_global::<EditorServices>().cloned();
        let languages = services
            .as_ref()
            .map(|s| s.config.ocr.languages.clone())
            .unwrap_or_default();
        let clipboard = services.and_then(|s| s.clipboard);
        let image = self.shot.image.clone();
        let region = self
            .doc
            .read(cx)
            .crop()
            .map(|c| c.round_out())
            .unwrap_or(image.bounds());
        let upscale = crate::ocr::upscale_for(self.shot.scale);
        self.reading_text = true;
        self.show_toast("Reading text…".into(), false, cx);
        let task =
            cx.background_spawn(
                async move { crate::ocr::extract(&image, region, upscale, &languages) },
            );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |e, cx| {
                e.reading_text = false;
                match result {
                    Ok(found) if found.text.is_empty() => {
                        e.show_toast("No text found".into(), false, cx)
                    }
                    Ok(found) => {
                        e.text_flash = Some((
                            found
                                .lines
                                .iter()
                                .map(|l| {
                                    let r = l.rect;
                                    RectF::new(r.x as f32, r.y as f32, r.w as f32, r.h as f32)
                                })
                                .collect(),
                            Instant::now(),
                        ));
                        match &clipboard {
                            Some(c) => {
                                c.set_text(&found.text);
                                let n = found.lines.len();
                                let lines = if n == 1 {
                                    "1 line".into()
                                } else {
                                    format!("{n} lines")
                                };
                                e.show_toast(format!("Copied {lines} of text"), false, cx);
                            }
                            None => e.show_toast("Clipboard is unavailable".into(), true, cx),
                        }
                    }
                    Err(err) => e.show_toast(format!("{err:#}"), true, cx),
                }
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
        // Fit on the first frame, whenever the window is resized and whenever
        // drawings grow the canvas, until the user takes over the view. A
        // resize jumps (the window itself is already changing size); a
        // grown canvas eases to the new fit.
        let area = self.doc.read(cx).canvas();
        if self.auto_fit
            && (bounds.size, area) != self.fitted_for
            && let Some(v) = self.fitted_view(area, window)
        {
            if bounds.size == self.fitted_for.0 {
                self.animate_to(v, STEP_ZOOM);
            } else {
                self.set_view(v);
            }
            self.fitted_for = (bounds.size, area);
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
        // What gets exported: the image plus drawings past its edges, whose
        // margin is transparent in the saved PNG.
        let canvas_rect = RectF::new(area.x as f32, area.y as f32, area.w as f32, area.h as f32);

        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            // A light backing marks the export area against the window; past
            // the screenshot it is all that shows.
            let frame = xf.rect_to_screen(canvas_rect.inflate(1.0 / zoom));
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
                let shape = Shape::new(0, t.kind(), t.style, doc.scale);
                paint_prims(
                    window,
                    shape.prims(&doc.base),
                    xf,
                    bounds,
                    &mut self.patches,
                );
                let layout = shape.text_layout().unwrap();
                let caret = caret_rect(t, &layout, t.buf.cursor_chars(), xf);
                window.paint_quad(fill(caret, hsla(t.style.color)));
                // The IME's pre-edit is underlined, one segment per line.
                if let Some(m) = t.buf.marked_chars() {
                    let mut segments: Vec<(f32, f32, f32)> = Vec::new();
                    for i in m {
                        let (a, b) = (
                            layout.caret(&t.buf.text, i),
                            layout.caret(&t.buf.text, i + 1),
                        );
                        if a.y != b.y {
                            continue;
                        }
                        let (x0, x1) = (a.x.min(b.x), a.x.max(b.x));
                        match segments.last_mut() {
                            Some(s) if s.2 == a.y && (s.1 - x0).abs() < 0.5 => s.1 = x1,
                            Some(s) if s.2 == a.y && (s.0 - x1).abs() < 0.5 => s.0 = x0,
                            _ => segments.push((x0, x1, a.y)),
                        }
                    }
                    let below = layout.ascent + layout.size * 0.12;
                    for (x0, x1, y) in segments {
                        let r = RectF::new(t.origin.x + x0, t.origin.y + y + below, x1 - x0, 0.0);
                        let mut b = xf.rect_to_screen(r);
                        b.size.height = px(1.5);
                        window.paint_quad(fill(b, hsla(t.style.color)));
                    }
                }
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
            // What a press would pick up, before it is selected.
            if self.tool == Tool::Select
                && self.drag == Drag::None
                && let Some(id) = self.hover.filter(|h| Some(*h) != self.selected)
                && let Some(s) = doc.shape(id)
            {
                let r = xf.rect_to_screen(s.bounds());
                let r = Bounds {
                    origin: r.origin - point(px(4.), px(4.)),
                    size: r.size + gpui::size(px(8.), px(8.)),
                };
                let mut o = outline(
                    r,
                    theme::selection().opacity(0.45),
                    gpui::BorderStyle::Solid,
                );
                o.border_widths = px(1.).into();
                o.corner_radii = px(3.).into();
                window.paint_quad(o);
            }

            // Lines found by text extraction light up and fade out.
            if let Some((rects, at)) = &self.text_flash {
                let t = at.elapsed().as_secs_f32() / TEXT_FLASH.as_secs_f32();
                if t < 1.0 {
                    let alpha = 1.0 - t * t;
                    for r in rects {
                        let b = xf.rect_to_screen(r.inflate(2.0 / zoom));
                        let mut q = outline(
                            b,
                            theme::selection().opacity(alpha),
                            gpui::BorderStyle::Solid,
                        );
                        q.background = theme::selection().opacity(0.18 * alpha).into();
                        q.border_widths = px(1.5).into();
                        q.corner_radii = px(3.).into();
                        window.paint_quad(q);
                    }
                    window.request_animation_frame();
                }
            }

            let crop = self.crop_draft.or(doc.crop());
            if let Some(c) = crop {
                paint_crop(window, xf.rect_to_screen(canvas_rect), xf.rect_to_screen(c));
            }
        });
        self.patches.end_frame(window);
        // While text is being typed the editor is a text input: Wayland
        // enables text-input-v3 for it, so input methods (fcitx5, IBus)
        // compose into it instead of seeing raw keys.
        if self.text.is_some() {
            window.handle_input(
                &self.focus,
                ElementInputHandler::new(bounds, cx.entity()),
                cx,
            );
        }

        let cursor = match (&self.drag, self.space_held) {
            (Drag::Pan { .. } | Drag::Move { .. }, _) => CursorStyle::ClosedHand,
            (_, true) => CursorStyle::OpenHand,
            _ if self.tool == Tool::Select && self.hover.is_some() => CursorStyle::OpenHand,
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
    match std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from) {
        Some(home) => match path.strip_prefix(&home) {
            Ok(rest) => {
                #[cfg(windows)]
                let rest = rest.display().to_string().replace('\\', "/");
                #[cfg(not(windows))]
                let rest = rest.display();
                format!("~/{rest}")
            }
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

/// The caret before char `index` of the text being edited, in window
/// coordinates.
fn caret_rect(
    t: &TextEdit,
    layout: &TextLayout,
    index: usize,
    xf: ViewTransform,
) -> Bounds<Pixels> {
    let caret = layout.caret(&t.buf.text, index);
    Bounds {
        origin: xf.to_screen(t.origin.offset(caret.x, caret.y)),
        size: gpui::size(px(2.), px(layout.line_height * xf.zoom)),
    }
}

/// Input-method protocol for the text being edited. Ranges are UTF-16, as
/// the platform sends them.
impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let b = &self.text.as_ref()?.buf;
        let r = b.range_from_utf16(&range);
        *adjusted = Some(b.range_to_utf16(&r));
        Some(b.text[r].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let b = &self.text.as_ref()?.buf;
        let c = b.to_utf16(b.cursor);
        Some(UTF16Selection {
            range: c..c,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        let b = &self.text.as_ref()?.buf;
        b.marked.as_ref().map(|m| b.range_to_utf16(m))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = &mut self.text {
            t.buf.marked = None;
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(t) = &mut self.text else { return };
        let range = range.map(|r| t.buf.range_from_utf16(&r));
        t.buf.replace(range, text);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(t) = &mut self.text else { return };
        let range = range.map(|r| t.buf.range_from_utf16(&r));
        // The selection is in UTF-16 units of `new_text`.
        let selected = new_selected_range.map(|r| {
            let sub = EditBuffer::new(new_text.to_string());
            sub.range_from_utf16(&r)
        });
        t.buf.set_marked(range, new_text, selected);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let t = self.text.as_ref()?;
        let scale = self.doc.read(cx).scale;
        let layout = Shape::new(0, t.kind(), t.style, scale).text_layout()?;
        let at = t.buf.from_utf16(range.start);
        let index = t.buf.text[..at].chars().count();
        Some(caret_rect(t, &layout, index, self.transform()))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    fn accepts_text_input(&self, _: &mut Window, _: &mut Context<Self>) -> bool {
        self.text.is_some()
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = cx.entity();
        // Device pixels per image pixel: 100% is actual pixels on any scale.
        let zoom_label = format!("{:.0}%", self.view.zoom * window.scale_factor() * 100.0);
        let viewport = window.viewport_size();
        let (vw, vh) = (f32::from(viewport.width), f32::from(viewport.height));

        // Popovers mirror the current values (palette clicks, selection changes).
        let color = self.style.color;
        self.color_picker
            .update(cx, |p, cx| p.set_color(color, window, cx));
        let picker = self.picker_at.map(|at| {
            let w = super::color_picker::PANEL_WIDTH;
            let x = (f32::from(at) - w / 2.0).clamp(8.0, (vw - w - 8.0).max(8.0));
            div()
                .absolute()
                .left(px(x))
                .top(px(TOOLBAR_SPACE - 4.0))
                .child(self.color_picker.clone())
        });

        let font_menu = match self.font_target(cx) {
            Some((font, rect)) => {
                let r = self.transform().rect_to_screen(rect);
                let (mw, mh) = (font_menu::MENU_WIDTH, font_menu::MENU_HEIGHT);
                let x = f32::from(r.left()).clamp(8.0, (vw - mw - 8.0).max(8.0));
                // Above the text when there is room under the toolbar, else below.
                let above = f32::from(r.top()) - mh - 10.0;
                let y = if above >= TOOLBAR_SPACE {
                    above
                } else {
                    f32::from(r.bottom()) + 10.0
                }
                .clamp(TOOLBAR_SPACE, (vh - mh - 8.0).max(TOOLBAR_SPACE));
                let drop_up = y + mh + font_menu::DROPDOWN_HEIGHT > vh
                    && y - font_menu::DROPDOWN_HEIGHT > 0.0;
                self.font_menu.update(cx, |m, cx| {
                    m.set_spec(&font, window, cx);
                    m.set_drop_up(drop_up, cx);
                });
                Some(
                    div()
                        .absolute()
                        .left(px(x))
                        .top(px(y))
                        .child(self.font_menu.clone()),
                )
            }
            None => {
                self.font_menu.update(cx, |m, cx| m.close(cx));
                None
            }
        };
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
                        // Long messages (install hints) wrap inside the
                        // window instead of running off both edges.
                        .max_w(px(560.))
                        .mx_4()
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
                            e.fit(window, cx);
                            cx.notify();
                        }))
                        .child(zoom_label),
                ),
            )
            .children(toast)
            .children(font_menu)
            .children(picker)
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
        let home = PathBuf::from(
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap(),
        );
        assert_eq!(tilde(&home.join("Pictures/a.png")), "~/Pictures/a.png");
        assert_eq!(tilde(std::path::Path::new("/etc/x")), "/etc/x");
    }
}
