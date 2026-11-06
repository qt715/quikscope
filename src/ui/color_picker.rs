//! Color picker popover: hue/saturation wheel, value and alpha sliders, and
//! hex / RGBA fields.

use std::sync::Arc;

use gpui::{
    AppContext as _, Bounds, Context, DispatchPhase, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    Pixels, Point, Render, RenderImage, Styled as _, Window, canvas, div, fill, linear_color_stop,
    linear_gradient, point, px, size,
};

use super::field::{Field, FieldEvent, FieldKind};
use super::{hsla, render_image, theme};
use crate::color::{Hsv, parse_hex, to_hex};
use crate::document::Rgba;
use crate::raster::Image;

/// Wheel diameter in logical pixels.
pub const WHEEL: f32 = 176.0;
pub const PANEL_WIDTH: f32 = 216.0;
const SLIDER_H: f32 = 12.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PickerEvent {
    /// `live` while a wheel or slider drag is in progress; the final
    /// `live: false` event ends one undoable change.
    Changed { color: Rgba, live: bool },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Part {
    Wheel,
    Value,
    Alpha,
}

pub struct ColorPicker {
    hsv: Hsv,
    alpha: u8,
    hex: Entity<Field>,
    channels: [Entity<Field>; 4],
    wheel: Option<(f32, Arc<RenderImage>)>,
    drag: Option<Part>,
    bounds: [Bounds<Pixels>; 3],
}

impl EventEmitter<PickerEvent> for ColorPicker {}

/// Hue (degrees, counter-clockwise from the right) and saturation (0 at the
/// centre, 1 at the rim) for a point relative to the wheel centre.
pub fn wheel_hs(dx: f32, dy: f32, radius: f32) -> (f32, f32) {
    let h = (-dy).atan2(dx).to_degrees().rem_euclid(360.0);
    let s = (dx.hypot(dy) / radius).min(1.0);
    (h, s)
}

/// The hue/saturation disc at full value, `diameter` pixels square, with an
/// anti-aliased rim and transparent corners.
pub fn wheel_image(diameter: u32) -> Image {
    let mut img = Image::new(diameter, diameter);
    let r = diameter as f32 / 2.0;
    for y in 0..diameter {
        for x in 0..diameter {
            let (dx, dy) = (x as f32 + 0.5 - r, y as f32 + 0.5 - r);
            let coverage = (r - dx.hypot(dy) + 0.5).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let (h, s) = wheel_hs(dx, dy, r);
            let [cr, cg, cb] = Hsv { h, s, v: 1.0 }.to_rgb();
            let i = ((y * diameter + x) * 4) as usize;
            img.data[i..i + 4].copy_from_slice(&[cb, cg, cr, (coverage * 255.0) as u8]);
        }
    }
    img
}

impl ColorPicker {
    pub fn new(color: Rgba, window: &mut Window, cx: &mut Context<Self>) -> ColorPicker {
        let hex = cx.new(|cx| Field::new(FieldKind::Hex, 0.0, window, cx));
        let byte = FieldKind::Number { min: 0, max: 255 };
        let channels = [0, 1, 2, 3].map(|_| cx.new(|cx| Field::new(byte, 40.0, window, cx)));
        cx.subscribe_in(
            &hex,
            window,
            |this, _, ev: &FieldEvent, window, cx| match ev {
                // A complete hex value applies while typing; anything else waits for Enter.
                FieldEvent::Changed(t) if matches!(t.trim_start_matches('#').len(), 6 | 8) => {
                    if let Some(c) = parse_hex(t) {
                        this.apply(c, false, window, cx);
                    }
                }
                FieldEvent::Submit(t) => match parse_hex(t) {
                    Some(c) => this.apply(c, false, window, cx),
                    None => this.sync_fields(window, cx),
                },
                _ => {}
            },
        )
        .detach();
        for (i, field) in channels.iter().enumerate() {
            cx.subscribe_in(
                field,
                window,
                move |this, _, ev: &FieldEvent, window, cx| {
                    let (FieldEvent::Submit(t) | FieldEvent::Changed(t)) = ev else {
                        return;
                    };
                    let Ok(v) = t.parse::<u8>() else {
                        if matches!(ev, FieldEvent::Submit(_)) {
                            this.sync_fields(window, cx);
                        }
                        return;
                    };
                    let mut c = this.color();
                    match i {
                        0 => c.r = v,
                        1 => c.g = v,
                        2 => c.b = v,
                        _ => c.a = v,
                    }
                    this.apply(c, false, window, cx);
                },
            )
            .detach();
        }
        let mut picker = ColorPicker {
            hsv: Hsv {
                h: 0.0,
                s: 0.0,
                v: 0.0,
            },
            alpha: 255,
            hex,
            channels,
            wheel: None,
            drag: None,
            bounds: [Bounds::default(); 3],
        };
        picker.load(color);
        picker.sync_fields(window, cx);
        picker
    }

    pub fn color(&self) -> Rgba {
        let [r, g, b] = self.hsv.to_rgb();
        Rgba {
            r,
            g,
            b,
            a: self.alpha,
        }
    }

    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    fn load(&mut self, c: Rgba) {
        self.hsv = Hsv::from_rgb([c.r, c.g, c.b], self.hsv);
        self.alpha = c.a;
    }

    /// Shows `color` (chosen elsewhere, e.g. a palette swatch) without
    /// emitting a change.
    pub fn set_color(&mut self, color: Rgba, window: &Window, cx: &mut Context<Self>) {
        if self.drag.is_some() || color == self.color() {
            return;
        }
        self.load(color);
        self.sync_fields(window, cx);
        cx.notify();
    }

    fn apply(&mut self, color: Rgba, live: bool, window: &Window, cx: &mut Context<Self>) {
        self.load(color);
        self.sync_fields(window, cx);
        cx.emit(PickerEvent::Changed {
            color: self.color(),
            live,
        });
        cx.notify();
    }

    fn sync_fields(&mut self, window: &Window, cx: &mut Context<Self>) {
        let c = self.color();
        self.hex
            .update(cx, |f, cx| f.set_text(to_hex(c), window, cx));
        for (field, v) in self.channels.iter().zip([c.r, c.g, c.b, c.a]) {
            field.update(cx, |f, cx| f.set_text(v.to_string(), window, cx));
        }
    }

    fn pointer(&mut self, p: Point<Pixels>, live: bool, window: &Window, cx: &mut Context<Self>) {
        let Some(part) = self.drag else { return };
        let b = self.bounds[part as usize];
        let fx = (f32::from(p.x - b.left()) / f32::from(b.size.width)).clamp(0.0, 1.0);
        match part {
            Part::Wheel => {
                let c = b.center();
                let (h, s) = wheel_hs(
                    f32::from(p.x - c.x),
                    f32::from(p.y - c.y),
                    f32::from(b.size.width) / 2.0,
                );
                self.hsv.h = h;
                self.hsv.s = s;
                if self.hsv.v < 0.05 {
                    // Picking a hue on a black color would change nothing visible.
                    self.hsv.v = 1.0;
                }
            }
            Part::Value => self.hsv.v = fx,
            Part::Alpha => self.alpha = (fx * 255.0).round() as u8,
        }
        self.sync_fields(window, cx);
        cx.emit(PickerEvent::Changed {
            color: self.color(),
            live,
        });
        cx.notify();
    }

    fn start(
        &mut self,
        part: Part,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drag = Some(part);
        self.pointer(ev.position, true, window, cx);
        cx.stop_propagation();
    }

    fn part(&self, part: Part, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let this = cx.entity();
        canvas(
            move |bounds, _, cx| {
                this.update(cx, |p, _| p.bounds[part as usize] = bounds);
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }
}

fn checker(window: &mut Window, b: Bounds<Pixels>) {
    let cell = 6.0;
    let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
    window.paint_quad(fill(b, gpui::rgb(0xffffff)));
    let mut y = 0.0;
    while y < h {
        let mut x = if ((y / cell) as i32) % 2 == 0 {
            0.0
        } else {
            cell
        };
        while x < w {
            let cw = cell.min(w - x);
            let ch = cell.min(h - y);
            window.paint_quad(fill(
                Bounds {
                    origin: b.origin + point(px(x), px(y)),
                    size: size(px(cw), px(ch)),
                },
                gpui::rgb(0xc8c8c8),
            ));
            x += 2.0 * cell;
        }
        y += cell;
    }
}

fn knob(at: Point<Pixels>, fill_color: gpui::Hsla) -> impl IntoElement {
    div()
        .absolute()
        .top_0()
        .left_0()
        .left(at.x - px(7.))
        .top(at.y - px(7.))
        .size(px(14.))
        .rounded_full()
        .border_2()
        .border_color(gpui::white())
        .bg(fill_color)
        .shadow_sm()
}

impl Render for ColorPicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scale = window.scale_factor();
        if self.wheel.as_ref().is_none_or(|(s, _)| *s != scale) {
            let d = (WHEEL * scale).round() as u32;
            let img = wheel_image(d);
            self.wheel = Some((scale, render_image(d, d, img.data)));
        }
        let wheel = self.wheel.as_ref().unwrap().1.clone();
        let color = self.color();
        let opaque = Rgba { a: 255, ..color };
        let full = {
            let [r, g, b] = Hsv { v: 1.0, ..self.hsv }.to_rgb();
            Rgba { r, g, b, a: 255 }
        };
        let darken = 1.0 - self.hsv.v;
        let rad = self.hsv.h.to_radians();
        let wheel_knob = point(
            px(WHEEL / 2.0 * (1.0 + self.hsv.s * rad.cos())),
            px(WHEEL / 2.0 * (1.0 - self.hsv.s * rad.sin())),
        );
        let slider_w = PANEL_WIDTH - 24.0;

        let this = cx.entity();
        let drag_listener = canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let picker = this.clone();
                window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble {
                        picker.update(cx, |p, cx| p.pointer(ev.position, true, window, cx));
                    }
                });
                let picker = this.clone();
                window.on_mouse_event(move |ev: &MouseUpEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left {
                        picker.update(cx, |p, cx| {
                            if p.drag.is_some() {
                                p.pointer(ev.position, false, window, cx);
                                p.drag = None;
                            }
                        });
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_0();

        let label = |t: &'static str| div().text_xs().text_color(theme::muted()).child(t);

        div()
            .id("color-picker")
            .occlude()
            .w(px(PANEL_WIDTH))
            .p(px(12.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .rounded_lg()
            .bg(theme::panel())
            .border_1()
            .border_color(theme::panel_border())
            .shadow_lg()
            .font_family(super::UI_FONT)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(drag_listener)
            .child(
                div()
                    .relative()
                    .size(px(WHEEL))
                    .mx_auto()
                    .cursor_crosshair()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|p, ev, window, cx| p.start(Part::Wheel, ev, window, cx)),
                    )
                    .child(self.part(Part::Wheel, cx))
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |b, _, window, _| {
                                let _ = window.paint_image(b, b, (0.).into(), wheel, 0, false);
                                let mut q = fill(b, gpui::rgba(0));
                                q.background = gpui::black().opacity(darken).into();
                                q.corner_radii = px(WHEEL / 2.0).into();
                                window.paint_quad(q);
                            },
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
                    .child(knob(wheel_knob, hsla(opaque))),
            )
            .child(
                div()
                    .relative()
                    .w(px(slider_w))
                    .h(px(SLIDER_H))
                    .rounded_full()
                    .bg(linear_gradient(
                        90.,
                        linear_color_stop(gpui::black(), 0.),
                        linear_color_stop(hsla(full), 1.),
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|p, ev, window, cx| p.start(Part::Value, ev, window, cx)),
                    )
                    .child(self.part(Part::Value, cx))
                    .child(knob(
                        point(px(slider_w * self.hsv.v), px(SLIDER_H / 2.0)),
                        hsla(opaque),
                    )),
            )
            .child(
                div()
                    .relative()
                    .w(px(slider_w))
                    .h(px(SLIDER_H))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|p, ev, window, cx| p.start(Part::Alpha, ev, window, cx)),
                    )
                    .child(self.part(Part::Alpha, cx))
                    .child(
                        canvas(|_, _, _| {}, move |b, _, window, _| checker(window, b))
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .rounded_full(),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .rounded_full()
                            .bg(linear_gradient(
                                90.,
                                linear_color_stop(hsla(Rgba { a: 0, ..opaque }), 0.),
                                linear_color_stop(hsla(opaque), 1.),
                            )),
                    )
                    .child(knob(
                        point(px(slider_w * self.alpha as f32 / 255.0), px(SLIDER_H / 2.0)),
                        hsla(color),
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .relative()
                            .size(px(24.))
                            .rounded_md()
                            .overflow_hidden()
                            .child(
                                canvas(|_, _, _| {}, move |b, _, window, _| checker(window, b))
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .size_full(),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .size_full()
                                    .bg(hsla(color)),
                            ),
                    )
                    .child(div().flex_1().child(self.hex.clone())),
            )
            .child(
                div().flex().justify_between().children(
                    ["R", "G", "B", "A"]
                        .into_iter()
                        .zip(self.channels.iter())
                        .map(|(name, field)| {
                            div()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap(px(2.))
                                .child(field.clone())
                                .child(label(name))
                        }),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_angles_follow_the_hue_circle() {
        assert_eq!(wheel_hs(10.0, 0.0, 10.0), (0.0, 1.0), "right is red");
        let (h, s) = wheel_hs(0.0, -5.0, 10.0);
        assert!(
            (h - 90.0).abs() < 1e-3 && (s - 0.5).abs() < 1e-6,
            "up is 90°"
        );
        assert_eq!(wheel_hs(0.0, 0.0, 10.0).1, 0.0, "centre is unsaturated");
        assert_eq!(
            wheel_hs(30.0, 0.0, 10.0).1,
            1.0,
            "outside clamps to the rim"
        );
    }

    #[test]
    fn wheel_image_has_white_centre_red_rim_and_clear_corners() {
        let img = wheel_image(101);
        assert_eq!(img.pixel(50, 50), [255, 255, 255, 255]);
        let rim = img.pixel(98, 50);
        assert!(rim[2] > 240 && rim[0] < 20 && rim[1] < 20, "{rim:?} (BGRA)");
        assert_eq!(img.pixel(0, 0)[3], 0);
    }
}
