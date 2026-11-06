//! Flattens a document into pixels on the CPU (tiny-skia) for saving and
//! copying. Runs on a background thread from a [`RenderJob`] snapshot.

use std::time::Instant;

use tiny_skia::{FillRule, FilterQuality, Paint, Pixmap, PixmapMut, PixmapPaint, Transform};

use crate::document::{RenderJob, Rgba};
use crate::raster::Image;
use crate::render::Prim;

/// Renders shapes over the screenshot and applies the crop.
///
/// The pixmap is our BGRA buffer reinterpreted as tiny-skia's RGBA: colors
/// are passed with red and blue swapped, so no channel conversion pass is
/// needed in either direction.
pub fn render(job: &RenderJob) -> Image {
    let started = Instant::now();
    let base = &*job.base;
    let crop = job
        .crop
        .and_then(|c| c.to_pixel_rect(base.width, base.height))
        .unwrap_or(base.bounds());

    if job.shapes.is_empty() {
        return if crop == base.bounds() {
            base.clone()
        } else {
            base.crop(crop)
        };
    }

    // Only the cropped area is rendered: shapes are translated into it.
    let mut out = base.crop(crop);
    let opaque = out.is_opaque();
    if !opaque {
        premultiply(&mut out.data);
    }
    {
        let (w, h) = (out.width, out.height);
        let mut pixmap =
            PixmapMut::from_bytes(&mut out.data, w, h).expect("image size matches buffer");
        let tf = Transform::from_translate(-crop.x as f32, -crop.y as f32);
        for shape in &job.shapes {
            for prim in shape.prims(base).iter() {
                match prim {
                    Prim::Fill { path, color } => {
                        let mut paint = Paint::default();
                        paint.set_color(swapped(*color));
                        paint.anti_alias = true;
                        pixmap.fill_path(path, &paint, FillRule::Winding, tf, None);
                    }
                    Prim::Pixels { x, y, image } => {
                        blit_premultiplied(&mut pixmap, image, x - crop.x, y - crop.y);
                    }
                    Prim::Image { rect, image } => {
                        draw_image(&mut pixmap, *rect, image, tf);
                    }
                }
            }
        }
    }
    if !opaque {
        demultiply(&mut out.data);
    }
    log::debug!(
        "rendered {}x{} with {} shapes in {:.1}ms",
        out.width,
        out.height,
        job.shapes.len(),
        started.elapsed().as_secs_f64() * 1e3
    );
    out
}

fn swapped(c: Rgba) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(c.b, c.g, c.r, c.a)
}

fn blit_premultiplied(pixmap: &mut PixmapMut, src: &Image, dx: i32, dy: i32) {
    let (w, h) = (pixmap.width() as i32, pixmap.height() as i32);
    let data = pixmap.data_mut();
    for sy in 0..src.height as i32 {
        let y = dy + sy;
        if y < 0 || y >= h {
            continue;
        }
        for sx in 0..src.width as i32 {
            let x = dx + sx;
            if x < 0 || x >= w {
                continue;
            }
            let p = src.pixel(sx as u32, sy as u32);
            let a = p[3] as u32;
            let i = (y as usize * w as usize + x as usize) * 4;
            for ch in 0..3 {
                data[i + ch] = ((p[ch] as u32 * a + 127) / 255) as u8;
            }
            data[i + 3] = p[3];
        }
    }
}

/// Draws a straight-alpha bitmap scaled into `rect` (image coordinates).
fn draw_image(pixmap: &mut PixmapMut, rect: crate::geom::RectF, src: &Image, tf: Transform) {
    let mut data = src.data.clone();
    premultiply(&mut data);
    let Some(size) = tiny_skia::IntSize::from_wh(src.width, src.height) else {
        return;
    };
    let Some(img) = Pixmap::from_vec(data, size) else {
        return;
    };
    let paint = PixmapPaint {
        quality: FilterQuality::Bilinear,
        ..PixmapPaint::default()
    };
    let place = Transform::from_row(
        rect.w / src.width as f32,
        0.0,
        0.0,
        rect.h / src.height as f32,
        rect.x,
        rect.y,
    )
    .post_concat(tf);
    pixmap.draw_pixmap(0, 0, img.as_ref(), &paint, place, None);
}

fn premultiply(data: &mut [u8]) {
    for px in data.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
    }
}

fn demultiply(data: &mut [u8]) {
    for px in data.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::document::{Document, ShapeKind, Style};
    use crate::geom::{Pt, RectF};

    #[test]
    fn untouched_document_exports_the_original() {
        let base = Arc::new(Image::filled(8, 4, [1, 2, 3, 255]));
        let d = Document::new(base.clone(), 1.0);
        assert_eq!(render(&d.snapshot()), *base);
    }

    #[test]
    fn crop_without_shapes_is_a_plain_crop() {
        let base = Arc::new(Image::filled(8, 4, [1, 2, 3, 255]));
        let mut d = Document::new(base, 1.0);
        d.set_crop(Some(RectF::new(2.0, 1.0, 3.0, 2.0)));
        let out = render(&d.snapshot());
        assert_eq!((out.width, out.height), (3, 2));
    }

    #[test]
    fn filled_rect_paints_its_color_in_bgra_order() {
        let base = Arc::new(Image::filled(20, 20, [255, 255, 255, 255]));
        let mut d = Document::new(base, 1.0);
        let s = d.make_shape(
            ShapeKind::Rect {
                a: Pt::new(5.0, 5.0),
                b: Pt::new(15.0, 15.0),
            },
            Style {
                color: Rgba::rgb(0xff0000),
                size: 0,
                fill: true,
            },
        );
        d.add(s);
        let out = render(&d.snapshot());
        assert_eq!(out.pixel(10, 10), [0, 0, 255, 255], "pure red as BGRA");
        assert_eq!(out.pixel(1, 1), [255, 255, 255, 255]);
    }

    #[test]
    fn shapes_follow_the_crop_origin() {
        let base = Arc::new(Image::filled(40, 40, [0, 0, 0, 255]));
        let mut d = Document::new(base, 1.0);
        let s = d.make_shape(
            ShapeKind::Rect {
                a: Pt::new(20.0, 20.0),
                b: Pt::new(30.0, 30.0),
            },
            Style {
                color: Rgba::rgb(0x00ff00),
                size: 0,
                fill: true,
            },
        );
        d.add(s);
        d.set_crop(Some(RectF::new(20.0, 20.0, 20.0, 20.0)));
        let out = render(&d.snapshot());
        assert_eq!(out.pixel(5, 5), [0, 255, 0, 255]);
        assert_eq!(out.pixel(15, 15), [0, 0, 0, 255]);
    }

    #[test]
    fn transparent_gaps_survive_rendering() {
        let mut base = Image::filled(10, 10, [9, 9, 9, 255]);
        base.data[3] = 0; // pixel (0,0) transparent
        let mut d = Document::new(Arc::new(base), 1.0);
        let s = d.make_shape(
            ShapeKind::Line {
                a: Pt::new(5.0, 5.0),
                b: Pt::new(9.0, 9.0),
            },
            Style::default(),
        );
        d.add(s);
        let out = render(&d.snapshot());
        assert_eq!(out.pixel(0, 0)[3], 0);
        assert_eq!(out.pixel(9, 0), [9, 9, 9, 255]);
    }
}
