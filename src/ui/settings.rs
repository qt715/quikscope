//! Live settings and the short first-run setup. Persistence is deliberately
//! shared with the command-line controls: invalid existing files are never replaced.
use std::{collections::HashMap, path::PathBuf, sync::Arc};

use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, FontWeight,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Render, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, Window, actions, div,
    prelude::FluentBuilder as _, px, svg,
};
use toml_edit::Value;

use super::{
    UI_FONT,
    field::{Field, FieldEvent, FieldKind},
    theme,
};
use crate::config::{self, Config};
use crate::media::{AudioDevice, DEFAULT_INPUT, DEFAULT_OUTPUT, OutputFormat};

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

fn input_row(input: &Input) -> AnyElement {
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
        .child(input.field.clone())
        .into_any_element()
}

/// The audio devices recordings can include, listed off the UI thread
/// because PulseAudio/WASAPI/Core Audio enumeration can block.
enum AudioDevices {
    Loading,
    Ready(Vec<AudioDevice>),
    Failed(String),
}

/// One choice of a segmented recording setting.
struct Segment {
    key: String,
    label: String,
    value: Value,
    on: bool,
}

/// Segmented recording settings: element id, label and config path.
const SEGMENTED: [(&str, &str, &[&str]); 5] = [
    (
        "record-video-format",
        "Video format",
        &["record", "video_format"],
    ),
    (
        "record-video-fps",
        "Video frame rate",
        &["record", "video_fps"],
    ),
    (
        "record-animation-format",
        "Animation format",
        &["record", "animation_format"],
    ),
    (
        "record-animation-fps",
        "Animation frame rate",
        &["record", "animation_fps"],
    ),
    ("record-encoder", "Encoder", &["record", "encoder"]),
];

pub struct Settings {
    config: Arc<Config>,
    path: PathBuf,
    welcome: bool,
    apply: Apply,
    error: Option<String>,
    inputs: Vec<Input>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    audio_devices: AudioDevices,
    /// Ids of the scrolled body's children in render order, for
    /// [`Settings::control_bounds`].
    items: Vec<&'static str>,
    /// Records the option bounds of segmented settings and the audio list.
    rows: HashMap<&'static str, ScrollHandle>,
    #[cfg(any(windows, target_os = "macos"))]
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
            #[cfg(any(windows, target_os = "macos"))]
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
            audio_devices: AudioDevices::Loading,
            items: Vec::new(),
            rows: SEGMENTED
                .iter()
                .map(|(id, ..)| *id)
                .chain(["record-audio"])
                .map(|id| (id, ScrollHandle::new()))
                .collect(),
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
            this.add_input(
                "record-save-dir",
                "Recordings folder (~ expands to home)",
                &["record", "save_dir"],
                c.record.save_dir.clone(),
                window,
                cx,
            );
            let devices = cx.background_spawn(async { crate::media::audio::devices() });
            cx.spawn(async move |this, cx| {
                let result = devices.await;
                let _ = this.update(cx, |this, cx| {
                    this.audio_devices = match result {
                        Ok(devices) => AudioDevices::Ready(devices),
                        Err(e) => {
                            log::warn!("listing audio devices: {e:#}");
                            AudioDevices::Failed(format!("{e:#}"))
                        }
                    };
                    cx.notify();
                });
            })
            .detach();
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
                ["record", "save_dir"] => config.record.save_dir.clone(),
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

    /// Painted window-local bounds of a setting: a choice or text input by
    /// id, a segmented option as `<setting>:<value>` (`record-video-fps:30`)
    /// and an audio device as `record-audio:<device id>`. Automation can
    /// inspect geometry without accessing mutable UI state.
    pub fn control_bounds(&self, id: &str) -> Option<gpui::Bounds<gpui::Pixels>> {
        if let Some((row, option)) = id.split_once(':') {
            let index = if row == "record-audio" {
                self.audio_rows().iter().position(|d| d.id == option)?
            } else {
                self.segments(row)?.iter().position(|s| s.key == option)?
            };
            // Nested inside the scrolled body: already laid out scrolled.
            return self.rows.get(row)?.bounds_for_item(index);
        }
        let index = self.items.iter().position(|item| *item == id)?;
        let mut bounds = self.scroll.bounds_for_item(index)?;
        bounds.origin += self.scroll.offset();
        if self.inputs.iter().any(|input| input.id == id) {
            bounds.origin.y = bounds.bottom() - px(24.);
            bounds.size.height = px(24.);
        }
        Some(bounds)
    }

    /// The choices of segmented setting `row`, the configured one marked.
    /// A value set in the file but not offered (`video_fps = 50`) is
    /// offered too, so the control never shows nothing selected.
    fn segments(&self, row: &str) -> Option<Vec<Segment>> {
        let r = &self.config.record;
        let formats = |list: &[OutputFormat], current: &str| {
            list.iter()
                .map(|f| Segment {
                    key: f.extension().into(),
                    label: f.label().into(),
                    value: Value::from(f.extension()),
                    on: OutputFormat::parse(current) == Some(*f),
                })
                .collect()
        };
        let rates = |list: &[u32], current: u32| {
            let mut rates = list.to_vec();
            if !rates.contains(&current) {
                rates.push(current);
                rates.sort_unstable();
            }
            rates
                .into_iter()
                .map(|n| Segment {
                    key: n.to_string(),
                    label: format!("{n} fps"),
                    value: Value::from(n as i64),
                    on: n == current,
                })
                .collect()
        };
        Some(match row {
            "record-video-format" => formats(&OutputFormat::VIDEO, &r.video_format),
            "record-animation-format" => formats(&OutputFormat::ANIMATION, &r.animation_format),
            "record-video-fps" => rates(&crate::platform::VIDEO_FPS_CHOICES, r.video_fps),
            "record-animation-fps" => {
                rates(&crate::platform::ANIMATION_FPS_CHOICES, r.animation_fps)
            }
            "record-encoder" => [("auto", "Automatic"), ("software", "Software only")]
                .into_iter()
                .map(|(key, label)| Segment {
                    key: key.into(),
                    label: label.into(),
                    value: Value::from(key),
                    on: r.encoder == key,
                })
                .collect(),
            _ => return None,
        })
    }

    /// Audio devices to offer: the listed ones (defaults first), plus any
    /// the config names that are not connected now, so they can be removed.
    fn audio_rows(&self) -> Vec<AudioDevice> {
        let mut rows = match &self.audio_devices {
            AudioDevices::Ready(devices) => devices.clone(),
            // The defaults always exist; offering them while listing (or
            // after it failed) keeps the common choice one click away.
            AudioDevices::Loading | AudioDevices::Failed(_) => vec![
                AudioDevice {
                    id: DEFAULT_OUTPUT.into(),
                    name: "Default desktop audio".into(),
                    output: true,
                },
                AudioDevice {
                    id: DEFAULT_INPUT.into(),
                    name: "Default microphone".into(),
                    output: false,
                },
            ],
        };
        let listed = matches!(self.audio_devices, AudioDevices::Ready(_));
        for id in &self.config.record.audio {
            if !rows.iter().any(|d| &d.id == id) {
                rows.push(AudioDevice {
                    id: id.clone(),
                    name: if listed {
                        format!("{id} (not connected)")
                    } else {
                        id.clone()
                    },
                    output: false,
                });
            }
        }
        rows
    }

    fn toggle_audio(&mut self, id: &str, cx: &mut Context<Self>) {
        let mut audio = self.config.record.audio.clone();
        match audio.iter().position(|d| d == id) {
            Some(i) => {
                audio.remove(i);
            }
            None => audio.push(id.to_owned()),
        }
        let array: toml_edit::Array = audio.iter().map(String::as_str).collect();
        self.persist(&[(&["record", "audio"], Value::Array(array))], cx);
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
            "record-outline" => {
                next.record.outline = !next.record.outline;
                (&["record", "outline"], next.record.outline)
            }
            "record-cursor" => {
                next.record.cursor = !next.record.cursor;
                (&["record", "cursor"], next.record.cursor)
            }
            "record-clipboard" => {
                next.record.copy_to_clipboard = !next.record.copy_to_clipboard;
                (
                    &["record", "copy_to_clipboard"],
                    next.record.copy_to_clipboard,
                )
            }
            "record-notify" => {
                next.record.notify = !next.record.notify;
                (&["record", "notify"], next.record.notify)
            }
            "record-open-editor" => {
                next.record.open_editor = !next.record.open_editor;
                (&["record", "open_editor"], next.record.open_editor)
            }
            "record-tray" => {
                next.record.tray = !next.record.tray;
                (&["record", "tray"], next.record.tray)
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

    #[cfg(any(windows, target_os = "macos"))]
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

    /// A setting with a few fixed values, as a row of options.
    fn segmented(
        &self,
        row: &'static str,
        label: &'static str,
        path: &'static [&'static str],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let options = self.segments(row).unwrap_or_default();
        div()
            .id(row)
            .flex_none()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_xs().text_color(theme::muted()).child(label))
            .child(
                div()
                    .id(gpui::ElementId::Name(format!("{row}-options").into()))
                    .track_scroll(&self.rows[row])
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .children(options.into_iter().enumerate().map(|(i, s)| {
                        let value = s.value;
                        div()
                            .id((row, i))
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .cursor_pointer()
                            .when(s.on, |d| {
                                d.border_color(theme::accent())
                                    .bg(theme::accent_bg())
                                    .text_color(theme::accent())
                            })
                            .when(!s.on, |d| {
                                d.border_color(theme::panel_border())
                                    .hover(|d| d.bg(theme::hover()))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.persist(&[(path, value.clone())], cx);
                            }))
                            .child(s.label)
                    })),
            )
            .into_any_element()
    }

    /// Audio devices recorded into videos, as checkboxes.
    fn audio_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = &self.config.record.audio;
        let rows = self
            .audio_rows()
            .into_iter()
            .enumerate()
            .map(|(i, device)| {
                let on = selected.contains(&device.id);
                let id = device.id.clone();
                div()
                    .id(("audio-device", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|d| d.bg(theme::hover()))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_audio(&id, cx)))
                    .child(
                        div()
                            .flex_none()
                            .size(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_sm()
                            .border_1()
                            .when(on, |d| d.bg(theme::accent()).border_color(theme::accent()))
                            .when(!on, |d| d.border_color(theme::muted()))
                            .when(on, |d| {
                                d.child(
                                    svg()
                                        .path("icons/check.svg")
                                        .size(px(12.))
                                        .text_color(theme::on_accent()),
                                )
                            }),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(device.name))
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme::muted())
                            .child(if device.output { "Output" } else { "Input" }),
                    )
            });
        let status = match &self.audio_devices {
            AudioDevices::Loading => {
                Some(("Looking for audio devices…".to_owned(), theme::muted()))
            }
            AudioDevices::Failed(e) => Some((
                format!("Audio devices could not be listed: {e}"),
                gpui::rgb(0xff9999),
            )),
            AudioDevices::Ready(_) => None,
        };
        div()
            .id("record-audio")
            .flex_none()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(theme::muted())
                    .child("Audio in videos (none checked: silent)"),
            )
            .child(
                div()
                    .id("record-audio-list")
                    .track_scroll(&self.rows["record-audio"])
                    .flex()
                    .flex_col()
                    .children(rows),
            )
            .children(
                status.map(|(text, color)| div().px_2().text_xs().text_color(color).child(text)),
            )
            .into_any_element()
    }

    /// The `[record]` settings, appended to the scrolled body.
    fn recording(&self, items: &mut Vec<(&'static str, AnyElement)>, cx: &mut Context<Self>) {
        let r = self.config.record.clone();
        items.push((
            "record-title",
            div()
                .flex_none()
                .mt_4()
                .pb_1()
                .border_b_1()
                .border_color(theme::panel_border())
                .font_weight(FontWeight::MEDIUM)
                .child("Recording")
                .into_any_element(),
        ));
        for (row, label, path) in SEGMENTED {
            items.push((row, self.segmented(row, label, path, cx)));
        }
        if let Some(input) = self.inputs.iter().find(|i| i.id == "record-save-dir") {
            items.push((input.id, input_row(input)));
        }
        items.push(("record-audio", self.audio_list(cx)));
        let mut choices = vec![
            (
                "record-outline",
                "Outline the recorded area",
                "A thin line just outside the region; it is never recorded.",
                r.outline,
            ),
            (
                "record-cursor",
                "Record the pointer",
                "Include the mouse pointer in recordings.",
                r.cursor,
            ),
            (
                "record-clipboard",
                "Copy recordings to clipboard",
                "Put each finished recording on the clipboard as a file.",
                r.copy_to_clipboard,
            ),
            (
                "record-notify",
                "Notify when saved",
                "Show a notification when a recording is saved; click it to edit.",
                r.notify,
            ),
            (
                "record-open-editor",
                "Open the editor after recording",
                "Open the video editor or annotator as soon as a recording stops.",
                r.open_editor,
            ),
        ];
        if cfg!(target_os = "linux") {
            choices.push((
                "record-tray",
                "Tray icon",
                "Show recording controls in the system tray.",
                r.tray,
            ));
        }
        for (id, label, hint, on) in choices {
            items.push((id, self.choice(id, label, hint, on, cx).into_any_element()));
        }
    }
}

impl Render for Settings {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = self.config.clone();
        let mut items: Vec<(&'static str, AnyElement)> = Vec::new();
        let mut choices = vec![
            (
                "freeze",
                "Freeze desktop",
                "Select from a still image captured before selection starts.",
                c.freeze,
            ),
            (
                "live",
                "Live desktop",
                "Select over the moving desktop; capture after selection finishes.",
                !c.freeze,
            ),
            (
                "clipboard",
                "Copy to clipboard",
                "Copy each capture as an image.",
                c.copy_to_clipboard,
            ),
            (
                "notify",
                "Desktop notifications",
                "Show a notification after each capture.",
                c.notify,
            ),
            (
                "open-editor",
                "Open editor",
                "Open the annotation editor immediately after capture.",
                c.open_editor,
            ),
            (
                "cursor",
                "Include cursor",
                "Include the pointer in captured images.",
                c.cursor,
            ),
        ];
        if !self.welcome {
            choices.extend([
                (
                    "close-on-save",
                    "Close after saving",
                    "Close the editor after a successful save.",
                    c.editor.close_on_save,
                ),
                (
                    "font-italic",
                    "Italic text",
                    "Start new text annotations in italic.",
                    c.editor.font_italic,
                ),
            ]);
        }
        for (id, label, hint, on) in choices {
            items.push((id, self.choice(id, label, hint, on, cx).into_any_element()));
        }
        if !self.welcome {
            for input in self.inputs.iter().filter(|i| i.path[0] != "record") {
                items.push((input.id, input_row(input)));
            }
            if !self.inputs.iter().any(|input| input.id == "ocr-languages") {
                items.push((
                    "ocr-missing",
                    div()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(
                            "OCR languages: install Tesseract and language data to enable text extraction.",
                        )
                        .into_any_element(),
                ));
            }
            #[cfg(any(windows, target_os = "macos"))]
            {
                items.push((
                    "shortcuts",
                    div()
                        .child("Global shortcuts (click to enable/disable)")
                        .into_any_element(),
                ));
                #[cfg(windows)]
                items.push((
                    "print-screen-notice",
                    div()
                        .text_xs()
                        .text_color(theme::muted())
                        .child(crate::platform::windows::PRINT_SCREEN_NOTICE)
                        .into_any_element(),
                ));
                for (index, key, label, binding) in [
                    (0, "region", "Region", &c.hotkeys.region),
                    (1, "screen", "Screen", &c.hotkeys.screen),
                    (2, "window", "Window", &c.hotkeys.window),
                    (3, "open_last", "Open last", &c.hotkeys.open_last),
                ] {
                    items.push((
                        key,
                        self.shortcut(index, key, label, binding, cx)
                            .into_any_element(),
                    ));
                }
            }
            self.recording(&mut items, cx);
        }
        self.items = items.iter().map(|(id, _)| *id).collect();
        let body = div()
            .id("settings-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .children(items.into_iter().map(|(_, element)| element));
        div().id("settings").size_full().flex().flex_col().bg(theme::panel())
            .font_family(UI_FONT).text_sm().text_color(theme::text()).track_focus(&self.focus)
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| {
                window.focus(&this.focus, cx);
            }))
            .child(div().flex_none().min_h(px(88.)).p_4().border_b_1().border_color(theme::panel_border())
                .child(if self.welcome { "Welcome to quikscope" } else { "Settings" })
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
