//! Shapes → renderer-independent primitives.
//!
//! Every stroke is converted to its filled outline here (tiny-skia's
//! stroker), so the GPU editor and the CPU exporter fill identical
//! geometry; neither renderer's own stroker is ever used.

use std::sync::Arc;

use tiny_skia::{LineCap, LineJoin, Path, PathBuilder, Rect, Stroke};

use crate::document::{Rgba, Shape, ShapeKind};
use crate::geom::{Pt, RectF};
use crate::raster::Image;
use crate::text;

#[derive(Debug)]
pub enum Prim {
    /// Non-zero-winding fill in image coordinates.
    Fill { path: Path, color: Rgba },
    /// Opaque pixels placed at an integer position in image coordinates.
    Pixels { x: i32, y: i32, image: Arc<Image> },
}

/// Stroker precision: outlines stay smooth up to this zoom factor.
const RES_SCALE: f32 = 4.0;
const HIGHLIGHT_ALPHA: u8 = 96;

pub fn shape_prims(shape: &Shape, base: &Image) -> Vec<Prim> {
    let color = shape.style.color;
    let width = shape.stroke_width();
    let mut out = Vec::new();
    let stroke = |path: Option<Path>, color: Rgba, cap: LineCap, out: &mut Vec<Prim>| {
        let Some(path) = path else { return };
        let s = Stroke {
            width,
            line_cap: cap,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        if let Some(outline) = path.stroke(&s, RES_SCALE) {
            out.push(Prim::Fill {
                path: outline,
                color,
            });
        }
    };
    match &shape.kind {
        ShapeKind::Pen { points } => stroke(smooth_path(points), color, LineCap::Round, &mut out),
        ShapeKind::Highlighter { points } => stroke(
            smooth_path(points),
            // The chosen alpha scales the highlighter's own translucency.
            color.with_alpha((color.a as u32 * HIGHLIGHT_ALPHA as u32 / 255) as u8),
            LineCap::Round,
            &mut out,
        ),
        ShapeKind::Line { a, b } => stroke(segment(*a, *b), color, LineCap::Round, &mut out),
        ShapeKind::Arrow { a, b } => {
            let mut pb = PathBuilder::new();
            pb.move_to(a.x, a.y);
            pb.line_to(b.x, b.y);
            if let Some((l, r)) = arrow_head(*a, *b, width) {
                pb.move_to(l.x, l.y);
                pb.line_to(b.x, b.y);
                pb.line_to(r.x, r.y);
            }
            stroke(pb.finish(), color, LineCap::Round, &mut out);
        }
        ShapeKind::Rect { a, b } => {
            let r = RectF::from_points(*a, *b);
            let path = rounded_rect(r, (width * 1.5).min(r.w / 2.0).min(r.h / 2.0));
            if shape.style.fill {
                if let Some(path) = path {
                    out.push(Prim::Fill { path, color });
                }
            } else {
                stroke(path, color, LineCap::Round, &mut out);
            }
        }
        ShapeKind::Ellipse { a, b } => {
            let r = RectF::from_points(*a, *b);
            let path = Rect::from_xywh(r.x, r.y, r.w.max(0.5), r.h.max(0.5))
                .and_then(PathBuilder::from_oval);
            if shape.style.fill {
                if let Some(path) = path {
                    out.push(Prim::Fill { path, color });
                }
            } else {
                stroke(path, color, LineCap::Round, &mut out);
            }
        }
        ShapeKind::Text { origin, .. } => {
            let layout = shape.text_layout().unwrap();
            if let Some(path) = text::outline(&layout, *origin) {
                out.push(Prim::Fill { path, color });
            }
        }
        ShapeKind::Step { center, number } => {
            let radius = shape.step_radius();
            if let Some(circle) = PathBuilder::from_circle(center.x, center.y, radius) {
                out.push(Prim::Fill {
                    path: circle,
                    color,
                });
            }
            let label = number.to_string();
            let layout = text::layout(
                &label,
                &crate::fonts::FontSpec::default().with_size(radius * 1.1),
            );
            let origin = Pt::new(
                center.x - layout.width / 2.0,
                center.y - layout.line_height / 2.0,
            );
            if let Some(path) = text::outline(&layout, origin) {
                let ink = if color.is_light() {
                    Rgba::rgb(0x1e1e1e)
                } else {
                    Rgba::rgb(0xffffff)
                };
                out.push(Prim::Fill { path, color: ink });
            }
        }
        ShapeKind::Pixelate { a, b } => {
            if let Some(r) = RectF::from_points(*a, *b).to_pixel_rect(base.width, base.height) {
                let block = (12.0 * shape.scale).round().max(4.0) as u32;
                // Align blocks to the image grid so neighbouring redactions
                // and re-renders produce the same mosaic.
                let gx = r.x - r.x % block as i32;
                let gy = r.y - r.y % block as i32;
                let grid = crate::geom::IRect::new(gx, gy, r.right() - gx, r.bottom() - gy);
                let mosaic = base.crop(grid).pixelate(block);
                let patch = mosaic.crop(crate::geom::IRect::new(r.x - gx, r.y - gy, r.w, r.h));
                out.push(Prim::Pixels {
                    x: r.x,
                    y: r.y,
                    image: Arc::new(patch),
                });
            }
        }
    }
    out
}

fn segment(a: Pt, b: Pt) -> Option<Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(a.x, a.y);
    // A zero-length segment would vanish; nudge it so a click leaves a dot.
    if a == b {
        pb.line_to(b.x + 0.01, b.y);
    } else {
        pb.line_to(b.x, b.y);
    }
    pb.finish()
}

/// Smooth curve through freehand points: quadratic segments between
/// successive midpoints, which removes mouse jitter without lagging behind
/// the pointer.
pub fn smooth_path(points: &[Pt]) -> Option<Path> {
    match points {
        [] => None,
        [p] => segment(*p, *p),
        [a, b] => segment(*a, *b),
        _ => {
            let mut pb = PathBuilder::new();
            pb.move_to(points[0].x, points[0].y);
            for w in points.windows(2).skip(1) {
                let (c, n) = (w[0], w[1]);
                let mid = Pt::new((c.x + n.x) / 2.0, (c.y + n.y) / 2.0);
                pb.quad_to(c.x, c.y, mid.x, mid.y);
            }
            let last = points[points.len() - 1];
            pb.line_to(last.x, last.y);
            pb.finish()
        }
    }
}

pub fn arrow_head_len(a: Pt, b: Pt, width: f32) -> f32 {
    (width * 3.0 + 10.0).min(a.dist(b) * 0.45)
}

/// End points of the two barbs of an arrow pointing at `b`.
pub fn arrow_head(a: Pt, b: Pt, width: f32) -> Option<(Pt, Pt)> {
    let len = a.dist(b);
    if len < 1.0 {
        return None;
    }
    let head = arrow_head_len(a, b, width);
    let (ux, uy) = ((b.x - a.x) / len, (b.y - a.y) / len);
    let angle: f32 = 28f32.to_radians();
    let (s, c) = angle.sin_cos();
    let rot = |sx: f32| {
        let (dx, dy) = (-ux * c - sx * -uy * s, -uy * c - sx * ux * s);
        Pt::new(b.x + dx * head, b.y + dy * head)
    };
    Some((rot(1.0), rot(-1.0)))
}

fn rounded_rect(r: RectF, radius: f32) -> Option<Path> {
    if radius <= 0.5 {
        return Rect::from_xywh(r.x, r.y, r.w.max(0.5), r.h.max(0.5)).map(PathBuilder::from_rect);
    }
    // Circular corners from cubic arcs (kappa approximation).
    const K: f32 = 0.552_284_8;
    let (x0, y0, x1, y1) = (r.x, r.y, r.right(), r.bottom());
    let k = radius * K;
    let mut pb = PathBuilder::new();
    pb.move_to(x0 + radius, y0);
    pb.line_to(x1 - radius, y0);
    pb.cubic_to(x1 - radius + k, y0, x1, y0 + radius - k, x1, y0 + radius);
    pb.line_to(x1, y1 - radius);
    pb.cubic_to(x1, y1 - radius + k, x1 - radius + k, y1, x1 - radius, y1);
    pb.line_to(x0 + radius, y1);
    pb.cubic_to(x0 + radius - k, y1, x0, y1 - radius + k, x0, y1 - radius);
    pb.line_to(x0, y0 + radius);
    pb.cubic_to(x0, y0 + radius - k, x0 + radius - k, y0, x0 + radius, y0);
    pb.close();
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrow_barbs_are_symmetric_behind_the_tip() {
        let (l, r) = arrow_head(Pt::new(0.0, 0.0), Pt::new(100.0, 0.0), 4.0).unwrap();
        assert!(l.x < 100.0 && r.x < 100.0);
        assert!((l.x - r.x).abs() < 1e-3);
        assert!((l.y + r.y).abs() < 1e-3 && l.y.abs() > 1.0);
        assert!(arrow_head(Pt::new(5.0, 5.0), Pt::new(5.0, 5.0), 4.0).is_none());
    }

    #[test]
    fn short_arrows_keep_heads_proportional() {
        let len = arrow_head_len(Pt::new(0.0, 0.0), Pt::new(10.0, 0.0), 8.0);
        assert!(len <= 4.5);
    }

    #[test]
    fn smoothing_passes_through_endpoints() {
        let pts = [
            Pt::new(0.0, 0.0),
            Pt::new(10.0, 5.0),
            Pt::new(20.0, 0.0),
            Pt::new(30.0, 8.0),
        ];
        let path = smooth_path(&pts).unwrap();
        let b = path.bounds();
        assert_eq!((b.left(), b.right()), (0.0, 30.0));
        assert!(smooth_path(&[]).is_none());
        assert!(
            smooth_path(&[Pt::new(1.0, 1.0)]).is_some(),
            "single click leaves a dot"
        );
    }
}
