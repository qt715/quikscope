//! A minimal single-line input for the color picker and font menu.
//!
//! Clicking selects the whole value so typing replaces it; Enter, Tab or
//! losing focus submits. Number fields step with the arrow keys.

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, ParentElement as _, Pixels, Render, SharedString,
    Styled as _, Window, div, prelude::FluentBuilder as _, px,
};

use super::theme;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FieldKind {
    /// Integer in `min..=max`.
    Number { min: i64, max: i64 },
    /// Hex color, `#` optional.
    Hex,
    /// Free text (search boxes).
    Text,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FieldEvent {
    /// The text changed while typing.
    Changed(String),
    /// Enter, Tab, an arrow step, or focus left the field.
    Submit(String),
    /// Escape.
    Cancel,
}

pub struct Field {
    kind: FieldKind,
    text: String,
    /// The whole value is selected; the next character replaces it.
    selected: bool,
    focus: FocusHandle,
    width: Pixels,
    placeholder: SharedString,
}

impl EventEmitter<FieldEvent> for Field {}

/// Applies one key to `text` for a field of `kind`. Returns the new text
/// and whether the key was consumed; pure so it can be tested directly.
pub fn apply_key(
    kind: FieldKind,
    text: &str,
    selected: bool,
    key: &str,
    key_char: Option<&str>,
    shift: bool,
) -> Option<(String, bool)> {
    match key {
        "backspace" => {
            if selected {
                return Some((String::new(), false));
            }
            let mut t = text.to_string();
            t.pop();
            Some((t, false))
        }
        "up" | "down" => {
            let FieldKind::Number { min, max } = kind else {
                return None;
            };
            let step = if shift { 10 } else { 1 } * if key == "up" { 1 } else { -1 };
            let v = text.parse::<i64>().unwrap_or(min).saturating_add(step);
            Some((v.clamp(min, max).to_string(), true))
        }
        _ => {
            let typed = key_char?;
            if typed.is_empty() || typed.chars().any(char::is_control) {
                return None;
            }
            let base = if selected { "" } else { text };
            let next = format!("{base}{typed}");
            let ok = match kind {
                FieldKind::Number { max, .. } => {
                    typed.chars().all(|c| c.is_ascii_digit()) && next.len() <= max.to_string().len()
                }
                FieldKind::Hex => {
                    typed.chars().all(|c| c.is_ascii_hexdigit() || c == '#')
                        && next.trim_start_matches('#').len() <= 8
                }
                FieldKind::Text => true,
            };
            // Rejected characters are still consumed so they never reach the
            // editor's tool shortcuts.
            Some((if ok { next } else { text.to_string() }, false))
        }
    }
}

impl Field {
    pub fn new(kind: FieldKind, width: f32, window: &mut Window, cx: &mut Context<Self>) -> Field {
        let focus = cx.focus_handle();
        cx.on_blur(&focus, window, |this: &mut Field, _, cx| {
            this.selected = false;
            if this.kind != FieldKind::Text {
                cx.emit(FieldEvent::Submit(this.text.clone()));
            }
            cx.notify();
        })
        .detach();
        Field {
            kind,
            text: String::new(),
            selected: false,
            focus,
            width: px(width),
            placeholder: SharedString::default(),
        }
    }

    pub fn placeholder(mut self, text: &'static str) -> Self {
        self.placeholder = text.into();
        self
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    /// Replaces the value unless the user is typing in it.
    pub fn set_text(&mut self, text: impl Into<String>, window: &Window, cx: &mut Context<Self>) {
        if self.focus.is_focused(window) {
            return;
        }
        let text = text.into();
        if text != self.text {
            self.text = text;
            cx.notify();
        }
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.text.clear();
        cx.notify();
    }

    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = true;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if ks.modifiers.control || ks.modifiers.platform || ks.modifiers.alt {
            if ks.key == "a" {
                self.selected = true;
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }
        match ks.key.as_str() {
            "enter" | "tab" => {
                self.selected = true;
                cx.emit(FieldEvent::Submit(self.text.clone()));
            }
            "escape" => cx.emit(FieldEvent::Cancel),
            key => {
                let Some((text, submit)) = apply_key(
                    self.kind,
                    &self.text,
                    self.selected,
                    key,
                    ks.key_char.as_deref(),
                    ks.modifiers.shift,
                ) else {
                    // Arrows in text fields drive the list the field filters.
                    return;
                };
                self.selected = submit;
                if text != self.text {
                    self.text = text;
                    cx.emit(FieldEvent::Changed(self.text.clone()));
                }
                if submit {
                    cx.emit(FieldEvent::Submit(self.text.clone()));
                }
            }
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Focusable for Field {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Field {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus.is_focused(window);
        let empty = self.text.is_empty();
        div()
            .track_focus(&self.focus)
            // Width 0 means "fill the parent".
            .when(self.width > px(0.), |d| d.w(self.width))
            .when(self.width <= px(0.), |d| d.w_full())
            .h(px(24.))
            .px_1p5()
            .flex()
            .items_center()
            .rounded_md()
            .bg(gpui::rgba(0x0000_0040))
            .border_1()
            .border_color(if focused {
                theme::accent()
            } else {
                theme::panel_border()
            })
            .text_xs()
            .text_color(theme::text())
            .cursor_text()
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.focus(window, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .when(focused && self.selected && !empty, |d| {
                        d.bg(theme::accent_bg()).rounded_sm()
                    })
                    .when(empty, |d| d.text_color(theme::muted()))
                    .child(if empty {
                        self.placeholder.clone()
                    } else {
                        SharedString::from(self.text.clone())
                    }),
            )
            .when(focused && !self.selected, |d| {
                d.child(div().w(px(1.)).h(px(12.)).bg(theme::text()))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BYTE: FieldKind = FieldKind::Number { min: 0, max: 255 };

    #[test]
    fn number_fields_accept_digits_within_length() {
        assert_eq!(
            apply_key(BYTE, "12", false, "3", Some("3"), false),
            Some(("123".into(), false))
        );
        assert_eq!(
            apply_key(BYTE, "123", false, "4", Some("4"), false),
            Some(("123".into(), false))
        );
        assert_eq!(
            apply_key(BYTE, "12", false, "x", Some("x"), false),
            Some(("12".into(), false))
        );
        assert_eq!(
            apply_key(BYTE, "12", true, "7", Some("7"), false),
            Some(("7".into(), false))
        );
    }

    #[test]
    fn arrows_step_and_clamp_numbers() {
        assert_eq!(
            apply_key(BYTE, "250", false, "up", None, true),
            Some(("255".into(), true))
        );
        assert_eq!(
            apply_key(BYTE, "3", false, "down", None, true),
            Some(("0".into(), true))
        );
        assert_eq!(
            apply_key(BYTE, "", false, "up", None, false),
            Some(("1".into(), true))
        );
        assert_eq!(
            apply_key(FieldKind::Text, "a", false, "down", None, false),
            None
        );
    }

    #[test]
    fn hex_fields_take_hex_digits_only() {
        let k = FieldKind::Hex;
        assert_eq!(
            apply_key(k, "#ab", false, "c", Some("c"), false),
            Some(("#abc".into(), false))
        );
        assert_eq!(
            apply_key(k, "#ab", false, "z", Some("z"), false),
            Some(("#ab".into(), false))
        );
        assert_eq!(
            apply_key(k, "#12345678", false, "9", Some("9"), false),
            Some(("#12345678".into(), false))
        );
    }

    #[test]
    fn backspace_clears_a_selection_or_deletes_one_char() {
        assert_eq!(
            apply_key(BYTE, "128", true, "backspace", None, false),
            Some((String::new(), false))
        );
        assert_eq!(
            apply_key(BYTE, "128", false, "backspace", None, false),
            Some(("12".into(), false))
        );
    }
}
