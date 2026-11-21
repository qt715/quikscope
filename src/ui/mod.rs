//! GPUI front end: the region selection overlay and the annotation editor.

pub mod clips;
pub mod color_picker;
pub mod color_window;
pub mod edit_buffer;
pub mod editor;
mod export_job;
pub mod eyedropper;
pub mod field;
pub mod font_menu;
pub mod jobs;
pub mod motion;
pub mod overlay;
mod playback;
pub mod save_as;
pub mod settings;
pub mod toolbar;
pub mod video_editor;
pub mod zoom;

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
    pub fn accent_hover() -> Rgba {
        rgb(0xa3a0f5)
    }
    /// Text and icons on an [`accent`] fill.
    pub fn on_accent() -> Rgba {
        rgb(0x17162b)
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

/// GPUI 0.3.10 consumes this on X11; Wayland uses the desktop entry's icon.
#[cfg(target_os = "linux")]
pub fn window_icon() -> Arc<image::RgbaImage> {
    static ICON: std::sync::LazyLock<Arc<image::RgbaImage>> = std::sync::LazyLock::new(|| {
        Arc::new(
            image::load_from_memory_with_format(
                include_bytes!("../../assets/branding/png/256x256.png"),
                image::ImageFormat::Png,
            )
            .expect("bundled application icon is a valid PNG")
            .into_rgba8(),
        )
    });
    Arc::clone(&ICON)
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
            "folder-down",
            "copy",
            "scan-text",
            "paint-bucket",
            "palette",
            "settings",
            "italic",
            "chevron-down",
            "plus",
            "pipette",
            "play",
            "pause",
            "volume-2",
            "volume-x",
            "image-plus",
            "x",
            "check",
        );
        Ok(bytes.map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

/// GPUI family of the bundled Inter. Inter 4's variable font names its family
/// "Inter Variable"; GPUI matches family names exactly, so "Inter" would miss
/// the bundled face and fall back to system fonts (or panic without any).
pub const UI_FONT: &str = "Inter Variable";

/// GPUI family that renders the annotation font `family`: the annotation
/// renderer calls the bundled face "Inter", GPUI knows it as [`UI_FONT`].
pub fn ui_family(family: &str) -> SharedString {
    if family == crate::fonts::DEFAULT_FAMILY {
        UI_FONT.into()
    } else {
        SharedString::from(family.to_owned())
    }
}

/// Registers the embedded font for UI labels.
pub fn init(cx: &mut App) {
    if let Err(e) = cx
        .text_system()
        .add_fonts(vec![Cow::Borrowed(crate::fonts::INTER)])
    {
        log::warn!("could not register the UI font: {e:#}");
    }
}

/// Closes `window` the way its own close commands should: removed from a
/// task rather than during the current update, and on Windows hidden first,
/// with the deactivation and pointer-leave notifications that hiding causes
/// handled before the removal. Removing the active window directly (or
/// letting Windows destroy it) delivers those after GPUI forgot the window,
/// which logs "window not found".
pub fn close_window(window: &Window, cx: &mut App) {
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        #[cfg(windows)]
        if let Err(error) = crate::platform::windows::hide_window(handle, cx).await {
            // Already gone (closed twice): nothing to hide or remove.
            log::debug!("hiding a window before closing it: {error:#}");
        }
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    })
    .detach();
}

/// Makes the title bar's close button (and Alt+F4) close `window` through
/// [`close_window`]. Only Windows needs it: there the native close destroys
/// the window before GPUI handles its deactivation.
pub fn close_natively_through_task(window: &Window, cx: &App) {
    #[cfg(windows)]
    window.on_window_should_close(cx, |window, cx| {
        close_window(window, cx);
        false
    });
    #[cfg(not(windows))]
    let _ = (window, cx);
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
    /// The image annotated; for an animation its first frame, which fixes
    /// the annotation coordinate space for every frame.
    pub image: Arc<Image>,
    /// Physical pixels per logical pixel.
    pub scale: f32,
    pub taken: SystemTime,
    /// Set when the shot was opened from a file rather than captured.
    pub source: Option<PathBuf>,
    /// Set for animations (opened GIF/WebP/AVIF/JXL files and animation
    /// recordings): the annotator plays it and re-encodes every frame.
    pub animation: Option<Arc<Animation>>,
    tiles: OnceLock<Arc<[Tile]>>,
}

/// The frames behind an animated shot.
#[derive(Clone, Debug)]
pub struct Animation {
    /// What frames are decoded from: the recording's lossless master, or
    /// the opened file itself.
    pub path: PathBuf,
    pub info: crate::media::MediaInfo,
    /// Format saves and copies are written in.
    pub format: crate::media::OutputFormat,
}

impl Shot {
    pub fn new(id: u64, image: Arc<Image>, scale: f32, source: Option<PathBuf>) -> Shot {
        Shot {
            id,
            image,
            scale,
            taken: SystemTime::now(),
            source,
            animation: None,
            tiles: OnceLock::new(),
        }
    }

    pub fn with_animation(mut self, animation: Animation) -> Shot {
        self.animation = Some(Arc::new(animation));
        self
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

/// GPU textures for pixelate patches and colour glyphs, keyed by the image
/// allocation.
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
            Prim::Image { rect, image } => {
                let tex = patches.get(image);
                let _ =
                    window.paint_image(clip, xf.rect_to_screen(*rect), (0.).into(), tex, 0, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn window_icon_preserves_transparency_and_reuses_decoded_pixels() {
        let icon = window_icon();
        assert!(Arc::ptr_eq(&icon, &window_icon()));
        assert_eq!(icon.dimensions(), (256, 256));
        assert_eq!(icon.get_pixel(0, 0).0, [0, 0, 0, 0]);
        assert_eq!(icon.get_pixel(128, 128).0, [0x8f, 0x8c, 0xf2, 255]);
        for (x, y) in [(32, 32), (224, 32), (32, 224), (224, 224)] {
            assert_eq!(icon.get_pixel(x, y).0, [0x8f, 0x8c, 0xf2, 255]);
        }
        assert_eq!(icon.get_pixel(128, 32).0, [0, 0, 0, 0]);
    }

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
        let used = crate::ui::toolbar::ICONS
            .iter()
            .chain(&crate::ui::video_editor::ICONS)
            .chain(&["check"]);
        for name in used {
            assert!(
                Assets.load(&format!("icons/{name}.svg")).unwrap().is_some(),
                "{name}"
            );
        }
        assert!(Assets.load("icons/nope.svg").unwrap().is_none());
    }

    /// GPUI selects faces by exact family name from this database; a name
    /// that misses the bundled font silently renders the UI in a system font
    /// and panics where none is installed.
    #[test]
    fn ui_font_names_a_family_of_the_bundled_font() {
        let mut db = fontdb::Database::new();
        db.load_font_data(crate::fonts::INTER.to_vec());
        let families: Vec<String> = db
            .faces()
            .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
            .collect();
        assert!(families.iter().any(|f| f == UI_FONT), "{families:?}");
        assert_eq!(ui_family(crate::fonts::DEFAULT_FAMILY), UI_FONT);
    }
}
