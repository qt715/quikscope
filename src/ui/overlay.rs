//! Frozen-screen region selection: one layer-shell surface per output shows
//! the snapshot just taken; the user drags a rectangle or clicks a window.
//!
//! All overlays share one [`Selection`] entity in global logical
//! coordinates. Wayland keeps delivering pointer motion to the surface that
//! saw the button press, even outside it, so a drag that starts on one
//! output can end on another.

use std::sync::Arc;

use futures::channel::oneshot;
use gpui::{
    App, Bounds, Context, CursorStyle, DispatchPhase, DisplayId, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement as _, Render, Styled as _, Window, canvas, div,
    fill, outline, point, px, size,
};

use super::{Tile, ViewTransform, paint_tiles, theme};
use crate::geom::{IRect, Pt, RectF};
use crate::platform::{WindowRect, window_at};

/// Below this drag distance (logical px) a press counts as a click.
const CLICK_SLOP: i32 = 4;

pub struct Selection {
    windows: Vec<WindowRect>,
    start: Option<(i32, i32)>,
    current: Option<IRect>,
    hover: Option<IRect>,
    /// Fractional logical coordinates preserve physical-pixel precision on HiDPI outputs.
    pointer: Option<Pt>,
    done: Option<oneshot::Sender<Option<IRect>>>,
}

impl Selection {
    pub fn new(windows: Vec<WindowRect>) -> (Selection, oneshot::Receiver<Option<IRect>>) {
        let (tx, rx) = oneshot::channel();
        (
            Selection {
                windows,
                start: None,
                current: None,
                hover: None,
                pointer: None,
                done: Some(tx),
            },
            rx,
        )
    }

    /// Windows to snap to; for overlays opened before the window list was
    /// known.
    pub fn set_windows(&mut self, windows: Vec<WindowRect>) {
        self.windows = windows;
    }

    pub fn finish(&mut self, result: Option<IRect>) {
        if let Some(tx) = self.done.take() {
            let _ = tx.send(result);
        }
    }

    fn press(&mut self, x: i32, y: i32) {
        self.start = Some((x, y));
        self.current = Some(IRect::new(x, y, 0, 0));
    }

    fn motion(&mut self, x: i32, y: i32) {
        match self.start {
            Some((sx, sy)) => self.current = Some(IRect::from_corners(sx, sy, x, y)),
            None => self.hover = window_at(&self.windows, x, y),
        }
    }

    /// Ends a press at (`x`, `y`). A click selects the window under the
    /// pointer, or the whole output when there is none.
    fn release(&mut self, x: i32, y: i32, output: IRect) {
        let Some((sx, sy)) = self.start.take() else {
            return;
        };
        let rect = if (x - sx).abs() < CLICK_SLOP && (y - sy).abs() < CLICK_SLOP {
            window_at(&self.windows, x, y).unwrap_or(output)
        } else {
            IRect::from_corners(sx, sy, x, y)
        };
        self.finish(Some(rect));
    }

    /// The rectangle to show as selected right now.
    fn highlight(&self) -> Option<IRect> {
        if self.start.is_some() {
            self.current
        } else {
            self.hover
        }
    }
}

/// Frozen candidates for the single fullscreen surface on desktops without layer-shell.
pub struct FullscreenFrame {
    pub name: String,
    pub display: Option<DisplayId>,
    pub logical: IRect,
    pub image_width: u32,
    pub tiles: Arc<[Tile]>,
}

pub struct Overlay {
    selection: Entity<Selection>,
    /// This output's logical rectangle.
    output: IRect,
    /// Physical pixels per logical pixel of this output.
    scale: f32,
    tiles: Arc<[Tile]>,
    image_width: u32,
    focus: FocusHandle,
    /// Set until the first frame, for latency logging.
    opened: Option<std::time::Instant>,
    /// Logged once, when the first pointer motion arrives: until then
    /// clicks may still land on whatever was under the overlay.
    pointer_seen: bool,
    fullscreen: Option<Vec<FullscreenFrame>>,
    display: Option<DisplayId>,
    /// False while an overlay opened ahead of the capture waits for its
    /// frozen frame ([`Overlay::pending`]); it then paints nothing and
    /// ignores input, so the live desktop shows through unchanged.
    has_frame: bool,
}

impl Overlay {
    pub fn new(
        selection: Entity<Selection>,
        output: IRect,
        image_width: u32,
        tiles: Arc<[Tile]>,
        window: &mut Window,
        cx: &mut Context<Overlay>,
    ) -> Overlay {
        cx.observe(&selection, |_, _, cx| cx.notify()).detach();
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Overlay {
            selection,
            scale: image_width as f32 / output.w.max(1) as f32,
            output,
            tiles,
            image_width,
            focus,
            opened: Some(std::time::Instant::now()),
            pointer_seen: false,
            fullscreen: None,
            display: None,
            has_frame: true,
        }
    }

    /// An overlay opened before its output's frame exists; see
    /// [`Overlay::set_frame`].
    pub fn pending(
        selection: Entity<Selection>,
        window: &mut Window,
        cx: &mut Context<Overlay>,
    ) -> Overlay {
        let mut overlay = Self::new(selection, IRect::default(), 1, Arc::from([]), window, cx);
        overlay.has_frame = false;
        overlay
    }

    /// Shows the frozen frame of the output covering `output` (global
    /// logical coordinates).
    pub fn set_frame(
        &mut self,
        output: IRect,
        image_width: u32,
        tiles: Arc<[Tile]>,
        cx: &mut Context<Self>,
    ) {
        self.output = output;
        self.image_width = image_width;
        self.scale = image_width as f32 / output.w.max(1) as f32;
        self.tiles = tiles;
        self.has_frame = true;
        cx.notify();
    }

    pub fn fullscreen(
        selection: Entity<Selection>,
        frames: Vec<FullscreenFrame>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let first = &frames[0];
        let mut overlay = Self::new(
            selection,
            first.logical,
            first.image_width,
            first.tiles.clone(),
            window,
            cx,
        );
        overlay.fullscreen = Some(frames);
        cx.observe_window_display(window, |overlay, window, cx| {
            overlay.sync_display(window, cx);
            cx.notify();
        })
        .detach();
        overlay
    }

    fn sync_display(&mut self, window: &Window, cx: &App) {
        let Some(frames) = &self.fullscreen else {
            return;
        };
        let Some(display) = window.display(cx) else {
            return;
        };
        let id = display.id();
        if self.display == Some(id) {
            return;
        }
        let Some(frame) = frames.iter().find(|frame| frame.display == Some(id)) else {
            return;
        };
        self.display = Some(id);
        self.output = frame.logical;
        self.image_width = frame.image_width;
        self.scale = frame.image_width as f32 / frame.logical.w.max(1) as f32;
        self.tiles = frame.tiles.clone();
        log::debug!(
            "fullscreen selection mapped to {} image-width={} viewport={} ({})",
            frame.name,
            frame.image_width,
            f32::from(window.viewport_size().width),
            frame.logical
        );
    }

    fn global(&self, p: gpui::Point<gpui::Pixels>, window: &Window) -> (i32, i32) {
        let factor = self.output.w as f32 / f32::from(window.viewport_size().width).max(1.0);
        let x = self.output.x + (f32::from(p.x) * factor).round() as i32;
        let y = self.output.y + (f32::from(p.y) * factor).round() as i32;
        if self.fullscreen.is_some() {
            (
                x.clamp(self.output.x, self.output.right()),
                y.clamp(self.output.y, self.output.bottom()),
            )
        } else {
            (x, y)
        }
    }

    fn pointer(&self, p: gpui::Point<gpui::Pixels>, window: &Window) -> Pt {
        let factor = self.output.w as f32 / f32::from(window.viewport_size().width).max(1.);
        Pt::new(
            self.output.x as f32 + f32::from(p.x) * factor,
            self.output.y as f32 + f32::from(p.y) * factor,
        )
    }

    /// Whether the frozen frame is on screen and input means something.
    fn ready(&self) -> bool {
        self.has_frame && (self.fullscreen.is_none() || self.display.is_some())
    }

    fn mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ready() {
            return;
        }
        let (x, y) = self.global(ev.position, window);
        let pointer = self.pointer(ev.position, window);
        self.selection.update(cx, |s, cx| {
            s.pointer = Some(pointer);
            s.press(x, y);
            cx.notify();
        });
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.selection.update(cx, |s, _| s.finish(None));
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "escape" | "q" => self.cancel(cx),
            // Enter takes the whole output under the keyboard focus.
            "enter" if self.ready() => {
                let out = self.output;
                self.selection.update(cx, |s, _| s.finish(Some(out)));
            }
            _ => {}
        }
    }
}

impl Focusable for Overlay {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Overlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let frame_started = log::log_enabled!(log::Level::Debug).then(std::time::Instant::now);
        self.sync_display(window, cx);
        // X11 and Windows capture coordinates can be physical while the UI uses DIPs.
        let factor = self.output.w as f32 / f32::from(window.viewport_size().width).max(1.0);
        let this = cx.entity();
        let selection = self.selection.clone();
        let output = self.output;
        let tiles = self.tiles.clone();
        let image_width = self.image_width;
        // A fullscreen surface can land on any output; never flash the first
        // candidate's frozen frame before Wayland tells us where it mapped.
        let ready = self.ready();
        let state = self.selection.read(cx);
        let hint_visible = ready && state.start.is_none();
        let pointer = state.pointer;
        let scale = self.scale;
        let highlight = state.highlight();
        let image_height = tiles.iter().map(|t| t.y + t.h).max().unwrap_or(0);
        let precision = pointer
            .filter(|p| {
                ready
                    && p.x >= output.x as f32
                    && p.y >= output.y as f32
                    && p.x < output.right() as f32
                    && p.y < output.bottom() as f32
            })
            .and_then(|pointer| {
                let pixel = sample_coordinates(pointer, output, scale, image_width, image_height)?;
                let color = sample_color(&tiles, pixel.0, pixel.1)?;
                let pointer = Pt::new(
                    (pointer.x - output.x as f32) / factor,
                    (pointer.y - output.y as f32) / factor,
                );
                Some(Precision {
                    pointer,
                    pixel,
                    color,
                    loupe: loupe_placement(
                        pointer,
                        f32::from(window.viewport_size().width),
                        f32::from(window.viewport_size().height),
                    ),
                })
            });
        let loupe_label = precision.map(|p| {
            div()
                .absolute()
                .left(px(p.loupe.x + 8.))
                .top(px(p.loupe.y + 132.))
                .text_xs()
                .text_color(theme::text())
                .child(format!(
                    "{}, {} px · #{:02x}{:02x}{:02x}",
                    p.pixel.0, p.pixel.1, p.color[0], p.color[1], p.color[2]
                ))
        });
        let hint =
            hint_visible.then(|| {
                div().absolute().bottom_4().left_0().right_0()
            .flex().justify_center().child(div().px_3().py_1p5().rounded_md()
                .bg(theme::panel().opacity(0.88)).text_xs().text_color(theme::text())
                .child("Drag to select · Click a window · Enter: whole output · Esc: cancel"))
            });
        if let Some(opened) = self.opened.take() {
            log::debug!(
                "overlay first frame {:.1}ms after creation",
                opened.elapsed().as_secs_f64() * 1e3
            );
        }

        let label = ready
            .then_some(highlight)
            .flatten()
            .and_then(|h| h.intersect(&output).map(|_| h))
            .filter(|h| h.w > 0 && h.h > 0)
            .map(|h| {
                let (lx, ly) = (
                    (h.x - output.x) as f32 / factor,
                    (h.bottom() - output.y) as f32 / factor + 8.0,
                );
                // Keep the label on screen when the selection hugs the bottom edge.
                let ly = if ly + 28.0 > output.h as f32 / factor {
                    (h.y - output.y) as f32 / factor - 30.0
                } else {
                    ly
                };
                div()
                    .absolute()
                    .left(px(lx.max(4.0)))
                    .top(px(ly.max(4.0)))
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(theme::panel())
                    .text_color(theme::text())
                    .text_xs()
                    .child(format!(
                        "{} × {}",
                        (h.w as f32 * scale).round(),
                        (h.h as f32 * scale).round()
                    ))
            });

        div()
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .cursor(CursorStyle::Crosshair)
            .font_family(super::UI_FONT)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _: &MouseDownEvent, _, cx| this.cancel(cx)),
            )
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds: Bounds<gpui::Pixels>, _, window, _cx| {
                        let xf = ViewTransform {
                            origin: bounds.origin,
                            zoom: f32::from(bounds.size.width) / image_width.max(1) as f32,
                        };
                        if ready {
                            paint_tiles(window, &tiles, xf, bounds);
                        }
                        // Dimming before the frame exists would darken the
                        // live desktop, and the capture still being taken.
                        if ready {
                            paint_dim(window, bounds, highlight.map(|h| local(h, output, factor)));
                        }
                        if let Some(precision) = precision {
                            paint_precision(
                                window,
                                bounds,
                                &tiles,
                                image_width,
                                image_height,
                                precision,
                            );
                        }

                        let sel = selection.clone();
                        let overlay = this.clone();
                        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
                            if phase != DispatchPhase::Bubble {
                                return;
                            }
                            let Some((x, y, pointer)) = overlay.update(cx, |o, _| {
                                if !o.ready() {
                                    return None;
                                }
                                if !std::mem::replace(&mut o.pointer_seen, true) {
                                    log::debug!("overlay has the pointer");
                                }
                                let (x, y) = o.global(ev.position, window);
                                Some((x, y, o.pointer(ev.position, window)))
                            }) else {
                                return;
                            };
                            sel.update(cx, |s, cx| {
                                s.pointer = Some(pointer);
                                s.motion(x, y);
                                cx.notify();
                            });
                        });
                        let sel = selection.clone();
                        let overlay = this.clone();
                        window.on_mouse_event(move |ev: &MouseUpEvent, phase, window, cx| {
                            if phase != DispatchPhase::Bubble || ev.button != MouseButton::Left {
                                return;
                            }
                            let o = overlay.read(cx);
                            if !o.ready() {
                                return;
                            }
                            let (x, y) = o.global(ev.position, window);
                            sel.update(cx, |s, cx| {
                                s.release(x, y, output);
                                cx.notify();
                            });
                        });
                    },
                )
                .size_full(),
            )
            .children(label)
            .children(hint)
            .children(loupe_label)
            .children(
                frame_started
                    .filter(|_| ready && precision.is_some())
                    .map(|started| {
                        canvas(
                            |_, _, _| {},
                            move |_, _, _, _| {
                                log::debug!(
                                    "overlay frame CPU {:.3}ms",
                                    started.elapsed().as_secs_f64() * 1e3
                                );
                            },
                        )
                        .absolute()
                        .size_full()
                    }),
            )
    }
}

const LOUPE_PIXELS: i32 = 15;
const LOUPE_ZOOM: f32 = 8.;

#[derive(Clone, Copy)]
struct Precision {
    pointer: Pt,
    pixel: (u32, u32),
    color: [u8; 3],
    loupe: RectF,
}

/// Flip independently on each axis, then clamp for unusually small outputs.
fn loupe_placement(pointer: Pt, width: f32, height: f32) -> RectF {
    let w = 184.;
    let h = 158.;
    let place = |p: f32, extent: f32, length: f32| {
        let near = p + 24.;
        let candidate = if near + length + 8. <= extent {
            near
        } else {
            p - 24. - length
        };
        candidate.max(8.).min((extent - length - 8.).max(0.))
    };
    RectF::new(
        place(pointer.x, width, w),
        place(pointer.y, height, h),
        w.min(width),
        h.min(height),
    )
}

fn sample_coordinates(
    pointer: Pt,
    output: IRect,
    scale: f32,
    width: u32,
    height: u32,
) -> Option<(u32, u32)> {
    if width == 0 || height == 0 {
        return None;
    }
    Some((
        ((pointer.x - output.x as f32) * scale)
            .floor()
            .clamp(0., (width - 1) as f32) as u32,
        ((pointer.y - output.y as f32) * scale)
            .floor()
            .clamp(0., (height - 1) as f32) as u32,
    ))
}

fn sample_color(tiles: &[Tile], x: u32, y: u32) -> Option<[u8; 3]> {
    let tile = tiles
        .iter()
        .find(|t| x >= t.x && y >= t.y && x - t.x < t.w && y - t.y < t.h)?;
    let offset = (((y - tile.y) * tile.w + x - tile.x) * 4) as usize;
    let bgra = tile.image.as_bytes(0)?.get(offset..offset + 4)?;
    Some([bgra[2], bgra[1], bgra[0]])
}

fn paint_precision(
    window: &mut Window,
    bounds: Bounds<gpui::Pixels>,
    tiles: &[Tile],
    width: u32,
    height: u32,
    p: Precision,
) {
    let rect = |x, y, w, h| Bounds::new(bounds.origin + point(px(x), px(y)), size(px(w), px(h)));
    let x = p.pointer.x;
    let y = p.pointer.y;
    for (line, shadow) in [
        (
            rect(0., y, f32::from(bounds.size.width), 1.),
            rect(0., y - 1., f32::from(bounds.size.width), 3.),
        ),
        (
            rect(x, 0., 1., f32::from(bounds.size.height)),
            rect(x - 1., 0., 3., f32::from(bounds.size.height)),
        ),
    ] {
        window.paint_quad(fill(shadow, gpui::black().opacity(0.4)));
        window.paint_quad(fill(line, gpui::white().opacity(0.65)));
    }
    let loupe = rect(p.loupe.x, p.loupe.y, p.loupe.w, p.loupe.h);
    window.paint_quad(fill(loupe, theme::panel()));
    window.paint_quad(outline(
        loupe,
        theme::panel_border(),
        gpui::BorderStyle::Solid,
    ));
    let left = p.loupe.x + (p.loupe.w - LOUPE_PIXELS as f32 * LOUPE_ZOOM) / 2.;
    let top = p.loupe.y + 6.;
    // Frozen tile bytes are retained by RenderImage. Painting constant-colour
    // cells avoids resampling, image allocation and texture uploads on motion.
    for row in 0..LOUPE_PIXELS {
        for col in 0..LOUPE_PIXELS {
            let sx = (p.pixel.0 as i32 + col - LOUPE_PIXELS / 2).clamp(0, width as i32 - 1) as u32;
            let sy = (p.pixel.1 as i32 + row - LOUPE_PIXELS / 2).clamp(0, height as i32 - 1) as u32;
            if let Some([r, g, b]) = sample_color(tiles, sx, sy) {
                window.paint_quad(fill(
                    rect(
                        left + col as f32 * LOUPE_ZOOM,
                        top + row as f32 * LOUPE_ZOOM,
                        LOUPE_ZOOM,
                        LOUPE_ZOOM,
                    ),
                    gpui::rgb((r as u32) << 16 | (g as u32) << 8 | b as u32),
                ));
            }
        }
    }
    let center = rect(
        left + 7. * LOUPE_ZOOM,
        top + 7. * LOUPE_ZOOM,
        LOUPE_ZOOM,
        LOUPE_ZOOM,
    );
    window.paint_quad(outline(center, gpui::black(), gpui::BorderStyle::Solid));
    window.paint_quad(outline(
        Bounds::new(
            center.origin - point(px(1.), px(1.)),
            center.size + size(px(2.), px(2.)),
        ),
        gpui::white(),
        gpui::BorderStyle::Solid,
    ));
}

/// `r` (global logical) relative to this output's surface.
fn local(r: IRect, output: IRect, factor: f32) -> Bounds<gpui::Pixels> {
    Bounds {
        origin: point(
            px((r.x - output.x) as f32 / factor),
            px((r.y - output.y) as f32 / factor),
        ),
        size: size(px(r.w as f32 / factor), px(r.h as f32 / factor)),
    }
}

/// Darkens everything except `hole`, which gets an accent outline.
fn paint_dim(
    window: &mut Window,
    bounds: Bounds<gpui::Pixels>,
    hole: Option<Bounds<gpui::Pixels>>,
) {
    let dim = gpui::rgba(0x0000_0070);
    let Some(hole) = hole.map(|h| Bounds {
        origin: h.origin + bounds.origin,
        size: h.size,
    }) else {
        window.paint_quad(fill(bounds, dim));
        return;
    };
    let (l, t, r, b) = (bounds.left(), bounds.top(), bounds.right(), bounds.bottom());
    let (hl, ht) = (hole.left().max(l).min(r), hole.top().max(t).min(b));
    let (hr, hb) = (hole.right().max(l).min(r), hole.bottom().max(t).min(b));
    for rect in [
        Bounds::from_corners(point(l, t), point(r, ht)),
        Bounds::from_corners(point(l, hb), point(r, b)),
        Bounds::from_corners(point(l, ht), point(hl, hb)),
        Bounds::from_corners(point(hr, ht), point(r, hb)),
    ] {
        if rect.size.width > px(0.) && rect.size.height > px(0.) {
            window.paint_quad(fill(rect, dim));
        }
    }
    let mut border = outline(hole, theme::accent(), gpui::BorderStyle::Solid);
    border.border_widths = px(2.).into();
    window.paint_quad(border);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loupe_flips_at_each_edge_without_leaving_the_viewport() {
        for (width, height) in [(2560., 1440.), (480., 320.)] {
            for pointer in [
                Pt::new(0., 0.),
                Pt::new(width - 1., 0.),
                Pt::new(0., height - 1.),
                Pt::new(width - 1., height - 1.),
            ] {
                let rect = loupe_placement(pointer, width, height);
                assert!(rect.x >= 8. && rect.y >= 8.);
                assert!(rect.right() <= width - 8. && rect.bottom() <= height - 8.);
                if pointer.x == 0. {
                    assert!(rect.x > pointer.x);
                } else {
                    assert!(rect.right() < pointer.x);
                }
                if pointer.y == 0. {
                    assert!(rect.y > pointer.y);
                } else {
                    assert!(rect.bottom() < pointer.y);
                }
            }
        }
    }

    #[test]
    fn sampling_keeps_fractional_hidpi_positions_and_clamps_edges() {
        let output = IRect::new(-100, 50, 100, 80);
        assert_eq!(
            sample_coordinates(Pt::new(-99.25, 50.75), output, 2., 200, 160),
            Some((1, 1))
        );
        assert_eq!(
            sample_coordinates(Pt::new(0., 130.), output, 2., 200, 160),
            Some((199, 159))
        );
        assert_eq!(
            sample_coordinates(Pt::new(-101., 49.), output, 2., 200, 160),
            Some((0, 0))
        );
        assert_eq!(
            sample_coordinates(Pt::new(-99., 51.), output, 1., 100, 80),
            Some((1, 1))
        );
        assert_eq!(sample_coordinates(Pt::default(), output, 1., 0, 80), None);
    }

    #[test]
    fn sampling_reads_bgra_across_frozen_tile_boundaries() {
        let tiles = [
            Tile {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
                image: super::super::render_image(1, 1, vec![0x56, 0x34, 0x12, 255]),
            },
            Tile {
                x: 1,
                y: 0,
                w: 1,
                h: 1,
                image: super::super::render_image(1, 1, vec![0xef, 0xcd, 0xab, 255]),
            },
        ];
        assert_eq!(sample_color(&tiles, 0, 0), Some([0x12, 0x34, 0x56]));
        assert_eq!(sample_color(&tiles, 1, 0), Some([0xab, 0xcd, 0xef]));
        assert_eq!(sample_color(&tiles, 2, 0), None);
    }
    #[test]
    fn physical_selection_hole_maps_to_scaled_viewport() {
        let bounds = local(
            IRect::new(120, 90, 400, 200),
            IRect::new(100, 50, 1920, 1080),
            2.0,
        );
        assert_eq!(bounds.origin, point(px(10.0), px(20.0)));
        assert_eq!(bounds.size, size(px(200.0), px(100.0)));
    }

    fn win(x: i32, y: i32, w: i32, h: i32) -> WindowRect {
        WindowRect {
            rect: IRect::new(x, y, w, h),
            floating: false,
            focus_order: 0,
        }
    }

    #[test]
    fn drag_selects_normalized_rectangle() {
        let (mut s, mut rx) = Selection::new(vec![]);
        s.press(100, 100);
        s.motion(40, 160);
        assert_eq!(s.highlight(), Some(IRect::new(40, 100, 60, 60)));
        s.release(40, 160, IRect::new(0, 0, 1920, 1080));
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(Some(IRect::new(40, 100, 60, 60)))
        );
    }

    #[test]
    fn click_selects_window_under_pointer_or_output() {
        let output = IRect::new(0, 0, 1920, 1080);
        let (mut s, mut rx) = Selection::new(vec![win(10, 10, 300, 200)]);
        s.motion(50, 50);
        assert_eq!(s.highlight(), Some(IRect::new(10, 10, 300, 200)));
        s.press(50, 50);
        s.release(52, 51, output);
        assert_eq!(
            rx.try_recv().unwrap(),
            Some(Some(IRect::new(10, 10, 300, 200)))
        );

        let (mut s, mut rx) = Selection::new(vec![win(10, 10, 300, 200)]);
        s.press(900, 900);
        s.release(900, 900, output);
        assert_eq!(rx.try_recv().unwrap(), Some(Some(output)));
    }

    #[test]
    fn selection_resolves_only_once() {
        let (mut s, mut rx) = Selection::new(vec![]);
        s.finish(None);
        s.finish(Some(IRect::new(0, 0, 5, 5)));
        assert_eq!(rx.try_recv().unwrap(), Some(None));
    }

    #[test]
    fn release_without_press_is_ignored() {
        let (mut s, mut rx) = Selection::new(vec![]);
        s.release(5, 5, IRect::new(0, 0, 10, 10));
        assert_eq!(rx.try_recv().unwrap(), None);
    }
}
