//! GPUI front end: the region selection overlay and the annotation editor.

pub mod editor;
pub mod motion;
pub mod overlay;
pub mod toolbar;

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use gpui::{
    App, AssetSource, Bounds, FillOptions, Hsla, PathBuilder, PathStyle, Pixels, Point,
    RenderImage, SharedString, Window, point, px, size,
};
use smallvec::SmallVec;

use crate::document::Rgba;
use crate::geom::Pt;
use crate::raster::Image;
use crate::render::Prim;

/// Largest square tile uploaded as one texture. Every GPU GPUI runs on
/// supports this; larger captures (multi-monitor 4K) are split.
pub const MAX_TILE: u32 = 4096;

pub mod theme {
    use gpui::{Hsla, Rgba, rgb, rgba};

    pub fn canvas_bg() -> Rgba {
        rgb(0x121214)
    }
    pub fn panel() -> Rgba {
        rgb(0x232329)
    }
    pub fn panel_border() -> Rgba {
        rgba(0xffffff14)
    }
    pub fn hover() -> Rgba {
        rgba(0xffffff12)
    }
    pub fn accent() -> Rgba {
        rgb(0x8f8cf2)
    }
    pub fn accent_bg() -> Rgba {
        rgba(0x8f8cf233)
    }
    pub fn text() -> Rgba {
        rgb(0xe3e3e8)
    }
    pub fn muted() -> Rgba {
        rgb(0x8b8b94)
    }
    pub fn selection() -> Hsla {
        rgb(0x8f8cf2).into()
    }
}

/// Embedded icons (Lucide) and the UI font.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        macro_rules! icons {
            ($($name:literal),* $(,)?) => {
                match path {
                    $(concat!("icons/", $name, ".svg") => {
                        Some(include_bytes!(concat!("../../assets/icons/", $name, ".svg")).as_slice())
                    })*
                    _ => None,
                }
            };
        }
        let bytes = icons!(
            "mouse-pointer-2",
            "hand",
            "pencil",
            "highlighter",
            "move-up-right",
            "minus",
            "square",
            "circle",
            "type",
            "list-ordered",
            "grid-3x3",
            "crop",
            "undo-2",
            "redo-2",
            "save",
            "copy",
            "paint-bucket",
        );
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

pub const UI_FONT: &str = "Inter";

/// Registers the embedded font for UI labels.
pub fn init(cx: &mut App) {
    if let Err(e) = cx
        .text_system()
        .add_fonts(vec![Cow::Borrowed(crate::text::FONT_DATA)])
    {
        log::warn!("could not register the UI font: {e:#}");
    }
}

/// One GPU texture's worth of a screenshot.
pub struct Tile {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub image: Arc<RenderImage>,
}

/// Splits `image` into GPU-sized tiles. A capture that fits one tile is
/// copied once; there is no per-frame upload after the first paint.
pub fn make_tiles(image: &Image) -> Arc<[Tile]> {
    let mut tiles = Vec::new();
    let mut y = 0;
    while y < image.height {
        let h = MAX_TILE.min(image.height - y);
        let mut x = 0;
        while x < image.width {
            let w = MAX_TILE.min(image.width - x);
            let data = if w == image.width && h == image.height {
                image.data.clone()
            } else {
                image
                    .crop(crate::geom::IRect::new(
                        x as i32, y as i32, w as i32, h as i32,
                    ))
                    .data
            };
            tiles.push(Tile {
                x,
                y,
                w,
                h,
                image: render_image(w, h, data),
            });
            x += w;
        }
        y += h;
    }
    tiles.into()
}

/// Wraps BGRA bytes as a GPUI image without conversion.
pub fn render_image(w: u32, h: u32, bgra: Vec<u8>) -> Arc<RenderImage> {
    let buffer = image::RgbaImage::from_raw(w, h, bgra).expect("buffer matches size");
    let frames: SmallVec<[image::Frame; 1]> = SmallVec::from_elem(image::Frame::new(buffer), 1);
    Arc::new(RenderImage::new(frames))
}

/// A capture held by the daemon.
pub struct Shot {
    pub id: u64,
    pub image: Arc<Image>,
    /// Physical pixels per logical pixel.
    pub scale: f32,
    pub taken: SystemTime,
    /// Set when the shot was opened from a file rather than captured.
    pub source: Option<PathBuf>,
    tiles: OnceLock<Arc<[Tile]>>,
}

impl Shot {
    pub fn new(id: u64, image: Arc<Image>, scale: f32, source: Option<PathBuf>) -> Shot {
        Shot {
            id,
            image,
            scale,
            taken: SystemTime::now(),
            source,
            tiles: OnceLock::new(),
        }
    }

    /// GPU tiles, built on first use. The daemon builds them on a background
    /// thread right after capture so the editor opens without this cost.
    pub fn tiles(&self) -> &Arc<[Tile]> {
        self.tiles.get_or_init(|| make_tiles(&self.image))
    }
}

/// Maps image pixels to window pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewTransform {
    /// Window position of image pixel (0, 0).
    pub origin: Point<Pixels>,
    /// Window pixels per image pixel.
    pub zoom: f32,
}

impl ViewTransform {
    pub fn to_screen(&self, p: Pt) -> Point<Pixels> {
        point(
            self.origin.x + px(p.x * self.zoom),
            self.origin.y + px(p.y * self.zoom),
        )
    }

    pub fn to_image(&self, p: Point<Pixels>) -> Pt {
        Pt::new(
            f32::from(p.x - self.origin.x) / self.zoom,
            f32::from(p.y - self.origin.y) / self.zoom,
        )
    }

    pub fn rect_to_screen(&self, r: crate::geom::RectF) -> Bounds<Pixels> {
        Bounds {
            origin: self.to_screen(Pt::new(r.x, r.y)),
            size: size(px(r.w * self.zoom), px(r.h * self.zoom)),
        }
    }
}

pub fn hsla(c: Rgba) -> Hsla {
    gpui::Rgba {
        r: c.r as f32 / 255.0,
        g: c.g as f32 / 255.0,
        b: c.b as f32 / 255.0,
        a: c.a as f32 / 255.0,
    }
    .into()
}

pub fn paint_tiles(window: &mut Window, tiles: &[Tile], xf: ViewTransform, clip: Bounds<Pixels>) {
    for t in tiles {
        let r = crate::geom::RectF::new(t.x as f32, t.y as f32, t.w as f32, t.h as f32);
        let dest = xf.rect_to_screen(r);
        if let Err(e) = window.paint_image(clip, dest, (0.).into(), t.image.clone(), 0, false) {
            log::error!("painting screenshot tile failed: {e:#}");
        }
    }
}

/// Converts an image-space outline into a window-space GPUI path.
pub fn gpui_path(path: &tiny_skia::Path, xf: ViewTransform) -> Option<gpui::Path<Pixels>> {
    use tiny_skia::PathSegment as S;
    let mut b = PathBuilder::fill().with_style(PathStyle::Fill(FillOptions::non_zero()));
    let p = |q: tiny_skia::Point| xf.to_screen(Pt::new(q.x, q.y));
    for seg in path.segments() {
        match seg {
            S::MoveTo(a) => b.move_to(p(a)),
            S::LineTo(a) => b.line_to(p(a)),
            S::QuadTo(c, a) => b.curve_to(p(a), p(c)),
            S::CubicTo(c1, c2, a) => b.cubic_bezier_to(p(a), p(c1), p(c2)),
            S::Close => b.close(),
        }
    }
    b.build().ok()
}

/// GPU textures for pixelate patches, keyed by the patch allocation.
#[derive(Default)]
pub struct PatchCache {
    images: HashMap<usize, Arc<RenderImage>>,
    used: Vec<usize>,
}

impl PatchCache {
    fn get(&mut self, image: &Arc<Image>) -> Arc<RenderImage> {
        let key = Arc::as_ptr(image) as usize;
        self.used.push(key);
        self.images
            .entry(key)
            .or_insert_with(|| render_image(image.width, image.height, image.data.clone()))
            .clone()
    }

    /// Releases textures of patches that were not painted this frame.
    pub fn end_frame(&mut self, window: &mut Window) {
        let used = std::mem::take(&mut self.used);
        self.images.retain(|k, img| {
            let keep = used.contains(k);
            if !keep {
                let _ = window.drop_image(img.clone());
            }
            keep
        });
    }
}

pub fn paint_prims(
    window: &mut Window,
    prims: &[Prim],
    xf: ViewTransform,
    clip: Bounds<Pixels>,
    patches: &mut PatchCache,
) {
    for prim in prims {
        match prim {
            Prim::Fill { path, color } => {
                if let Some(path) = gpui_path(path, xf) {
                    window.paint_path(path, hsla(*color));
                }
            }
            Prim::Pixels { x, y, image } => {
                let r = crate::geom::RectF::new(
                    *x as f32,
                    *y as f32,
                    image.width as f32,
                    image.height as f32,
                );
                let tex = patches.get(image);
                let _ = window.paint_image(clip, xf.rect_to_screen(r), (0.).into(), tex, 0, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_cover_the_image_exactly_once() {
        let img = Image::new(MAX_TILE + 10, 7);
        let tiles = make_tiles(&img);
        assert_eq!(tiles.len(), 2);
        assert_eq!((tiles[0].x, tiles[0].w), (0, MAX_TILE));
        assert_eq!((tiles[1].x, tiles[1].w), (MAX_TILE, 10));
        let area: u64 = tiles.iter().map(|t| t.w as u64 * t.h as u64).sum();
        assert_eq!(area, img.width as u64 * img.height as u64);
    }

    #[test]
    fn view_transform_round_trips() {
        let xf = ViewTransform {
            origin: point(px(30.), px(-12.)),
            zoom: 0.5,
        };
        let p = Pt::new(100.0, 40.0);
        let s = xf.to_screen(p);
        assert_eq!(s, point(px(80.), px(8.)));
        assert_eq!(xf.to_image(s), p);
    }

    #[test]
    fn assets_resolve_every_toolbar_icon() {
        for name in crate::ui::toolbar::ICONS {
            assert!(
                Assets.load(&format!("icons/{name}.svg")).unwrap().is_some(),
                "{name}"
            );
        }
        assert!(Assets.load("icons/nope.svg").unwrap().is_none());
    }
}
