//! Font popover shown above the text being edited or selected: family,
//! weight, italic and size.

use std::ops::Range;
use std::sync::Arc;

use gpui::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _, Render,
    ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled as _,
    UniformListScrollHandle, Window, div, prelude::FluentBuilder as _, px, svg, uniform_list,
};

use super::field::{Field, FieldEvent, FieldKind};
use super::theme;
use crate::fonts::{self, FontSpec};

pub const MENU_HEIGHT: f32 = 40.0;
pub const MENU_WIDTH: f32 = 372.0;
/// Space the family dropdown needs below (or above) the menu row.
pub const DROPDOWN_HEIGHT: f32 = 300.0;
const LIST_HEIGHT: f32 = 240.0;
const ROW_HEIGHT: f32 = 26.0;

/// Steps for the − / + size buttons.
const SIZE_STEPS: [f32; 17] = [
    8.0, 10.0, 12.0, 14.0, 16.0, 18.0, 20.0, 24.0, 28.0, 32.0, 40.0, 48.0, 64.0, 72.0, 96.0, 128.0,
    192.0,
];

pub fn next_size(size: f32, up: bool) -> f32 {
    let next = if up {
        SIZE_STEPS.iter().copied().find(|s| *s > size + 0.01)
    } else {
        SIZE_STEPS.iter().rev().copied().find(|s| *s < size - 0.01)
    };
    next.unwrap_or(size).clamp(fonts::MIN_SIZE, fonts::MAX_SIZE)
}

/// Families matching `query` (case-insensitive substring), prefix matches first.
pub fn filter_families(all: &[Arc<str>], query: &str) -> Vec<Arc<str>> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return all.to_vec();
    }
    let (mut prefix, mut inner): (Vec<_>, Vec<_>) = (Vec::new(), Vec::new());
    for f in all {
        let l = f.to_lowercase();
        if l.starts_with(&q) {
            prefix.push(f.clone());
        } else if l.contains(&q) {
            inner.push(f.clone());
        }
    }
    prefix.extend(inner);
    prefix
}

#[derive(Clone, Debug, PartialEq)]
pub enum FontMenuEvent {
    Changed(FontSpec),
    /// The menu is done with the keyboard; give focus back to the canvas.
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Dropdown {
    Family,
    Weight,
}

pub struct FontMenu {
    spec: FontSpec,
    open: Option<Dropdown>,
    filter: Entity<Field>,
    size: Entity<Field>,
    matches: Vec<Arc<str>>,
    highlighted: usize,
    weights: Vec<u16>,
    drop_up: bool,
    scroll: UniformListScrollHandle,
}

impl EventEmitter<FontMenuEvent> for FontMenu {}

impl FontMenu {
    pub fn new(spec: FontSpec, window: &mut Window, cx: &mut Context<Self>) -> FontMenu {
        let filter =
            cx.new(|cx| Field::new(FieldKind::Text, 0.0, window, cx).placeholder("Search fonts…"));
        let size = cx.new(|cx| {
            Field::new(
                FieldKind::Number {
                    min: fonts::MIN_SIZE as i64,
                    max: fonts::MAX_SIZE as i64,
                },
                40.0,
                window,
                cx,
            )
        });
        cx.subscribe_in(
            &filter,
            window,
            |this, _, ev: &FieldEvent, window, cx| match ev {
                FieldEvent::Changed(q) => {
                    this.matches = filter_families(fonts::families(), q);
                    this.highlighted = 0;
                    this.scroll.scroll_to_item(0, ScrollStrategy::Top);
                    cx.notify();
                }
                FieldEvent::Submit(_) => {
                    if let Some(name) = this.matches.get(this.highlighted).cloned() {
                        this.pick_family(name, window, cx);
                    }
                }
                FieldEvent::Cancel => this.close(cx),
            },
        )
        .detach();
        cx.subscribe_in(
            &size,
            window,
            |this, _, ev: &FieldEvent, window, cx| match ev {
                FieldEvent::Submit(t) => match t.parse::<f32>() {
                    Ok(v) => {
                        let spec = this.spec.with_size(v);
                        this.change(spec, window, cx);
                    }
                    Err(_) => this.sync_size(window, cx),
                },
                FieldEvent::Cancel => {
                    this.sync_size(window, cx);
                    cx.emit(FontMenuEvent::Done);
                }
                FieldEvent::Changed(_) => {}
            },
        )
        .detach();
        let mut menu = FontMenu {
            weights: fonts::family_weights(&spec.family),
            spec,
            open: None,
            filter,
            size,
            matches: Vec::new(),
            highlighted: 0,
            drop_up: false,
            scroll: UniformListScrollHandle::new(),
        };
        menu.sync_size(window, cx);
        menu
    }

    pub fn spec(&self) -> &FontSpec {
        &self.spec
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Shows `spec` (from the selected text) without emitting a change.
    pub fn set_spec(&mut self, spec: &FontSpec, window: &Window, cx: &mut Context<Self>) {
        if *spec == self.spec {
            return;
        }
        if spec.family != self.spec.family {
            self.weights = fonts::family_weights(&spec.family);
        }
        self.spec = spec.clone();
        self.sync_size(window, cx);
        cx.notify();
    }

    pub fn set_drop_up(&mut self, up: bool, cx: &mut Context<Self>) {
        if self.drop_up != up {
            self.drop_up = up;
            cx.notify();
        }
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if self.open.take().is_some() {
            cx.emit(FontMenuEvent::Done);
            cx.notify();
        }
    }

    fn sync_size(&mut self, window: &Window, cx: &mut Context<Self>) {
        let text = format!("{}", self.spec.size.round() as i64);
        self.size.update(cx, |f, cx| f.set_text(text, window, cx));
    }

    fn change(&mut self, spec: FontSpec, window: &Window, cx: &mut Context<Self>) {
        self.spec = spec.clone();
        self.sync_size(window, cx);
        cx.emit(FontMenuEvent::Changed(spec));
        cx.notify();
    }

    fn toggle(&mut self, which: Dropdown, window: &mut Window, cx: &mut Context<Self>) {
        if self.open == Some(which) {
            self.close(cx);
            return;
        }
        self.open = Some(which);
        if which == Dropdown::Family {
            self.matches = fonts::families().to_vec();
            self.highlighted = self
                .matches
                .iter()
                .position(|f| *f == self.spec.family)
                .unwrap_or(0);
            self.scroll
                .scroll_to_item(self.highlighted, ScrollStrategy::Center);
            self.filter.update(cx, |f, cx| {
                f.clear(cx);
                f.focus(window, cx);
            });
        }
        cx.notify();
    }

    fn pick_family(&mut self, family: Arc<str>, window: &Window, cx: &mut Context<Self>) {
        self.weights = fonts::family_weights(&family);
        // Keep the weight if the family has it, else the nearest one it has.
        let weight = self
            .weights
            .iter()
            .copied()
            .min_by_key(|w| w.abs_diff(self.spec.weight))
            .unwrap_or(400);
        let spec = FontSpec {
            family,
            weight,
            ..self.spec.clone()
        };
        self.open = None;
        self.change(spec, window, cx);
        cx.emit(FontMenuEvent::Done);
    }

    fn pick_weight(&mut self, weight: u16, window: &Window, cx: &mut Context<Self>) {
        self.open = None;
        let spec = FontSpec {
            weight,
            ..self.spec.clone()
        };
        self.change(spec, window, cx);
        cx.emit(FontMenuEvent::Done);
    }

    fn key_down(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.open != Some(Dropdown::Family) || self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() - 1;
        self.highlighted = match ev.keystroke.key.as_str() {
            "down" => (self.highlighted + 1).min(last),
            "up" => self.highlighted.saturating_sub(1),
            "pagedown" => (self.highlighted + 8).min(last),
            "pageup" => self.highlighted.saturating_sub(8),
            _ => return,
        };
        self.scroll
            .scroll_to_item(self.highlighted, ScrollStrategy::Nearest);
        cx.stop_propagation();
        cx.notify();
    }

    fn family_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        range
            .filter_map(|i| self.matches.get(i).cloned().map(|f| (i, f)))
            .map(|(i, family)| {
                let active = family == self.spec.family;
                let name = SharedString::from(family.to_string());
                div()
                    .id(("family", i))
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .text_sm()
                    .font_family(super::ui_family(&family))
                    .text_color(if active {
                        theme::accent()
                    } else {
                        theme::text()
                    })
                    .when(i == self.highlighted, |d| d.bg(theme::hover()))
                    .hover(|d| d.bg(theme::hover()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.pick_family(family.clone(), window, cx)
                    }))
                    .child(name)
                    .into_any_element()
            })
            .collect()
    }
}

fn chip(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(28.))
        .px_2()
        .flex()
        .items_center()
        .gap(px(4.))
        .rounded_md()
        .text_sm()
        .text_color(theme::text())
        .hover(|d| d.bg(theme::hover()))
}

fn icon(name: &'static str) -> impl IntoElement {
    svg()
        .path(format!("icons/{name}.svg"))
        .size(px(14.))
        .text_color(theme::muted())
}

impl Render for FontMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let spec = self.spec.clone();
        let family = super::ui_family(&spec.family);
        let italic = spec.italic;

        let row =
            div()
                .id("font-menu")
                .h(px(MENU_HEIGHT))
                .w(px(MENU_WIDTH))
                .px(px(6.))
                .flex()
                .items_center()
                .gap(px(2.))
                .rounded_lg()
                .bg(theme::panel())
                .border_1()
                .border_color(theme::panel_border())
                .shadow_lg()
                .child(
                    chip("font-family")
                        .w(px(150.))
                        .justify_between()
                        .when(self.open == Some(Dropdown::Family), |d| {
                            d.bg(theme::accent_bg())
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle(Dropdown::Family, window, cx)
                        }))
                        .child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .font_family(family)
                                .child(spec.family.to_string()),
                        )
                        .child(icon("chevron-down")),
                )
                .child(
                    chip("font-weight")
                        .w(px(96.))
                        .justify_between()
                        .when(self.open == Some(Dropdown::Weight), |d| {
                            d.bg(theme::accent_bg())
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle(Dropdown::Weight, window, cx)
                        }))
                        .child(
                            div()
                                .font_weight(FontWeight(spec.weight as f32))
                                .child(fonts::weight_name(spec.weight)),
                        )
                        .child(icon("chevron-down")),
                )
                .child(
                    chip("font-italic")
                        .w(px(28.))
                        .justify_center()
                        .when(italic, |d| d.bg(theme::accent_bg()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let spec = FontSpec {
                                italic: !this.spec.italic,
                                ..this.spec.clone()
                            };
                            this.change(spec, window, cx);
                            cx.emit(FontMenuEvent::Done);
                        }))
                        .child(svg().path("icons/italic.svg").size(px(14.)).text_color(
                            if italic {
                                theme::accent()
                            } else {
                                theme::text()
                            },
                        )),
                )
                .child(
                    chip("font-smaller")
                        .w(px(24.))
                        .justify_center()
                        .on_click(cx.listener(|this, _, window, cx| {
                            let spec = this.spec.with_size(next_size(this.spec.size, false));
                            this.change(spec, window, cx);
                            cx.emit(FontMenuEvent::Done);
                        }))
                        .child(icon("minus")),
                )
                .child(self.size.clone())
                .child(
                    chip("font-larger")
                        .w(px(24.))
                        .justify_center()
                        .on_click(cx.listener(|this, _, window, cx| {
                            let spec = this.spec.with_size(next_size(this.spec.size, true));
                            this.change(spec, window, cx);
                            cx.emit(FontMenuEvent::Done);
                        }))
                        .child(icon("plus")),
                );

        let dropdown =
            self.open.map(|which| {
                let panel = div()
                    .id("font-dropdown")
                    .occlude()
                    .absolute()
                    .left(px(0.))
                    .p(px(6.))
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .rounded_lg()
                    .bg(theme::panel())
                    .border_1()
                    .border_color(theme::panel_border())
                    .shadow_lg()
                    .when(self.drop_up, |d| d.bottom(px(MENU_HEIGHT + 4.)))
                    .when(!self.drop_up, |d| d.top(px(MENU_HEIGHT + 4.)));
                match which {
                    Dropdown::Family => panel.w(px(260.)).child(self.filter.clone()).child(
                        if self.matches.is_empty() {
                            div()
                                .h(px(ROW_HEIGHT))
                                .px_2()
                                .text_sm()
                                .text_color(theme::muted())
                                .child("No matching fonts")
                                .into_any_element()
                        } else {
                            uniform_list(
                                "families",
                                self.matches.len(),
                                cx.processor(|this, range, _, cx| this.family_rows(range, cx)),
                            )
                            .track_scroll(&self.scroll)
                            .h(px(LIST_HEIGHT.min(self.matches.len() as f32 * ROW_HEIGHT)))
                            .into_any_element()
                        },
                    ),
                    Dropdown::Weight => panel.left(px(150.)).w(px(150.)).children(
                        self.weights.clone().into_iter().map(|w| {
                            let active = w == spec.weight;
                            div()
                                .id(("weight", w as usize))
                                .h(px(ROW_HEIGHT))
                                .px_2()
                                .flex()
                                .items_center()
                                .rounded_md()
                                .text_sm()
                                .font_family(super::ui_family(&spec.family))
                                .font_weight(FontWeight(w as f32))
                                .when(italic, |d| d.italic())
                                .text_color(if active {
                                    theme::accent()
                                } else {
                                    theme::text()
                                })
                                .hover(|d| d.bg(theme::hover()))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.pick_weight(w, window, cx)
                                }))
                                .child(format!("{} {w}", fonts::weight_name(w)))
                        }),
                    ),
                }
            });

        div()
            .relative()
            .occlude()
            .font_family(super::UI_FONT)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(row)
            .children(dropdown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_steps_move_to_the_neighbouring_preset() {
        assert_eq!(next_size(28.0, true), 32.0);
        assert_eq!(next_size(28.0, false), 24.0);
        assert_eq!(next_size(30.0, false), 28.0, "between presets");
        assert_eq!(next_size(192.0, true), 192.0, "top stays put");
        assert_eq!(next_size(8.0, false), 8.0);
    }

    #[test]
    fn family_filter_ranks_prefix_matches_first() {
        let all: Vec<Arc<str>> = [
            "Inter",
            "Fira Mono",
            "JetBrains Mono",
            "Mononoki",
            "Noto Sans",
        ]
        .into_iter()
        .map(Arc::from)
        .collect();
        let found = filter_families(&all, "mono");
        let names: Vec<&str> = found.iter().map(|s| &**s).collect();
        assert_eq!(names, ["Mononoki", "Fira Mono", "JetBrains Mono"]);
        assert_eq!(filter_families(&all, "  ").len(), all.len());
        assert!(filter_families(&all, "zzz").is_empty());
    }
}
