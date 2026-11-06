//! Font selection for text annotations: the bundled Inter (variable, with a
//! real italic) and every font installed on the system.
//!
//! Faces are resolved to raw font data and drawn as glyph outlines by
//! [`crate::text`], so the editor and the exporter render the same font the
//! same way regardless of what the GPU text stack would pick.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use ttf_parser::Tag;

pub static INTER: &[u8] = include_bytes!("../assets/fonts/InterVariable.ttf");
pub static INTER_ITALIC: &[u8] = include_bytes!("../assets/fonts/InterVariable-Italic.ttf");

pub const DEFAULT_FAMILY: &str = "Inter";
pub const DEFAULT_WEIGHT: u16 = 600;
pub const DEFAULT_SIZE: f32 = 28.0;
pub const MIN_SIZE: f32 = 4.0;
pub const MAX_SIZE: f32 = 512.0;

/// Shear applied to fonts without an italic face.
const SYNTHETIC_SLANT: f32 = 0.2;

pub const WEIGHTS: [(u16, &str); 9] = [
    (100, "Thin"),
    (200, "Extra Light"),
    (300, "Light"),
    (400, "Regular"),
    (500, "Medium"),
    (600, "Semibold"),
    (700, "Bold"),
    (800, "Extra Bold"),
    (900, "Black"),
];

pub fn weight_name(weight: u16) -> &'static str {
    WEIGHTS
        .iter()
        .min_by_key(|(w, _)| w.abs_diff(weight))
        .map(|(_, n)| *n)
        .unwrap_or("Regular")
}

/// The typeface of a text annotation. `size` is in logical pixels; shapes
/// multiply it by the screenshot's scale.
#[derive(Clone, Debug, PartialEq)]
pub struct FontSpec {
    pub family: Arc<str>,
    pub weight: u16,
    pub italic: bool,
    pub size: f32,
}

impl Default for FontSpec {
    fn default() -> Self {
        FontSpec {
            family: DEFAULT_FAMILY.into(),
            weight: DEFAULT_WEIGHT,
            italic: false,
            size: DEFAULT_SIZE,
        }
    }
}

impl FontSpec {
    pub fn with_size(&self, size: f32) -> FontSpec {
        FontSpec {
            size: size.clamp(MIN_SIZE, MAX_SIZE),
            ..self.clone()
        }
    }

    /// Ctrl+B: bold unless already bold, then back to regular.
    pub fn toggle_bold(&self) -> FontSpec {
        FontSpec {
            weight: if self.weight >= 700 { 400 } else { 700 },
            ..self.clone()
        }
    }
}

type Data = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// A resolved, parseable font face plus how to instance it.
pub struct Face {
    data: Data,
    index: u32,
    /// Variation axes present in the face, with their ranges.
    axes: Vec<(Tag, f32, f32)>,
    weight: f32,
    /// Slant the outlines because the family has no italic face.
    pub synthetic_italic: bool,
    /// The family actually used (differs from the request after a fallback).
    pub family: Arc<str>,
}

impl Face {
    /// Parses the face, instanced at this weight and (for optical-size
    /// fonts like Inter) at `size_px`.
    pub fn parse(&self, size_px: f32) -> ttf_parser::Face<'_> {
        let mut face = ttf_parser::Face::parse(self.data.as_ref().as_ref(), self.index)
            .expect("font data was validated when resolved");
        for &(tag, min, max) in &self.axes {
            if tag == Tag::from_bytes(b"wght") {
                face.set_variation(tag, self.weight.clamp(min, max));
            } else if tag == Tag::from_bytes(b"opsz") {
                face.set_variation(tag, size_px.clamp(min, max));
            }
        }
        face
    }

    /// The face prepared for shaping, instanced like [`Face::parse`].
    pub fn shaper(&self, size_px: f32) -> rustybuzz::Face<'_> {
        rustybuzz::Face::from_face(self.parse(size_px))
    }

    /// Whether the face has a glyph for every visible char of `grapheme`.
    pub fn covers(&self, grapheme: &str) -> bool {
        let Ok(face) = ttf_parser::Face::parse(self.data.as_ref().as_ref(), self.index) else {
            return false;
        };
        grapheme
            .chars()
            .filter(|&c| !is_ignorable(c))
            .all(|c| face.glyph_index(c).is_some())
    }

    /// Identity of the underlying font data, for caches.
    pub fn key(&self) -> (usize, u32) {
        (self.data.as_ref().as_ref().as_ptr() as usize, self.index)
    }

    pub fn slant(&self) -> f32 {
        if self.synthetic_italic {
            SYNTHETIC_SLANT
        } else {
            0.0
        }
    }

    fn new(
        data: Data,
        index: u32,
        weight: u16,
        synthetic_italic: bool,
        family: Arc<str>,
    ) -> Option<Face> {
        let axes = {
            let face = ttf_parser::Face::parse(data.as_ref().as_ref(), index).ok()?;
            face.variation_axes()
                .into_iter()
                .map(|a| (a.tag, a.min_value, a.max_value))
                .collect()
        };
        Some(Face {
            data,
            index,
            axes,
            weight: weight as f32,
            synthetic_italic,
            family,
        })
    }
}

fn inter(weight: u16, italic: bool) -> Arc<Face> {
    let data: Data = Arc::new(if italic { INTER_ITALIC } else { INTER });
    Arc::new(
        Face::new(data, 0, weight, false, DEFAULT_FAMILY.into()).expect("bundled Inter parses"),
    )
}

static DB: LazyLock<fontdb::Database> = LazyLock::new(|| {
    let started = std::time::Instant::now();
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    log::debug!(
        "indexed {} system font faces in {:.0}ms",
        db.len(),
        started.elapsed().as_secs_f64() * 1e3
    );
    db
});

static FAMILIES: LazyLock<Vec<Arc<str>>> = LazyLock::new(|| {
    let mut names: Vec<String> = DB
        .faces()
        .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
        .filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case(DEFAULT_FAMILY))
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup();
    std::iter::once(DEFAULT_FAMILY.to_string())
        .chain(names)
        .map(Arc::from)
        .collect()
});

static FILES: LazyLock<Mutex<HashMap<PathBuf, Data>>> = LazyLock::new(Default::default);
type Key = (Arc<str>, u16, bool);
static RESOLVED: LazyLock<Mutex<HashMap<Key, Arc<Face>>>> = LazyLock::new(Default::default);

/// Indexes system fonts on a background thread so the font menu opens
/// without the scan (which can take a few hundred milliseconds).
pub fn warm() {
    std::thread::Builder::new()
        .name("moment-fonts".into())
        .spawn(|| {
            LazyLock::force(&FAMILIES);
            LazyLock::force(&CANDIDATES);
        })
        .ok();
}

/// Every selectable family: the bundled Inter first, then installed fonts.
pub fn families() -> &'static [Arc<str>] {
    &FAMILIES
}

/// Weights offered for `family`: every standard weight inside a variable
/// font's range, otherwise the weights of the installed faces.
pub fn family_weights(family: &str) -> Vec<u16> {
    if family.eq_ignore_ascii_case(DEFAULT_FAMILY) {
        return WEIGHTS.iter().map(|(w, _)| *w).collect();
    }
    let mut weights = Vec::new();
    for info in DB
        .faces()
        .filter(|f| f.families.iter().any(|(n, _)| n == family))
    {
        let variable = load(&info.source).and_then(|data| {
            let face = ttf_parser::Face::parse(data.as_ref().as_ref(), info.index).ok()?;
            face.variation_axes()
                .into_iter()
                .find(|a| a.tag == Tag::from_bytes(b"wght"))
                .map(|a| (a.min_value, a.max_value))
        });
        match variable {
            Some((min, max)) => weights.extend(
                WEIGHTS
                    .iter()
                    .map(|(w, _)| *w)
                    .filter(|w| (min..=max).contains(&(*w as f32))),
            ),
            None => weights.push(((info.weight.0 + 50) / 100 * 100).clamp(100, 900)),
        }
    }
    weights.sort_unstable();
    weights.dedup();
    if weights.is_empty() {
        weights.push(400);
    }
    weights
}

fn load(source: &fontdb::Source) -> Option<Data> {
    match source {
        fontdb::Source::Binary(data) | fontdb::Source::SharedFile(_, data) => Some(data.clone()),
        fontdb::Source::File(path) => {
            let mut files = FILES.lock().unwrap();
            if let Some(d) = files.get(path) {
                return Some(d.clone());
            }
            let file = std::fs::File::open(path).ok()?;
            // SAFETY: font files are read-only; a file replaced underneath
            // us would at worst render wrong glyphs, which ttf-parser
            // tolerates without unsafety of its own.
            let map = unsafe { memmap2::Mmap::map(&file).ok()? };
            let data: Data = Arc::new(map);
            files.insert(path.clone(), data.clone());
            Some(data)
        }
    }
}

/// The face to draw `spec` with. Unknown families fall back to Inter; a
/// family without an italic face gets a synthetic slant.
pub fn resolve(spec: &FontSpec) -> Arc<Face> {
    let key = (spec.family.clone(), spec.weight, spec.italic);
    if let Some(face) = RESOLVED.lock().unwrap().get(&key) {
        return face.clone();
    }
    let face = if spec.family.eq_ignore_ascii_case(DEFAULT_FAMILY) {
        inter(spec.weight, spec.italic)
    } else {
        resolve_system(spec).unwrap_or_else(|| {
            log::debug!("font family {:?} not found; using Inter", spec.family);
            inter(spec.weight, spec.italic)
        })
    };
    RESOLVED.lock().unwrap().insert(key, face.clone());
    face
}

/// Inter at the same weight/style, for characters the chosen font lacks.
pub fn fallback(spec: &FontSpec) -> Arc<Face> {
    resolve(&FontSpec {
        family: DEFAULT_FAMILY.into(),
        ..spec.clone()
    })
}

fn resolve_system(spec: &FontSpec) -> Option<Arc<Face>> {
    let id = DB.query(&fontdb::Query {
        families: &[fontdb::Family::Name(&spec.family)],
        weight: fontdb::Weight(spec.weight),
        stretch: fontdb::Stretch::Normal,
        style: if spec.italic {
            fontdb::Style::Italic
        } else {
            fontdb::Style::Normal
        },
    })?;
    let info = DB.face(id)?;
    // `query` falls back to any family; only accept the one asked for.
    if !info
        .families
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case(&spec.family))
    {
        return None;
    }
    let data = load(&info.source)?;
    let synthetic_italic = spec.italic && info.style == fontdb::Style::Normal;
    Face::new(
        data,
        info.index,
        spec.weight,
        synthetic_italic,
        spec.family.clone(),
    )
    .map(Arc::new)
}

/// Which regional glyph forms Han characters should take. Japanese,
/// Korean and Chinese share code points but not glyph shapes, and CJK font
/// families come in one variant per region.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    Ja,
    Ko,
    ZhHans,
    ZhHant,
    ZhHk,
}

impl Lang {
    /// The family-name suffix of this region's CJK fonts (Noto, Source Han).
    fn suffix(self) -> &'static str {
        match self {
            Lang::Ja => " JP",
            Lang::Ko => " KR",
            Lang::ZhHans => " SC",
            Lang::ZhHant => " TC",
            Lang::ZhHk => " HK",
        }
    }

    /// BCP 47 tag for the shaper (selects `locl` forms).
    pub fn tag(self) -> &'static str {
        match self {
            Lang::Ja => "ja",
            Lang::Ko => "ko",
            Lang::ZhHans => "zh-Hans",
            Lang::ZhHant => "zh-Hant",
            Lang::ZhHk => "zh-HK",
        }
    }

    /// From the text itself when it says (kana, hangul, bopomofo), otherwise
    /// from the locale, otherwise Japanese.
    pub fn detect(text: &str) -> Lang {
        for c in text.chars() {
            match c as u32 {
                0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F => return Lang::Ja,
                0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7AF => return Lang::Ko,
                0x3100..=0x312F | 0x31A0..=0x31BF => return Lang::ZhHant,
                _ => {}
            }
        }
        *LOCALE_LANG
    }

    fn from_locale(locale: &str) -> Option<Lang> {
        let l = locale.to_ascii_lowercase().replace('-', "_");
        Some(if l.starts_with("ja") {
            Lang::Ja
        } else if l.starts_with("ko") {
            Lang::Ko
        } else if l.starts_with("zh_tw") || l.contains("hant") {
            Lang::ZhHant
        } else if l.starts_with("zh_hk") || l.starts_with("zh_mo") {
            Lang::ZhHk
        } else if l.starts_with("zh") {
            Lang::ZhHans
        } else {
            return None;
        })
    }
}

static LOCALE_LANG: LazyLock<Lang> = LazyLock::new(|| {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty())
        .and_then(|v| Lang::from_locale(&v))
        .unwrap_or(Lang::Ja)
});

/// Characters that never need a glyph of their own: joiners and variation
/// selectors only steer the shaping of their neighbours.
pub fn is_ignorable(c: char) -> bool {
    matches!(c as u32,
        0x200B..=0x200F | 0x2060..=0x2064 | 0xFE00..=0xFE0F | 0xE0000..=0xE0FFF | 0xFEFF)
}

/// Whether `grapheme` asks for an emoji (pictographic) rendering rather
/// than a text one.
pub fn is_emoji(grapheme: &str) -> bool {
    let mut chars = grapheme.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if grapheme.contains('\u{FE0F}') || grapheme.contains('\u{20E3}') {
        return true;
    }
    if grapheme.contains('\u{FE0E}') {
        return false;
    }
    // Code points whose default presentation is emoji.
    matches!(first as u32,
        0x1F1E6..=0x1F1FF // regional indicators (flags)
        | 0x1F300..=0x1F5FF
        | 0x1F600..=0x1F64F
        | 0x1F680..=0x1F6FF
        | 0x1F7E0..=0x1F7EB
        | 0x1F90C..=0x1F9FF
        | 0x1FA70..=0x1FAFF
        | 0x2614..=0x2615 | 0x2648..=0x2653 | 0x267F | 0x2693 | 0x26A1
        | 0x26AA..=0x26AB | 0x26BD..=0x26BE | 0x26C4..=0x26C5 | 0x26CE | 0x26D4
        | 0x26EA | 0x26F2..=0x26F3 | 0x26F5 | 0x26FA | 0x26FD | 0x2705
        | 0x270A..=0x270B | 0x2728 | 0x274C | 0x274E | 0x2753..=0x2755 | 0x2757
        | 0x2795..=0x2797 | 0x27B0 | 0x27BF | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55
        | 0x231A..=0x231B | 0x23E9..=0x23EC | 0x23F0 | 0x23F3 | 0x25FD..=0x25FE)
}

/// One installed family considered for fallback, represented by its most
/// regular face.
struct Candidate {
    family: Arc<str>,
    source: fontdb::Source,
    index: u32,
    emoji: bool,
    /// "Noto Sans …" 0, other Noto 1, everything else 2: Noto covers the
    /// most scripts with a consistent design.
    rank: u8,
    /// Region suffix for CJK families, e.g. " JP".
    region: Option<&'static str>,
}

static CANDIDATES: LazyLock<Vec<Candidate>> = LazyLock::new(|| {
    let mut best: HashMap<&str, &fontdb::FaceInfo> = HashMap::new();
    for info in DB.faces() {
        let Some((name, _)) = info.families.first() else {
            continue;
        };
        let score =
            |f: &fontdb::FaceInfo| (f.style != fontdb::Style::Normal, f.weight.0.abs_diff(400));
        let slot = best.entry(name.as_str()).or_insert(info);
        if score(info) < score(slot) {
            *slot = info;
        }
    }
    let regions = [" JP", " KR", " SC", " TC", " HK"];
    let mut out: Vec<Candidate> = best
        .into_iter()
        .map(|(name, info)| Candidate {
            family: name.into(),
            source: info.source.clone(),
            index: info.index,
            emoji: name.contains("Emoji"),
            rank: if name.starts_with("Noto Sans") {
                0
            } else if name.starts_with("Noto") {
                1
            } else {
                2
            },
            region: regions.into_iter().find(|r| name.ends_with(r)),
        })
        .collect();
    out.sort_by(|a, b| (a.rank, &a.family).cmp(&(b.rank, &b.family)));
    out
});

type FallbackKey = (Box<str>, Lang);
static FALLBACKS: LazyLock<Mutex<HashMap<FallbackKey, Option<Arc<str>>>>> =
    LazyLock::new(Default::default);

/// An installed family that can draw `grapheme`, for when neither the
/// chosen font nor Inter can. Emoji prefer colour emoji fonts and Han
/// characters prefer the CJK variant for `lang`. Results (including "none")
/// are cached, so a scan over the installed fonts happens once per grapheme.
pub fn fallback_family(grapheme: &str, lang: Lang) -> Option<Arc<str>> {
    let key = (Box::from(grapheme), lang);
    if let Some(hit) = FALLBACKS.lock().unwrap().get(&key) {
        return hit.clone();
    }
    let started = std::time::Instant::now();
    let emoji = is_emoji(grapheme);
    let mut order: Vec<&Candidate> = CANDIDATES.iter().collect();
    order.sort_by_key(|c| {
        let region = match c.region {
            Some(r) if r == lang.suffix() => 0,
            None => 1,
            Some(_) => 2,
        };
        (c.emoji != emoji, region)
    });
    let wanted: Vec<char> = grapheme.chars().filter(|&c| !is_ignorable(c)).collect();
    let found = order
        .into_iter()
        .find(|c| {
            load(&c.source)
                .and_then(|data| {
                    let face = ttf_parser::Face::parse(data.as_ref().as_ref(), c.index).ok()?;
                    Some(wanted.iter().all(|&ch| face.glyph_index(ch).is_some()))
                })
                .unwrap_or(false)
        })
        .map(|c| c.family.clone());
    log::debug!(
        "fallback for {grapheme:?}: {found:?} in {:.1}ms",
        started.elapsed().as_secs_f64() * 1e3
    );
    FALLBACKS.lock().unwrap().insert(key, found.clone());
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advance(face: &Face, ch: char) -> f32 {
        let f = face.parse(28.0);
        let gid = f.glyph_index(ch).unwrap();
        f.glyph_hor_advance(gid).unwrap() as f32 / f.units_per_em() as f32
    }

    #[test]
    fn default_font_is_the_bundled_inter() {
        let face = resolve(&FontSpec::default());
        assert_eq!(&*face.family, DEFAULT_FAMILY);
        assert!(!face.synthetic_italic);
        assert!(std::ptr::eq(
            face.data.as_ref().as_ref().as_ptr(),
            INTER.as_ptr()
        ));
    }

    #[test]
    fn inter_weight_axis_changes_glyph_widths() {
        let spec = |weight| FontSpec {
            weight,
            ..FontSpec::default()
        };
        let thin = advance(&resolve(&spec(100)), 'm');
        let black = advance(&resolve(&spec(900)), 'm');
        assert!(black > thin * 1.05, "black {black} vs thin {thin}");
    }

    #[test]
    fn inter_italic_uses_the_real_italic_face() {
        let face = resolve(&FontSpec {
            italic: true,
            ..FontSpec::default()
        });
        assert!(!face.synthetic_italic);
        assert!(std::ptr::eq(
            face.data.as_ref().as_ref().as_ptr(),
            INTER_ITALIC.as_ptr()
        ));
    }

    #[test]
    fn unknown_families_fall_back_to_inter() {
        let face = resolve(&FontSpec {
            family: "No Such Family 7f3a".into(),
            ..FontSpec::default()
        });
        assert_eq!(&*face.family, DEFAULT_FAMILY);
    }

    #[test]
    fn family_list_starts_with_inter_and_has_no_duplicates() {
        let f = families();
        assert_eq!(&*f[0], DEFAULT_FAMILY);
        let mut sorted: Vec<_> = f.iter().map(|s| s.to_lowercase()).collect();
        sorted.dedup();
        assert_eq!(sorted.len(), f.len());
        assert_eq!(family_weights(DEFAULT_FAMILY).len(), 9);
    }

    #[test]
    fn weight_names_and_bold_toggle() {
        assert_eq!(weight_name(600), "Semibold");
        assert_eq!(weight_name(650), "Semibold");
        let f = FontSpec::default();
        assert_eq!(f.toggle_bold().weight, 700);
        assert_eq!(f.toggle_bold().toggle_bold().weight, 400);
        assert_eq!(f.with_size(10_000.0).size, MAX_SIZE);
    }
}
