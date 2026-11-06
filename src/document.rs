//! The annotation document: shapes over a screenshot, crop, and undo history.
//!
//! Shapes are immutable `Arc`s; edits replace them. Undo snapshots are
//! therefore just vectors of pointers, and a shape's render primitives can be
//! cached inside it for its whole life.

use std::sync::{Arc, OnceLock};

use crate::fonts::FontSpec;
use crate::geom::{Pt, RectF, dist_to_segment};
use crate::raster::Image;
use crate::render::{self, Prim};
use crate::text::{self, TextLayout};

pub type ShapeId = u64;

/// Straight-alpha sRGB color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn rgb(hex: u32) -> Rgba {
        Rgba {
            r: (hex >> 16) as u8,
            g: (hex >> 8) as u8,
            b: hex as u8,
            a: 255,
        }
    }

    pub const fn with_alpha(self, a: u8) -> Rgba {
        Rgba { a, ..self }
    }

    pub fn hex(self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | self.b as u32
    }

    /// Relative luminance, used to pick readable text on a filled badge.
    pub fn is_light(self) -> bool {
        (0.299 * self.r as f32 + 0.587 * self.g as f32 + 0.114 * self.b as f32) > 160.0
    }
}

/// The palette offered in the toolbar (Excalidraw's stroke colors).
pub const PALETTE: [Rgba; 8] = [
    Rgba::rgb(0xe03131),
    Rgba::rgb(0xf08c00),
    Rgba::rgb(0xffd43b),
    Rgba::rgb(0x2f9e44),
    Rgba::rgb(0x1971c2),
    Rgba::rgb(0x9c36b5),
    Rgba::rgb(0x1e1e1e),
    Rgba::rgb(0xffffff),
];

/// Stroke widths for size 0/1/2, in logical pixels (scaled by the image scale).
pub const STROKE_WIDTHS: [f32; 3] = [2.0, 4.0, 8.0];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub color: Rgba,
    /// 0 = thin, 1 = medium, 2 = thick.
    pub size: u8,
    /// Fill rectangles/ellipses instead of outlining them.
    pub fill: bool,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            color: PALETTE[0],
            size: 1,
            fill: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ShapeKind {
    Pen {
        points: Vec<Pt>,
    },
    Highlighter {
        points: Vec<Pt>,
    },
    Line {
        a: Pt,
        b: Pt,
    },
    Arrow {
        a: Pt,
        b: Pt,
    },
    Rect {
        a: Pt,
        b: Pt,
    },
    Ellipse {
        a: Pt,
        b: Pt,
    },
    Text {
        origin: Pt,
        text: String,
        /// Family, weight, italic and size (logical pixels).
        font: FontSpec,
    },
    /// Numbered badge for step-by-step instructions.
    Step {
        center: Pt,
        number: u32,
    },
    /// Mosaic over the screenshot (never over other annotations).
    Pixelate {
        a: Pt,
        b: Pt,
    },
}

#[derive(Debug)]
pub struct Shape {
    pub id: ShapeId,
    pub kind: ShapeKind,
    pub style: Style,
    /// Physical pixels per logical pixel of the screenshot; sizes scale by it
    /// so annotations look the same on HiDPI captures.
    pub scale: f32,
    prims: OnceLock<Arc<[Prim]>>,
}

impl Clone for Shape {
    fn clone(&self) -> Self {
        Shape::new(self.id, self.kind.clone(), self.style, self.scale)
    }
}

impl PartialEq for Shape {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.kind == other.kind
            && self.style == other.style
            && self.scale == other.scale
    }
}

impl Shape {
    pub fn new(id: ShapeId, kind: ShapeKind, style: Style, scale: f32) -> Shape {
        Shape {
            id,
            kind,
            style,
            scale,
            prims: OnceLock::new(),
        }
    }

    pub fn stroke_width(&self) -> f32 {
        let base = STROKE_WIDTHS[self.style.size.min(2) as usize] * self.scale;
        match self.kind {
            ShapeKind::Highlighter { .. } => base * 4.0,
            _ => base,
        }
    }

    /// Text size in image pixels.
    pub fn text_size(&self) -> f32 {
        match &self.kind {
            ShapeKind::Text { font, .. } => font.size * self.scale,
            _ => 0.0,
        }
    }

    pub fn step_radius(&self) -> f32 {
        [12.0, 16.0, 22.0][self.style.size.min(2) as usize] * self.scale
    }

    pub fn text_layout(&self) -> Option<TextLayout> {
        match &self.kind {
            ShapeKind::Text { text, font, .. } => Some(text::layout(
                text,
                &FontSpec {
                    size: font.size * self.scale,
                    ..font.clone()
                },
            )),
            _ => None,
        }
    }

    /// Render primitives in image space, computed once per shape.
    pub fn prims(&self, base: &Image) -> &Arc<[Prim]> {
        self.prims
            .get_or_init(|| render::shape_prims(self, base).into())
    }

    /// Axis-aligned bounds in image space, including stroke width.
    pub fn bounds(&self) -> RectF {
        let pad = self.stroke_width() / 2.0;
        match &self.kind {
            ShapeKind::Pen { points } | ShapeKind::Highlighter { points } => {
                let first = points.first().copied().unwrap_or_default();
                points
                    .iter()
                    .fold(RectF::from_points(first, first), |r, p| {
                        r.union(&RectF::from_points(*p, *p))
                    })
                    .inflate(pad)
            }
            ShapeKind::Line { a, b } | ShapeKind::Arrow { a, b } => {
                let extra = if matches!(self.kind, ShapeKind::Arrow { .. }) {
                    render::arrow_head_len(*a, *b, self.stroke_width())
                } else {
                    0.0
                };
                RectF::from_points(*a, *b).inflate(pad + extra)
            }
            ShapeKind::Rect { a, b } | ShapeKind::Ellipse { a, b } => {
                RectF::from_points(*a, *b).inflate(if self.style.fill { 0.0 } else { pad })
            }
            ShapeKind::Pixelate { a, b } => RectF::from_points(*a, *b),
            ShapeKind::Text { origin, .. } => {
                let l = self.text_layout().unwrap();
                RectF::new(origin.x, origin.y, l.width.max(1.0), l.height())
            }
            ShapeKind::Step { center, .. } => {
                let r = self.step_radius();
                RectF::new(center.x - r, center.y - r, 2.0 * r, 2.0 * r)
            }
        }
    }

    /// Whether `p` is within `tolerance` image pixels of the visible shape.
    pub fn hit(&self, p: Pt, tolerance: f32) -> bool {
        let reach = self.stroke_width() / 2.0 + tolerance;
        let polyline_hit = |pts: &[Pt]| match pts {
            [] => false,
            [only] => only.dist(p) <= reach,
            _ => pts
                .windows(2)
                .any(|w| dist_to_segment(p, w[0], w[1]) <= reach),
        };
        match &self.kind {
            ShapeKind::Pen { points } | ShapeKind::Highlighter { points } => polyline_hit(points),
            ShapeKind::Line { a, b } => dist_to_segment(p, *a, *b) <= reach,
            ShapeKind::Arrow { a, b } => {
                dist_to_segment(p, *a, *b) <= reach
                    || render::arrow_head(*a, *b, self.stroke_width())
                        .is_some_and(|(l, r)| polyline_hit(&[l, *b, r]))
            }
            ShapeKind::Rect { a, b } => {
                let r = RectF::from_points(*a, *b);
                if self.style.fill {
                    return r.inflate(tolerance).contains(p);
                }
                let c = [
                    Pt::new(r.x, r.y),
                    Pt::new(r.right(), r.y),
                    Pt::new(r.right(), r.bottom()),
                    Pt::new(r.x, r.bottom()),
                    Pt::new(r.x, r.y),
                ];
                polyline_hit(&c)
            }
            ShapeKind::Ellipse { a, b } => {
                let r = RectF::from_points(*a, *b);
                let (rx, ry) = (r.w / 2.0, r.h / 2.0);
                if rx < 1.0 || ry < 1.0 {
                    return r.inflate(reach).contains(p);
                }
                let c = r.center();
                let (dx, dy) = (p.x - c.x, p.y - c.y);
                let norm = ((dx / rx).powi(2) + (dy / ry).powi(2)).sqrt();
                if self.style.fill {
                    return norm <= 1.0 + tolerance / rx.min(ry);
                }
                // Distance to the outline along the ray from the centre,
                // accurate enough for picking.
                if norm < 1e-3 {
                    return rx.min(ry) <= reach;
                }
                dx.hypot(dy) * (1.0 - 1.0 / norm).abs() <= reach
            }
            ShapeKind::Text { .. } | ShapeKind::Pixelate { .. } => {
                self.bounds().inflate(tolerance).contains(p)
            }
            ShapeKind::Step { center, .. } => center.dist(p) <= self.step_radius() + tolerance,
        }
    }

    pub fn translated(&self, dx: f32, dy: f32) -> Shape {
        let mv = |p: &Pt| p.offset(dx, dy);
        let kind = match &self.kind {
            ShapeKind::Pen { points } => ShapeKind::Pen {
                points: points.iter().map(mv).collect(),
            },
            ShapeKind::Highlighter { points } => ShapeKind::Highlighter {
                points: points.iter().map(mv).collect(),
            },
            ShapeKind::Line { a, b } => ShapeKind::Line { a: mv(a), b: mv(b) },
            ShapeKind::Arrow { a, b } => ShapeKind::Arrow { a: mv(a), b: mv(b) },
            ShapeKind::Rect { a, b } => ShapeKind::Rect { a: mv(a), b: mv(b) },
            ShapeKind::Ellipse { a, b } => ShapeKind::Ellipse { a: mv(a), b: mv(b) },
            ShapeKind::Pixelate { a, b } => ShapeKind::Pixelate { a: mv(a), b: mv(b) },
            ShapeKind::Text { origin, text, font } => ShapeKind::Text {
                origin: mv(origin),
                text: text.clone(),
                font: font.clone(),
            },
            ShapeKind::Step { center, number } => ShapeKind::Step {
                center: mv(center),
                number: *number,
            },
        };
        Shape::new(self.id, kind, self.style, self.scale)
    }

    pub fn with_style(&self, style: Style) -> Shape {
        Shape::new(self.id, self.kind.clone(), style, self.scale)
    }

    /// Shapes too small to see are dropped instead of committed.
    pub fn is_degenerate(&self) -> bool {
        match &self.kind {
            ShapeKind::Pen { points } | ShapeKind::Highlighter { points } => points.is_empty(),
            ShapeKind::Line { a, b } | ShapeKind::Arrow { a, b } => a.dist(*b) < 2.0,
            ShapeKind::Rect { a, b }
            | ShapeKind::Ellipse { a, b }
            | ShapeKind::Pixelate { a, b } => (a.x - b.x).abs() < 2.0 || (a.y - b.y).abs() < 2.0,
            ShapeKind::Text { text, .. } => text.trim().is_empty(),
            ShapeKind::Step { .. } => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct State {
    shapes: Vec<Arc<Shape>>,
    crop: Option<RectF>,
}

/// History depth; older steps are dropped.
const MAX_UNDO: usize = 200;

pub struct Document {
    pub base: Arc<Image>,
    /// Physical pixels per logical pixel of the screenshot.
    pub scale: f32,
    state: State,
    undo: Vec<State>,
    redo: Vec<State>,
    next_id: ShapeId,
    /// Bumped on every change; compare against a saved revision for "dirty".
    revision: u64,
}

impl Document {
    pub fn new(base: Arc<Image>, scale: f32) -> Document {
        Document {
            base,
            scale: if scale > 0.0 { scale } else { 1.0 },
            state: State {
                shapes: Vec::new(),
                crop: None,
            },
            undo: Vec::new(),
            redo: Vec::new(),
            next_id: 1,
            revision: 0,
        }
    }

    pub fn width(&self) -> u32 {
        self.base.width
    }

    pub fn height(&self) -> u32 {
        self.base.height
    }

    pub fn shapes(&self) -> &[Arc<Shape>] {
        &self.state.shapes
    }

    pub fn shape(&self, id: ShapeId) -> Option<&Arc<Shape>> {
        self.state.shapes.iter().find(|s| s.id == id)
    }

    pub fn crop(&self) -> Option<RectF> {
        self.state.crop
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_pristine(&self) -> bool {
        self.state.shapes.is_empty() && self.state.crop.is_none()
    }

    /// Records the current state for undo. Call once before a user action;
    /// live edits during a drag (via [`Document::replace_live`]) then fold
    /// into that single step.
    pub fn checkpoint(&mut self) {
        self.undo.push(self.state.clone());
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn new_id(&mut self) -> ShapeId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn make_shape(&mut self, kind: ShapeKind, style: Style) -> Shape {
        let id = self.new_id();
        Shape::new(id, kind, style, self.scale)
    }

    /// Adds a finished shape as one undo step.
    pub fn add(&mut self, shape: Shape) -> ShapeId {
        let id = shape.id;
        self.checkpoint();
        self.state.shapes.push(Arc::new(shape));
        self.revision += 1;
        id
    }

    /// Replaces a shape as one undo step.
    pub fn replace(&mut self, shape: Shape) {
        self.checkpoint();
        self.replace_live(shape);
    }

    /// Replaces a shape without recording history (for drags after a
    /// [`Document::checkpoint`]).
    pub fn replace_live(&mut self, shape: Shape) {
        if let Some(slot) = self.state.shapes.iter_mut().find(|s| s.id == shape.id) {
            *slot = Arc::new(shape);
            self.revision += 1;
        }
    }

    pub fn remove(&mut self, id: ShapeId) -> bool {
        let Some(pos) = self.state.shapes.iter().position(|s| s.id == id) else {
            return false;
        };
        self.checkpoint();
        self.state.shapes.remove(pos);
        self.revision += 1;
        true
    }

    pub fn clear(&mut self) {
        if self.is_pristine() {
            return;
        }
        self.checkpoint();
        self.state.shapes.clear();
        self.state.crop = None;
        self.revision += 1;
    }

    pub fn set_crop(&mut self, crop: Option<RectF>) {
        let full = RectF::new(0.0, 0.0, self.width() as f32, self.height() as f32);
        let crop = crop
            .and_then(|c| c.intersect(&full))
            .map(|c| {
                // Crops snap to whole pixels so the export has no half-pixel edge.
                let x = c.x.round();
                let y = c.y.round();
                RectF::new(
                    x,
                    y,
                    (c.right().round() - x).max(1.0),
                    (c.bottom().round() - y).max(1.0),
                )
            })
            .filter(|c| c.w >= 2.0 && c.h >= 2.0 && *c != full);
        if crop == self.state.crop {
            return;
        }
        self.checkpoint();
        self.state.crop = crop;
        self.revision += 1;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) -> bool {
        let Some(prev) = self.undo.pop() else {
            return false;
        };
        self.redo.push(std::mem::replace(&mut self.state, prev));
        self.revision += 1;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(next) = self.redo.pop() else {
            return false;
        };
        self.undo.push(std::mem::replace(&mut self.state, next));
        self.revision += 1;
        true
    }

    /// Topmost shape under `p`.
    pub fn hit_test(&self, p: Pt, tolerance: f32) -> Option<ShapeId> {
        self.state
            .shapes
            .iter()
            .rev()
            .find(|s| s.hit(p, tolerance))
            .map(|s| s.id)
    }

    /// Number for the next step badge: one more than the highest present.
    pub fn next_step_number(&self) -> u32 {
        self.state
            .shapes
            .iter()
            .filter_map(|s| match s.kind {
                ShapeKind::Step { number, .. } => Some(number),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            + 1
    }

    /// The region that will be exported.
    pub fn output_rect(&self) -> RectF {
        self.state.crop.unwrap_or(RectF::new(
            0.0,
            0.0,
            self.width() as f32,
            self.height() as f32,
        ))
    }

    /// Everything the exporter needs, detached from the document so it can be
    /// rendered on a background thread.
    pub fn snapshot(&self) -> RenderJob {
        RenderJob {
            base: self.base.clone(),
            shapes: self.state.shapes.clone(),
            crop: self.state.crop,
        }
    }
}

/// Immutable inputs for [`crate::export::render`].
#[derive(Clone)]
pub struct RenderJob {
    pub base: Arc<Image>,
    pub shapes: Vec<Arc<Shape>>,
    pub crop: Option<RectF>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Document {
        Document::new(Arc::new(Image::new(200, 100)), 1.0)
    }

    fn rect(doc: &mut Document, x: f32, y: f32) -> ShapeId {
        let s = doc.make_shape(
            ShapeKind::Rect {
                a: Pt::new(x, y),
                b: Pt::new(x + 20.0, y + 20.0),
            },
            Style::default(),
        );
        doc.add(s)
    }

    #[test]
    fn undo_and_redo_walk_history() {
        let mut d = doc();
        let a = rect(&mut d, 0.0, 0.0);
        let b = rect(&mut d, 50.0, 50.0);
        assert_eq!(d.shapes().len(), 2);
        assert!(d.undo());
        assert_eq!(d.shapes().len(), 1);
        assert_eq!(d.shapes()[0].id, a);
        assert!(d.redo());
        assert_eq!(d.shapes()[1].id, b);
        assert!(!d.redo(), "nothing left to redo");
        assert!(d.undo() && d.undo());
        assert!(d.is_pristine());
        assert!(!d.undo());
    }

    #[test]
    fn new_action_clears_redo() {
        let mut d = doc();
        rect(&mut d, 0.0, 0.0);
        d.undo();
        assert!(d.can_redo());
        rect(&mut d, 10.0, 10.0);
        assert!(!d.can_redo());
    }

    #[test]
    fn live_drag_is_one_undo_step() {
        let mut d = doc();
        let id = rect(&mut d, 0.0, 0.0);
        d.checkpoint();
        for i in 1..=10 {
            let moved = d.shape(id).unwrap().translated(1.0, 0.0);
            d.replace_live(moved);
            assert_eq!(d.shape(id).unwrap().bounds().x, i as f32 - 2.0);
        }
        d.undo();
        assert_eq!(d.shape(id).unwrap().bounds().x, -2.0);
    }

    #[test]
    fn revision_tracks_every_change() {
        let mut d = doc();
        let r0 = d.revision();
        let id = rect(&mut d, 0.0, 0.0);
        assert!(d.revision() > r0);
        let r1 = d.revision();
        d.remove(id);
        assert!(d.revision() > r1);
        let r2 = d.revision();
        assert!(!d.remove(id));
        assert_eq!(d.revision(), r2);
    }

    #[test]
    fn hit_test_prefers_topmost_shape() {
        let mut d = doc();
        let below = rect(&mut d, 0.0, 0.0);
        let above = rect(&mut d, 10.0, 0.0);
        assert_eq!(d.hit_test(Pt::new(10.0, 10.0), 2.0), Some(above));
        assert_eq!(d.hit_test(Pt::new(0.0, 10.0), 2.0), Some(below));
        assert_eq!(
            d.hit_test(Pt::new(5.0, 10.0), 0.5),
            None,
            "unfilled interior"
        );
    }

    #[test]
    fn hit_tests_per_kind() {
        let mut d = doc();
        let style = Style::default();
        let mut mk = |kind| d.make_shape(kind, style);
        let line = mk(ShapeKind::Line {
            a: Pt::new(0.0, 0.0),
            b: Pt::new(100.0, 0.0),
        });
        assert!(line.hit(Pt::new(50.0, 3.0), 2.0));
        assert!(!line.hit(Pt::new(50.0, 10.0), 2.0));

        let ellipse = mk(ShapeKind::Ellipse {
            a: Pt::new(0.0, 0.0),
            b: Pt::new(100.0, 50.0),
        });
        assert!(ellipse.hit(Pt::new(100.0, 25.0), 1.0), "on the outline");
        assert!(!ellipse.hit(Pt::new(50.0, 25.0), 1.0), "hollow centre");
        let filled = ellipse.with_style(Style {
            fill: true,
            ..style
        });
        assert!(filled.hit(Pt::new(50.0, 25.0), 1.0));

        let pen = mk(ShapeKind::Pen {
            points: vec![Pt::new(0.0, 0.0), Pt::new(10.0, 10.0)],
        });
        assert!(pen.hit(Pt::new(5.0, 5.0), 0.0));
        let step = mk(ShapeKind::Step {
            center: Pt::new(50.0, 50.0),
            number: 1,
        });
        assert!(step.hit(Pt::new(60.0, 50.0), 0.0));
        assert!(!step.hit(Pt::new(80.0, 50.0), 0.0));
    }

    #[test]
    fn crop_is_clamped_snapped_and_ignored_when_full() {
        let mut d = doc();
        d.set_crop(Some(RectF::new(-10.0, 10.4, 50.6, 500.0)));
        assert_eq!(d.crop(), Some(RectF::new(0.0, 10.0, 41.0, 90.0)));
        d.set_crop(Some(RectF::new(0.0, 0.0, 200.0, 100.0)));
        assert_eq!(d.crop(), None);
        assert!(d.undo());
        assert!(d.crop().is_some());
        let rev = d.revision();
        d.set_crop(d.crop());
        assert_eq!(d.revision(), rev, "unchanged crop records nothing");
    }

    #[test]
    fn step_numbers_continue_from_highest() {
        let mut d = doc();
        assert_eq!(d.next_step_number(), 1);
        for n in [1, 5] {
            let s = d.make_shape(
                ShapeKind::Step {
                    center: Pt::new(0.0, 0.0),
                    number: n,
                },
                Style::default(),
            );
            d.add(s);
        }
        assert_eq!(d.next_step_number(), 6);
    }

    #[test]
    fn degenerate_shapes_are_detected() {
        let mut d = doc();
        let s = Style::default();
        let tiny = d.make_shape(
            ShapeKind::Rect {
                a: Pt::new(0.0, 0.0),
                b: Pt::new(1.0, 50.0),
            },
            s,
        );
        assert!(tiny.is_degenerate());
        let blank = d.make_shape(
            ShapeKind::Text {
                origin: Pt::default(),
                text: " \n ".into(),
                font: FontSpec::default(),
            },
            s,
        );
        assert!(blank.is_degenerate());
    }

    #[test]
    fn sizes_scale_with_hidpi() {
        let mut d = Document::new(Arc::new(Image::new(10, 10)), 2.0);
        let s = d.make_shape(
            ShapeKind::Line {
                a: Pt::new(0.0, 0.0),
                b: Pt::new(5.0, 5.0),
            },
            Style::default(),
        );
        assert_eq!(s.stroke_width(), STROKE_WIDTHS[1] * 2.0);
    }
}
