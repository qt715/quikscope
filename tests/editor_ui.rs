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
use quickscope::document::{Document, PALETTE, ShapeKind, Style};
use quickscope::geom::Pt;
use quickscope::raster::Image;
use quickscope::ui::color_picker::WHEEL;
use quickscope::ui::editor::Editor;
use quickscope::ui::{Assets, Shot};

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
        cx.update(quickscope::ui::init);
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

fn text_font(doc: &Document) -> quickscope::fonts::FontSpec {
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
        e.set_tool(quickscope::ui::editor::Tool::Select, cx);
        e.selected = BTreeSet::from([id]);
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
        e.set_tool(quickscope::ui::editor::Tool::Select, cx);
        e.selected = BTreeSet::from([id, second]);
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

/// Regression: a key the input method lets through while it is composing
/// (Backspace with a pre-edit showing) used to leave the field's marked range
/// pointing past its shortened text, and the next render panicked slicing it.
#[test]
fn field_key_edits_end_ime_composition() {
    use quickscope::ui::field::{Field, FieldKind};

    let text = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text,
        Arc::new(Assets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(quickscope::ui::init);
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

#[test]
fn select_tool_drags_text_and_hollow_shapes_from_inside_once_selected() {
    let mut doc = blank();
    let label = doc.make_shape(
        ShapeKind::Text {
            origin: Pt::new(40.0, 40.0),
            text: "Move me".into(),
            font: quickscope::fonts::FontSpec::default(),
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
            font: quickscope::fonts::FontSpec::default(),
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

fn selection_bounds(ui: &mut Ui) -> Vec<quickscope::geom::RectF> {
    ui.doc(|d| d.shapes().iter().map(|s| s.bounds()).collect())
}

#[test]
fn marquee_contains_bounds_and_shift_click_toggles() {
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

#[test]
fn multi_move_and_nudge_each_record_one_undo_step() {
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

#[test]
fn multi_style_and_delete_are_atomic() {
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
