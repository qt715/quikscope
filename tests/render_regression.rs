//! Golden-image regression tests for the PNG exporter.
//!
//! Renders a fixed scene with every annotation kind and compares it with
//! `tests/golden/*.png`. After an intentional rendering change, regenerate
//! the goldens with `QUICKSCOPE_BLESS=1 cargo test --test render_regression`
//! and review the diff images before committing.

#[path = "support/scene.rs"]
mod scene;

use std::path::PathBuf;

use quickscope::export;
use quickscope::raster::{Compression, Image};

fn golden(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.png"))
}

fn check(name: &str, actual: &Image) {
    let path = golden(name);
    if std::env::var_os("QUICKSCOPE_BLESS").is_some() {
        actual.save_png(&path, Compression::Balanced).unwrap();
        eprintln!("blessed {}", path.display());
        return;
    }
    let expected = Image::load(&path).unwrap_or_else(|e| {
        panic!("{e:#}\nrun with QUICKSCOPE_BLESS=1 to create the golden image")
    });
    // Anti-aliasing may differ by a step between tiny-skia builds (SIMD
    // paths); geometry or color regressions move far more pixels than this.
    let (over, mean) = scene::diff(actual, &expected, 6);
    let budget = (actual.width * actual.height / 500) as usize;
    if over > budget || mean > 0.5 {
        let out = std::env::temp_dir().join(format!("quickscope-{name}-actual.png"));
        actual.save_png(&out, Compression::Fast).unwrap();
        panic!(
            "{name}: {over} pixels differ (budget {budget}), mean diff {mean:.3}; actual written to {}",
            out.display()
        );
    }
}

#[test]
fn every_shape_kind_renders_like_the_golden() {
    check("scene", &export::render(&scene::document().snapshot()));
}

#[test]
fn crop_renders_like_the_golden() {
    let out = export::render(&scene::cropped_document().snapshot());
    assert_eq!((out.width, out.height), (300, 200));
    check("scene-cropped", &out);
}

#[test]
fn rendering_is_deterministic() {
    let a = export::render(&scene::document().snapshot());
    let b = export::render(&scene::document().snapshot());
    assert!(a == b, "two renders of the same document differ");
}

#[test]
fn undo_restores_the_untouched_capture() {
    let mut doc = scene::document();
    while doc.undo() {}
    assert_eq!(export::render(&doc.snapshot()), scene::base_image());
}

#[test]
fn hidpi_documents_scale_annotations() {
    use quickscope::document::{Document, ShapeKind, Style};
    use quickscope::geom::Pt;
    use std::sync::Arc;

    // The same logical stroke covers 4x the pixels at scale 2.
    let count = |scale: f32| {
        let base = Image::filled(200, 200, [0, 0, 0, 255]);
        let mut doc = Document::new(Arc::new(base), scale);
        let s = doc.make_shape(
            ShapeKind::Line {
                a: Pt::new(20.0, 100.0),
                b: Pt::new(180.0, 100.0),
            },
            Style::default(),
        );
        doc.add(s);
        let out = export::render(&doc.snapshot());
        out.data.chunks_exact(4).filter(|p| p[2] > 128).count()
    };
    let (x1, x2) = (count(1.0), count(2.0));
    assert!(
        x2 > x1 * 3 / 2,
        "scale 2 stroke ({x2}) should be wider than scale 1 ({x1})"
    );
}
