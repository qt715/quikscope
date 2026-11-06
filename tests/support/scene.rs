//! A deterministic screenshot and one annotation of every kind, shared by
//! the CPU golden-image test and the GPU/CPU parity test.

#![allow(dead_code)]

use std::sync::Arc;

use moment::document::{Document, PALETTE, ShapeKind, Style};
use moment::fonts::FontSpec;
use moment::geom::{Pt, RectF};
use moment::raster::Image;

pub const W: u32 = 480;
pub const H: u32 = 320;

/// Gradient with a checkerboard, so pixelation and alpha blending show.
pub fn base_image() -> Image {
    let mut img = Image::new(W, H);
    for y in 0..H {
        for x in 0..W {
            let check = ((x / 16) + (y / 16)) % 2 == 0;
            let i = ((y * W + x) * 4) as usize;
            let v = if check { 40 } else { 0 };
            img.data[i] = (200 - (y * 150 / H)) as u8; // B
            img.data[i + 1] = (x * 200 / W) as u8 + v; // G
            img.data[i + 2] = 60 + v; // R
            img.data[i + 3] = 255;
        }
    }
    img
}

fn style(color: usize, size: u8, fill: bool) -> Style {
    Style {
        color: PALETTE[color],
        size,
        fill,
    }
}

/// Adds one shape of every kind to `doc`.
pub fn populate(doc: &mut Document) {
    let p = Pt::new;
    let shapes = [
        (
            ShapeKind::Rect {
                a: p(20.0, 20.0),
                b: p(140.0, 100.0),
            },
            style(0, 1, false),
        ),
        (
            ShapeKind::Rect {
                a: p(160.0, 20.0),
                b: p(220.0, 70.0),
            },
            style(6, 1, true),
        ),
        (
            ShapeKind::Ellipse {
                a: p(240.0, 20.0),
                b: p(360.0, 100.0),
            },
            style(4, 2, false),
        ),
        (
            ShapeKind::Arrow {
                a: p(30.0, 200.0),
                b: p(180.0, 130.0),
            },
            style(1, 1, false),
        ),
        (
            ShapeKind::Line {
                a: p(200.0, 130.0),
                b: p(300.0, 200.0),
            },
            style(3, 0, false),
        ),
        (
            ShapeKind::Pen {
                points: (0..30)
                    .map(|i| {
                        let t = i as f32 / 29.0;
                        p(320.0 + t * 140.0, 160.0 + (t * 9.0).sin() * 25.0)
                    })
                    .collect(),
            },
            style(5, 1, false),
        ),
        (
            ShapeKind::Highlighter {
                points: vec![p(30.0, 240.0), p(120.0, 242.0), p(200.0, 238.0)],
            },
            style(2, 1, false),
        ),
        (
            ShapeKind::Text {
                origin: p(220.0, 225.0),
                text: "Hello, moment!\nSecond line".into(),
                font: FontSpec::default(),
            },
            style(7, 1, false),
        ),
        (
            ShapeKind::Text {
                origin: p(20.0, 268.0),
                text: "Light italic".into(),
                font: FontSpec {
                    weight: 300,
                    italic: true,
                    size: 22.0,
                    ..FontSpec::default()
                },
            },
            style(0, 1, false),
        ),
        (
            ShapeKind::Text {
                origin: p(250.0, 108.0),
                text: "BLACK 900".into(),
                font: FontSpec {
                    weight: 900,
                    size: 16.0,
                    ..FontSpec::default()
                },
            },
            style(7, 1, false),
        ),
        (
            ShapeKind::Step {
                center: p(400.0, 60.0),
                number: 1,
            },
            style(0, 1, false),
        ),
        (
            ShapeKind::Step {
                center: p(440.0, 60.0),
                number: 12,
            },
            style(2, 1, false),
        ),
        (
            ShapeKind::Pixelate {
                a: p(380.0, 220.0),
                b: p(470.0, 310.0),
            },
            style(0, 1, false),
        ),
    ];
    for (kind, style) in shapes {
        let s = doc.make_shape(kind, style);
        doc.add(s);
    }
}

pub fn document() -> Document {
    let mut doc = Document::new(Arc::new(base_image()), 1.0);
    populate(&mut doc);
    doc
}

pub fn cropped_document() -> Document {
    let mut doc = document();
    doc.set_crop(Some(RectF::new(100.0, 50.0, 300.0, 200.0)));
    doc
}

/// Per-pixel comparison: (pixels whose largest channel difference exceeds
/// `tolerance`, mean absolute channel difference).
pub fn diff(a: &Image, b: &Image, tolerance: u8) -> (usize, f64) {
    assert_eq!(
        (a.width, a.height),
        (b.width, b.height),
        "image sizes differ"
    );
    let mut over = 0;
    let mut total: u64 = 0;
    for (pa, pb) in a.data.chunks_exact(4).zip(b.data.chunks_exact(4)) {
        let d = pa
            .iter()
            .zip(pb)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap();
        total += pa
            .iter()
            .zip(pb)
            .map(|(x, y)| x.abs_diff(*y) as u64)
            .sum::<u64>();
        if d > tolerance {
            over += 1;
        }
    }
    (over, total as f64 / a.data.len() as f64)
}
