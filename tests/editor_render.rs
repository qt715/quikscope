//! The editor (GPU, GPUI) and the exporter (CPU, tiny-skia) must draw the
//! same picture: what the user sees is what Ctrl+S writes. This renders the
//! real `Editor` view offscreen with GPUI's headless WGPU renderer and
//! compares the image area pixel by pixel with the exported PNG.
//!
//! Needs a Vulkan or GL implementation (Mesa's lavapipe/llvmpipe in CI).

#[path = "support/scene.rs"]
mod scene;

use std::sync::Arc;

use gpui::{AppContext as _, HeadlessAppContext, px, size};
use moment::document::Document;
use moment::raster::Image;
use moment::ui::editor::Editor;
use moment::ui::{Assets, Shot};

fn context() -> HeadlessAppContext {
    let text = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text,
        Arc::new(Assets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(moment::ui::init);
    cx
}

struct Rendered {
    screenshot: image::RgbaImage,
    /// Image origin in device pixels.
    origin: (f32, f32),
    /// Device pixels per image pixel.
    zoom: f32,
    /// Device pixels per logical pixel.
    factor: f32,
}

/// Renders `doc` in an editor window of `w`×`h` logical pixels.
fn render_editor(doc: Document, w: f32, h: f32) -> Rendered {
    let mut cx = context();
    let base = doc.base.clone();
    let scale = doc.scale;
    let doc = cx.update(|cx| cx.new(|_| doc));
    let shot = Arc::new(Shot::new(1, base, scale, None));
    let window = cx
        .open_window(size(px(w), px(h)), |window, cx| {
            cx.new(|cx| Editor::new(shot, doc, window, cx))
        })
        .unwrap();
    cx.run_until_parked();
    // The first frame lays out the canvas and fits the image; the second
    // paints with the fitted transform.
    cx.update_window(window.into(), |_, window, _| window.refresh())
        .unwrap();
    cx.run_until_parked();
    let screenshot = cx.capture_screenshot(window.into()).unwrap();
    let (origin, zoom, factor) = cx
        .update_window(window.into(), |view, window, cx| {
            let editor = view.downcast::<Editor>().unwrap();
            let xf = editor.read(cx).transform();
            (xf.origin, xf.zoom, window.scale_factor())
        })
        .unwrap();
    Rendered {
        screenshot,
        origin: (f32::from(origin.x) * factor, f32::from(origin.y) * factor),
        zoom: zoom * factor,
        factor,
    }
}

#[test]
fn editor_canvas_matches_the_exported_png() {
    let doc = scene::document();
    let exported = moment::export::render(&doc.snapshot());
    let Rendered {
        screenshot: shot,
        origin: (ox, oy),
        zoom,
        ..
    } = render_editor(doc, 1000.0, 800.0);
    assert!(
        (zoom - 1.0).abs() < 1e-3,
        "a small image is shown at one image pixel per device pixel, got {zoom}"
    );
    assert_eq!(ox.fract(), 0.0, "image origin is pixel-aligned");
    assert_eq!(oy.fract(), 0.0, "image origin is pixel-aligned");

    let (ox, oy) = (ox as u32, oy as u32);
    let mut on_screen = Image::new(scene::W, scene::H);
    for y in 0..scene::H {
        for x in 0..scene::W {
            let p = shot.get_pixel(ox + x, oy + y).0;
            let i = ((y * scene::W + x) * 4) as usize;
            on_screen.data[i..i + 4].copy_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }

    // Anti-aliasing differs (4x MSAA vs analytic coverage), so edge pixels
    // may disagree; misplaced or missing geometry would move whole strokes.
    let (over, mean) = scene::diff(&on_screen, &exported, 48);
    let budget = (scene::W * scene::H / 50) as usize;
    if over > budget || mean > 3.0 {
        let dir = std::env::temp_dir();
        on_screen
            .save_png(
                &dir.join("moment-parity-gpu.png"),
                moment::raster::Compression::Fast,
            )
            .unwrap();
        exported
            .save_png(
                &dir.join("moment-parity-cpu.png"),
                moment::raster::Compression::Fast,
            )
            .unwrap();
        panic!(
            "editor and export differ: {over} pixels over tolerance (budget {budget}), mean {mean:.2}; see {}",
            dir.display()
        );
    }
}

#[test]
fn large_images_are_scaled_to_fit_the_window() {
    let doc = Document::new(Arc::new(Image::filled(4000, 3000, [0, 0, 0, 255])), 1.0);
    let r = render_editor(doc, 800.0, 600.0);
    assert!(
        r.origin.0 >= 0.0 && r.origin.1 >= 0.0,
        "image is inside the window"
    );
    // The window is 800x600 logical pixels; compare in device pixels.
    let (w, h) = (800.0 * r.factor, 600.0 * r.factor);
    assert!(
        4000.0 * r.zoom <= w && 3000.0 * r.zoom <= h,
        "zoom {}",
        r.zoom
    );
    assert!(4000.0 * r.zoom > w / 2.0, "uses the available width");
}
