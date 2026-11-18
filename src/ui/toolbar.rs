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

/// Every icon the toolbar references (checked against the asset table in tests).
pub const ICONS: [&str; 20] = [
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
];

/// Distance from the window edges to every panel.
pub const EDGE: f32 = 10.0;
const BUTTON: f32 = 32.0;
/// Between neighbouring buttons.
const GAP: f32 = 2.0;
const PAD: f32 = 4.0;
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
}

fn icon(name: &'static str, color: gpui::Rgba) -> impl IntoElement {
    svg()
        .path(format!("icons/{name}.svg"))
        .size(px(16.))
        .text_color(color)
}

struct Tip(&'static str);

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
fn base(id: &'static str, tip: &'static str) -> gpui::Stateful<gpui::Div> {
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

fn button(
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

fn separator() -> impl IntoElement {
    div()
        .flex_shrink_0()
        .w(px(1.))
        .h(px(20.))
        .mx_1()
        .bg(theme::panel_border())
}

/// A floating panel; `probe` records where it was laid out.
fn panel(id: &'static str, probe: &ScrollHandle) -> gpui::Stateful<gpui::Div> {
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
            button("copy", "Copy image (Ctrl+C)", false, true)
                .on_click(cx.listener(|e, _, _, cx| e.copy(cx)))
                .child(icon("copy", theme::text())),
        )
        .child(
            // The one primary action: saving is what most edits end with.
            base("save", "Save to screenshots folder (Ctrl+S)")
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

pub fn shortcut_help(
    viewport: gpui::Size<gpui::Pixels>,
    scroll: &gpui::ScrollHandle,
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
    let section = |title: &'static str, rows: &'static [&'static str]| {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_sm().text_color(theme::accent()).child(title))
            .children(rows.iter().map(|row| div().text_xs().child(*row)))
    };
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
                        .child(
                            div()
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
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_3()
                                .child(section(
                                    "Edit",
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
                                ))
                                .child(section(
                                    "View",
                                    &[
                                        "Space+drag / middle drag / H: pan · Scroll: pan",
                                        "Ctrl+scroll / Ctrl++ / Ctrl+−: zoom",
                                        "Ctrl+0: fit · Ctrl+1: actual size",
                                        "? / F1: toggle help · Esc: close help",
                                    ],
                                ))
                                .child(section(
                                    "Text",
                                    &[
                                        "Double-click / Enter on selected text: edit",
                                        "Enter: new line · Esc / click away: finish",
                                        "Ctrl+B / Ctrl+I: bold / italic",
                                        "Ctrl+Shift+> / <: increase / decrease font size",
                                    ],
                                ))
                                .child(section(
                                    "Output",
                                    &[
                                        "Ctrl+C: copy image · Ctrl+Shift+C: extract text",
                                        "Ctrl+S: save · Ctrl+Shift+S: save new copy",
                                        "Ctrl+W / Ctrl+Q: close editor",
                                    ],
                                )),
                        ),
                ),
        )
}
