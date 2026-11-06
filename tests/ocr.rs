//! Text extraction against the real Tesseract, on screenshots rendered
//! with quickscope's own text engine (English, European, CJK, Arabic, Thai,
//! Vietnamese; dark and light themes).
//!
//! Needs `libtesseract` and its language data. Without them (or without a
//! language's model) the affected checks are skipped and say so, unless
//! `QUICKSCOPE_REQUIRE_OCR=1`, which CI sets.

use std::sync::Arc;

use quickscope::document::{Document, Rgba, ShapeKind, Style};
use quickscope::fonts::FontSpec;
use quickscope::geom::{IRect, Pt};
use quickscope::ocr;
use quickscope::raster::Image;

fn required() -> bool {
    std::env::var_os("QUICKSCOPE_REQUIRE_OCR").is_some()
}

/// The installed models, or `None` (test skipped) without Tesseract.
fn installed() -> Option<Vec<String>> {
    let found = ocr::tess::version()
        .ok()
        .and_then(|_| ocr::tessdata_dir())
        .map(|d| ocr::installed_languages(&d));
    if found.is_none() {
        assert!(!required(), "Tesseract or its language data is missing");
        eprintln!("skipped: Tesseract is not installed");
    }
    found
}

/// Renders `lines` like a chat window: 16 px text, one line every 32 px.
fn screenshot(lines: &[&str], dark: bool) -> Image {
    let bg = if dark {
        [0x26, 0x23, 0x1f, 255]
    } else {
        [255, 255, 255, 255]
    };
    let h = lines.len() as u32 * 32 + 20;
    let mut d = Document::new(Arc::new(Image::filled(720, h, bg)), 1.0);
    for (i, l) in lines.iter().enumerate() {
        let s = d.make_shape(
            ShapeKind::Text {
                origin: Pt::new(12.0, 10.0 + i as f32 * 32.0),
                text: (*l).into(),
                font: FontSpec {
                    weight: 400,
                    ..FontSpec::default()
                }
                .with_size(16.0),
            },
            Style {
                color: if dark {
                    Rgba::rgb(0xdbdee1)
                } else {
                    Rgba::rgb(0x111111)
                },
                size: 1,
                fill: false,
            },
        );
        d.add(s);
    }
    quickscope::export::render(&d.snapshot())
}

/// (text, model that should read it). English is the primary model.
const LINES: [(&str, &str); 9] = [
    ("is avif cpu/gpu expensive? (edited)", "eng"),
    ("The quick brown fox jumps over 13 lazy dogs.", "eng"),
    ("日本語のテキストを読み取ります", "jpn"),
    ("中文文本识别测试", "chi_sim"),
    ("한국어 텍스트 인식", "kor"),
    ("مرحبا بالعالم", "ara"),
    ("ภาษาไทยทดสอบ", "tha"),
    ("Tiếng Việt có dấu", "vie"),
    ("Größe für Straße", "deu"),
];

#[test]
fn each_line_is_read_in_its_own_language() {
    let Some(installed) = installed() else { return };
    for dark in [true, false] {
        let texts: Vec<&str> = LINES.iter().map(|(t, _)| *t).collect();
        let image = screenshot(&texts, dark);
        let found = ocr::extract(&image, image.bounds(), 2, &[]).unwrap();
        assert_eq!(found.lines.len(), LINES.len(), "dark={dark}: {found:#?}");
        for ((want, lang), got) in LINES.iter().zip(&found.lines) {
            if !installed.iter().any(|l| l == lang) {
                assert!(!required(), "the {lang} model is not installed");
                eprintln!("skipped {lang}: model not installed");
                continue;
            }
            assert_eq!(
                (got.text.as_str(), got.lang.as_str()),
                (*want, *lang),
                "dark={dark}"
            );
        }
        // Lines come back in reading order, located in screenshot pixels.
        let rows: Vec<i32> = found.lines.iter().map(|l| l.rect.y).collect();
        assert!(rows.windows(2).all(|w| w[0] < w[1]), "{rows:?}");
        assert!(found.lines.iter().all(|l| l.rect.x >= 8 && l.rect.x <= 16));
        // The copied text is those lines in order, one per line.
        let read: Vec<&str> = found.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(found.text, read.join("\n"));
    }
}

#[test]
fn a_region_reads_only_what_it_covers_and_maps_back() {
    if installed().is_none() {
        return;
    }
    let image = screenshot(
        &["first line here", "second line here", "third line here"],
        true,
    );
    // Just the second line (y 42..74).
    let found = ocr::extract(&image, IRect::new(0, 38, 720, 34), 2, &[]).unwrap();
    assert_eq!(found.text, "second line here");
    let r = found.lines[0].rect;
    assert!((42..=50).contains(&r.y) && r.bottom() <= 72, "{r:?}");
    // Nothing readable: empty, not an error.
    let blank = Image::filled(300, 80, [255, 255, 255, 255]);
    assert_eq!(
        ocr::extract(&blank, blank.bounds(), 2, &[]).unwrap().text,
        ""
    );
}

#[test]
fn configured_languages_limit_the_models() {
    let Some(installed) = installed() else { return };
    if !installed.iter().any(|l| l == "jpn") {
        assert!(!required(), "the jpn model is not installed");
        return;
    }
    let image = screenshot(&["日本語のテキストを読み取ります"], false);
    let only_english = ocr::extract(&image, image.bounds(), 2, &["eng".into()]).unwrap();
    assert!(only_english.lines.iter().all(|l| l.lang == "eng"));
    assert_ne!(only_english.text, "日本語のテキストを読み取ります");
    let japanese = ocr::extract(&image, image.bounds(), 2, &["jpn".into()]).unwrap();
    assert_eq!(japanese.text, "日本語のテキストを読み取ります");
    let missing = ocr::extract(&image, image.bounds(), 2, &["xx_none".into()]).unwrap_err();
    assert!(missing.to_string().contains("not installed"), "{missing}");
}
