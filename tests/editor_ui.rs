//! Editor interaction through GPUI's real input dispatch (offscreen window,
//! headless WGPU renderer): text formatting shortcuts and the color picker.

// GPUI 0.3.10 has no Windows offscreen renderer; native interaction belongs
// to the interactive VM smoke suite rather than this WGPU-specific harness.
#![cfg(not(windows))]

use std::collections::BTreeSet;
use std::sync::Arc;

use gpui::EntityInputHandler as _;
use gpui::{
    AnyWindowHandle, AppContext as _, Entity, HeadlessAppContext, KeyDownEvent, Keystroke,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Point,
    WindowHandle, point, px, size,
};
use quikscope::document::{Document, PALETTE, ShapeKind, Style};
use quikscope::geom::Pt;
use quikscope::raster::Image;
use quikscope::ui::color_picker::{LEFT_COLUMN, PANEL_WIDTH, WHEEL};
use quikscope::ui::editor::Editor;
use quikscope::ui::{Assets, Shot};

struct Ui {
    cx: HeadlessAppContext,
    window: WindowHandle<Editor>,
    doc: Entity<Document>,
}

impl Ui {
    fn new(doc: Document) -> Ui {
        Self::with_size(doc, size(px(1000.), px(700.)))
    }

    fn with_size(doc: Document, viewport: gpui::Size<gpui::Pixels>) -> Ui {
        Self::with_animation(doc, viewport, None)
    }

    fn with_animation(
        doc: Document,
        viewport: gpui::Size<gpui::Pixels>,
        animation: Option<quikscope::ui::Animation>,
    ) -> Ui {
        let text = gpui_platform::current_platform(true).text_system();
        let mut cx = HeadlessAppContext::with_platform(
            text,
            Arc::new(Assets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(quikscope::ui::init);
        let (base, scale) = (doc.base.clone(), doc.scale);
        let doc = cx.update(|cx| cx.new(|_| doc));
        let mut shot = Shot::new(1, base, scale, None);
        shot.animation = animation.map(Arc::new);
        let shot = Arc::new(shot);
        let d = doc.clone();
        let window = cx
            .open_window(viewport, |window, cx| {
                cx.new(|cx| Editor::new(shot, d, window, cx))
            })
            .unwrap();
        let mut ui = Ui { cx, window, doc };
        ui.frame();
        ui
    }

    fn any(&self) -> AnyWindowHandle {
        self.window.into()
    }

    /// Lays out and paints, so hit-testing sees current bounds.
    fn frame(&mut self) {
        self.cx
            .update_window(self.any(), |_, window, _| window.refresh())
            .unwrap();
        self.cx.run_until_parked();
        self.cx.capture_screenshot(self.any()).unwrap();
    }

    fn input(&mut self, event: PlatformInput) {
        self.cx
            .update_window(self.any(), |_, window, cx| {
                window.dispatch_event(event, cx);
            })
            .unwrap();
        self.cx.run_until_parked();
    }

    fn key(&mut self, key: &str, ch: Option<&str>, modifiers: Modifiers) {
        self.input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke: Keystroke {
                modifiers,
                key: key.into(),
                key_char: ch.map(Into::into),
            },
            is_held: false,
            prefer_character_input: false,
        }));
        self.frame();
    }

    fn typ(&mut self, text: &str) {
        for c in text.chars() {
            let s = c.to_string();
            self.key(&s.to_lowercase(), Some(&s), Modifiers::default());
        }
    }

    fn ctrl(&mut self, key: &str, shift: bool) {
        let m = Modifiers {
            control: true,
            shift,
            ..Default::default()
        };
        self.key(key, None, m);
    }

    fn mouse(&mut self, kind: &str, at: Point<gpui::Pixels>) {
        let event = match kind {
            "down" => PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position: at,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            }),
            "move" => PlatformInput::MouseMove(MouseMoveEvent {
                position: at,
                pressed_button: Some(MouseButton::Left),
                modifiers: Modifiers::default(),
            }),
            _ => PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position: at,
                modifiers: Modifiers::default(),
                click_count: 1,
            }),
        };
        self.input(event);
        self.frame();
    }

    fn click(&mut self, at: Point<gpui::Pixels>) {
        self.mouse("down", at);
        self.mouse("up", at);
    }

    fn editor<R>(&mut self, f: impl FnOnce(&mut Editor, &mut gpui::Context<Editor>) -> R) -> R {
        let any = self.any();
        self.cx
            .update_window(any, |view, _, cx| {
                view.downcast::<Editor>()
                    .unwrap()
                    .update(cx, |e, cx| f(e, cx))
            })
            .unwrap()
    }

    /// Calls the editor's input-method protocol, as the platform does.
    fn ime<R>(
        &mut self,
        f: impl FnOnce(&mut Editor, &mut gpui::Window, &mut gpui::Context<Editor>) -> R,
    ) -> R {
        let any = self.any();
        let r = self
            .cx
            .update_window(any, |view, window, cx| {
                view.downcast::<Editor>()
                    .unwrap()
                    .update(cx, |e, cx| f(e, window, cx))
            })
            .unwrap();
        self.frame();
        r
    }

    /// Window position of image point `p`.
    fn screen(&mut self, p: Pt) -> Point<gpui::Pixels> {
        self.editor(|e, _| e.transform().to_screen(p))
    }

    fn doc<R>(&mut self, f: impl FnOnce(&Document) -> R) -> R {
        let doc = self.doc.clone();
        self.cx.update(|cx| f(doc.read(cx)))
    }
}

pub(super) fn animation_scrubs_and_exports_annotations_on_every_frame() {
    use quikscope::media::{OutputFormat, decode::Decoder};
    use quikscope::ui::{Animation, editor::EditorServices};
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    for (index, color) in [[30, 50, 80, 255], [80, 160, 30, 255], [180, 60, 20, 255]]
        .into_iter()
        .enumerate()
    {
        Image::filled(160, 120, color)
            .save_png(
                &dir.path().join(format!("frame-{index}.png")),
                quikscope::raster::Compression::Fast,
            )
            .unwrap();
    }
    let source = dir.path().join("source.gif");
    let output = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-framerate", "2", "-i"])
        .arg(dir.path().join("frame-%d.png"))
        .args(["-loop", "0"])
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let info = quikscope::media::decode::probe(&source).unwrap();
    let first = Decoder::open(&source)
        .unwrap()
        .next_frame()
        .unwrap()
        .unwrap()
        .image;
    let mut ui = Ui::with_animation(
        Document::new(Arc::new(first), 1.0),
        size(px(1000.), px(700.)),
        Some(Animation {
            path: source.clone(),
            info,
            format: OutputFormat::Gif,
        }),
    );
    let mut config = quikscope::config::Config::default();
    config.record.save_dir = dir.path().to_string_lossy().into_owned();
    config.record.filename = "annotated".into();
    ui.cx.update(|cx| {
        cx.set_global(EditorServices {
            config: Arc::new(config),
            clipboard: None,
            pick_color: None,
        })
    });
    let strip = ui.editor(|e, _| e.chrome.playback.bounds());
    let history = ui.editor(|e, _| e.chrome.history.bounds());
    let outputs = ui.editor(|e, _| e.chrome.output.bounds());
    assert!(strip.size.width > px(300.));
    assert!(history.right() < strip.left() && strip.right() < outputs.left());

    let original = ui.editor(|e, _| e.current_image().data[0..4].to_vec());
    let scrub = ui.editor(|e, _| e.chrome.scrubber.bounds());
    ui.click(point(scrub.right() - px(1.), scrub.center().y));
    let deadline = Instant::now() + Duration::from_secs(10);
    while ui.editor(|e, _| e.current_image().data[0..4].to_vec()) == original {
        assert!(
            Instant::now() < deadline,
            "scrub did not show another frame"
        );
        std::thread::sleep(Duration::from_millis(10));
        ui.frame();
    }
    ui.key("space", None, Modifiers::default());
    assert!(ui.editor(|e, _| e.animation_status().unwrap().0));
    ui.key("space", None, Modifiers::default());
    assert!(!ui.editor(|e, _| e.animation_status().unwrap().0));
    ui.typ("r");
    ui.editor(|e, _| {
        e.style.color = quikscope::document::Rgba {
            r: 255,
            g: 0,
            b: 0,
            a: 255,
        };
        e.style.fill = true;
    });
    let a = ui.screen(Pt::new(30., 30.));
    let b = ui.screen(Pt::new(70., 70.));
    ui.mouse("down", a);
    ui.mouse("move", b);
    ui.mouse("up", b);
    ui.cx
        .capture_screenshot(ui.any())
        .unwrap()
        .save("/tmp/quikscope-animation-mode.png")
        .unwrap();
    ui.typ("c");
    let a = ui.screen(Pt::new(20., 20.));
    let b = ui.screen(Pt::new(100., 100.));
    ui.mouse("down", a);
    ui.mouse("move", b);
    ui.mouse("up", b);
    ui.ctrl("s", false);
    let deadline = Instant::now() + Duration::from_secs(30);
    while ui.editor(|e, _| e.saved_path().is_none()) {
        assert!(Instant::now() < deadline, "animation save did not finish");
        std::thread::sleep(Duration::from_millis(10));
        ui.frame();
    }
    let saved = ui.editor(|e, _| e.saved_path().unwrap().clone());
    assert_eq!(saved.extension().unwrap(), "gif");
    let mut decoder = Decoder::open(&saved).unwrap();
    let mut frames = Vec::new();
    while let Some(frame) = decoder.next_frame().unwrap() {
        frames.push(frame.image);
    }
    assert_eq!(frames.len(), 3);
    for image in [frames.first().unwrap(), frames.last().unwrap()] {
        assert_eq!(
            (image.width, image.height),
            (80, 80),
            "crop applies to every frame"
        );
        let at = ((30 * image.width + 30) * 4) as usize;
        let pixel = &image.data[at..at + 4];
        assert!(
            pixel[2] > 230 && pixel[0] < 30 && pixel[1] < 30,
            "{pixel:?}"
        );
    }
}

fn blank() -> Document {
    Document::new(Arc::new(Image::filled(600, 400, [40, 40, 40, 255])), 1.0)
}

fn text_font(doc: &Document) -> quikscope::fonts::FontSpec {
    match &doc.shapes().last().expect("a text shape").kind {
        ShapeKind::Text { font, .. } => font.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

/// Where the color picker was laid out, checking it hangs beside the style
/// panel, level with its top, at its full width.
fn picker_bounds(ui: &mut Ui) -> gpui::Bounds<gpui::Pixels> {
    let panel = ui.editor(|e, cx| e.picker_scroll(cx)).bounds();
    let side = ui.editor(|e, _| e.chrome.style.bounds());
    assert_eq!(panel.size.width, px(PANEL_WIDTH));
    assert_eq!(
        panel.origin,
        point(side.right() + px(8.), side.top()),
        "{panel:?} beside {side:?}"
    );
    panel
}

pub(super) fn input_method_composition_commits_into_the_text() {
    let mut ui = Ui::new(blank());
    // Without text being edited the editor is no text input, so an input
    // method leaves the single-key tool shortcuts alone.
    assert!(!ui.ime(|e, w, cx| e.accepts_text_input(w, cx)));
    ui.typ("t");
    let at = ui.screen(Pt::new(100.0, 100.0));
    ui.click(at);
    ui.typ("A");
    assert!(ui.ime(|e, w, cx| e.accepts_text_input(w, cx)));

    // A Japanese IME composes "nihon", each step replacing its pre-edit.
    ui.ime(|e, w, cx| e.replace_and_mark_text_in_range(None, "に", None, w, cx));
    ui.ime(|e, w, cx| e.replace_and_mark_text_in_range(None, "にほん", None, w, cx));
    assert_eq!(ui.ime(|e, w, cx| e.marked_text_range(w, cx)), Some(1..4));
    assert_eq!(
        ui.ime(|e, w, cx| e.selected_text_range(false, w, cx))
            .map(|s| s.range),
        Some(4..4)
    );
    // The candidate window is anchored at the pre-edit, on the text line.
    let anchor = ui
        .ime(|e, w, cx| e.bounds_for_range(1..1, gpui::Bounds::default(), w, cx))
        .unwrap();
    let mid_y = anchor.origin.y + anchor.size.height / 2.0;
    assert!(
        anchor.origin.x > at.x && anchor.origin.x < at.x + px(60.),
        "{anchor:?}"
    );
    assert!((mid_y - at.y).abs() < px(3.), "{anchor:?} vs {at:?}");

    // Conversion commits "日本" in place of the pre-edit.
    ui.ime(|e, w, cx| e.replace_text_in_range(None, "日本", w, cx));
    assert_eq!(ui.ime(|e, w, cx| e.marked_text_range(w, cx)), None);
    // Typing continues after the committed text.
    ui.typ("!");
    ui.key("escape", None, Modifiers::default());
    let text = ui.doc(|d| match &d.shapes()[0].kind {
        ShapeKind::Text { text, .. } => text.clone(),
        _ => unreachable!(),
    });
    assert_eq!(text, "A日本!");
    assert_eq!(ui.doc(|d| d.shapes().len()), 1, "one shape, one undo step");
}

pub(super) fn formatting_shortcuts_apply_while_typing_and_to_selected_text() {
    let mut ui = Ui::new(blank());
    ui.typ("t");
    let at = ui.screen(Pt::new(100.0, 100.0));
    ui.click(at);
    ui.typ("Hi there");
    ui.ctrl("b", false);
    ui.ctrl("i", false);
    ui.ctrl(".", true);
    ui.key("escape", None, Modifiers::default());

    let font = ui.doc(text_font);
    assert_eq!((font.weight, font.italic, font.size), (700, true, 32.0));
    let text = ui.doc(|d| match &d.shapes()[0].kind {
        ShapeKind::Text { text, .. } => text.clone(),
        _ => unreachable!(),
    });
    assert_eq!(text, "Hi there", "shortcuts were not typed as text");

    // Select the text and un-bold it; that is one undo step.
    let id = ui.doc(|d| d.shapes()[0].id);
    ui.editor(|e, cx| {
        e.set_tool(quikscope::ui::editor::Tool::Select, cx);
        e.selected = BTreeSet::from([id]);
    });
    ui.ctrl("b", false);
    assert_eq!(ui.doc(text_font).weight, 400);
    ui.ctrl("z", false);
    assert_eq!(ui.doc(text_font).weight, 700);
    // New text uses the last font chosen.
    assert_eq!(ui.editor(|e, _| e.font.weight), 400);
}

pub(super) fn color_picker_drag_recolors_the_selection_as_one_undo_step() {
    let mut doc = blank();
    let rect = doc.make_shape(
        ShapeKind::Rect {
            a: Pt::new(50.0, 50.0),
            b: Pt::new(200.0, 150.0),
        },
        Style::default(),
    );
    let id = doc.add(rect);
    let second = doc.make_shape(
        ShapeKind::Rect {
            a: Pt::new(300., 200.),
            b: Pt::new(400., 300.),
        },
        Style::default(),
    );
    let second = doc.add(second);
    let mut ui = Ui::new(doc);
    ui.editor(|e, cx| {
        e.set_tool(quikscope::ui::editor::Tool::Select, cx);
        e.selected = BTreeSet::from([id, second]);
        e.toggle_picker(cx);
    });
    ui.frame();

    // The panel hangs beside the style panel, level with its top; the wheel
    // is centred in its left column.
    let panel = picker_bounds(&mut ui);
    let x = f32::from(panel.left()) + 1.0 + 12.0 + LEFT_COLUMN / 2.0;
    let centre = point(px(x), panel.top() + px(1.0 + 12.0 + WHEEL / 2.0));
    ui.mouse("down", centre + point(px(WHEEL / 2.0 - 4.0), px(0.)));
    ui.mouse("move", centre + point(px(0.), px(-(WHEEL / 2.0 - 4.0))));
    ui.mouse("move", centre + point(px(-(WHEEL / 2.0 - 4.0)), px(0.)));
    ui.mouse("up", centre + point(px(-(WHEEL / 2.0 - 4.0)), px(0.)));

    let color = ui.doc(|d| d.shape(id).unwrap().style.color);
    // Left edge of the wheel is cyan.
    assert!(color.r < 40 && color.g > 200 && color.b > 200, "{color:?}");
    assert_eq!(ui.editor(|e, _| e.style.color), color, "tool color follows");
    assert!(ui.doc(|d| d.shapes().iter().all(|s| s.style.color == color)));

    let undo_steps = ui.doc(|d| d.can_undo());
    assert!(undo_steps);
    ui.ctrl("z", false);
    assert_eq!(
        ui.doc(|d| d.shape(id).unwrap().style.color),
        PALETTE[0],
        "one undo restores the color from before the drag"
    );
    assert!(ui.doc(|d| d.shapes().iter().all(|s| s.style.color == PALETTE[0])));
    ui.ctrl("z", false);
    assert!(
        ui.doc(|d| d.shape(second).is_none() && d.shape(id).is_some()),
        "next undo removes the last original shape, not a partial recolor"
    );

    // Escape closes the picker without touching the drawing.
    assert!(ui.editor(|e, _| e.picker_open()));
    ui.key("escape", None, Modifiers::default());
    assert!(!ui.editor(|e, _| e.picker_open()));
}

/// Records clipboard text instead of reaching a desktop.
#[derive(Default)]
struct TextClipboard(std::sync::Mutex<Vec<String>>);

impl quikscope::platform::Clipboard for TextClipboard {
    fn set_image(&self, image: Arc<Image>) -> Arc<quikscope::clipboard::PendingPng> {
        quikscope::clipboard::PendingPng::encode(image)
    }

    fn set_text(&self, text: &str) {
        self.0.lock().unwrap().push(text.into());
    }

    fn set_files(&self, paths: &[std::path::PathBuf]) {
        for path in paths {
            self.0.lock().unwrap().push(path.display().to_string());
        }
    }
}

/// The standalone color window on a 640×360 screen (a 1280×720 display at
/// scale 2): the four format fields wrap into two rows, and every one of
/// them, plus the copy status, must stay reachable.
pub(super) fn color_window_fits_narrow_screens_and_scrolls_when_short() {
    use quikscope::color::Format;
    use quikscope::document::Rgba;
    use quikscope::ui::color_window::{self, ColorWindow};
    use quikscope::ui::editor::EditorServices;

    let color = Rgba {
        r: 0,
        g: 103,
        b: 237,
        a: 255,
    };
    let clipboard = Arc::new(TextClipboard::default());
    let open = |w: f32, h: f32| {
        let text = gpui_platform::current_platform(true).text_system();
        let mut cx = HeadlessAppContext::with_platform(
            text,
            Arc::new(Assets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(quikscope::ui::init);
        let services = EditorServices {
            config: Arc::new(quikscope::config::Config::default()),
            clipboard: Some(clipboard.clone()),
            pick_color: None,
        };
        cx.update(|cx| cx.set_global(services));
        let window: AnyWindowHandle = cx
            .open_window(size(px(w), px(h)), |window, cx| {
                cx.new(|cx| ColorWindow::new(color, window, cx))
            })
            .unwrap()
            .into();
        (cx, window)
    };
    let frame = |cx: &mut HeadlessAppContext, window: AnyWindowHandle| {
        cx.update_window(window, |_, w, _| w.refresh()).unwrap();
        cx.run_until_parked();
        cx.capture_screenshot(window).unwrap();
    };
    let view = |cx: &mut HeadlessAppContext, window: AnyWindowHandle| {
        cx.update_window(window, |v, _, cx| {
            let v = v.downcast::<ColorWindow>().unwrap();
            let v = v.read(cx);
            (v.scroll.clone(), v.status.clone())
        })
        .unwrap()
    };
    let input = |cx: &mut HeadlessAppContext, window: AnyWindowHandle, event: PlatformInput| {
        cx.update_window(window, |_, w, cx| {
            w.dispatch_event(event, cx);
        })
        .unwrap();
        cx.run_until_parked();
    };
    // Clicks the HSL field, the last of the two wrapped rows, expects its
    // value on the clipboard, and returns the status line's on-screen bounds.
    // Child bounds are layout positions; the scroll offset moves them.
    let copy_hsl = |cx: &mut HeadlessAppContext, window: AnyWindowHandle| {
        clipboard.0.lock().unwrap().clear();
        let (scroll, _) = view(cx, window);
        let mut panel = scroll.bounds_for_item(0).unwrap();
        panel.origin += scroll.offset();
        let inner = panel.size.width - px(26.);
        let cell = (inner - px(12.)) / 2.0;
        let at = point(
            panel.left() + px(13.) + cell + px(12.) + cell / 2.0,
            panel.bottom() - px(13. + 20.),
        );
        for (down, event) in [(true, at), (false, at)] {
            let event = if down {
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: event,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                    first_mouse: false,
                })
            } else {
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position: event,
                    modifiers: Modifiers::default(),
                    click_count: 1,
                })
            };
            input(cx, window, event);
        }
        frame(cx, window);
        let hsl = Format::Hsl.format(color);
        assert_eq!(*clipboard.0.lock().unwrap(), [hsl.as_str()]);
        let (scroll, status) = view(cx, window);
        assert_eq!(status, Some((format!("Copied {hsl}"), false)));
        let mut line = scroll.bounds_for_item(1).unwrap();
        line.origin += scroll.offset();
        line
    };

    // Opened for the narrow screen, the window is tall enough for both
    // rows: nothing to scroll, the status line inside the window.
    let (w, h) = color_window::size_for(640.0, 360.0);
    assert_eq!((w, h), (608.0, 236.0 + 58.0));
    let (mut cx, window) = open(w, h);
    frame(&mut cx, window);
    assert_eq!(view(&mut cx, window).0.max_offset().y, px(0.));
    let status = copy_hsl(&mut cx, window);
    assert!(status.bottom() <= px(h), "{status:?}");

    // Resized shorter than its content, the window scrolls to the last
    // row and the status.
    let (mut cx, window) = open(w, 200.0);
    frame(&mut cx, window);
    let max = view(&mut cx, window).0.max_offset().y;
    assert!(max > px(0.), "content taller than 200 px scrolls");
    // Scrolling targets the hovered element.
    input(
        &mut cx,
        window,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: point(px(w / 2.0), px(100.)),
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
    );
    input(
        &mut cx,
        window,
        PlatformInput::Scroll(gpui::ScrollEvent {
            position: point(px(w / 2.0), px(100.)),
            // Wheel notches; pixel deltas are touchpad input.
            delta: gpui::ScrollDelta::Lines(point(0., -20.)),
            ..Default::default()
        }),
    );
    frame(&mut cx, window);
    assert_eq!(view(&mut cx, window).0.offset().y, -max);
    let status = copy_hsl(&mut cx, window);
    assert!(status.bottom() <= px(200.), "{status:?}");
}

/// The smallest editor window (480×320): the picker is limited to the room
/// below the toolbar and scrolls, so HSL, its last field, can still be copied.
pub(super) fn color_picker_scrolls_in_short_windows() {
    let mut ui = Ui::with_size(blank(), size(px(480.), px(320.)));
    let clipboard = Arc::new(TextClipboard::default());
    let services = quikscope::ui::editor::EditorServices {
        config: Arc::new(quikscope::config::Config::default()),
        clipboard: Some(clipboard.clone()),
        pick_color: None,
    };
    ui.cx.update(|cx| cx.set_global(services));
    ui.editor(|e, cx| e.toggle_picker(cx));
    ui.frame();

    let scroll = ui.editor(|e, cx| e.picker_scroll(cx));
    let panel = scroll.bounds();
    assert!(
        panel.top() >= px(0.) && panel.bottom() <= px(320. - 8.),
        "{panel:?}"
    );
    let max = scroll.max_offset().y;
    assert!(max > px(0.), "the picker is taller than the room for it");

    // A wheel over the picker scrolls it, not the canvas behind it.
    let over = panel.center();
    ui.input(PlatformInput::MouseMove(MouseMoveEvent {
        position: over,
        pressed_button: None,
        modifiers: Modifiers::default(),
    }));
    let view_before = ui.editor(|e, _| e.transform());
    ui.input(PlatformInput::Scroll(gpui::ScrollEvent {
        position: over,
        delta: gpui::ScrollDelta::Lines(point(0., -20.)),
        ..Default::default()
    }));
    ui.frame();
    assert_eq!(scroll.offset().y, -max);
    assert_eq!(ui.editor(|e, _| e.transform()), view_before);

    // HSL is the last field of the right column, now on screen.
    let mut row = scroll.bounds_for_item(1).unwrap();
    row.origin += scroll.offset();
    let hsl = point(
        row.left() + px(LEFT_COLUMN + 16.0 + 100.0),
        row.top() + px(28.0 + 18.0 + 48.0 + 18.0 + 3.0 * 58.0 + 20.0),
    );
    assert!(hsl.y < panel.bottom(), "{hsl:?} in {panel:?}");
    ui.click(hsl);
    assert_eq!(
        *clipboard.0.lock().unwrap(),
        [quikscope::color::Format::Hsl.format(PALETTE[0])]
    );
}

pub(super) fn picker_eyedropper_picks_from_screen_and_copies_each_format() {
    use std::cell::Cell;
    use std::rc::Rc;

    use quikscope::document::Rgba;
    use quikscope::ui::editor::{EditorServices, Tool};

    let mut doc = blank();
    let rect = doc.make_shape(
        ShapeKind::Rect {
            a: Pt::new(50.0, 50.0),
            b: Pt::new(200.0, 150.0),
        },
        Style::default(),
    );
    let id = doc.add(rect);
    let mut ui = Ui::new(doc);
    let blue = Rgba {
        r: 0,
        g: 103,
        b: 237,
        a: 255,
    };
    // Stands in for the daemon's frozen-desktop pick.
    let picks = Rc::new(Cell::new(0));
    let clipboard = Arc::new(TextClipboard::default());
    let services = EditorServices {
        config: Arc::new(quikscope::config::Config::default()),
        clipboard: Some(clipboard.clone()),
        pick_color: Some(Rc::new({
            let picks = picks.clone();
            move |_: &mut gpui::App| {
                picks.set(picks.get() + 1);
                gpui::Task::ready(Ok(Some(blue)))
            }
        })),
    };
    ui.cx.update(|cx| cx.set_global(services));
    ui.editor(|e, cx| {
        e.set_tool(Tool::Select, cx);
        e.selected = BTreeSet::from([id]);
    });

    // `I` picks from the screen: the selection and the drawing color take
    // the color, and the picker opens beside the style panel to show it.
    ui.typ("i");
    assert_eq!(picks.get(), 1);
    assert_eq!(ui.doc(|d| d.shape(id).unwrap().style.color), blue);
    assert_eq!(ui.editor(|e, _| e.style.color), blue);
    assert!(ui.editor(|e, _| e.picker_open()));

    // The picker's right column: the eyedropper button, then HEX and one
    // field per row for RGB, CMYK, HSV and HSL.
    let panel = picker_bounds(&mut ui);
    let left = f32::from(panel.left());
    let column = left + 1.0 + 12.0 + LEFT_COLUMN + 16.0 + 100.0;
    let top = f32::from(panel.top()) + 1.0 + 12.0;
    ui.click(point(px(column), px(top + 14.0)));
    assert_eq!(picks.get(), 2, "the eyedropper button picks again");
    ui.click(point(px(column), px(top + 28.0 + 18.0 + 24.0)));
    for row in 0..4 {
        let y = top + 28.0 + 18.0 + 48.0 + 18.0 + 20.0 + row as f32 * 58.0;
        ui.click(point(px(column), px(y)));
    }
    assert_eq!(
        *clipboard.0.lock().unwrap(),
        [
            "#0067ed",
            "0, 103, 237",
            "100%, 57%, 0%, 7%",
            "214°, 100%, 93%",
            "214°, 100%, 46%",
        ]
    );

    // Esc closes the picker; one undo restores the color from before both
    // picks.
    ui.key("escape", None, Modifiers::default());
    assert!(!ui.editor(|e, _| e.picker_open()));
    ui.ctrl("z", false);
    assert_eq!(ui.doc(|d| d.shape(id).unwrap().style.color), PALETTE[0]);
}

/// Regression: a key the input method lets through while it is composing
/// (Backspace with a pre-edit showing) used to leave the field's marked range
/// pointing past its shortened text, and the next render panicked slicing it.
pub(super) fn field_key_edits_end_ime_composition() {
    use quikscope::ui::field::{Field, FieldKind};

    let text = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text,
        Arc::new(Assets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(quikscope::ui::init);
    let window = cx
        .open_window(size(px(300.), px(60.)), |window, cx| {
            cx.new(|cx| Field::new(FieldKind::Text, 0.0, window, cx))
        })
        .unwrap();
    let any: AnyWindowHandle = window.into();
    let field =
        |cx: &mut HeadlessAppContext,
         f: &mut dyn FnMut(&mut Field, &mut gpui::Window, &mut gpui::Context<Field>)| {
            cx.update_window(any, |view, window, cx| {
                view.downcast::<Field>()
                    .unwrap()
                    .update(cx, |fl, cx| f(fl, window, cx))
            })
            .unwrap();
            cx.run_until_parked();
            cx.capture_screenshot(any).unwrap();
        };
    field(&mut cx, &mut |f, w, cx| f.focus(w, cx));
    field(&mut cx, &mut |f, w, cx| {
        f.replace_and_mark_text_in_range(None, "に", None, w, cx)
    });
    assert_eq!(
        cx.update_window(any, |view, w, cx| view
            .downcast::<Field>()
            .unwrap()
            .update(cx, |f, cx| f.marked_text_range(w, cx)))
            .unwrap(),
        Some(0..1)
    );
    // Backspace reaches the field and deletes the pre-edit; rendering the
    // result must not panic, and composition is over.
    cx.update_window(any, |_, window, cx| {
        window.dispatch_event(
            PlatformInput::KeyDown(KeyDownEvent {
                keystroke: Keystroke {
                    modifiers: Modifiers::default(),
                    key: "backspace".into(),
                    key_char: None,
                },
                is_held: false,
                prefer_character_input: false,
            }),
            cx,
        );
    })
    .unwrap();
    cx.run_until_parked();
    cx.capture_screenshot(any).unwrap();
    let (text, marked) = cx
        .update_window(any, |view, w, cx| {
            view.downcast::<Field>().unwrap().update(cx, |f, cx| {
                (f.text().to_string(), f.marked_text_range(w, cx))
            })
        })
        .unwrap();
    assert_eq!((text.as_str(), marked), ("", None));
}

/// Scale factor of headless test windows; the sizes below assume it.
fn ui_scale() -> f32 {
    let mut ui = Ui::new(blank());
    ui.ime(|_, w, _| w.scale_factor())
}

/// The editor's zoom on screen once transitions settle, in device pixels per
/// image pixel (what the zoom label shows).
fn settled_zoom(ui: &mut Ui) -> f32 {
    std::thread::sleep(std::time::Duration::from_millis(320));
    ui.frame();
    ui.ime(|e, w, _| e.zoom() * w.scale_factor())
}

pub(super) fn zoom_moves_in_tenths_below_actual_size_and_quarters_above() {
    // The headless window is 1000x700 logical at scale 2: 3000x2000 fits at
    // 55% of actual pixels between the style panel's margins, which snaps to
    // the 50% level.
    let doc = Document::new(Arc::new(Image::filled(3000, 2000, [40, 40, 40, 255])), 1.0);
    assert_eq!(ui_scale(), 2.0);
    let mut ui = Ui::new(doc);
    let pct = |z: f32| (z * 100.0).round() as u32;
    assert_eq!(pct(settled_zoom(&mut ui)), 50);
    let mut seen = Vec::new();
    for _ in 0..6 {
        ui.ctrl("=", false);
        seen.push(pct(settled_zoom(&mut ui)));
    }
    assert_eq!(seen, [60, 70, 80, 90, 100, 125]);
    ui.ctrl("-", false);
    ui.ctrl("-", false);
    assert_eq!(pct(settled_zoom(&mut ui)), 90);
    // Fit snaps back down to a level.
    ui.ctrl("0", false);
    assert_eq!(pct(settled_zoom(&mut ui)), 50);
}

pub(super) fn drawing_past_the_edge_refits_the_view_to_the_contents() {
    // Fits at actual pixels in the 1000x700 (scale 2) test window.
    let doc = Document::new(Arc::new(Image::filled(1600, 1000, [40, 40, 40, 255])), 1.0);
    let mut ui = Ui::new(doc);
    assert_eq!((settled_zoom(&mut ui) * 100.0).round() as u32, 100);
    // A rectangle 1200 px off the left edge of the screenshot.
    let doc = ui.doc.clone();
    ui.cx.update(|cx| {
        doc.update(cx, |d, _| {
            let s = d.make_shape(
                ShapeKind::Rect {
                    a: Pt::new(-1200.0, 100.0),
                    b: Pt::new(-1000.0, 200.0),
                },
                Style::default(),
            );
            d.add(s);
        })
    });
    ui.frame();
    // 2800+ px of content now fits at 50%, all of it on screen and clear of
    // the style panel at the left edge.
    assert_eq!((settled_zoom(&mut ui) * 100.0).round() as u32, 50);
    let left = ui.screen(Pt::new(-1205.0, 100.0));
    let right = ui.screen(Pt::new(1600.0, 1000.0));
    let side = px(quikscope::ui::toolbar::SIDE_SPACE);
    assert!(
        left.x >= side && right.x <= px(1000.) - side,
        "{left:?} {right:?}"
    );
}

pub(super) fn select_tool_drags_text_and_hollow_shapes_from_inside_once_selected() {
    let mut doc = blank();
    let label = doc.make_shape(
        ShapeKind::Text {
            origin: Pt::new(40.0, 40.0),
            text: "Move me".into(),
            font: quikscope::fonts::FontSpec::default(),
        },
        Style::default(),
    );
    let label = doc.add(label);
    let rect = doc.make_shape(
        ShapeKind::Rect {
            a: Pt::new(300.0, 100.0),
            b: Pt::new(500.0, 300.0),
        },
        Style::default(),
    );
    let rect = doc.add(rect);
    // A second label inside the rectangle.
    let inner = doc.make_shape(
        ShapeKind::Text {
            origin: Pt::new(330.0, 250.0),
            text: "inside".into(),
            font: quikscope::fonts::FontSpec::default(),
        },
        Style::default(),
    );
    let inner = doc.add(inner);
    let mut ui = Ui::new(doc);
    ui.typ("v");
    let bounds = |ui: &mut Ui, id| ui.doc(|d| d.shape(id).unwrap().bounds());
    let image_at = |ui: &mut Ui, p| ui.editor(|e, _| e.transform().to_image(p));
    let drag = |ui: &mut Ui, from: Point<gpui::Pixels>, by: Point<gpui::Pixels>| {
        ui.mouse("down", from);
        ui.mouse("move", from + by * 0.5);
        ui.mouse("move", from + by);
        ui.mouse("up", from + by);
    };

    // Text moves with the pointer, as one undo step.
    let before = bounds(&mut ui, label);
    let from = ui.screen(before.center());
    let by = point(px(30.), px(20.));
    let (a, b) = (image_at(&mut ui, from), image_at(&mut ui, from + by));
    drag(&mut ui, from, by);
    let after = bounds(&mut ui, label);
    assert!(
        (after.x - before.x - (b.x - a.x)).abs() < 0.01,
        "{before:?} -> {after:?}"
    );
    assert!((after.y - before.y - (b.y - a.y)).abs() < 0.01);
    ui.ctrl("z", false);
    assert_eq!(bounds(&mut ui, label), before, "one undo puts it back");

    // Pressing inside a hollow, unselected rectangle does not grab it.
    let r0 = bounds(&mut ui, rect);
    let interior = ui.screen(Pt::new(400.0, 150.0));
    drag(&mut ui, interior, by);
    assert_eq!(bounds(&mut ui, rect), r0);
    // Clicking its outline selects it; then the interior drags it.
    let edge = ui.screen(Pt::new(300.0, 200.0));
    ui.click(edge);
    assert_eq!(ui.editor(|e, _| e.selected.clone()), BTreeSet::from([rect]));
    let interior = ui.screen(Pt::new(400.0, 150.0));
    drag(&mut ui, interior, by);
    let r1 = bounds(&mut ui, rect);
    assert!(r1.x > r0.x && r1.y > r0.y, "{r0:?} -> {r1:?}");

    // Items inside the selected rectangle are still grabbed directly.
    let inner_before = bounds(&mut ui, inner);
    let at = ui.screen(inner_before.center());
    drag(&mut ui, at, by);
    assert_eq!(
        ui.editor(|e, _| e.selected.clone()),
        BTreeSet::from([inner])
    );
    assert_eq!(bounds(&mut ui, rect), r1, "the rectangle stayed");
    assert!(bounds(&mut ui, inner).x > inner_before.x);
}

fn selection_scene() -> (Ui, Vec<u64>) {
    let mut doc = blank();
    let ids = (0..3)
        .map(|i| {
            let shape = doc.make_shape(
                ShapeKind::Rect {
                    a: Pt::new(100. + i as f32 * 100., 140.),
                    b: Pt::new(150. + i as f32 * 100., 190.),
                },
                Style {
                    fill: true,
                    ..Style::default()
                },
            );
            doc.add(shape)
        })
        .collect();
    let mut ui = Ui::new(doc);
    ui.typ("v");
    (ui, ids)
}

fn selection_bounds(ui: &mut Ui) -> Vec<quikscope::geom::RectF> {
    ui.doc(|d| d.shapes().iter().map(|s| s.bounds()).collect())
}

pub(super) fn marquee_contains_bounds_and_shift_click_toggles() {
    let (mut ui, ids) = selection_scene();
    let a = ui.screen(Pt::new(80., 120.));
    let b = ui.screen(Pt::new(230., 210.));
    ui.mouse("down", a);
    ui.mouse("move", b);
    ui.mouse("up", b);
    assert_eq!(
        ui.editor(|e, _| e.selected.clone()),
        BTreeSet::from([ids[0]])
    );
    for id in [ids[1], ids[0]] {
        let p = ui.doc(|d| d.shape(id).unwrap().bounds().center());
        let at = ui.screen(p);
        ui.input(PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: at,
            modifiers: Modifiers {
                shift: true,
                ..Default::default()
            },
            click_count: 1,
            first_mouse: false,
        }));
        ui.mouse("up", at);
    }
    assert_eq!(
        ui.editor(|e, _| e.selected.clone()),
        BTreeSet::from([ids[1]])
    );
}

pub(super) fn multi_move_and_nudge_each_record_one_undo_step() {
    let (mut ui, ids) = selection_scene();
    let before = selection_bounds(&mut ui);
    ui.ctrl("a", false);
    let at = ui.screen(Pt::new(120., 160.));
    ui.mouse("down", at);
    ui.mouse("move", at + point(px(15.), px(10.)));
    ui.mouse("move", at + point(px(30.), px(20.)));
    ui.mouse("up", at + point(px(30.), px(20.)));
    let after = selection_bounds(&mut ui);
    for (a, b) in before.iter().zip(&after) {
        assert!(b.x > a.x && b.y > a.y);
    }
    ui.ctrl("z", false);
    assert_eq!(selection_bounds(&mut ui), before);
    ui.ctrl("a", false);
    ui.key(
        "right",
        None,
        Modifiers {
            shift: true,
            ..Default::default()
        },
    );
    for (a, b) in before.iter().zip(selection_bounds(&mut ui)) {
        assert_eq!(b.x, a.x + 10.);
    }
    ui.ctrl("z", false);
    assert_eq!(selection_bounds(&mut ui), before);
    ui.ctrl("z", false);
    assert!(
        ui.doc(|d| d.shape(ids[2]).is_none()),
        "no hidden gesture checkpoints"
    );
}

pub(super) fn multi_style_and_delete_are_atomic() {
    let (mut ui, ids) = selection_scene();
    ui.ctrl("a", false);
    let before = selection_bounds(&mut ui);
    ui.editor(|e, cx| e.set_color(PALETTE[2], cx));
    assert!(ui.doc(|d| d.shapes().iter().all(|s| s.style.color == PALETTE[2])));
    ui.ctrl("z", false);
    assert!(ui.doc(|d| d.shapes().iter().all(|s| s.style.color == PALETTE[0])));
    ui.ctrl("a", false);
    ui.key("delete", None, Modifiers::default());
    assert!(ui.doc(|d| d.shapes().is_empty()));
    ui.ctrl("z", false);
    assert_eq!(selection_bounds(&mut ui), before);
    ui.ctrl("z", false);
    assert!(ui.doc(|d| d.shape(ids[2]).is_none()));
}

pub(super) fn duplicate_selection_has_offset_new_ids_and_one_undo_step() {
    let (mut ui, ids) = selection_scene();
    let before = selection_bounds(&mut ui);
    ui.ctrl("a", false);
    let zoom = ui.editor(|e, _| e.zoom());
    ui.ctrl("d", false);
    assert_eq!(ui.doc(|d| d.shapes().len()), 6);
    let selected = ui.editor(|e, _| e.selected.clone());
    assert_eq!(selected.len(), 3);
    assert!(ids.iter().all(|id| !selected.contains(id)));
    let copies = ui.doc(|d| {
        d.shapes()[3..]
            .iter()
            .map(|s| s.bounds())
            .collect::<Vec<_>>()
    });
    for (a, b) in before.iter().zip(copies) {
        assert!((b.x - a.x - 10. / zoom).abs() < 0.01);
        assert!((b.y - a.y - 10. / zoom).abs() < 0.01);
    }
    ui.ctrl("z", false);
    assert_eq!(selection_bounds(&mut ui), before);
    ui.ctrl("z", false);
    assert!(ui.doc(|d| d.shape(ids[2]).is_none()));
}

pub(super) fn selection_stacking_preserves_relative_order_and_undo() {
    let (mut ui, ids) = selection_scene();
    let a = ui.screen(Pt::new(80., 120.));
    let b = ui.screen(Pt::new(280., 210.));
    ui.mouse("down", a);
    ui.mouse("move", b);
    ui.mouse("up", b);
    ui.ctrl("]", true);
    assert_eq!(
        ui.doc(|d| d.shapes().iter().map(|s| s.id).collect::<Vec<_>>()),
        vec![ids[2], ids[0], ids[1]]
    );
    ui.ctrl("[", true);
    assert_eq!(
        ui.doc(|d| d.shapes().iter().map(|s| s.id).collect::<Vec<_>>()),
        ids
    );
    ui.ctrl("z", false);
    assert_eq!(ui.doc(|d| d.shapes().last().unwrap().id), ids[1]);
}

pub(super) fn shortcut_help_toggles_and_escape_keeps_editor_open() {
    let (mut ui, _) = selection_scene();
    ui.key(
        "/",
        Some("?"),
        Modifiers {
            shift: true,
            ..Default::default()
        },
    );
    assert!(ui.editor(|e, _| e.help_open));
    ui.key("escape", None, Modifiers::default());
    assert!(!ui.editor(|e, _| e.help_open));
    ui.key("f1", None, Modifiers::default());
    assert!(ui.editor(|e, _| e.help_open));
    ui.key("f1", None, Modifiers::default());
    assert!(!ui.editor(|e, _| e.help_open));
    ui.typ("t");
    let at = ui.screen(Pt::new(100., 100.));
    ui.click(at);
    ui.typ("Still editing");
    ui.key("f1", None, Modifiers::default());
    assert!(ui.editor(|e, _| e.help_open));
    ui.key("escape", None, Modifiers::default());
    assert!(ui.editor(|e, _| e.text.is_some()));
}

pub(super) fn shortcut_help_fits_small_windows_and_scrolls() {
    let mut ui = Ui::with_size(blank(), size(px(480.), px(320.)));
    ui.key("f1", None, Modifiers::default());
    let bounds = ui.editor(|e, _| e.help_scroll.bounds());
    assert!(bounds.size.width > px(0.) && bounds.size.height > px(0.));
    assert!(
        bounds.left() >= px(16.) && bounds.top() >= px(16.),
        "{bounds:?}"
    );
    assert!(
        bounds.right() <= px(464.) && bounds.bottom() <= px(304.),
        "{bounds:?}"
    );
    assert!(ui.editor(|e, _| e.help_scroll.max_offset().y) > px(0.));
}

/// Window position of style-panel swatch `i` (two per row).
fn swatch(panel: gpui::Bounds<gpui::Pixels>, i: usize) -> Point<gpui::Pixels> {
    // Border and padding, then 28 px cells 2 px apart.
    let at = |n: usize| px(1.0 + 6.0 + 14.0 + n as f32 * 30.0);
    panel.origin + point(at(i % 2), at(i / 2))
}

pub(super) fn style_panel_shows_for_drawing_tools_and_hides_for_pan_and_crop() {
    use quikscope::ui::editor::Tool;

    let mut doc = blank();
    let rect = doc.make_shape(
        ShapeKind::Rect {
            a: Pt::new(50.0, 50.0),
            b: Pt::new(200.0, 150.0),
        },
        Style::default(),
    );
    let id = doc.add(rect);
    let mut ui = Ui::new(doc);
    let shown = |ui: &mut Ui| ui.editor(|e, _| e.style_panel_shown());
    for tool in [
        Tool::Rect,
        Tool::Ellipse,
        Tool::Arrow,
        Tool::Line,
        Tool::Pen,
        Tool::Text,
        Tool::Highlighter,
        Tool::Step,
    ] {
        ui.editor(|e, cx| e.set_tool(tool, cx));
        ui.frame();
        assert!(shown(&mut ui), "{tool:?}");
    }

    // Its swatches work where it was laid out: left edge, centred.
    let panel = ui.editor(|e, _| e.chrome.style.bounds());
    assert_eq!(panel.left(), px(quikscope::ui::toolbar::EDGE));
    assert!((panel.center().y - px(350.)).abs() <= px(1.), "{panel:?}");
    ui.click(swatch(panel, 1));
    assert_eq!(ui.editor(|e, _| e.style.color), PALETTE[1]);

    // Hidden, the same spot no longer reaches a swatch.
    for tool in [Tool::Hand, Tool::Crop, Tool::Pixelate] {
        ui.editor(|e, cx| e.set_tool(tool, cx));
        ui.frame();
        assert!(!shown(&mut ui), "{tool:?}");
        ui.click(swatch(panel, 2));
        assert_eq!(ui.editor(|e, _| e.style.color), PALETTE[1], "{tool:?}");
    }

    // The select tool shows it only for a selection to restyle.
    ui.editor(|e, cx| e.set_tool(Tool::Select, cx));
    ui.frame();
    assert!(!shown(&mut ui));
    ui.editor(|e, _| e.selected = BTreeSet::from([id]));
    ui.frame();
    assert!(shown(&mut ui));
    ui.click(swatch(panel, 2));
    assert_eq!(ui.doc(|d| d.shape(id).unwrap().style.color), PALETTE[2]);
}

/// At the narrow end the clusters stay apart (and inside the window), and
/// a fitted image stays clear of all of them.
pub(super) fn chrome_clusters_never_overlap_in_narrow_windows() {
    let image = || Document::new(Arc::new(Image::filled(1600, 900, [40, 40, 40, 255])), 1.0);
    for (w, h) in [(520.0, 600.0), (480.0, 320.0)] {
        let mut ui = Ui::with_size(image(), size(px(w), px(h)));
        ui.editor(|e, cx| e.set_tool(quikscope::ui::editor::Tool::Rect, cx));
        ui.frame();
        let panels = ui.editor(|e, _| {
            [
                ("tools", e.chrome.tools.bounds()),
                ("style", e.chrome.style.bounds()),
                ("history", e.chrome.history.bounds()),
                ("output", e.chrome.output.bounds()),
            ]
        });
        let window = gpui::Bounds::new(point(px(0.), px(0.)), size(px(w), px(h)));
        for (i, (name, a)) in panels.iter().enumerate() {
            assert!(a.size.width > px(0.), "{name} at {w}x{h} was not laid out");
            let last = a.bottom_right() - point(px(1.), px(1.));
            assert!(
                window.contains(&a.origin) && window.contains(&last),
                "{name} {a:?} outside {w}x{h}"
            );
            for (other, b) in &panels[i + 1..] {
                assert!(
                    !a.intersects(b),
                    "{name} {a:?} overlaps {other} {b:?} at {w}x{h}"
                );
            }
        }
        if h == 600.0 {
            let image = gpui::Bounds::from_corners(
                ui.screen(Pt::new(0.0, 0.0)),
                ui.screen(Pt::new(1600.0, 900.0)),
            );
            for (name, b) in &panels {
                assert!(!image.intersects(b), "image {image:?} under {name} {b:?}");
            }
        }
    }
}
