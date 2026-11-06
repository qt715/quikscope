//! Floating Excalidraw-style toolbar: tools, palette, stroke size, fill,
//! and history/output actions.

use gpui::{
    AnyElement, AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px,
    svg,
};

use super::editor::{Editor, Tool};
use super::{hsla, theme};
use crate::document::PALETTE;

/// Every icon the toolbar references (checked against the asset table in tests).
pub const ICONS: [&str; 18] = [
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
    "save",
];

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

fn button(
    id: &'static str,
    tip: &'static str,
    active: bool,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .tooltip(move |_, cx| cx.new(|_| Tip(tip)).into())
        .when(active, |d| d.bg(theme::accent_bg()))
        .when(enabled && !active, |d| d.hover(|s| s.bg(theme::hover())))
        .when(!enabled, |d| d.opacity(0.35))
}

fn separator() -> impl IntoElement {
    div().w(px(1.)).h(px(20.)).mx_1().bg(theme::panel_border())
}

pub fn toolbar(editor: &Editor, cx: &mut Context<Editor>) -> impl IntoElement {
    let doc = editor.doc.read(cx);
    let (can_undo, can_redo) = (doc.can_undo(), doc.can_redo());
    let style = editor.style;

    let tools: Vec<AnyElement> = Tool::ALL
        .iter()
        .map(|&tool| {
            let active = editor.tool == tool;
            let digit = tool.digit();
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
                .children(digit.map(|d| {
                    div()
                        .absolute()
                        .bottom(px(1.))
                        .right(px(3.))
                        .text_size(px(8.))
                        .text_color(theme::muted())
                        .child(d.to_string())
                }))
                .into_any_element()
        })
        .collect();

    let mut swatches: Vec<AnyElement> = PALETTE
        .iter()
        .enumerate()
        .map(|(i, &color)| {
            let active = style.color == color;
            div()
                .id(("color", i))
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .when(active, |d| d.border_2().border_color(theme::accent()))
                .on_click(cx.listener(move |e, _, _, cx| e.set_color(color, cx)))
                .child(
                    div()
                        .size(px(14.))
                        .rounded_full()
                        .bg(hsla(color))
                        .border_1()
                        .border_color(theme::panel_border()),
                )
                .into_any_element()
        })
        .collect();
    // Any other color (picked in the color picker) shows here; clicking opens
    // the picker.
    let custom = !PALETTE.contains(&style.color);
    swatches.push(
        div()
            .id("color-custom")
            .size(px(26.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .tooltip(|_, cx| cx.new(|_| Tip("Custom color")).into())
            .when(custom || editor.picker_open(), |d| {
                d.border_2().border_color(theme::accent())
            })
            .hover(|d| d.bg(theme::hover()))
            .on_click(
                cx.listener(|e, ev: &gpui::ClickEvent, _, cx| e.toggle_picker(ev.position().x, cx)),
            )
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

    let sizes: Vec<AnyElement> = (0u8..3)
        .map(|size| {
            let dot = [4.0, 7.0, 11.0][size as usize];
            let (id, tip) = [
                ("size-s", "Thin ([)"),
                ("size-m", "Medium"),
                ("size-l", "Thick (])"),
            ][size as usize];
            button(id, tip, style.size == size, true)
                .size(px(26.))
                .on_click(cx.listener(move |e, _, _, cx| e.set_size(size, cx)))
                .child(div().size(px(dot)).rounded_full().bg(theme::text()))
                .into_any_element()
        })
        .collect();

    let bar = div()
        .id("toolbar")
        .occlude()
        .flex()
        // Narrow windows wrap the toolbar onto a second row instead of
        // clipping tools off both edges.
        .flex_wrap()
        .justify_center()
        .max_w_full()
        .items_center()
        .gap(px(2.))
        .p(px(4.))
        .rounded_lg()
        .bg(theme::panel())
        .border_1()
        .border_color(theme::panel_border())
        .shadow_lg()
        .children(tools)
        .child(separator())
        .children(swatches)
        .child(separator())
        .children(sizes)
        .child(
            button("fill", "Fill shapes (F)", style.fill, true)
                .on_click(cx.listener(|e, _, _, cx| e.toggle_fill(cx)))
                .child(icon(
                    "paint-bucket",
                    if style.fill {
                        theme::accent()
                    } else {
                        theme::text()
                    },
                )),
        )
        .child(separator())
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
        .child(
            button("copy", "Copy image (Ctrl+C)", false, true)
                .on_click(cx.listener(|e, _, _, cx| e.copy(cx)))
                .child(icon("copy", theme::text())),
        )
        .child(
            button("save", "Save to screenshots folder (Ctrl+S)", false, true)
                .on_click(cx.listener(|e, _, window, cx| e.save(false, window, cx)))
                .child(icon("save", theme::accent())),
        );

    div()
        .absolute()
        .top_3()
        .left_0()
        .right_0()
        .px_2()
        .flex()
        .justify_center()
        .child(bar)
}
