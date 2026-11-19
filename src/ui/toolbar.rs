//! The editor's floating chrome in the "split" layout: tools in a pill at
//! the top centre, style controls (colors, stroke size, fill) in a panel at
//! the left edge, history and zoom at the bottom left, outputs at the bottom
//! right. The geometry is fixed here rather than measured after layout
//! because the canvas fit, the color picker and the font bar must keep clear
//! of it on the very first frame.

use gpui::{
    AnyElement, AppContext as _, Bounds, Context, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, Pixels, Render, ScrollHandle, Size, StatefulInteractiveElement as _,
    Styled as _, div, point, prelude::FluentBuilder as _, px, size, svg,
};

use super::editor::{Editor, Tool};
use super::{hsla, theme};
use crate::document::{PALETTE, STROKE_WIDTHS};
use crate::media::OutputFormat;

/// Every icon the toolbar references (checked against the asset table in tests).
pub const ICONS: [&str; 23] = [
    "palette",
    "mouse-pointer-2",
    "hand",
    "square",
    "circle",
    "move-up-right",
    "minus",
    "pencil",
    "type",
    "highlighter",
    "list-ordered",
    "grid-3x3",
    "crop",
    "paint-bucket",
    "undo-2",
    "redo-2",
    "copy",
    "scan-text",
    "save",
    "settings",
    "play",
    "pause",
    "chevron-down",
];

/// Distance from the window edges to every panel.
pub const EDGE: f32 = 10.0;
pub(super) const BUTTON: f32 = 32.0;
/// Between neighbouring buttons.
pub(super) const GAP: f32 = 2.0;
pub(super) const PAD: f32 = 4.0;
const BORDER: f32 = 1.0;
/// A 1 px rule with 4 px either side.
const SEPARATOR: f32 = 1.0 + 2.0 * 4.0;
/// Height of the tool pill and of both bottom clusters.
pub const BAR: f32 = BUTTON + 2.0 * (PAD + BORDER);
/// Space kept between the chrome and a fitted image.
pub const CLEARANCE: f32 = 8.0;
/// The bands at the top and bottom of the window that the bars occupy.
pub const BAR_SPACE: f32 = EDGE + BAR + CLEARANCE;

const ZOOM_W: f32 = 52.0;
const SAVE_W: f32 = 72.0;
/// The animation strip's format button and its menu.
const FORMAT_W: f32 = 64.0;
const MENU_W: f32 = 220.0;
/// Bottom-left cluster: undo, redo | zoom.
const HISTORY_W: f32 = 2.0 * (PAD + BORDER) + 2.0 * BUTTON + SEPARATOR + ZOOM_W + 3.0 * GAP;
/// Bottom-right cluster: copy text, copy, save | settings, help.
const OUTPUT_W: f32 = 2.0 * (PAD + BORDER) + 4.0 * BUTTON + SAVE_W + SEPARATOR + 5.0 * GAP;
/// An image narrower than the window minus this on both sides passes
/// between the bottom clusters.
pub const CORNER_SPACE: f32 = EDGE + OUTPUT_W.max(HISTORY_W) + CLEARANCE;

/// Style panel cells: two per row.
const CELL: f32 = 28.0;
const SIDE_PAD: f32 = 6.0;
const GROUP_W: f32 = 2.0 * CELL + GAP;
/// Eight swatches, then the custom color and fill toggle.
const COLORS_H: f32 = 5.0 * CELL + 4.0 * GAP;
/// One full-width row per stroke size.
const STROKES_H: f32 = 3.0 * CELL + 2.0 * GAP;
/// The rule between the groups and its margins.
const SIDE_RULE: f32 = 1.0 + 2.0 * 5.0;
const SIDE_FRAME: f32 = 2.0 * (SIDE_PAD + BORDER);
/// Colors above strokes, in one column.
const TALL: (f32, f32) = (
    GROUP_W + SIDE_FRAME,
    COLORS_H + SIDE_RULE + STROKES_H + SIDE_FRAME,
);
/// Colors beside strokes, for windows too short for [`TALL`]; hiding
/// controls behind a toggle would cost a click on every style change there.
const WIDE: (f32, f32) = (
    2.0 * GROUP_W + SIDE_RULE + SIDE_FRAME,
    COLORS_H + SIDE_FRAME,
);
/// A fitted image stays this far from both sides, so the style panel never
/// covers the image where you would draw. Both sides keep the image centred
/// under the tool pill.
pub const SIDE_SPACE: f32 = EDGE + TALL.0 + CLEARANCE;

/// Where the style panel sits in a `viewport`: at the left edge, centred
/// between the bars, one column when it fits there.
pub fn style_panel_bounds(viewport: Size<Pixels>) -> Bounds<Pixels> {
    let vh = f32::from(viewport.height);
    let room = vh - 2.0 * BAR_SPACE;
    let (w, h) = if room >= TALL.1 { TALL } else { WIDE };
    // Whole pixels keep its 1 px border and rules crisp at any scale.
    let y = (BAR_SPACE + (room - h) / 2.0).max(EDGE).round();
    Bounds::new(point(px(EDGE), px(y)), size(px(w), px(h)))
}

/// The animation strip's preferred width, and the narrowest that still
/// leaves its scrubber usable.
const STRIP_W: f32 = 380.0;
const STRIP_MIN: f32 = 260.0;

/// Where the animation strip sits in a `viewport`: in the bottom band
/// between the clusters when it fits there (centred in the window if it
/// can be, else in the gap, since the clusters differ in width), otherwise
/// just above them, beside the style panel in windows short enough for the
/// two to meet.
pub fn strip_bounds(viewport: Size<Pixels>) -> Bounds<Pixels> {
    let (vw, vh) = (f32::from(viewport.width), f32::from(viewport.height));
    let (mut left, mut right) = (2.0 * EDGE + HISTORY_W, vw - 2.0 * EDGE - OUTPUT_W);
    let mut bottom = EDGE;
    if right - left < STRIP_MIN {
        bottom = EDGE + BAR + CLEARANCE;
        let side = style_panel_bounds(viewport);
        left = if f32::from(side.bottom()) + CLEARANCE > vh - bottom - BAR {
            f32::from(side.right()) + EDGE
        } else {
            EDGE
        };
        right = vw - EDGE;
    }
    let w = STRIP_W.min(right - left).max(1.0);
    let x = ((vw - w) / 2.0).clamp(left, (right - w).max(left));
    Bounds::new(
        point(px(x.ceil()), px(vh - bottom - BAR)),
        size(px(w.floor()), px(BAR)),
    )
}

/// Height of the band at the bottom of the window the chrome occupies,
/// clearance included: the bottom clusters, and with an animation the
/// strip, which may sit above them. Toasts and the canvas fit stay above it.
pub fn bottom_band(viewport: Size<Pixels>, animation: bool) -> f32 {
    let strip =
        animation.then(|| f32::from(viewport.height - strip_bounds(viewport).top()) + CLEARANCE);
    strip.unwrap_or(0.0).max(BAR_SPACE)
}

/// Rendered bounds of each panel. GPUI 0.3.10 reports element bounds to
/// tests only through `VisualTestContext::debug_bounds`, which the headless
/// harness has no equivalent of; a tracked [`ScrollHandle`] records them
/// without making the panel scrollable.
#[derive(Default)]
pub struct Probes {
    pub tools: ScrollHandle,
    pub style: ScrollHandle,
    pub history: ScrollHandle,
    pub output: ScrollHandle,
    pub playback: ScrollHandle,
    pub scrubber: ScrollHandle,
    pub format_menu: ScrollHandle,
    /// The annotator's toast and animation export progress, while shown.
    pub toast: ScrollHandle,
    pub progress: ScrollHandle,
}

pub(super) fn icon(name: &'static str, color: gpui::Rgba) -> impl IntoElement {
    svg()
        .path(format!("icons/{name}.svg"))
        .size(px(16.))
        .text_color(color)
}

pub(super) struct Tip(pub &'static str);

impl Render for Tip {
    fn render(&mut self, _: &mut gpui::Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(theme::panel())
            .border_1()
            .border_color(theme::panel_border())
            .text_xs()
            .text_color(theme::text())
            .font_family(super::UI_FONT)
            .child(self.0)
    }
}

/// A square button with a tooltip and no hover style yet (GPUI allows one).
pub(super) fn base(id: &'static str, tip: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(BUTTON))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_md()
        .tooltip(move |_, cx| cx.new(|_| Tip(tip)).into())
}

pub(super) fn button(
    id: &'static str,
    tip: &'static str,
    active: bool,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    base(id, tip)
        .when(active, |d| d.bg(theme::accent_bg()))
        .when(enabled && !active, |d| d.hover(|s| s.bg(theme::hover())))
        .when(!enabled, |d| d.opacity(0.35))
}

pub(super) fn separator() -> impl IntoElement {
    div()
        .flex_shrink_0()
        .w(px(1.))
        .h(px(20.))
        .mx_1()
        .bg(theme::panel_border())
}

/// A floating panel; `probe` records where it was laid out.
pub(super) fn panel(id: &'static str, probe: &ScrollHandle) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .track_scroll(probe)
        .occlude()
        .flex()
        .items_center()
        .rounded_lg()
        .bg(theme::panel())
        .border_1()
        .border_color(theme::panel_border())
        .shadow_lg()
}

/// Every floating panel, for the editor to lay over its canvas.
pub fn chrome(
    editor: &Editor,
    zoom_label: String,
    viewport: Size<Pixels>,
    cx: &mut Context<Editor>,
) -> Vec<AnyElement> {
    let mut out = vec![
        tools(editor, cx).into_any_element(),
        history(editor, zoom_label, cx).into_any_element(),
        output(editor, cx).into_any_element(),
    ];
    // Only while its controls change something: hidden for pan, crop,
    // pixelate and an empty selection. The canvas fit leaves its room either
    // way, so showing it never moves the image.
    if editor.style_panel_shown() {
        out.push(style_panel(editor, viewport, cx).into_any_element());
    }
    if editor.animation_status().is_some() {
        out.push(playback(editor, viewport, cx).into_any_element());
        if editor.animation_menu_open() {
            out.push(animation_format_menu(editor, viewport, cx).into_any_element());
        }
    }
    out
}

fn tools(editor: &Editor, cx: &mut Context<Editor>) -> impl IntoElement {
    let buttons = Tool::ALL.iter().map(|&tool| {
        let active = editor.tool == tool;
        button(tool.icon(), tool.label(), active, true)
            .relative()
            .on_click(cx.listener(move |e, _, _, cx| e.set_tool(tool, cx)))
            .child(icon(
                tool.icon(),
                if active {
                    theme::accent()
                } else {
                    theme::text()
                },
            ))
            .children(tool.digit().map(|d| {
                div()
                    .absolute()
                    .bottom(px(1.))
                    .right(px(3.))
                    .text_size(px(8.))
                    .text_color(theme::muted())
                    .child(d.to_string())
            }))
    });
    div()
        .absolute()
        .top(px(EDGE))
        .left_0()
        .right_0()
        .px(px(EDGE))
        .flex()
        .justify_center()
        .child(
            panel("toolbar", &editor.chrome.tools)
                // Below the 480 px minimum width the twelve tools would wrap
                // rather than clip off both edges.
                .flex_wrap()
                .justify_center()
                .max_w_full()
                .gap(px(GAP))
                .p(px(PAD))
                .children(buttons),
        )
}

fn style_panel(
    editor: &Editor,
    viewport: Size<Pixels>,
    cx: &mut Context<Editor>,
) -> impl IntoElement {
    let style = editor.style;
    let bounds = style_panel_bounds(viewport);
    let tall = bounds.size.height == px(TALL.1);

    let cell = |id: gpui::ElementId| {
        div()
            .id(id)
            .size(px(CELL))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
    };
    let mut colors: Vec<AnyElement> = PALETTE
        .iter()
        .enumerate()
        .map(|(i, &color)| {
            let active = style.color == color;
            cell(("color", i).into())
                .when(active, |d| d.border_2().border_color(theme::accent()))
                .when(!active, |d| d.hover(|s| s.bg(theme::hover())))
                .on_click(cx.listener(move |e, _, _, cx| e.set_color(color, cx)))
                .child(
                    div()
                        .size(px(16.))
                        .rounded_full()
                        .bg(hsla(color))
                        .border_1()
                        .border_color(theme::panel_border()),
                )
                .into_any_element()
        })
        .collect();
    // Any other color (picked in the color picker) shows here; clicking opens
    // the picker beside the panel.
    let custom = !PALETTE.contains(&style.color);
    colors.push(
        cell("color-custom".into())
            .tooltip(|_, cx| cx.new(|_| Tip("Custom color")).into())
            .when(custom || editor.picker_open(), |d| {
                d.border_2().border_color(theme::accent())
            })
            .hover(|d| d.bg(theme::hover()))
            .on_click(cx.listener(|e, _, _, cx| e.toggle_picker(cx)))
            .child(if custom {
                div()
                    .size(px(16.))
                    .rounded_full()
                    .bg(hsla(style.color))
                    .border_1()
                    .border_color(theme::panel_border())
                    .into_any_element()
            } else {
                icon("palette", theme::text()).into_any_element()
            })
            .into_any_element(),
    );
    colors.push(
        button("fill", "Fill shapes (F)", style.fill, true)
            .size(px(CELL))
            .on_click(cx.listener(|e, _, _, cx| e.toggle_fill(cx)))
            .child(icon(
                "paint-bucket",
                if style.fill {
                    theme::accent()
                } else {
                    theme::text()
                },
            ))
            .into_any_element(),
    );

    // A sample of each stroke at its real width reads better in a narrow
    // column than dots of three sizes.
    let strokes = (0u8..3).map(|size| {
        let (id, tip) = [
            ("size-s", "Thin ([)"),
            ("size-m", "Medium"),
            ("size-l", "Thick (])"),
        ][size as usize];
        let active = style.size == size;
        button(id, tip, active, true)
            .w(px(GROUP_W))
            .h(px(CELL))
            .on_click(cx.listener(move |e, _, _, cx| e.set_size(size, cx)))
            .child(
                div()
                    .w(px(30.))
                    .h(px(STROKE_WIDTHS[size as usize]))
                    .rounded_full()
                    .bg(if active {
                        theme::accent()
                    } else {
                        theme::text()
                    }),
            )
    });

    let rule = div().flex_shrink_0().bg(theme::panel_border());
    let rule = if tall {
        rule.w(px(GROUP_W)).h(px(1.)).my(px(5.))
    } else {
        rule.w(px(1.)).h(px(COLORS_H)).mx(px(5.))
    };
    panel("style-panel", &editor.chrome.style)
        .absolute()
        .left(bounds.origin.x)
        .top(bounds.origin.y)
        .w(bounds.size.width)
        .h(bounds.size.height)
        .p(px(SIDE_PAD))
        .when(tall, |d| d.flex_col())
        .items_start()
        .child(
            div()
                .w(px(GROUP_W))
                .flex()
                .flex_wrap()
                .gap(px(GAP))
                .children(colors),
        )
        .child(rule)
        .child(
            div()
                .w(px(GROUP_W))
                .flex()
                .flex_col()
                .gap(px(GAP))
                .children(strokes),
        )
}

fn history(editor: &Editor, zoom_label: String, cx: &mut Context<Editor>) -> impl IntoElement {
    let doc = editor.doc.read(cx);
    let (can_undo, can_redo) = (doc.can_undo(), doc.can_redo());
    panel("history", &editor.chrome.history)
        .absolute()
        .left(px(EDGE))
        .bottom(px(EDGE))
        .w(px(HISTORY_W))
        .gap(px(GAP))
        .p(px(PAD))
        .child(
            button("undo", "Undo (Ctrl+Z)", false, can_undo)
                .when(can_undo, |d| {
                    d.on_click(cx.listener(|e, _, _, cx| e.undo(cx)))
                })
                .child(icon("undo-2", theme::text())),
        )
        .child(
            button("redo", "Redo (Ctrl+Shift+Z)", false, can_redo)
                .when(can_redo, |d| {
                    d.on_click(cx.listener(|e, _, _, cx| e.redo(cx)))
                })
                .child(icon("redo-2", theme::text())),
        )
        .child(separator())
        .child(
            button("zoom", "Fit to window (Ctrl+0)", false, true)
                .w(px(ZOOM_W))
                .text_xs()
                .text_color(theme::muted())
                .on_click(cx.listener(|e, _, window, cx| {
                    e.fit(window, cx);
                    cx.notify();
                }))
                .child(zoom_label),
        )
}

fn output(editor: &Editor, cx: &mut Context<Editor>) -> impl IntoElement {
    // An animation is re-encoded and saved with the recordings.
    let (copy_tip, save_tip) = if editor.animation_status().is_some() {
        (
            "Copy animation (Ctrl+C)",
            "Save to recordings folder (Ctrl+S)",
        )
    } else {
        ("Copy image (Ctrl+C)", "Save to screenshots folder (Ctrl+S)")
    };
    panel("output", &editor.chrome.output)
        .absolute()
        .right(px(EDGE))
        .bottom(px(EDGE))
        .w(px(OUTPUT_W))
        .gap(px(GAP))
        .p(px(PAD))
        .child(
            button("scan-text", "Copy text (Ctrl+Shift+C)", false, true)
                .on_click(cx.listener(|e, _, _, cx| e.copy_text(cx)))
                .child(icon("scan-text", theme::text())),
        )
        .child(
            button("copy", copy_tip, false, true)
                .on_click(cx.listener(|e, _, _, cx| e.copy(cx)))
                .child(icon("copy", theme::text())),
        )
        .child(
            // The one primary action: saving is what most edits end with.
            base("save", save_tip)
                .w(px(SAVE_W))
                .gap_1p5()
                .bg(theme::accent())
                .hover(|s| s.bg(theme::accent_hover()))
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::on_accent())
                .on_click(cx.listener(|e, _, window, cx| e.save(false, window, cx)))
                .child(icon("save", theme::on_accent()))
                .child("Save"),
        )
        .child(separator())
        .child(
            button("settings", "Settings", false, true)
                .on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(super::settings::OpenSettings), cx);
                })
                .child(icon("settings", theme::text())),
        )
        .child(
            button(
                "shortcut-help",
                "Keyboard shortcuts (? / F1)",
                editor.help_open,
                true,
            )
            .on_click(cx.listener(|e, _, _, cx| e.toggle_help(cx)))
            .child("?"),
        )
}

fn playback(editor: &Editor, viewport: Size<Pixels>, cx: &mut Context<Editor>) -> impl IntoElement {
    let (playing, position, format) = editor.animation_status().expect("animation strip");
    let info = &editor.shot.animation.as_ref().expect("animation shot").info;
    let duration = info.duration.as_secs_f64().max(0.001);
    let fraction = (position.as_secs_f64() / duration).clamp(0.0, 1.0);
    let frame = (position.as_secs_f64() * info.fps.max(1.0)).floor() as u64 + 1;
    let count = info
        .frames
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".into());
    let bounds = strip_bounds(viewport);
    let menu_open = editor.animation_menu_open();
    panel("animation-playback", &editor.chrome.playback)
        .absolute()
        .left(bounds.left())
        .top(bounds.top())
        .w(bounds.size.width)
        .h(bounds.size.height)
        .p(px(PAD))
        .gap_2()
        .child(
            button(
                "animation-play",
                if playing {
                    "Pause (Space)"
                } else {
                    "Play (Space)"
                },
                false,
                true,
            )
            .on_click(cx.listener(|e, _, _, cx| e.toggle_playback(cx)))
            .child(icon(if playing { "pause" } else { "play" }, theme::text())),
        )
        .child(
            div()
                .id("animation-scrubber")
                .track_scroll(&editor.chrome.scrubber)
                .h(px(BUTTON))
                .flex_1()
                .min_w(px(30.))
                .flex()
                .items_center()
                .cursor_pointer()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|e, ev: &gpui::MouseDownEvent, _, cx| {
                        let bounds = e.chrome.scrubber.bounds();
                        e.scrub_animation(
                            f32::from(ev.position.x - bounds.left())
                                / f32::from(bounds.size.width).max(1.0),
                            cx,
                        );
                        cx.stop_propagation();
                    }),
                )
                .on_mouse_move(cx.listener(|e, ev: &gpui::MouseMoveEvent, _, cx| {
                    if ev.pressed_button == Some(gpui::MouseButton::Left) {
                        let bounds = e.chrome.scrubber.bounds();
                        e.scrub_animation(
                            f32::from(ev.position.x - bounds.left())
                                / f32::from(bounds.size.width).max(1.0),
                            cx,
                        );
                        cx.stop_propagation();
                    }
                }))
                .child(
                    div()
                        .h(px(4.))
                        .w_full()
                        .rounded_full()
                        .bg(theme::hover())
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(fraction as f32))
                                .rounded_full()
                                .bg(theme::accent()),
                        ),
                ),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_xs()
                .text_color(theme::muted())
                .child(format!("{frame}/{count}  {:.1}s", position.as_secs_f64())),
        )
        .child(
            base("animation-format", "Animation format")
                .w(px(FORMAT_W))
                .gap_1()
                .text_xs()
                .text_color(theme::text())
                .when(menu_open, |d| d.bg(theme::accent_bg()))
                .when(!menu_open, |d| d.hover(|s| s.bg(theme::hover())))
                .on_click(cx.listener(|e, _, _, cx| e.toggle_animation_menu(cx)))
                .child(super::video_editor::short_label(format))
                .child(icon("chevron-down", theme::muted())),
        )
}

/// The animation format menu, above the strip's format button. Item `i + 1`
/// of [`Probes::format_menu`] is `OutputFormat::ANIMATION[i]`.
fn animation_format_menu(
    editor: &Editor,
    viewport: Size<Pixels>,
    cx: &mut Context<Editor>,
) -> impl IntoElement {
    let (_, _, current) = editor.animation_status().expect("animation strip");
    let strip = strip_bounds(viewport);
    let item = |format: OutputFormat| {
        let active = current == format;
        div()
            .id(gpui::ElementId::Name(
                format!("animation-format-{}", format.extension()).into(),
            ))
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .h(px(28.))
            .px_2()
            .rounded_md()
            .text_sm()
            .when(active, |d| {
                d.bg(theme::accent_bg()).text_color(theme::accent())
            })
            .when(!active, |d| {
                d.text_color(theme::text()).hover(|s| s.bg(theme::hover()))
            })
            .on_click(cx.listener(move |e, _, _, cx| e.set_animation_format(format, cx)))
            .child(format.label())
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child(super::video_editor::format_hint(format)),
            )
    };
    let menu = div()
        .id("animation-format-menu")
        .track_scroll(&editor.chrome.format_menu)
        .occlude()
        .absolute()
        .right(viewport.width - strip.right())
        .bottom(viewport.height - strip.top() + px(6.))
        .w(px(MENU_W))
        .p(px(PAD))
        .flex()
        .flex_col()
        .rounded_lg()
        .bg(theme::panel())
        .border_1()
        .border_color(theme::panel_border())
        .shadow_lg()
        .child(
            div()
                .px_2()
                .pt_1p5()
                .pb_0p5()
                .text_xs()
                .text_color(theme::muted())
                .child("Animation format"),
        )
        .children(OutputFormat::ANIMATION.map(item));
    // A backdrop over the window: clicking anywhere else closes the menu
    // without also pressing whatever is under the pointer.
    div()
        .absolute()
        .inset_0()
        .child(
            div()
                .id("animation-format-backdrop")
                .absolute()
                .inset_0()
                .occlude()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|e, _, _, cx| e.toggle_animation_menu(cx)),
                ),
        )
        .child(menu)
}

/// A titled list of shortcuts for [`help_panel`].
pub(super) fn help_section(title: &'static str, rows: &'static [&'static str]) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_sm().text_color(theme::accent()).child(title))
        .children(rows.iter().map(|row| div().text_xs().child(*row)))
}

/// The keyboard shortcut overlay: `columns` side by side (stacked in
/// narrow windows) in a centred panel that scrolls when the window is short.
pub(super) fn help_panel(
    viewport: Size<Pixels>,
    scroll: &ScrollHandle,
    columns: impl IntoIterator<Item = impl IntoElement>,
) -> impl IntoElement {
    div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .id("shortcut-help-panel")
                .occlude()
                .p_6()
                .rounded_lg()
                .bg(theme::panel().opacity(0.96))
                .border_1()
                .border_color(theme::panel_border())
                .shadow_lg()
                .flex()
                .flex_col()
                .gap_3()
                .max_w_full()
                .w((viewport.width - px(32.)).min(px(760.)))
                .max_h((viewport.height - px(32.)).max(px(1.)))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .child(div().flex_shrink_0().text_lg().child("Keyboard shortcuts"))
                .child(
                    div()
                        .flex()
                        .flex_shrink_0()
                        .when(viewport.width < px(640.), |d| d.flex_col())
                        .gap_6()
                        .children(columns),
                ),
        )
}

/// The annotator's shortcuts; `animation` swaps in what differs for an
/// animated shot (Space plays, outputs are re-encoded animations).
pub fn shortcut_help(
    viewport: Size<Pixels>,
    scroll: &ScrollHandle,
    animation: bool,
) -> impl IntoElement {
    let tools = Tool::ALL.into_iter().map(|tool| {
        let keys = ('a'..='z')
            .chain('0'..='9')
            .filter(|key| Tool::from_key(key.encode_utf8(&mut [0; 4])) == Some(tool))
            .map(|key| key.to_ascii_uppercase().to_string())
            .collect::<Vec<_>>()
            .join(" / ");
        let label = tool.label().split(" (").next().unwrap();
        div().text_xs().child(format!("{label} — {keys}"))
    });
    let tools = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_sm().text_color(theme::accent()).child("Tools"))
        .children(tools)
        .child(
            div()
                .mt_2()
                .text_xs()
                .child("Shift: square / circle / 15° snap"),
        );
    type Rows = &'static [&'static str];
    let (edit, view, output): (Rows, Rows, Rows) = if animation {
        (
            &[
                "Drag empty canvas: marquee (fully contained)",
                "Shift+click: toggle selection · Ctrl+A: select all",
                "Drag selection: move · Arrows: nudge · Shift: ×10",
                "Delete / Backspace: delete · Ctrl+D: duplicate",
                "Ctrl+Z: undo · Ctrl+Shift+Z / Ctrl+Y: redo",
                "Ctrl+Shift+] / [: bring to front / send to back",
                "[ / ]: stroke size · F: toggle fill",
                "I: pick a color anywhere on screen",
                "Esc: cancel / cancel export / clear selection / close",
            ],
            &[
                "Middle drag / H: pan · Scroll: pan",
                "Ctrl+scroll / Ctrl++ / Ctrl+−: zoom",
                "Ctrl+0: fit · Ctrl+1: actual size",
                "? / F1: toggle help · Esc: close help",
            ],
            &[
                "Ctrl+C: copy animation · Ctrl+Shift+C: extract text",
                "Ctrl+S: save to recordings folder",
                "Ctrl+Shift+S: save a new copy",
                "Ctrl+W / Ctrl+Q: close (a running export finishes)",
            ],
        )
    } else {
        (
            &[
                "Drag empty canvas: marquee (fully contained)",
                "Shift+click: toggle selection · Ctrl+A: select all",
                "Drag selection: move · Arrows: nudge · Shift: ×10",
                "Delete / Backspace: delete · Ctrl+D: duplicate",
                "Ctrl+Z: undo · Ctrl+Shift+Z / Ctrl+Y: redo",
                "Ctrl+Shift+] / [: bring to front / send to back",
                "[ / ]: stroke size · F: toggle fill",
                "I: pick a color anywhere on screen",
                "Esc: cancel / clear selection / close editor",
            ],
            &[
                "Space+drag / middle drag / H: pan · Scroll: pan",
                "Ctrl+scroll / Ctrl++ / Ctrl+−: zoom",
                "Ctrl+0: fit · Ctrl+1: actual size",
                "? / F1: toggle help · Esc: close help",
            ],
            &[
                "Ctrl+C: copy image · Ctrl+Shift+C: extract text",
                "Ctrl+S: save · Ctrl+Shift+S: save new copy",
                "Ctrl+W / Ctrl+Q: close editor",
            ],
        )
    };
    let sections = div()
        .flex()
        .flex_col()
        .gap_3()
        .when(animation, |d| {
            d.child(help_section(
                "Animation",
                &[
                    "Space: play / pause",
                    "Click or drag the strip: scrub",
                    "Format button: GIF, WebP, AVIF or JPEG XL",
                    "Drawings apply to every frame; drawing pauses playback",
                ],
            ))
        })
        .child(help_section("Edit", edit))
        .child(help_section("View", view))
        .child(help_section(
            "Text",
            &[
                "Double-click / Enter on selected text: edit",
                "Enter: new line · Esc / click away: finish",
                "Ctrl+B / Ctrl+I: bold / italic",
                "Ctrl+Shift+> / <: increase / decrease font size",
            ],
        ))
        .child(help_section("Output", output));
    help_panel(viewport, scroll, [tools, sections])
}
