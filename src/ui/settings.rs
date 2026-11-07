//! Live settings and the short first-run setup. Persistence is deliberately
//! shared with the command-line controls: invalid existing files are never replaced.
use std::{path::PathBuf, sync::Arc};

use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Render, ScrollHandle, StatefulInteractiveElement as _,
    Styled as _, Window, actions, div, prelude::FluentBuilder as _, px,
};
use toml_edit::Value;

use super::{
    UI_FONT,
    field::{Field, FieldEvent, FieldKind},
    theme,
};
use crate::config::{self, Config};

actions!(settings, [OpenSettings]);

type Apply = Arc<dyn Fn(Arc<Config>, &mut App)>;

fn configuration_error(path: &std::path::Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Config::parse(&text)
            .err()
            .map(|error| format!("{}: {error:#}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(format!("{}: {error}", path.display())),
    }
}

struct Input {
    id: &'static str,
    label: &'static str,
    path: &'static [&'static str],
    field: Entity<Field>,
    saved: String,
}

pub struct Settings {
    config: Arc<Config>,
    path: PathBuf,
    welcome: bool,
    apply: Apply,
    error: Option<String>,
    inputs: Vec<Input>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    #[cfg(windows)]
    bindings: [String; 4],
}

impl Settings {
    pub fn new(
        config: Arc<Config>,
        path: PathBuf,
        welcome: bool,
        apply: Apply,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let error = configuration_error(&path);
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            weak.update(cx, |this, cx| this.commit_close(cx))
                .unwrap_or(true)
        });
        let mut this = Self {
            #[cfg(windows)]
            bindings: [
                config.hotkeys.region.clone(),
                config.hotkeys.screen.clone(),
                config.hotkeys.window.clone(),
                config.hotkeys.open_last.clone(),
            ],
            config,
            path,
            welcome,
            apply,
            error,
            inputs: Vec::new(),
            scroll: ScrollHandle::new(),
            focus,
        };
        if !welcome {
            let c = this.config.clone();
            for (id, label, path, text) in [
                (
                    "save-dir",
                    "Save directory (~ expands to home)",
                    &["save_dir"][..],
                    c.save_dir.clone(),
                ),
                (
                    "filename",
                    "Filename (%Y %m %d %H %M %S %s and %%)",
                    &["filename"][..],
                    c.filename.clone(),
                ),
                (
                    "notification-timeout",
                    "Notification duration (ms; -1 = system default)",
                    &["notification_timeout_ms"][..],
                    c.notification_timeout_ms.to_string(),
                ),
                (
                    "color",
                    "Annotation color (#rrggbb or #rrggbbaa)",
                    &["editor", "color"][..],
                    c.editor.color.clone(),
                ),
                (
                    "stroke",
                    "Stroke (0 thin, 1 medium, 2 thick)",
                    &["editor", "stroke"][..],
                    c.editor.stroke.to_string(),
                ),
                (
                    "font-family",
                    "Font family",
                    &["editor", "font_family"][..],
                    c.editor.font_family.clone(),
                ),
                (
                    "font-size",
                    "Font size (4–512 pixels)",
                    &["editor", "font_size"][..],
                    c.editor.font_size.to_string(),
                ),
                (
                    "font-weight",
                    "Font weight (1–1000)",
                    &["editor", "font_weight"][..],
                    c.editor.font_weight.to_string(),
                ),
            ] {
                this.add_input(id, label, path, text, window, cx);
            }
            if crate::ocr::tess::version().is_ok() {
                this.add_input(
                    "ocr-languages",
                    "OCR languages (comma separated; empty = automatic)",
                    &["ocr", "languages"],
                    c.ocr.languages.join(", "),
                    window,
                    cx,
                );
            }
        }
        this
    }

    fn add_input(
        &mut self,
        id: &'static str,
        label: &'static str,
        path: &'static [&'static str],
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = cx.new(|cx| {
            let mut field = Field::new(FieldKind::Text, 0., window, cx).submit_on_blur();
            field.set_text(text.clone(), window, cx);
            field
        });
        let index = self.inputs.len();
        cx.subscribe(&field, move |this, field, event, cx| match event {
            FieldEvent::Submit(text) => this.submit(index, text, cx),
            FieldEvent::Cancel => {
                let saved = this.inputs[index].saved.clone();
                field.update(cx, |field, cx| field.restore_text(saved, cx));
                this.error = configuration_error(&this.path);
                cx.notify();
            }
            FieldEvent::Changed(_) => {}
        })
        .detach();
        self.inputs.push(Input {
            id,
            label,
            path,
            field,
            saved: text,
        });
    }

    /// Synchronize IPC/tray changes without replacing an in-progress field edit.
    pub(crate) fn refresh_config(
        &mut self,
        config: Arc<Config>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let mut changed = self.config != config;
        for input in &mut self.inputs {
            let text = match input.path {
                ["save_dir"] => config.save_dir.clone(),
                ["filename"] => config.filename.clone(),
                ["notification_timeout_ms"] => config.notification_timeout_ms.to_string(),
                ["editor", "color"] => config.editor.color.clone(),
                ["editor", "stroke"] => config.editor.stroke.to_string(),
                ["editor", "font_family"] => config.editor.font_family.clone(),
                ["editor", "font_size"] => config.editor.font_size.to_string(),
                ["editor", "font_weight"] => config.editor.font_weight.to_string(),
                ["ocr", "languages"] => config.ocr.languages.join(", "),
                _ => unreachable!("settings input path"),
            };
            let field = input.field.read(cx);
            if text != input.saved && !field.is_focused(window) {
                let clean = field.text() == input.saved;
                input.saved = text.clone();
                if clean {
                    input
                        .field
                        .update(cx, |field, cx| field.set_text(text, window, cx));
                }
                changed = true;
            }
        }
        self.config = config;
        if changed {
            cx.notify();
        }
    }

    /// The currently displayed persistence/validation error, also rendered inline.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Painted window-local bounds for a capture choice or text input.
    /// Automation can inspect geometry without accessing mutable UI state.
    pub fn control_bounds(&self, id: &str) -> Option<gpui::Bounds<gpui::Pixels>> {
        let choices = [
            "freeze",
            "live",
            "clipboard",
            "notify",
            "open-editor",
            "cursor",
            "close-on-save",
            "font-italic",
        ];
        let (index, field) = if let Some(index) = choices.iter().position(|key| *key == id) {
            if self.welcome && index >= 6 {
                return None;
            }
            (index, false)
        } else {
            (
                8 + self.inputs.iter().position(|input| input.id == id)?,
                true,
            )
        };
        let mut bounds = self.scroll.bounds_for_item(index)?;
        bounds.origin += self.scroll.offset();
        if field {
            bounds.origin.y = bounds.bottom() - px(24.);
            bounds.size.height = px(24.);
        }
        Some(bounds)
    }

    fn input_value(path: &[&str], text: &str) -> anyhow::Result<Value> {
        Ok(match path {
            ["notification_timeout_ms"] => Value::from(text.parse::<i32>()? as i64),
            ["editor", "stroke"] => {
                let n = text.parse::<u8>()?;
                anyhow::ensure!(n <= 2, "stroke must be 0, 1 or 2");
                Value::from(n as i64)
            }
            ["editor", "font_weight"] => Value::from(text.parse::<u16>()? as i64),
            ["editor", "font_size"] => Value::from(text.parse::<f64>()?),
            ["ocr", "languages"] => {
                let mut array = toml_edit::Array::new();
                if !text.trim().is_empty() {
                    for lang in text.split(',') {
                        array.push(lang.trim());
                    }
                }
                Value::Array(array)
            }
            _ => Value::from(text),
        })
    }

    fn persist(&mut self, values: &[(&[&str], Value)], cx: &mut Context<Self>) -> bool {
        match config::persist_values(&self.path, values) {
            Ok(config) => {
                self.config = Arc::new(config);
                self.error = None;
                (self.apply)(self.config.clone(), cx);
                cx.notify();
                true
            }
            Err(e) => {
                self.error = Some(format!("Not saved: {e:#}"));
                cx.notify();
                false
            }
        }
    }

    fn submit(&mut self, index: usize, text: &str, cx: &mut Context<Self>) {
        let input = &self.inputs[index];
        if input.saved == text {
            self.error = configuration_error(&self.path);
            cx.notify();
            return;
        }
        let path = input.path;
        match Self::input_value(path, text) {
            Ok(value) => {
                if self.persist(&[(path, value)], cx) {
                    self.inputs[index].saved = text.to_owned();
                }
            }
            Err(e) => {
                self.error = Some(format!("{}: {e:#}", input.label));
                cx.notify();
            }
        }
    }

    fn toggle(&mut self, key: &'static str, cx: &mut Context<Self>) {
        let mut next = (*self.config).clone();
        let (path, value): (&[&str], bool) = match key {
            "freeze" => {
                next.freeze = true;
                (&["freeze"], true)
            }
            "live" => {
                next.freeze = false;
                (&["freeze"], false)
            }
            "clipboard" => {
                next.copy_to_clipboard = !next.copy_to_clipboard;
                (&["copy_to_clipboard"], next.copy_to_clipboard)
            }
            "notify" => {
                next.notify = !next.notify;
                (&["notify"], next.notify)
            }
            "open-editor" => {
                next.open_editor = !next.open_editor;
                (&["open_editor"], next.open_editor)
            }
            "cursor" => {
                next.cursor = !next.cursor;
                (&["cursor"], next.cursor)
            }
            "close-on-save" => {
                next.editor.close_on_save = !next.editor.close_on_save;
                (&["editor", "close_on_save"], next.editor.close_on_save)
            }
            "font-italic" => {
                next.editor.font_italic = !next.editor.font_italic;
                (&["editor", "font_italic"], next.editor.font_italic)
            }
            _ => unreachable!(),
        };
        if self.welcome {
            self.config = Arc::new(next);
            (self.apply)(self.config.clone(), cx);
            cx.notify();
        } else {
            self.persist(&[(path, Value::from(value))], cx);
        }
    }

    // Native close and the Done/Close button use this exact transaction.
    fn commit_close(&mut self, cx: &mut Context<Self>) -> bool {
        if self.welcome {
            let c = &self.config;
            self.persist(
                &[
                    (&["freeze"], Value::from(c.freeze)),
                    (&["copy_to_clipboard"], Value::from(c.copy_to_clipboard)),
                    (&["notify"], Value::from(c.notify)),
                    (&["open_editor"], Value::from(c.open_editor)),
                    (&["cursor"], Value::from(c.cursor)),
                ],
                cx,
            )
        } else {
            // Native close need not generate a blur event first.
            let mut values = Vec::new();
            for input in &self.inputs {
                let text = input.field.read(cx).text();
                if text != input.saved {
                    match Self::input_value(input.path, text) {
                        Ok(value) => values.push((input.path, value)),
                        Err(e) => {
                            self.error = Some(format!("{}: {e:#}", input.label));
                            cx.notify();
                            return false;
                        }
                    }
                }
            }
            if values.is_empty() {
                true
            } else {
                self.persist(&values, cx)
            }
        }
    }

    fn choice(
        &self,
        id: &'static str,
        label: &'static str,
        hint: &'static str,
        on: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex_none()
            .w_full()
            .min_h(px(52.))
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(if on {
                theme::accent()
            } else {
                theme::panel_border()
            })
            .bg(if on {
                theme::accent_bg()
            } else {
                theme::panel()
            })
            .cursor_pointer()
            .hover(|d| d.bg(theme::hover()))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(id, cx)))
            .child(
                div()
                    .line_height(px(18.))
                    .child(format!("{} {label}", if on { "●" } else { "○" })),
            )
            .child(
                div()
                    .text_xs()
                    .line_height(px(16.))
                    .text_color(theme::muted())
                    .child(hint),
            )
    }

    #[cfg(windows)]
    fn shortcut(
        &self,
        index: usize,
        key: &'static str,
        label: &'static str,
        binding: &str,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let on = !binding.is_empty();
        let shown = if on { binding } else { &self.bindings[index] };
        div()
            .id(key)
            .flex_none()
            .p_2()
            .rounded_md()
            .cursor_pointer()
            .hover(|d| d.bg(theme::hover()))
            .child(format!(
                "{} {label}: {}",
                if on { "●" } else { "○" },
                if shown.is_empty() {
                    "not configured"
                } else {
                    shown
                }
            ))
            .on_click(cx.listener(move |this, _, _, cx| {
                let defaults = crate::config::Hotkeys::default();
                let fallback = [
                    &defaults.region,
                    &defaults.screen,
                    &defaults.window,
                    &defaults.open_last,
                ];
                let current = [
                    &this.config.hotkeys.region,
                    &this.config.hotkeys.screen,
                    &this.config.hotkeys.window,
                    &this.config.hotkeys.open_last,
                ][index]
                    .clone();
                let value = if current.is_empty() {
                    if this.bindings[index].is_empty() {
                        fallback[index].clone()
                    } else {
                        this.bindings[index].clone()
                    }
                } else {
                    String::new()
                };
                if this.persist(&[(&["hotkeys", key], Value::from(value))], cx)
                    && !current.is_empty()
                {
                    this.bindings[index] = current;
                }
            }))
    }
}

impl Render for Settings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = &self.config;
        let mut body = div()
            .id("settings-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(self.choice(
                "freeze",
                "Freeze desktop",
                "Select from a still image captured before selection starts.",
                c.freeze,
                cx,
            ))
            .child(self.choice(
                "live",
                "Live desktop",
                "Select over the moving desktop; capture after selection finishes.",
                !c.freeze,
                cx,
            ))
            .child(self.choice(
                "clipboard",
                "Copy to clipboard",
                "Copy each capture as an image.",
                c.copy_to_clipboard,
                cx,
            ))
            .child(self.choice(
                "notify",
                "Desktop notifications",
                "Show a notification after each capture.",
                c.notify,
                cx,
            ))
            .child(self.choice(
                "open-editor",
                "Open editor",
                "Open the annotation editor immediately after capture.",
                c.open_editor,
                cx,
            ))
            .child(self.choice(
                "cursor",
                "Include cursor",
                "Include the pointer in captured images.",
                c.cursor,
                cx,
            ));
        if !self.welcome {
            body = body
                .child(self.choice(
                    "close-on-save",
                    "Close after saving",
                    "Close the editor after a successful save.",
                    c.editor.close_on_save,
                    cx,
                ))
                .child(self.choice(
                    "font-italic",
                    "Italic text",
                    "Start new text annotations in italic.",
                    c.editor.font_italic,
                    cx,
                ));
            for input in &self.inputs {
                body = body.child(
                    div()
                        .id(input.id)
                        .flex_none()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme::muted())
                                .child(input.label),
                        )
                        .child(input.field.clone()),
                );
            }
            if !self.inputs.iter().any(|input| input.id == "ocr-languages") {
                body = body.child(div().text_xs().text_color(theme::muted()).child(
                    "OCR languages: install Tesseract and language data to enable text extraction.",
                ));
            }
            #[cfg(windows)]
            {
                body = body.child(div().child("Global shortcuts (click to enable/disable)"));
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(crate::platform::windows::PRINT_SCREEN_NOTICE),
                );
                for (index, key, label, binding) in [
                    (0, "region", "Region", &c.hotkeys.region),
                    (1, "screen", "Screen", &c.hotkeys.screen),
                    (2, "window", "Window", &c.hotkeys.window),
                    (3, "open_last", "Open last", &c.hotkeys.open_last),
                ] {
                    body = body.child(self.shortcut(index, key, label, binding, cx));
                }
            }
        }
        div().id("settings").size_full().flex().flex_col().bg(theme::panel())
            .font_family(UI_FONT).text_sm().text_color(theme::text()).track_focus(&self.focus)
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                window.focus(&this.focus, cx);
            }))
            .child(div().flex_none().min_h(px(88.)).p_4().border_b_1().border_color(theme::panel_border())
                .child(if self.welcome { "Welcome to quickscope" } else { "Settings" })
                .child(div().text_xs().text_color(theme::muted()).child(if self.welcome {
                    "Choose how to capture. Changes are saved when you finish or close this window."
                } else { "Changes apply immediately. Text fields save on Enter, Tab or leaving the field." })))
            .child(body)
            .when_some(self.error.clone(), |d, error| d.child(div().id("settings-error")
                .flex_none().max_h(px(120.)).overflow_y_scroll().p_2().text_xs()
                .text_color(gpui::rgb(0xff9999)).child(error)))
            .child(div().flex_none().p_3().border_t_1().border_color(theme::panel_border())
                .child(div().id("settings-done").p_2().rounded_md().bg(theme::accent_bg())
                    .cursor_pointer().hover(|d| d.bg(theme::hover()))
                    .child(if self.welcome { "Done" } else { "Close" })
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.commit_close(cx) { window.remove_window(); }
                    }))))
    }
}
