use gpui::{
    App, Context, Entity, IntoElement, Keystroke, MouseButton, Render, Window, div, prelude::*, px,
    rgb,
};

const KEYBOARD_HEIGHT: f32 = 208.0;

/// Root-level Kobo overlay that adds a virtual keyboard without changing the
/// application's individual text-input components.
pub struct KoboKeyboardRoot<V: Render + 'static> {
    content: Entity<V>,
    shifted: bool,
    symbols: bool,
    dismissed: bool,
}

impl<V: Render + 'static> KoboKeyboardRoot<V> {
    pub fn new(content: Entity<V>) -> Self {
        Self {
            content,
            shifted: false,
            symbols: false,
            dismissed: false,
        }
    }

    fn dispatch_key(key: &str, window: &mut Window, cx: &mut App) {
        if let Ok(keystroke) = Keystroke::parse(key) {
            window.dispatch_keystroke(keystroke, cx);
        }
    }

    fn key(
        &self,
        id: &'static str,
        label: &'static str,
        key: &'static str,
        width: f32,
    ) -> impl IntoElement {
        div()
            .id(id)
            .w(px(width))
            .h(px(48.0))
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0xffffff))
            .border_2()
            .border_color(rgb(0x111111))
            .text_color(rgb(0x111111))
            .text_size(px(20.0))
            .child(label)
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(move |_, window, cx| Self::dispatch_key(key, window, cx))
    }

    fn character_key(&self, character: char) -> impl IntoElement {
        let display = if self.shifted {
            character.to_ascii_uppercase()
        } else {
            character
        };
        let key = if self.shifted {
            format!("shift-{character}")
        } else {
            character.to_string()
        };
        div()
            .id(("kobo-key", character as u32))
            .flex()
            .flex_1()
            .h(px(48.0))
            .items_center()
            .justify_center()
            .bg(rgb(0xffffff))
            .border_2()
            .border_color(rgb(0x111111))
            .text_color(rgb(0x111111))
            .text_size(px(20.0))
            .child(display.to_string())
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(move |_, window, cx| Self::dispatch_key(&key, window, cx))
    }

    fn literal_character_key(&self, character: char) -> impl IntoElement {
        let key = character.to_string();
        div()
            .id(("kobo-literal-key", character as u32))
            .flex()
            .flex_1()
            .h(px(48.0))
            .items_center()
            .justify_center()
            .bg(rgb(0xffffff))
            .border_2()
            .border_color(rgb(0x111111))
            .text_color(rgb(0x111111))
            .text_size(px(20.0))
            .child(character.to_string())
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(move |_, window, cx| Self::dispatch_key(&key, window, cx))
    }

    fn row(&self, id: &'static str, characters: &'static str) -> impl IntoElement {
        let mut row = div().id(id).flex().w_full().gap(px(3.0));
        for character in characters.chars() {
            row = row.child(self.character_key(character));
        }
        row
    }

    fn literal_row(&self, id: &'static str, characters: &'static str) -> impl IntoElement {
        let mut row = div().id(id).flex().w_full().gap(px(3.0));
        for character in characters.chars() {
            row = row.child(self.literal_character_key(character));
        }
        row
    }
}

impl<V: Render + 'static> Render for KoboKeyboardRoot<V> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input_active = window.is_text_input_active();
        if !input_active {
            self.dismissed = false;
            self.shifted = false;
            self.symbols = false;
        }

        let mut root = div().relative().size_full().child(self.content.clone());
        if !input_active {
            return root;
        }

        if self.dismissed {
            return root.child(
                div()
                    .id("kobo-show-keyboard")
                    .absolute()
                    .right(px(8.0))
                    .bottom(px(8.0))
                    .w(px(72.0))
                    .h(px(48.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(0xffffff))
                    .border_2()
                    .border_color(rgb(0x111111))
                    .child("KB")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.dismissed = false;
                        cx.notify();
                    })),
            );
        }

        let shift_label = if self.shifted { "SHIFT" } else { "Shift" };
        root = root.child(
            div()
                .id("kobo-virtual-keyboard")
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(px(KEYBOARD_HEIGHT))
                .p(px(5.0))
                .flex()
                .flex_col()
                .gap(px(3.0))
                .bg(rgb(0xe7e7e7))
                .border_t_2()
                .border_color(rgb(0x111111))
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .child(if self.symbols {
                    self.literal_row("kobo-number-row", "1234567890")
                        .into_any_element()
                } else {
                    self.row("kobo-top-row", "qwertyuiop").into_any_element()
                })
                .child(if self.symbols {
                    self.literal_row("kobo-symbol-row", "@#$%&*()-+")
                        .into_any_element()
                } else {
                    self.row("kobo-home-row", "asdfghjkl").into_any_element()
                })
                .child(if self.symbols {
                    div()
                        .id("kobo-punctuation-row")
                        .flex()
                        .w_full()
                        .gap(px(3.0))
                        .children(
                            ".,?!'\":;/"
                                .chars()
                                .map(|character| self.literal_character_key(character)),
                        )
                        .child(self.key("kobo-backspace", "Del", "backspace", 72.0))
                        .into_any_element()
                } else {
                    div()
                        .flex()
                        .w_full()
                        .gap(px(3.0))
                        .child(
                            div()
                                .id("kobo-shift")
                                .w(px(72.0))
                                .h(px(48.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(if self.shifted {
                                    rgb(0x111111)
                                } else {
                                    rgb(0xffffff)
                                })
                                .text_color(if self.shifted {
                                    rgb(0xffffff)
                                } else {
                                    rgb(0x111111)
                                })
                                .border_2()
                                .border_color(rgb(0x111111))
                                .child(shift_label)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.shifted = !this.shifted;
                                    cx.notify();
                                })),
                        )
                        .children(
                            "zxcvbnm"
                                .chars()
                                .map(|character| self.character_key(character)),
                        )
                        .child(self.key("kobo-backspace", "Del", "backspace", 72.0))
                        .into_any_element()
                })
                .child(
                    div()
                        .flex()
                        .w_full()
                        .gap(px(3.0))
                        .child(
                            div()
                                .id("kobo-symbols-toggle")
                                .w(px(76.0))
                                .h(px(48.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(if self.symbols {
                                    rgb(0x111111)
                                } else {
                                    rgb(0xffffff)
                                })
                                .text_color(if self.symbols {
                                    rgb(0xffffff)
                                } else {
                                    rgb(0x111111)
                                })
                                .border_2()
                                .border_color(rgb(0x111111))
                                .child(if self.symbols { "ABC" } else { "123" })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.symbols = !this.symbols;
                                    this.shifted = false;
                                    cx.notify();
                                })),
                        )
                        .children((!self.symbols).then(|| self.key("kobo-hyphen", "-", "-", 52.0)))
                        .child(self.key("kobo-space", "Space", "space", 220.0))
                        .child(self.key("kobo-enter", "Enter", "enter", 84.0))
                        .child(
                            div()
                                .id("kobo-hide-keyboard")
                                .w(px(62.0))
                                .h(px(48.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(rgb(0xffffff))
                                .border_2()
                                .border_color(rgb(0x111111))
                                .child("Hide")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.dismissed = true;
                                    cx.notify();
                                })),
                        ),
                ),
        );
        root
    }
}
