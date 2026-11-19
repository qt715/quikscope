//! The window that shows a color picked from the desktop (`quikscope pick`)
//! and copies it in any of the eyedropper's formats.

use std::rc::Rc;

use gpui::{
    Context, FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Render, ScrollHandle, StatefulInteractiveElement as _, Styled as _, Window,
    div, px,
};

use super::editor::EditorServices;
use super::eyedropper::{self, OnCopy, OnPick};
use super::theme;
use crate::color::Format;
use crate::document::Rgba;

/// Logical width that fits the panel with its formats in one row.
pub const WIDTH: f32 = eyedropper::PANEL_WIDTH + 32.0;
/// Logical height with the formats in one row.
const ONE_ROW_HEIGHT: f32 = 236.0;

/// The window size for a screen `screen_w` × `screen_h` logical pixels:
/// as wide as one row of formats needs where the screen allows, and tall
/// enough for the rows the formats wrap into at that width. Screens too
/// short for that, and later resizes, scroll instead.
pub fn size_for(screen_w: f32, screen_h: f32) -> (f32, f32) {
    let w = WIDTH.min(screen_w * 0.95);
    let rows = eyedropper::format_rows(w - 32.0);
    let h = ONE_ROW_HEIGHT + (rows - 1) as f32 * eyedropper::FORMAT_ROW;
    (w, h.min(screen_h * 0.95))
}

pub struct ColorWindow {
    color: Rgba,
    /// The last copy or pick outcome; errors are shown in red.
    pub status: Option<(String, bool)>,
    focus: FocusHandle,
    /// Scrolls the content when the window is shorter than it.
    pub scroll: ScrollHandle,
}

impl ColorWindow {
    pub fn new(color: Rgba, window: &mut Window, cx: &mut Context<Self>) -> ColorWindow {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        super::close_natively_through_task(window, cx);
        ColorWindow {
            color,
            status: None,
            focus,
            scroll: ScrollHandle::new(),
        }
    }

    pub fn color(&self) -> Rgba {
        self.color
    }

    pub fn set_color(&mut self, color: Rgba, cx: &mut Context<Self>) {
        self.color = color;
        self.status = None;
        cx.notify();
    }

    pub fn copy(&mut self, format: Format, cx: &mut Context<Self>) {
        let clipboard = cx
            .try_global::<EditorServices>()
            .and_then(|s| s.clipboard.clone());
        self.status = Some(match clipboard {
            Some(clipboard) => {
                let text = format.format(self.color);
                clipboard.set_text(&text);
                (format!("Copied {text}"), false)
            }
            None => ("Clipboard is unavailable".into(), true),
        });
        cx.notify();
    }

    /// Picks again from anywhere on the desktop, replacing this color.
    pub fn pick_again(&mut self, cx: &mut Context<Self>) {
        let Some(pick) = cx
            .try_global::<EditorServices>()
            .and_then(|s| s.pick_color.clone())
        else {
            self.status = Some(("Picking from the screen is unavailable".into(), true));
            cx.notify();
            return;
        };
        let task = pick(cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |w, cx| match result {
                Ok(Some(color)) => w.set_color(color, cx),
                Ok(None) => {}
                Err(error) => {
                    w.status = Some((format!("{error:#}"), true));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let command = ks.modifiers.control || ks.modifiers.platform;
        match ks.key.as_str() {
            "escape" => super::close_window(window, cx),
            "c" if command => self.copy(Format::Hex, cx),
            _ => return,
        }
        cx.stop_propagation();
    }
}

impl Focusable for ColorWindow {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ColorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let on_copy: OnCopy = Rc::new(move |format, _, cx| {
            let _ = this.update(cx, |w, cx| w.copy(format, cx));
        });
        let this = cx.entity().downgrade();
        let on_pick: OnPick = Rc::new(move |_, cx| {
            let _ = this.update(cx, |w, cx| w.pick_again(cx));
        });
        let width = window.viewport_size().width - px(32.);
        let (status, error) = self.status.clone().unwrap_or_default();
        div()
            .id("color-window")
            .key_context("ColorWindow")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .bg(theme::canvas_bg())
            .font_family(super::UI_FONT)
            .text_color(theme::text())
            .child(
                eyedropper::panel(
                    self.color,
                    width,
                    "Click a value to copy it · Ctrl+C copies HEX",
                    "Pick again",
                    on_copy,
                    on_pick,
                )
                .flex_shrink_0(),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .h(px(16.))
                    .text_xs()
                    .text_color(if error {
                        gpui::rgb(0xe03131)
                    } else {
                        theme::muted()
                    })
                    .child(status),
            )
    }
}
