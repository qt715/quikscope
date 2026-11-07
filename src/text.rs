//! Text layout and glyph rendering for text annotations.
//!
//! The editor and the PNG exporter both draw text from this one layout, as
//! filled glyph outlines (and colour bitmaps for emoji), so saved images
//! match the editor exactly. Using the platform text system on screen would
//! bring shaping and hinting that the CPU exporter could not reproduce.
//!
//! Each line is split into bidi runs in visual order (unicode-bidi), each
//! run into items that share a face and a script, and each item is shaped
//! with rustybuzz. Faces are chosen per grapheme: the chosen font, then the
//! bundled Inter, then any installed font that has the grapheme.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, LazyLock, Mutex};

use tiny_skia::{Path, PathBuilder};
use ttf_parser::{GlyphId, OutlineBuilder, RasterImageFormat};
use unicode_bidi::BidiInfo;
use unicode_script::{Script, UnicodeScript};
use unicode_segmentation::UnicodeSegmentation;

use crate::fonts::{self, Face, FontSpec, Lang};
use crate::geom::{Pt, RectF};
use crate::raster::Image;

/// Line height as a multiple of the font size.
pub const LINE_HEIGHT: f32 = 1.25;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glyph {
    /// Index into the layout's faces.
    pub face: u16,
    pub id: u16,
    /// Left edge relative to the line start (pen position plus the shaper's
    /// offset).
    pub x: f32,
    /// Shift up from the baseline.
    pub y: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LineLayout {
    /// In drawing (visual) order.
    pub glyphs: Vec<Glyph>,
    /// x of the caret before each char and after the last (`chars + 1`
    /// entries), in logical order.
    pub carets: Vec<f32>,
    pub width: f32,
    /// The line's base direction is right-to-left.
    pub rtl: bool,
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
    /// The chosen face first, then every fallback a glyph uses.
    faces: Vec<Arc<Face>>,
}

impl std::fmt::Debug for TextLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextLayout")
            .field("size", &self.size)
            .field("lines", &self.lines)
            .field(
                "faces",
                &self.faces.iter().map(|f| &*f.family).collect::<Vec<_>>(),
            )
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

    /// Family names of the faces glyphs are drawn from, chosen face first.
    pub fn families(&self) -> Vec<&str> {
        self.faces.iter().map(|f| &*f.family).collect()
    }
}

/// Faces available to one layout.
struct Faces<'a> {
    spec: &'a FontSpec,
    lang: Lang,
    list: Vec<Arc<Face>>,
}

impl Faces<'_> {
    fn index_of(&mut self, face: Arc<Face>) -> u16 {
        let i = match self.list.iter().position(|f| Arc::ptr_eq(f, &face)) {
            Some(i) => i,
            None => {
                self.list.push(face);
                self.list.len() - 1
            }
        };
        i as u16
    }

    /// The face that draws `grapheme`.
    fn pick(&mut self, grapheme: &str) -> u16 {
        let emoji = fonts::is_emoji(grapheme);
        if !emoji && let Some(i) = self.list.iter().position(|f| f.covers(grapheme)) {
            return i as u16;
        }
        if let Some(family) = fonts::fallback_family(grapheme, self.lang) {
            let face = fonts::resolve(&FontSpec {
                family,
                ..self.spec.clone()
            });
            return self.index_of(face);
        }
        // Text-presentation fallback for an emoji no colour font has, and
        // otherwise the chosen face's .notdef box.
        self.list
            .iter()
            .position(|f| f.covers(grapheme))
            .unwrap_or(0) as u16
    }
}

/// Lays out `text` in `font`, whose `size` is in pixels.
pub fn layout(text: &str, font: &FontSpec) -> TextLayout {
    let primary = fonts::resolve(font);
    let mut faces = Faces {
        spec: font,
        lang: Lang::detect(text),
        list: vec![primary.clone()],
    };
    faces.index_of(fonts::fallback(font));
    let size = font.size;
    let lines: Vec<LineLayout> = text
        .split('\n')
        .map(|line| layout_line(line, size, &mut faces))
        .collect();

    let face = primary.parse(size);
    let scale = size / face.units_per_em() as f32;
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
        faces: faces.list,
    }
}

/// A stretch of one bidi run drawn with one face in one script.
struct Item {
    range: Range<usize>,
    face: u16,
    script: Script,
    /// Picked for emoji: the colour font's spaces and punctuation are
    /// emoji-sized, so text never joins such an item.
    emoji: bool,
}

/// The x extent of one shaper cluster, starting at byte `start`.
struct Cluster {
    start: usize,
    x0: f32,
    x1: f32,
    rtl: bool,
}

fn is_common(s: Script) -> bool {
    matches!(s, Script::Common | Script::Inherited | Script::Unknown)
}

fn grapheme_script(g: &str) -> Script {
    g.chars()
        .map(|c| c.script())
        .find(|s| !is_common(*s))
        .unwrap_or(Script::Common)
}

fn itemize(line: &str, run: Range<usize>, faces: &mut Faces) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::new();
    for (i, g) in line[run.clone()].grapheme_indices(true) {
        let (start, end) = (run.start + i, run.start + i + g.len());
        let script = grapheme_script(g);
        let emoji = fonts::is_emoji(g);
        // Spaces and punctuation stay with their neighbours' face so a run
        // of CJK keeps its own punctuation and spacing.
        let face = match items.last() {
            Some(last)
                if is_common(script)
                    && !emoji
                    && !last.emoji
                    && faces.list[last.face as usize].covers(g) =>
            {
                last.face
            }
            _ => faces.pick(g),
        };
        if let Some(last) = items.last_mut()
            && last.face == face
            && last.emoji == emoji
            && (is_common(script) || is_common(last.script) || last.script == script)
        {
            if is_common(last.script) {
                last.script = script;
            }
            last.range.end = end;
            continue;
        }
        items.push(Item {
            range: start..end,
            face,
            script,
            emoji,
        });
    }
    items
}

fn shaper_script(s: Script) -> Option<rustybuzz::Script> {
    if is_common(s) {
        return None;
    }
    let tag: [u8; 4] = s.short_name().as_bytes().try_into().ok()?;
    rustybuzz::Script::from_iso15924_tag(ttf_parser::Tag::from_bytes(&tag))
}

#[allow(clippy::too_many_arguments)]
fn shape_item(
    line: &str,
    item: &Item,
    rtl: bool,
    size: f32,
    faces: &Faces,
    pen: &mut f32,
    glyphs: &mut Vec<Glyph>,
    clusters: &mut Vec<Cluster>,
) {
    let face = &faces.list[item.face as usize];
    let shaper = face.shaper(size);
    let mut buf = rustybuzz::UnicodeBuffer::new();
    // Context lets Arabic and other joining scripts join across items.
    buf.set_pre_context(&line[..item.range.start]);
    buf.push_str(&line[item.range.clone()]);
    buf.set_post_context(&line[item.range.end..]);
    buf.guess_segment_properties();
    buf.set_direction(if rtl {
        rustybuzz::Direction::RightToLeft
    } else {
        rustybuzz::Direction::LeftToRight
    });
    if let Some(script) = shaper_script(item.script) {
        buf.set_script(script);
    }
    if matches!(
        item.script,
        Script::Han | Script::Hiragana | Script::Katakana | Script::Hangul | Script::Bopomofo
    ) && let Ok(lang) = faces.lang.tag().parse()
    {
        buf.set_language(lang);
    }
    let out = rustybuzz::shape(&shaper, &[], buf);
    let scale = size / shaper.units_per_em() as f32;
    for (info, pos) in out.glyph_infos().iter().zip(out.glyph_positions()) {
        let start = item.range.start + info.cluster as usize;
        let x0 = *pen;
        glyphs.push(Glyph {
            face: item.face,
            id: info.glyph_id as u16,
            x: x0 + pos.x_offset as f32 * scale,
            y: pos.y_offset as f32 * scale,
        });
        *pen += pos.x_advance as f32 * scale;
        match clusters.last_mut() {
            Some(c) if c.start == start => {
                c.x0 = c.x0.min(x0);
                c.x1 = c.x1.max(*pen);
            }
            _ => clusters.push(Cluster {
                start,
                x0,
                x1: *pen,
                rtl,
            }),
        }
    }
}

fn layout_line(line: &str, size: f32, faces: &mut Faces) -> LineLayout {
    if line.is_empty() {
        return LineLayout {
            glyphs: Vec::new(),
            carets: vec![0.0],
            width: 0.0,
            rtl: false,
        };
    }
    // Same byte length, so offsets carry over.
    let line = &line.replace('\t', " ");
    let bidi = BidiInfo::new(line, None);
    let rtl = bidi.paragraphs.first().is_some_and(|p| p.level.is_rtl());
    let mut pen = 0.0;
    let mut glyphs = Vec::with_capacity(line.len());
    let mut clusters = Vec::new();
    for para in &bidi.paragraphs {
        let (levels, runs) = bidi.visual_runs(para, para.range.clone());
        for run in runs {
            let run_rtl = levels[run.start].is_rtl();
            let mut items = itemize(line, run, faces);
            if run_rtl {
                items.reverse();
            }
            for item in &items {
                shape_item(
                    line,
                    item,
                    run_rtl,
                    size,
                    faces,
                    &mut pen,
                    &mut glyphs,
                    &mut clusters,
                );
            }
        }
    }

    // Carets in logical order: a char's caret sits at its cluster's leading
    // edge, spread evenly when several chars share one cluster (ligatures,
    // conjuncts).
    clusters.sort_by_key(|c| c.start);
    let mut carets = Vec::with_capacity(line.len() + 1);
    let mut ci = 0;
    for (b, _) in line.char_indices() {
        while ci + 1 < clusters.len() && clusters[ci + 1].start <= b {
            ci += 1;
        }
        let Some(c) = clusters.get(ci) else {
            carets.push(0.0);
            continue;
        };
        let next = clusters.get(ci + 1).map_or(line.len(), |n| n.start);
        let span = line[c.start..next].chars().count().max(1);
        let t = line[c.start..b.max(c.start)].chars().count() as f32 / span as f32;
        carets.push(if c.rtl {
            c.x1 - (c.x1 - c.x0) * t
        } else {
            c.x0 + (c.x1 - c.x0) * t
        });
    }
    carets.push(match clusters.last() {
        Some(c) if c.rtl => c.x0,
        Some(c) => c.x1,
        None => 0.0,
    });
    LineLayout {
        glyphs,
        carets,
        width: pen,
        rtl,
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

/// What a layout draws: outlines in the text colour, plus colour bitmaps
/// (emoji) placed in image coordinates.
pub struct Ink {
    pub path: Option<Path>,
    pub images: Vec<(RectF, Arc<Image>)>,
}

type BitmapKey = ((usize, u32), u16, u32, u32);
/// Colour glyph bitmaps, decoded and resampled to the size they are drawn at.
static BITMAPS: LazyLock<Mutex<HashMap<BitmapKey, Option<Arc<Image>>>>> =
    LazyLock::new(Default::default);

/// The colour bitmap of glyph `id` at `size` px with its pen at
/// (`x`, `baseline`), if the face stores the glyph as a PNG strike.
fn color_glyph(
    meta: &Face,
    face: &ttf_parser::Face,
    id: u16,
    size: f32,
    x: f32,
    baseline: f32,
) -> Option<(RectF, Arc<Image>)> {
    let raster = face.glyph_raster_image(GlyphId(id), u16::MAX)?;
    if raster.format != RasterImageFormat::PNG || raster.pixels_per_em == 0 {
        return None;
    }
    let scale = size / raster.pixels_per_em as f32;
    let rect = RectF::new(
        x + raster.x as f32 * scale,
        baseline - (raster.y as f32 + raster.height as f32) * scale,
        raster.width as f32 * scale,
        raster.height as f32 * scale,
    );
    // Resample once to the drawn size: neither renderer mipmaps, so drawing
    // a 136 px strike at 28 px would alias.
    let (w, h) = (
        rect.w.round().max(1.0) as u32,
        rect.h.round().max(1.0) as u32,
    );
    let key = (meta.key(), id, w, h);
    let mut cache = BITMAPS.lock().unwrap();
    let image = cache
        .entry(key)
        .or_insert_with(|| {
            let decoded =
                image::load_from_memory_with_format(raster.data, image::ImageFormat::Png).ok()?;
            let resized = image::imageops::resize(
                &decoded.into_rgba8(),
                w,
                h,
                image::imageops::FilterType::Triangle,
            );
            Image::from_rgba(w, h, resized.into_raw())
                .ok()
                .map(Arc::new)
        })
        .clone()?;
    Some((rect, image))
}

/// Everything `layout` draws with its top-left at `origin`. Outlines form
/// one non-zero-winding path, `None` for whitespace-only text.
pub fn ink(layout: &TextLayout, origin: Pt) -> Ink {
    let parsed: Vec<ttf_parser::Face> = layout.faces.iter().map(|f| f.parse(layout.size)).collect();
    let mut pb = PathBuilder::new();
    let mut images = Vec::new();
    for (i, line) in layout.lines.iter().enumerate() {
        let baseline = origin.y + i as f32 * layout.line_height + layout.ascent;
        for g in &line.glyphs {
            let (meta, face) = (&layout.faces[g.face as usize], &parsed[g.face as usize]);
            let (x, y) = (origin.x + g.x, baseline - g.y);
            if let Some(image) = color_glyph(meta, face, g.id, layout.size, x, y) {
                images.push(image);
                continue;
            }
            let mut o = Outline {
                pb: &mut pb,
                scale: layout.size / face.units_per_em() as f32,
                slant: meta.slant(),
                x,
                y,
            };
            face.outline_glyph(GlyphId(g.id), &mut o);
        }
    }
    Ink {
        path: pb.finish(),
        images,
    }
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

    /// Script fonts are system fonts: CI installs them and sets
    /// `QUIKSCOPE_REQUIRE_FONTS=1`; elsewhere a test without them is skipped.
    fn has_font_for(sample: &str) -> bool {
        let found = fonts::fallback_family(sample, Lang::Ja).is_some();
        if !found {
            assert!(
                std::env::var_os("QUIKSCOPE_REQUIRE_FONTS").is_none(),
                "no installed font draws {sample:?}"
            );
            eprintln!("skipped: no installed font draws {sample:?}");
        }
        found
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
    fn ink_covers_text_box() {
        let l = layout("Hi", &font(40.0));
        let path = ink(&l, Pt::new(100.0, 50.0)).path.unwrap();
        let b = path.bounds();
        assert!(b.left() >= 100.0 && b.right() <= 100.0 + l.width + 1.0);
        assert!(b.top() >= 50.0 && b.bottom() <= 50.0 + l.line_height);
        assert!(
            ink(&layout("  ", &font(40.0)), Pt::new(0.0, 0.0))
                .path
                .is_none()
        );
    }

    #[test]
    fn bold_is_wider_and_italic_leans() {
        let regular = layout(
            "quikscope",
            &FontSpec {
                weight: 400,
                ..font(40.0)
            },
        );
        let bold = layout(
            "quikscope",
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
            let b = ink(&l, Pt::new(0.0, 0.0)).path.unwrap().bounds();
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
    fn kerning_and_ligatures_come_from_the_shaper() {
        // Inter kerns "AV": shaped, the pair is narrower than its glyphs'
        // advances side by side.
        let pair = layout("AV", &font(100.0)).width;
        let apart = layout("A", &font(100.0)).width + layout("V", &font(100.0)).width;
        assert!(pair < apart - 1.0, "{pair} vs {apart}");
    }

    #[test]
    fn characters_no_font_has_draw_the_notdef_box() {
        let l = layout("a\u{10FFFD}", &font(20.0));
        assert_eq!(l.lines[0].glyphs.len(), 2);
        assert_eq!(l.lines[0].glyphs[1].id, 0);
        assert_eq!(l.lines[0].glyphs[1].face, 0);
    }

    #[test]
    fn japanese_falls_back_to_an_installed_cjk_font() {
        if !has_font_for("日") {
            return;
        }
        let l = layout("Aにほん語", &font(28.0));
        let line = &l.lines[0];
        assert_eq!(line.glyphs.len(), 5);
        assert!(line.glyphs.iter().all(|g| g.id != 0), "{l:?}");
        assert_eq!(line.glyphs[0].face, 0, "Latin stays in the chosen font");
        assert!(
            line.glyphs[1..]
                .iter()
                .all(|g| g.face == line.glyphs[1].face)
        );
        assert_ne!(l.families()[line.glyphs[1].face as usize], "Inter");
        // Full-width ideographs advance about one em each.
        let w = line.carets[5] - line.carets[1];
        assert!((w - 4.0 * 28.0).abs() < 4.0, "{w}");
        let path = ink(&l, Pt::new(0.0, 0.0)).path.unwrap();
        assert!(path.bounds().width() > 4.0 * 28.0);
    }

    #[test]
    fn arabic_is_joined_and_laid_out_right_to_left() {
        if !has_font_for("ب") {
            return;
        }
        // "بيت": three letters that join, so the shaper picks contextual
        // forms (different glyphs than the isolated letters).
        let word = layout("بيت", &font(40.0));
        let line = &word.lines[0];
        assert!(line.rtl);
        let isolated: Vec<u16> = ["ب", "ي", "ت"]
            .iter()
            .map(|c| layout(c, &font(40.0)).lines[0].glyphs[0].id)
            .collect();
        let mut joined: Vec<u16> = line.glyphs.iter().map(|g| g.id).collect();
        joined.reverse();
        assert_ne!(joined, isolated, "letters were not joined");
        // The first letter sits at the right: its caret is the line's right edge.
        assert!(
            (line.carets[0] - line.width).abs() < 0.5,
            "{:?}",
            line.carets
        );
        assert!(line.carets[3] < 0.5);
        assert!(line.carets[1] < line.carets[0] && line.carets[2] < line.carets[1]);
    }

    #[test]
    fn mixed_direction_keeps_latin_left_to_right() {
        if !has_font_for("ש") {
            return;
        }
        let text = "ab שלום cd";
        let l = layout(text, &font(30.0));
        let c = &l.lines[0].carets;
        assert!(!l.lines[0].rtl);
        assert!(c[0] < c[1] && c[1] < c[2], "Latin advances right: {c:?}");
        // Hebrew advances left within its run: ש (index 3) is right of ם (6).
        assert!(c[3] > c[6], "{c:?}");
        assert!(c[8] > c[3], "trailing Latin follows the Hebrew run");
    }

    #[test]
    fn devanagari_conjuncts_shape_into_fewer_glyphs() {
        if !has_font_for("क") {
            return;
        }
        // क्ष is three code points that form one conjunct glyph.
        let l = layout("क्ष", &font(40.0));
        assert!(l.lines[0].glyphs.len() < 3, "{l:?}");
        assert_eq!(l.lines[0].carets.len(), 4);
        assert!(l.lines[0].carets.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn color_emoji_become_bitmaps_at_the_text_size() {
        if !has_font_for("😀") {
            return;
        }
        let l = layout("a😀", &font(40.0));
        let out = ink(&l, Pt::new(10.0, 20.0));
        let face = &l.faces[l.lines[0].glyphs[1].face as usize];
        let parsed = face.parse(40.0);
        let bitmap = parsed.glyph_raster_image(GlyphId(l.lines[0].glyphs[1].id), u16::MAX);
        if bitmap.is_none() {
            // An outline emoji font: drawn like any other glyph.
            assert!(out.images.is_empty());
            return;
        }
        assert_eq!(out.images.len(), 1);
        let (rect, image) = &out.images[0];
        assert!(rect.x >= 10.0 + l.lines[0].carets[1] - 4.0);
        assert!((rect.w - 40.0 * 1.2).abs() < 12.0, "{rect:?}");
        assert_eq!(
            (image.width, image.height),
            (rect.w.round() as u32, rect.h.round() as u32)
        );
        assert!(out.path.is_some(), "the 'a' is still an outline");
    }

    #[test]
    fn spaces_after_emoji_keep_the_text_font_width() {
        if !has_font_for("❤️") {
            return;
        }
        let space = layout(" ", &font(30.0)).width;
        for text in ["❤️ x", "😀 x"] {
            let c = &layout(text, &font(30.0)).lines[0].carets;
            let n = text.chars().count();
            // carets: before each char; the space is the char before "x".
            let gap = c[n - 1] - c[n - 2];
            assert!((gap - space).abs() < 0.5, "{text:?}: {gap} vs {space}");
        }
    }

    #[test]
    fn graphemes_with_joiners_stay_one_cluster() {
        if !has_font_for("👩\u{200D}💻") {
            return;
        }
        // Woman technologist: three code points, one glyph, one caret span.
        let text = "👩\u{200D}💻";
        let l = layout(text, &font(30.0));
        assert_eq!(l.lines[0].glyphs.len(), 1, "{l:?}");
        assert_eq!(l.lines[0].carets.len(), 4);
    }
}
