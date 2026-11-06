//! Editor interaction through GPUI's real input dispatch (offscreen window,
//! headless WGPU renderer): text formatting shortcuts and the color picker.

use std::sync::Arc;

use gpui::EntityInputHandler as _;
use gpui::{
    AnyWindowHandle, AppContext as _, Entity, HeadlessAppContext, KeyDownEvent, Keystroke,
    Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, Point,
    WindowHandle, point, px, size,
};
use moment::document::{Document, PALETTE, ShapeKind, Style};
use moment::geom::Pt;
use moment::raster::Image;
use moment::ui::color_picker::WHEEL;
use moment::ui::editor::Editor;
use moment::ui::{Assets, Shot};

struct Ui {
    cx: HeadlessAppContext,
    window: WindowHandle<Editor>,
    doc: Entity<Document>,
}

impl Ui {
    fn new(doc: Document) -> Ui {
        let text = gpui_platform::current_platform(true).text_system();
        let mut cx = HeadlessAppContext::with_platform(
            text,
            Arc::new(Assets),
            gpui_platform::current_headless_renderer,
        );
        cx.update(moment::ui::init);
        let (base, scale) = (doc.base.clone(), doc.scale);
        let doc = cx.update(|cx| cx.new(|_| doc));
        let shot = Arc::new(Shot::new(1, base, scale, None));
        let d = doc.clone();
        let window = cx
            .open_window(size(px(1000.), px(700.)), |window, cx| {
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

fn blank() -> Document {
    Document::new(Arc::new(Image::filled(600, 400, [40, 40, 40, 255])), 1.0)
}

fn text_font(doc: &Document) -> moment::fonts::FontSpec {
    match &doc.shapes().last().expect("a text shape").kind {
        ShapeKind::Text { font, .. } => font.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

#[test]
fn input_method_composition_commits_into_the_text() {
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

#[test]
fn formatting_shortcuts_apply_while_typing_and_to_selected_text() {
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
        e.set_tool(moment::ui::editor::Tool::Select, cx);
        e.selected = Some(id);
    });
    ui.ctrl("b", false);
    assert_eq!(ui.doc(text_font).weight, 400);
    ui.ctrl("z", false);
    assert_eq!(ui.doc(text_font).weight, 700);
    // New text uses the last font chosen.
    assert_eq!(ui.editor(|e, _| e.font.weight), 400);
}

#[test]
fn color_picker_drag_recolors_the_selection_as_one_undo_step() {
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
    ui.editor(|e, cx| {
        e.set_tool(moment::ui::editor::Tool::Select, cx);
        e.selected = Some(id);
        e.toggle_picker(px(500.), cx);
    });
    ui.frame();

    // The panel hangs centred under x = 500, below the toolbar.
    let centre = point(px(500.), px(60.0 + 1.0 + 12.0 + WHEEL / 2.0));
    ui.mouse("down", centre + point(px(WHEEL / 2.0 - 4.0), px(0.)));
    ui.mouse("move", centre + point(px(0.), px(-(WHEEL / 2.0 - 4.0))));
    ui.mouse("move", centre + point(px(-(WHEEL / 2.0 - 4.0)), px(0.)));
    ui.mouse("up", centre + point(px(-(WHEEL / 2.0 - 4.0)), px(0.)));

    let color = ui.doc(|d| d.shape(id).unwrap().style.color);
    // Left edge of the wheel is cyan.
    assert!(color.r < 40 && color.g > 200 && color.b > 200, "{color:?}");
    assert_eq!(ui.editor(|e, _| e.style.color), color, "tool color follows");

    let undo_steps = ui.doc(|d| d.can_undo());
    assert!(undo_steps);
    ui.ctrl("z", false);
    assert_eq!(
        ui.doc(|d| d.shape(id).unwrap().style.color),
        PALETTE[0],
        "one undo restores the color from before the drag"
    );
    ui.ctrl("z", false);
    assert!(
        ui.doc(|d| d.shape(id).is_none()),
        "next undo removes the rect itself"
    );

    // Escape closes the picker without touching the drawing.
    assert!(ui.editor(|e, _| e.picker_open()));
    ui.key("escape", None, Modifiers::default());
    assert!(!ui.editor(|e, _| e.picker_open()));
}

/// Regression: a key the input method lets through while it is composing
/// (Backspace with a pre-edit showing) used to leave the field's marked range
/// pointing past its shortened text, and the next render panicked slicing it.
#[test]
fn field_key_edits_end_ime_composition() {
    use moment::ui::field::{Field, FieldKind};

    let text = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text,
        Arc::new(Assets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(moment::ui::init);
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

#[test]
fn zoom_moves_in_tenths_below_actual_size_and_quarters_above() {
    // The headless window is 1000x700 logical at scale 2: 3000x2000 fits at
    // 61% of actual pixels, which snaps to the 60% level.
    let doc = Document::new(Arc::new(Image::filled(3000, 2000, [40, 40, 40, 255])), 1.0);
    assert_eq!(ui_scale(), 2.0);
    let mut ui = Ui::new(doc);
    let pct = |z: f32| (z * 100.0).round() as u32;
    assert_eq!(pct(settled_zoom(&mut ui)), 60);
    let mut seen = Vec::new();
    for _ in 0..5 {
        ui.ctrl("=", false);
        seen.push(pct(settled_zoom(&mut ui)));
    }
    assert_eq!(seen, [70, 80, 90, 100, 125]);
    ui.ctrl("-", false);
    ui.ctrl("-", false);
    assert_eq!(pct(settled_zoom(&mut ui)), 90);
    // Fit snaps back down to a level.
    ui.ctrl("0", false);
    assert_eq!(pct(settled_zoom(&mut ui)), 60);
}

#[test]
fn drawing_past_the_edge_refits_the_view_to_the_contents() {
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
    // 2800+ px of content now fits at 60%, and all of it is on screen.
    assert_eq!((settled_zoom(&mut ui) * 100.0).round() as u32, 60);
    let left = ui.screen(Pt::new(-1205.0, 100.0));
    let right = ui.screen(Pt::new(1600.0, 1000.0));
    assert!(left.x > px(0.) && right.x < px(1000.), "{left:?} {right:?}");
}
