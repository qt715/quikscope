//! The eyedropper: a magnifier that follows the pointer over a frozen
//! desktop, a button that starts a pick, and the form that copies a color as
//! hex, RGB, CMYK, HSV or HSL.

use std::rc::Rc;

use gpui::{
    App, Bounds, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Point, Size,
    StatefulInteractiveElement as _, Styled as _, Window, canvas, div, fill, outline, point,
    prelude::FluentBuilder as _, px, size, svg,
};

use super::{hsla, theme};
use crate::color::Format;
use crate::document::Rgba;
use crate::geom::Pt;
use crate::raster::Image;

/// Image pixels across the magnifier; odd, so one sits in the middle.
const LOUPE_PIXELS: i32 = 11;
/// Window pixels per magnified image pixel.
const LOUPE_CELL: f32 = 10.0;
/// Distance between the pointer and the magnifier.
const LOUPE_GAP: f32 = 24.0;
const LOUPE_LABEL: f32 = 22.0;
/// Wide enough for the four format fields in one row.
pub const PANEL_WIDTH: f32 = 780.0;
/// Narrowest format field: fits the longest value, `0%, 100%, 100%, 100%`;
/// narrower panels wrap the four fields into two rows.
pub const FIELD_MIN_WIDTH: f32 = 176.0;
/// Horizontal gap between format fields.
const FIELD_GAP: f32 = 12.0;
/// One more wrapped row of format fields: its height plus the gap above it.
pub const FORMAT_ROW: f32 = 40.0 + 18.0;

/// Rows the four non-hex format fields wrap into in a panel `width` wide
/// (its 1 px border and 12 px padding included).
pub fn format_rows(width: f32) -> usize {
    let inner = width - 2.0 - 24.0;
    let per_row = ((inner + FIELD_GAP) / (FIELD_MIN_WIDTH + FIELD_GAP)).floor() as usize;
    let fields = Format::ALL.len() - 1;
    fields.div_ceil(per_row.clamp(1, fields))
}

/// The pixel of `image` containing point `p`, or `None` outside it.
pub fn sample(image: &Image, p: Pt) -> Option<Rgba> {
    let (x, y) = (p.x.floor(), p.y.floor());
    if !(x >= 0.0 && y >= 0.0 && x < image.width as f32 && y < image.height as f32) {
        return None;
    }
    let [b, g, r, a] = image.pixel(x as u32, y as u32);
    Some(Rgba { r, g, b, a })
}

/// The magnified pixels around image point `p`, beside window point
/// `pointer`, with the centre pixel's hex below; `None` off the screenshot.
pub fn loupe(
    image: &Image,
    p: Pt,
    pointer: Point<Pixels>,
    viewport: Size<Pixels>,
) -> Option<impl IntoElement + use<>> {
    let center = sample(image, p)?;
    let (x0, y0) = (p.x.floor(), p.y.floor());
    let half = LOUPE_PIXELS / 2;
    let cells: Vec<Option<Rgba>> = (-half..=half)
        .flat_map(|dy| (-half..=half).map(move |dx| (dx, dy)))
        .map(|(dx, dy)| sample(image, Pt::new(x0 + dx as f32, y0 + dy as f32)))
        .collect();
    let side = LOUPE_PIXELS as f32 * LOUPE_CELL;
    // Frame: 4 px padding and a 1 px border on each side.
    let (w, h) = (side + 10.0, side + 10.0 + LOUPE_LABEL);
    // Below and right of the pointer, flipped where that would leave the window.
    let (px_, py_) = (f32::from(pointer.x), f32::from(pointer.y));
    let x = if px_ + LOUPE_GAP + w > f32::from(viewport.width) - 8.0 {
        px_ - LOUPE_GAP - w
    } else {
        px_ + LOUPE_GAP
    };
    let y = if py_ + LOUPE_GAP + h > f32::from(viewport.height) - 8.0 {
        py_ - LOUPE_GAP - h
    } else {
        py_ + LOUPE_GAP
    };
    let pixels = canvas(
        |_, _, _| {},
        move |b: Bounds<Pixels>, _, window, _| {
            let n = LOUPE_PIXELS as usize;
            let cell = |col: i32, row: i32| Bounds {
                origin: b.origin + point(px(col as f32 * LOUPE_CELL), px(row as f32 * LOUPE_CELL)),
                size: size(px(LOUPE_CELL), px(LOUPE_CELL)),
            };
            for (i, c) in cells.iter().enumerate() {
                let r = cell((i % n) as i32, (i / n) as i32);
                match c {
                    Some(c) => window.paint_quad(fill(r, hsla(*c))),
                    None => window.paint_quad(fill(r, theme::canvas_bg())),
                }
            }
            // The picked pixel: white inside black, visible on any color.
            let c = cell(half, half);
            window.paint_quad(outline(c, gpui::white(), gpui::BorderStyle::Solid));
            let outer = Bounds {
                origin: c.origin - point(px(1.), px(1.)),
                size: c.size + size(px(2.), px(2.)),
            };
            window.paint_quad(outline(outer, gpui::black(), gpui::BorderStyle::Solid));
        },
    )
    .size(px(side));
    Some(
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .p_1()
            .rounded_md()
            .bg(theme::panel())
            .border_1()
            .border_color(theme::panel_border())
            .shadow_lg()
            .flex()
            .flex_col()
            .child(pixels)
            .child(
                div()
                    .h(px(LOUPE_LABEL))
                    .flex()
                    .items_end()
                    .justify_center()
                    .text_xs()
                    .text_color(theme::text())
                    .child(Format::Hex.format(center)),
            ),
    )
}

/// Field outline, brighter than [`theme::panel_border`] so the boxes read as
/// fields.
fn field_border() -> gpui::Rgba {
    gpui::rgba(0xffffff30)
}

/// Copies the picked color in a format.
pub type OnCopy = Rc<dyn Fn(Format, &mut Window, &mut App)>;
/// Starts a pick from anywhere on the desktop.
pub type OnPick = Rc<dyn Fn(&mut Window, &mut App)>;

/// One outlined field showing `color` in `format`, its name set into the
/// top border; a click copies the value.
fn field(format: Format, color: Rgba, on_copy: &OnCopy) -> gpui::Stateful<gpui::Div> {
    let wide = format == Format::Hex;
    let legend = div()
        .px_1()
        .bg(theme::panel())
        .text_xs()
        .text_color(theme::muted())
        .child(format.label());
    let legend = div()
        .absolute()
        .top(px(-8.))
        .h(px(16.))
        .flex()
        .items_center()
        .child(legend);
    let legend = if wide {
        legend.left_0().right_0().justify_center()
    } else {
        legend.left(px(8.))
    };
    let on_copy = on_copy.clone();
    div()
        .id(format.label())
        .relative()
        .h(px(if wide { 48. } else { 40. }))
        .px(px(10.))
        .flex()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(field_border())
        .cursor_pointer()
        .hover(|s| s.border_color(theme::accent()))
        .on_click(move |_, window, cx| on_copy(format, window, cx))
        .child(legend)
        .child(
            div()
                // Centred between the swatch and the copy icon.
                .when(wide, |d| d.flex().justify_center())
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_sm()
                .child(format.format(color)),
        )
}

/// A button that picks a color from anywhere on screen.
pub fn pick_button(label: &'static str, on_pick: OnPick) -> gpui::Stateful<gpui::Div> {
    div()
        .id("pick-screen")
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .gap_1p5()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(field_border())
        .cursor_pointer()
        .hover(|s| s.border_color(theme::accent()))
        .text_xs()
        .on_click(move |_, window, cx| on_pick(window, cx))
        .child(
            svg()
                .path("icons/pipette.svg")
                .size(px(14.))
                .text_color(theme::text()),
        )
        .child(label)
}

/// `color` in every [`Format`], laid out like a form: hex on top, the other
/// formats in a row that wraps (one field per row in narrow columns).
pub fn formats(color: Rgba, on_copy: &OnCopy) -> gpui::Div {
    let hex = field(Format::Hex, color, on_copy)
        .child(
            div()
                .absolute()
                .left(px(12.))
                .size(px(18.))
                .rounded_sm()
                .bg(hsla(color))
                .border_1()
                .border_color(field_border()),
        )
        .child(
            svg()
                .absolute()
                .right(px(12.))
                .path("icons/copy.svg")
                .size(px(18.))
                .text_color(theme::text()),
        );
    let others = Format::ALL[1..]
        .iter()
        .map(|&f| field(f, color, on_copy).flex_1().min_w(px(FIELD_MIN_WIDTH)));
    div().flex().flex_col().gap(px(18.)).child(hex).child(
        div()
            .flex()
            .flex_wrap()
            .gap_x(px(FIELD_GAP))
            .gap_y(px(18.))
            .children(others),
    )
}

/// The standalone color panel, `width` wide: a line of help and a button
/// that picks again, then `color` in every format.
pub fn panel(
    color: Rgba,
    width: Pixels,
    hint: &'static str,
    pick_label: &'static str,
    on_copy: OnCopy,
    on_pick: OnPick,
) -> gpui::Stateful<gpui::Div> {
    let header = div()
        .flex()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .min_w_0()
                .text_xs()
                .text_color(theme::muted())
                .child(hint),
        )
        .child(pick_button(pick_label, on_pick));
    div()
        .id("color-formats")
        .w(width.max(px(1.)))
        .p_3()
        .rounded_lg()
        .bg(theme::panel())
        .border_1()
        .border_color(theme::panel_border())
        .shadow_lg()
        .flex()
        .flex_col()
        .gap(px(18.))
        .child(header)
        .child(formats(color, &on_copy))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_the_pixel_containing_the_point_and_nothing_outside() {
        let mut image = Image::filled(3, 2, [10, 20, 30, 255]);
        // BGRA (2, 1) = rgb(3, 2, 1) at half alpha.
        let i = (image.width as usize + 2) * 4;
        image.data[i..i + 4].copy_from_slice(&[1, 2, 3, 128]);
        let at = |x, y| sample(&image, Pt::new(x, y));
        assert_eq!(
            at(2.99, 1.0),
            Some(Rgba {
                r: 3,
                g: 2,
                b: 1,
                a: 128
            })
        );
        assert_eq!(at(0.0, 0.0).map(|c| (c.r, c.b)), Some((30, 10)));
        for (x, y) in [(-0.01, 0.0), (3.0, 0.0), (0.0, 2.0), (f32::NAN, 0.0)] {
            assert_eq!(at(x, y), None, "({x}, {y})");
        }
    }
}
