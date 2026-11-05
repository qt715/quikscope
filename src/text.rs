//! Text layout and glyph outlines from the embedded Inter font.
//!
//! The editor and the PNG exporter both draw text as filled glyph outlines
//! from this one layout, so saved images match the editor exactly. Using the
//! platform text system on screen would bring shaping and hinting that the
//! CPU exporter could not reproduce.

use std::sync::LazyLock;

use tiny_skia::{Path, PathBuilder};
use ttf_parser::{Face, GlyphId, OutlineBuilder};

use crate::geom::Pt;

pub static FONT_DATA: &[u8] = include_bytes!("../assets/fonts/Inter-SemiBold.ttf");

static FACE: LazyLock<Face<'static>> =
    LazyLock::new(|| Face::parse(FONT_DATA, 0).expect("embedded Inter font parses"));

/// Line height as a multiple of the font size.
pub const LINE_HEIGHT: f32 = 1.25;

#[derive(Clone, Debug, PartialEq)]
pub struct LineLayout {
    /// Glyph and its pen x position, relative to the line start.
    pub glyphs: Vec<(u16, f32)>,
    /// x position before each char and after the last (`chars + 1` entries).
    pub carets: Vec<f32>,
    pub width: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TextLayout {
    pub size: f32,
    pub lines: Vec<LineLayout>,
    pub width: f32,
    pub line_height: f32,
    /// Distance from a line's top to its baseline.
    pub ascent: f32,
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

pub fn layout(text: &str, size: f32) -> TextLayout {
    let face = &*FACE;
    let scale = size / face.units_per_em() as f32;
    let fallback = face.glyph_index('?').unwrap_or(GlyphId(0));
    let lines: Vec<LineLayout> = text
        .split('\n')
        .map(|line| {
            let mut x = 0.0;
            let mut glyphs = Vec::with_capacity(line.len());
            let mut carets = Vec::with_capacity(line.len() + 1);
            for ch in line.chars() {
                carets.push(x);
                let ch = if ch == '\t' { ' ' } else { ch };
                let gid = face.glyph_index(ch).unwrap_or(fallback);
                glyphs.push((gid.0, x));
                x += face.glyph_hor_advance(gid).unwrap_or(0) as f32 * scale;
            }
            carets.push(x);
            LineLayout {
                glyphs,
                carets,
                width: x,
            }
        })
        .collect();
    let asc = face.ascender() as f32 * scale;
    let desc = face.descender() as f32 * scale;
    let line_height = size * LINE_HEIGHT;
    TextLayout {
        size,
        width: lines.iter().map(|l| l.width).fold(0.0, f32::max),
        lines,
        line_height,
        // Centre the font's ascender–descender box within the line.
        ascent: (line_height - (asc - desc)) / 2.0 + asc,
    }
}

struct Outline<'a> {
    pb: &'a mut PathBuilder,
    scale: f32,
    x: f32,
    y: f32,
}

impl OutlineBuilder for Outline<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.pb
            .move_to(self.x + x * self.scale, self.y - y * self.scale);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.pb
            .line_to(self.x + x * self.scale, self.y - y * self.scale);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.pb.quad_to(
            self.x + x1 * self.scale,
            self.y - y1 * self.scale,
            self.x + x * self.scale,
            self.y - y * self.scale,
        );
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.pb.cubic_to(
            self.x + x1 * self.scale,
            self.y - y1 * self.scale,
            self.x + x2 * self.scale,
            self.y - y2 * self.scale,
            self.x + x * self.scale,
            self.y - y * self.scale,
        );
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

/// All glyph outlines of `layout` placed with its top-left at `origin`, as
/// one non-zero-winding path. `None` for whitespace-only text.
pub fn outline(layout: &TextLayout, origin: Pt) -> Option<Path> {
    let face = &*FACE;
    let scale = layout.size / face.units_per_em() as f32;
    let mut pb = PathBuilder::new();
    for (i, line) in layout.lines.iter().enumerate() {
        let baseline = origin.y + i as f32 * layout.line_height + layout.ascent;
        for &(gid, x) in &line.glyphs {
            let mut o = Outline {
                pb: &mut pb,
                scale,
                x: origin.x + x,
                y: baseline,
            };
            face.outline_glyph(GlyphId(gid), &mut o);
        }
    }
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_measures_lines_and_carets() {
        let l = layout("ab\nabc", 20.0);
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
        let l = layout(text, 10.0);
        assert_eq!(l.caret(text, 0), Pt::new(0.0, 0.0));
        assert_eq!(l.caret(text, 2), Pt::new(l.lines[0].width, 0.0));
        assert_eq!(l.caret(text, 3), Pt::new(0.0, l.line_height));
        assert_eq!(l.caret(text, 4).x, l.lines[1].width);
    }

    #[test]
    fn layout_scales_linearly_with_size() {
        let a = layout("Hello", 10.0);
        let b = layout("Hello", 30.0);
        assert!((b.width - 3.0 * a.width).abs() < 1e-3);
    }

    #[test]
    fn outline_covers_text_box() {
        let l = layout("Hi", 40.0);
        let path = outline(&l, Pt::new(100.0, 50.0)).unwrap();
        let b = path.bounds();
        assert!(b.left() >= 100.0 && b.right() <= 100.0 + l.width + 1.0);
        assert!(b.top() >= 50.0 && b.bottom() <= 50.0 + l.line_height);
        assert!(outline(&layout("  ", 40.0), Pt::new(0.0, 0.0)).is_none());
    }
}
