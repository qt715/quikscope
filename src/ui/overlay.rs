//! Region selection: live transparent overlays or the snapshot just taken;
//! the user drags a rectangle or clicks a window.
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
use crate::geom::IRect;
use crate::platform::{WindowRect, window_at};

/// Below this drag distance (logical px) a press counts as a click.
const CLICK_SLOP: i32 = 4;

pub struct Selection {
    windows: Vec<WindowRect>,
    start: Option<(i32, i32)>,
    current: Option<IRect>,
    hover: Option<IRect>,
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

/// Per-output metadata and optional frozen tiles for a single fullscreen surface.
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
    /// Pending early overlays must not dim the desktop being captured or
    /// accept input until their output metadata and frozen tiles arrive.
    active: bool,
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
            active: true,
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
        overlay.active = false;
        overlay
    }

    /// Reuse the pending transparent surface, activating it with geometry
    /// only. With no frozen tiles the desktop remains visible beneath the dim.
    pub fn live(
        selection: Entity<Selection>,
        output: IRect,
        image_width: u32,
        window: &mut Window,
        cx: &mut Context<Overlay>,
    ) -> Overlay {
        let mut overlay = Self::pending(selection, window, cx);
        overlay.set_frame(output, image_width, Arc::from([]), cx);
        overlay
    }

    /// Activates this output using global logical geometry and frozen tiles
    /// (or an empty tile list for live selection).
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
        self.active = true;
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

    /// Input is meaningful once output metadata (and any frozen tiles) is ready.
    fn ready(&self) -> bool {
        self.active && (self.fullscreen.is_none() || self.display.is_some())
    }

    fn mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ready() {
            return;
        }
        let (x, y) = self.global(ev.position, window);
        self.selection.update(cx, |s, cx| {
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
        let active = self.active;
        let scale = self.scale;
        let highlight = self.selection.read(cx).highlight();
        if let Some(opened) = self.opened.take() {
            log::debug!(
                "overlay first frame {:.1}ms after creation",
                opened.elapsed().as_secs_f64() * 1e3
            );
        }

        let label = highlight
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
                        // Pending capture overlays paint nothing; activated live
                        // overlays dim without covering the desktop with tiles.
                        if active {
                            paint_dim(window, bounds, highlight.map(|h| local(h, output, factor)));
                        }

                        let sel = selection.clone();
                        let overlay = this.clone();
                        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
                            if phase != DispatchPhase::Bubble {
                                return;
                            }
                            let Some((x, y)) = overlay.update(cx, |o, _| {
                                if !o.ready() {
                                    return None;
                                }
                                if !std::mem::replace(&mut o.pointer_seen, true) {
                                    log::debug!("overlay has the pointer");
                                }
                                Some(o.global(ev.position, window))
                            }) else {
                                return;
                            };
                            sel.update(cx, |s, cx| {
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
    }
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
