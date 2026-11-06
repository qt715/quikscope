//! Text layout and glyph outlines for text annotations.
//!
//! The editor and the PNG exporter both draw text as filled glyph outlines
//! from this one layout, so saved images match the editor exactly. Using the
//! platform text system on screen would bring shaping and hinting that the
//! CPU exporter could not reproduce.

use std::sync::Arc;

use tiny_skia::{Path, PathBuilder};
use ttf_parser::{GlyphId, OutlineBuilder};

use crate::fonts::{self, Face, FontSpec};
use crate::geom::Pt;

/// Line height as a multiple of the font size.
pub const LINE_HEIGHT: f32 = 1.25;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glyph {
    pub id: u16,
    /// Pen x position relative to the line start.
    pub x: f32,
    /// Drawn from the fallback face (Inter) because the font lacks it.
    pub fallback: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LineLayout {
    pub glyphs: Vec<Glyph>,
    /// x position before each char and after the last (`chars + 1` entries).
    pub carets: Vec<f32>,
    pub width: f32,
}

#[derive(Clone)]
pub struct TextLayout {
    /// Font size in pixels.
    pub size: f32,
    pub lines: Vec<LineLayout>,
    pub width: f32,
    pub line_height: f32,
    /// Distance from a line's top to its baseline.
    pub ascent: f32,
    face: Arc<Face>,
    fallback: Arc<Face>,
}

impl std::fmt::Debug for TextLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextLayout")
            .field("size", &self.size)
            .field("lines", &self.lines)
            .field("family", &self.face.family)
            .finish()
    }
}

impl TextLayout {
    pub fn height(&self) -> f32 {
        self.line_height * self.lines.len().max(1) as f32
    }

    /// Top-left of the caret before char `index` of `text` (in chars,
    /// counting newlines), relative to the text origin.
    pub fn caret(&self, text: &str, index: usize) -> Pt {
        let mut remaining = index;
        for (line_no, line) in text.split('\n').enumerate() {
            let n = line.chars().count();
            if remaining <= n {
                let x = self.lines[line_no].carets[remaining];
                return Pt::new(x, line_no as f32 * self.line_height);
            }
            remaining -= n + 1;
        }
        let last = self.lines.len().saturating_sub(1);
        Pt::new(
            self.lines.last().map_or(0.0, |l| l.width),
            last as f32 * self.line_height,
        )
    }
}

/// Lays out `text` in `font`, whose `size` is in pixels.
pub fn layout(text: &str, font: &FontSpec) -> TextLayout {
    let primary_face = fonts::resolve(font);
    let fallback_face = fonts::fallback(font);
    let size = font.size;
    let primary = primary_face.parse(size);
    let fallback = fallback_face.parse(size);
    let scale = size / primary.units_per_em() as f32;
    let fb_scale = size / fallback.units_per_em() as f32;
    let question = primary.glyph_index('?').unwrap_or(GlyphId(0));

    let lines: Vec<LineLayout> = text
        .split('\n')
        .map(|line| {
            let mut x = 0.0;
            let mut glyphs = Vec::with_capacity(line.len());
            let mut carets = Vec::with_capacity(line.len() + 1);
            for ch in line.chars() {
                carets.push(x);
                let ch = if ch == '\t' { ' ' } else { ch };
                let (gid, from_fallback) = match primary.glyph_index(ch) {
                    Some(g) => (g, false),
                    None => match fallback.glyph_index(ch) {
                        Some(g) => (g, true),
                        None => (question, false),
                    },
                };
                glyphs.push(Glyph {
                    id: gid.0,
                    x,
                    fallback: from_fallback,
                });
                x += if from_fallback {
                    fallback.glyph_hor_advance(gid).unwrap_or(0) as f32 * fb_scale
                } else {
                    primary.glyph_hor_advance(gid).unwrap_or(0) as f32 * scale
                };
            }
            carets.push(x);
            LineLayout {
                glyphs,
                carets,
                width: x,
            }
        })
        .collect();

    let asc = primary.ascender() as f32 * scale;
    let desc = primary.descender() as f32 * scale;
    let line_height = size * LINE_HEIGHT;
    TextLayout {
        size,
        width: lines.iter().map(|l| l.width).fold(0.0, f32::max),
        lines,
        line_height,
        // Centre the font's ascender–descender box within the line.
        ascent: (line_height - (asc - desc)) / 2.0 + asc,
        face: primary_face,
        fallback: fallback_face,
    }
}

struct Outline<'a> {
    pb: &'a mut PathBuilder,
    scale: f32,
    slant: f32,
    x: f32,
    y: f32,
}

impl Outline<'_> {
    fn pt(&self, x: f32, y: f32) -> (f32, f32) {
        // Synthetic italics shear around the baseline.
        (
            (x + y * self.slant) * self.scale + self.x,
            self.y - y * self.scale,
        )
    }
}

impl OutlineBuilder for Outline<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.pt(x, y);
        self.pb.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.pt(x, y);
        self.pb.line_to(x, y);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (x1, y1) = self.pt(x1, y1);
        let (x, y) = self.pt(x, y);
        self.pb.quad_to(x1, y1, x, y);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (x1, y1) = self.pt(x1, y1);
        let (x2, y2) = self.pt(x2, y2);
        let (x, y) = self.pt(x, y);
        self.pb.cubic_to(x1, y1, x2, y2, x, y);
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

/// All glyph outlines of `layout` placed with its top-left at `origin`, as
/// one non-zero-winding path. `None` for whitespace-only text.
pub fn outline(layout: &TextLayout, origin: Pt) -> Option<Path> {
    let primary = layout.face.parse(layout.size);
    let fallback = layout.fallback.parse(layout.size);
    let mut pb = PathBuilder::new();
    for (i, line) in layout.lines.iter().enumerate() {
        let baseline = origin.y + i as f32 * layout.line_height + layout.ascent;
        for g in &line.glyphs {
            let (face, slant) = if g.fallback {
                (&fallback, layout.fallback.slant())
            } else {
                (&primary, layout.face.slant())
            };
            let mut o = Outline {
                pb: &mut pb,
                scale: layout.size / face.units_per_em() as f32,
                slant,
                x: origin.x + g.x,
                y: baseline,
            };
            face.outline_glyph(GlyphId(g.id), &mut o);
        }
    }
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font(size: f32) -> FontSpec {
        FontSpec {
            size,
            ..FontSpec::default()
        }
    }

    #[test]
    fn layout_measures_lines_and_carets() {
        let l = layout("ab\nabc", &font(20.0));
        assert_eq!(l.lines.len(), 2);
        assert_eq!(l.lines[0].carets.len(), 3);
        assert_eq!(l.lines[1].carets.len(), 4);
        assert!(l.lines[1].width > l.lines[0].width);
        assert_eq!(l.width, l.lines[1].width);
        assert_eq!(l.height(), 50.0);
        assert!(l.ascent > 0.0 && l.ascent < 25.0);
    }

    #[test]
    fn caret_positions_walk_across_newlines() {
        let text = "ab\nc";
        let l = layout(text, &font(10.0));
        assert_eq!(l.caret(text, 0), Pt::new(0.0, 0.0));
        assert_eq!(l.caret(text, 2), Pt::new(l.lines[0].width, 0.0));
        assert_eq!(l.caret(text, 3), Pt::new(0.0, l.line_height));
        assert_eq!(l.caret(text, 4).x, l.lines[1].width);
    }

    #[test]
    fn outline_covers_text_box() {
        let l = layout("Hi", &font(40.0));
        let path = outline(&l, Pt::new(100.0, 50.0)).unwrap();
        let b = path.bounds();
        assert!(b.left() >= 100.0 && b.right() <= 100.0 + l.width + 1.0);
        assert!(b.top() >= 50.0 && b.bottom() <= 50.0 + l.line_height);
        assert!(outline(&layout("  ", &font(40.0)), Pt::new(0.0, 0.0)).is_none());
    }

    #[test]
    fn bold_is_wider_and_italic_leans() {
        let regular = layout(
            "moment",
            &FontSpec {
                weight: 400,
                ..font(40.0)
            },
        );
        let bold = layout(
            "moment",
            &FontSpec {
                weight: 800,
                ..font(40.0)
            },
        );
        assert!(bold.width > regular.width);

        // An upright "l" is a vertical bar; italics move its top right of its bottom.
        let lean = |italic| {
            let l = layout(
                "l",
                &FontSpec {
                    italic,
                    ..font(100.0)
                },
            );
            let b = outline(&l, Pt::new(0.0, 0.0)).unwrap().bounds();
            b.width()
        };
        assert!(
            lean(true) > lean(false) * 1.5,
            "{} vs {}",
            lean(true),
            lean(false)
        );
    }

    #[test]
    fn characters_missing_from_inter_render_as_a_placeholder() {
        let l = layout("a\u{10FFFD}", &font(20.0));
        assert_eq!(l.lines[0].glyphs.len(), 2);
        assert!(l.lines[0].glyphs.iter().all(|g| !g.fallback));
    }
}
