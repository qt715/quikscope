//! Real GPUI mouse/key dispatch against offscreen settings, never the user's desktop.
#![cfg(not(windows))]

use gpui::{
    AnyWindowHandle, AppContext as _, HeadlessAppContext, KeyDownEvent, Keystroke, Modifiers,
    MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput, Point, WindowHandle, point, px, size,
};
use quickscope::{
    config::Config,
    ui::{Assets, settings::Settings},
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

struct Ui {
    cx: HeadlessAppContext,
    window: WindowHandle<Settings>,
    path: PathBuf,
    applied: Arc<Mutex<Vec<Arc<Config>>>>,
    _dir: tempfile::TempDir,
}

impl Ui {
    fn new(text: Option<&str>, welcome: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        if let Some(text) = text {
            std::fs::write(&path, text).unwrap();
        }
        let config = text
            .and_then(|text| Config::parse(text).ok())
            .unwrap_or_default();
        let applied = Arc::new(Mutex::new(Vec::new()));
        let output = applied.clone();
        let callback =
            Arc::new(move |config, _: &mut gpui::App| output.lock().unwrap().push(config));
        let text_system = gpui_platform::current_platform(true).text_system();
        let mut cx = HeadlessAppContext::with_platform(
            text_system,
            Arc::new(Assets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(quickscope::ui::init);
        let p = path.clone();
        let window = cx
            .open_window(size(px(760.), px(1400.)), |window, cx| {
                cx.new(|cx| Settings::new(Arc::new(config), p, welcome, callback, window, cx))
            })
            .unwrap();
        let mut ui = Self {
            cx,
            window,
            path,
            applied,
            _dir: dir,
        };
        ui.frame();
        ui
    }

    fn any(&self) -> AnyWindowHandle {
        self.window.into()
    }
    fn frame(&mut self) {
        self.cx
            .update_window(self.any(), |_, window, _| window.refresh())
            .unwrap();
        self.cx.run_until_parked();
        self.cx.capture_screenshot(self.any()).unwrap();
    }
    fn input(&mut self, event: PlatformInput) {
        self.cx
            .update_window(self.any(), |_, window, cx| window.dispatch_event(event, cx))
            .unwrap();
        self.cx.run_until_parked();
    }
    fn click_at(&mut self, position: Point<gpui::Pixels>) {
        self.input(PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }));
        self.input(PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers: Modifiers::default(),
            click_count: 1,
        }));
    }
    fn click(&mut self, id: &str) {
        let position = self
            .cx
            .update_window(self.any(), |view, _, cx| {
                view.downcast::<Settings>()
                    .unwrap()
                    .read(cx)
                    .control_bounds(id)
                    .unwrap()
                    .center()
            })
            .unwrap();
        self.click_at(position);
        self.frame();
    }
    fn key(&mut self, key: &str, character: Option<&str>) {
        self.input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke: Keystroke {
                modifiers: Modifiers::default(),
                key: key.into(),
                key_char: character.map(Into::into),
            },
            is_held: false,
            prefer_character_input: false,
        }));
        self.frame();
    }
    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            let s = c.to_string();
            self.key(&s.to_lowercase(), Some(&s));
        }
    }
    fn error(&mut self) -> Option<String> {
        self.cx
            .update_window(self.any(), |view, _, cx| {
                view.downcast::<Settings>()
                    .unwrap()
                    .read(cx)
                    .error()
                    .map(str::to_owned)
            })
            .unwrap()
    }
    fn saved(&self) -> Config {
        Config::parse(&std::fs::read_to_string(&self.path).unwrap()).unwrap()
    }
    fn done(&mut self) {
        // Footer is pinned; its full-width button's center is 30 px from the bottom.
        self.click_at(point(px(200.), px(1370.)));
    }
}

#[test]
fn mouse_toggle_preserves_comment_and_applies_config() {
    let mut ui = Ui::new(
        Some("# keep this comment\nnotify = true # trailing\n"),
        false,
    );
    ui.click("notify");
    assert!(!ui.saved().notify);
    let text = std::fs::read_to_string(&ui.path).unwrap();
    assert!(text.contains("# keep this comment"));
    assert!(text.contains("# trailing"));
    assert!(!ui.applied.lock().unwrap().last().unwrap().notify);
}

#[test]
fn invalid_field_renders_error_and_leaves_file_unchanged() {
    let original = "# untouched\n[editor]\nfont_size = 28\n";
    let mut ui = Ui::new(Some(original), false);
    ui.click("font-size");
    ui.type_text("not-a-number");
    ui.key("enter", None);
    assert!(ui.error().is_some());
    assert_eq!(std::fs::read_to_string(&ui.path).unwrap(), original);
    assert!(ui.applied.lock().unwrap().is_empty());
    // The inline error uses a dedicated red foreground; assert it was painted,
    // not merely stored in view state.
    let image = ui.cx.capture_screenshot(ui.any()).unwrap();
    assert!(
        image
            .pixels()
            .any(|p| p[0] > 200 && p[1] > 90 && p[1] < 190 && p[2] < 190)
    );
    ui.click("font-size");
    ui.type_text("32");
    ui.key("enter", None);
    assert!(ui.error().is_none());
    assert_eq!(ui.saved().editor.font_size, 32.);
    assert_eq!(
        ui.applied.lock().unwrap().last().unwrap().editor.font_size,
        32.
    );
    ui.click("font-size");
    ui.type_text("invalid");
    ui.key("enter", None);
    assert!(ui.error().is_some());
    ui.click("font-size");
    ui.type_text("32");
    ui.key("enter", None);
    assert!(
        ui.error().is_none(),
        "restoring the saved value clears its old error"
    );
}

#[test]
fn escape_discards_invalid_field_text_and_allows_close() {
    let original = "# original\n[editor]\nfont_size = 28\n";
    let mut ui = Ui::new(Some(original), false);
    ui.click("font-size");
    ui.type_text("invalid");
    ui.key("enter", None);
    assert!(ui.error().is_some());
    ui.key("escape", None);
    assert!(ui.error().is_none());
    ui.done();
    assert_eq!(std::fs::read_to_string(&ui.path).unwrap(), original);
}

#[test]
fn invalid_existing_normal_config_shows_error_and_never_writes() {
    let original = "# typo\nnotifiy = true\n";
    let mut ui = Ui::new(Some(original), false);
    assert!(ui.error().unwrap().contains("notifiy"));
    ui.click("filename");
    ui.type_text("new.png");
    ui.key("enter", None);
    assert!(ui.error().is_some());
    assert_eq!(std::fs::read_to_string(&ui.path).unwrap(), original);
    ui.key("escape", None);
    ui.done();
    assert_eq!(std::fs::read_to_string(&ui.path).unwrap(), original);
}

#[test]
fn welcome_previews_without_file_then_done_commits_live_atomically() {
    let mut ui = Ui::new(None, true);
    ui.click("live");
    ui.click("notify");
    assert!(!ui.path.exists());
    assert!(!ui.applied.lock().unwrap().last().unwrap().freeze);
    ui.done();
    let saved = ui.saved();
    assert!(!saved.freeze);
    assert!(!saved.notify);
}

#[test]
fn welcome_done_commits_frozen_choice_too() {
    let mut ui = Ui::new(None, true);
    ui.click("live");
    ui.click("freeze");
    assert!(!ui.path.exists());
    ui.done();
    assert!(ui.saved().freeze);
}

#[test]
fn welcome_refuses_config_that_became_invalid_while_open() {
    let original = "# typo must be fixed by the user\nnotifiy = true\n";
    let mut ui = Ui::new(None, true);
    std::fs::write(&ui.path, original).unwrap();
    ui.click("live");
    ui.done();
    ui.frame(); // Done was refused, so the window still exists.
    assert!(ui.error().is_some());
    assert_eq!(std::fs::read_to_string(&ui.path).unwrap(), original);
}

// Native close is deliberately covered by compositor e2e, not simulated here:
// GPUI's Linux headless PlatformWindow::on_should_close ignores its callback.
