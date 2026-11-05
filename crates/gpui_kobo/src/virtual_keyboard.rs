use gpui::{
    App, Context, Div, ElementId, Entity, FontWeight, Global, IntoElement, Keystroke, MouseButton,
    Pixels, Render, SharedString, Stateful, Window, div, prelude::*, px, rems, rgb,
};

/// Letter rows for a language where every letter has its own key. Columns
/// come from the longest row, so key widths stay equal within a layout.
pub struct KoboKeyboardLayout {
    pub name: &'static str,
    pub code: &'static str,
    pub rows: [&'static str; 3],
}

impl KoboKeyboardLayout {
    fn columns(&self) -> f32 {
        let [top, middle, bottom] = self.rows.map(|row| row.chars().count());
        // Row 3 also holds shift and backspace, at least one column each.
        top.max(middle).max(bottom + 2) as f32
    }
}

pub const KOBO_KEYBOARD_LAYOUTS: &[KoboKeyboardLayout] = &[
    KoboKeyboardLayout { name: "Svenska", code: "SV", rows: ["qwertyuiopå", "asdfghjklöä", "zxcvbnm"] },
    KoboKeyboardLayout { name: "Suomi", code: "FI", rows: ["qwertyuiopå", "asdfghjklöä", "zxcvbnm"] },
    KoboKeyboardLayout { name: "Norsk", code: "NO", rows: ["qwertyuiopå", "asdfghjkløæ", "zxcvbnm"] },
    KoboKeyboardLayout { name: "Dansk", code: "DA", rows: ["qwertyuiopå", "asdfghjklæø", "zxcvbnm"] },
    KoboKeyboardLayout { name: "English", code: "EN", rows: ["qwertyuiop", "asdfghjkl", "zxcvbnm"] },
    KoboKeyboardLayout { name: "Nederlands", code: "NL", rows: ["qwertyuiop", "asdfghjkl", "zxcvbnm"] },
    KoboKeyboardLayout { name: "Deutsch", code: "DE", rows: ["qwertzuiopü", "asdfghjklöä", "yxcvbnmß"] },
    KoboKeyboardLayout { name: "Eesti", code: "ET", rows: ["qwertyuiopüõ", "asdfghjklöä", "zxcvbnmšž"] },
    KoboKeyboardLayout { name: "Hrvatski", code: "HR", rows: ["qwertzuiopšđ", "asdfghjklčćž", "yxcvbnm"] },
    KoboKeyboardLayout { name: "Slovenščina", code: "SL", rows: ["qwertzuiopš", "asdfghjklčž", "yxcvbnm"] },
    KoboKeyboardLayout { name: "Русский", code: "RU", rows: ["йцукенгшщзхъ", "фывапролджэё", "ячсмитьбю"] },
    KoboKeyboardLayout { name: "Українська", code: "UK", rows: ["йцукенгшщзхї", "фівапролджє", "ячсмитьбюґ"] },
    KoboKeyboardLayout { name: "Беларуская", code: "BE", rows: ["йцукенгшўзх", "фывапролджэё", "ячсмітьбю"] },
    KoboKeyboardLayout { name: "Српски", code: "SR", rows: ["љњертзуиопшђ", "асдфгхјклчћж", "џцвбнм"] },
    KoboKeyboardLayout { name: "Македонски", code: "MK", rows: ["љњертѕуиопшѓ", "асдфгхјклчќж", "зџцвбнм"] },
];

/// The selected layout by code. Legacy preference lists are accepted, with
/// the first known code used as the current layout. The application restores
/// this value from its preferences and observes it to save switches.
/// All layouts are always available in the keyboard's layout chooser.
pub struct KoboKeyboardLayouts(pub Vec<String>);

impl Global for KoboKeyboardLayouts {}

impl Default for KoboKeyboardLayouts {
    fn default() -> Self {
        Self(vec![KOBO_KEYBOARD_LAYOUTS[0].code.to_owned()])
    }
}

/// The current layout: the first saved code that names a known layout.
fn current_layout(cx: &App) -> &'static KoboKeyboardLayout {
    cx.try_global::<KoboKeyboardLayouts>()
        .and_then(|layouts| {
            layouts.0.iter().find_map(|code| KOBO_KEYBOARD_LAYOUTS.iter().find(|layout| layout.code == code.as_str()))
        })
        .unwrap_or(&KOBO_KEYBOARD_LAYOUTS[0])
}

/// The height the keyboard takes from the bottom of the window, zero while no
/// text input is active. Window-sized overlays subtract it so they end above
/// the keys, as the application content does.
pub fn keyboard_height(window: &Window) -> Pixels {
    if !window.is_text_input_active() {
        return px(0.0);
    }
    window.rem_size() * (KEY_HEIGHT_REM * 4.0)
        + px(ROW_GAP * 3.0 + PADDING_TOP + PADDING_BOTTOM + BORDER_TOP)
}

const SYMBOL_ROWS: [&str; 3] = ["1234567890+", "@#€%&*()-_=", "!?'\":;/"];
const SYMBOL_COLUMNS: f32 = 11.0;

const KEY_HEIGHT_REM: f32 = 3.25;
const CHARACTER_TEXT_REM: f32 = 1.4;
const ACTION_TEXT_REM: f32 = 1.05;
const KEY_GAP: f32 = 6.0;
const KEY_RADIUS: f32 = 6.0;
const ROW_GAP: f32 = 8.0;
const PADDING_TOP: f32 = 8.0;
const PADDING_BOTTOM: f32 = 10.0;
const PADDING_SIDE: f32 = 6.0;
const BORDER_TOP: f32 = 1.0;

const INK: u32 = 0x111111;
const PAPER: u32 = 0xffffff;
const MUTED: u32 = 0x555555;
const CHARACTER_FILL: u32 = 0xe4e4e4;
const ACTION_FILL: u32 = 0xbdbdbd;

/// Root-level Kobo view that adds a virtual keyboard below the application
/// without changing its individual text-input components. The content
/// shrinks above the keyboard so the focused field stays visible.
pub struct KoboKeyboardRoot<V: Render + 'static> {
    content: Entity<V>,
    font_family: fn(&App) -> SharedString,
    shifted: bool,
    symbols: bool,
    choosing_layout: bool,
}

impl<V: Render + 'static> KoboKeyboardRoot<V> {
    /// `font_family` returns the application's UI font, which the keyboard
    /// cannot inherit because it sits outside the application's root.
    pub fn new(content: Entity<V>, font_family: fn(&App) -> SharedString) -> Self {
        Self {
            content,
            font_family,
            shifted: false,
            symbols: false,
            choosing_layout: false,
        }
    }

    fn dispatch_key(key: &str, window: &mut Window, cx: &mut App) {
        if let Ok(keystroke) = Keystroke::parse(key) {
            window.defer_keystroke(keystroke, cx);
        }
    }

    fn key(id: impl Into<ElementId>, units: f32, fill: u32) -> Stateful<Div> {
        div()
            .id(id)
            .flex_grow(units)
            .flex_basis(px(0.0))
            .min_w(px(0.0))
            .h(rems(KEY_HEIGHT_REM))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(KEY_RADIUS))
            .bg(rgb(fill))
            .text_color(rgb(INK))
            .active(|style| style.bg(rgb(INK)).text_color(rgb(PAPER)))
    }

    /// Solid black marks an active key (shift or the selected layout).
    fn action_key(id: &'static str, label: impl Into<SharedString>, units: f32, on: bool) -> Stateful<Div> {
        Self::key(id, units, if on { INK } else { ACTION_FILL })
            .when(on, |key| key.text_color(rgb(PAPER)))
            .text_size(rems(ACTION_TEXT_REM))
            .font_weight(FontWeight::SEMIBOLD)
            .child(label.into())
    }

    fn character_key(&self, character: char, cx: &mut Context<Self>) -> Stateful<Div> {
        // Shift only applies to letters with a single-character capital, so
        // ß stays ß instead of becoming "SS".
        let capital: String = character.to_uppercase().collect();
        let shift = self.shifted && character.is_alphabetic() && capital.chars().count() == 1;
        let label = if shift { capital } else { character.to_string() };
        let keystroke = if shift {
            format!("shift-{character}")
        } else {
            character.to_string()
        };
        Self::key(("kobo-key", character as u32), 1.0, CHARACTER_FILL)
            .text_size(rems(CHARACTER_TEXT_REM))
            .child(label)
            .on_click(cx.listener(move |this, _, window, cx| {
                Self::dispatch_key(&keystroke, window, cx);
                if this.shifted {
                    this.shifted = false;
                    cx.notify();
                }
            }))
    }

    fn character_keys(&self, characters: &'static str, cx: &mut Context<Self>) -> Vec<Stateful<Div>> {
        characters.chars().map(|character| self.character_key(character, cx)).collect()
    }

    fn row() -> Div {
        div().flex().w_full().gap(px(KEY_GAP))
    }

    fn spacer(units: f32) -> Div {
        div().flex_grow(units).flex_basis(px(0.0)).min_w(px(0.0))
    }

    /// Rows shorter than the layout's column count are centred.
    fn centred_row(&self, characters: &'static str, columns: f32, cx: &mut Context<Self>) -> Div {
        let padding = (columns - characters.chars().count() as f32) / 2.0;
        Self::row()
            .when(padding > 0.0, |row| row.child(Self::spacer(padding)))
            .children(self.character_keys(characters, cx))
            .when(padding > 0.0, |row| row.child(Self::spacer(padding)))
    }

    fn backspace_key(units: f32) -> Stateful<Div> {
        Self::action_key("kobo-backspace", "←", units, false)
            .on_click(|_, window, cx| Self::dispatch_key("backspace", window, cx))
    }

    fn bottom_row(&self, layout: &'static KoboKeyboardLayout, columns: f32, cx: &mut Context<Self>) -> Div {
        Self::row()
            .child(
                Self::action_key("kobo-symbols-toggle", if self.symbols { "ABC" } else { "123" }, 1.5, false)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.symbols = !this.symbols;
                        this.shifted = false;
                        cx.notify();
                    })),
            )
            .child(
                Self::action_key("kobo-layout", layout.code, 1.0, false).on_click(cx.listener(|this, _, _, cx| {
                    this.choosing_layout = true;
                    cx.notify();
                })),
            )
            .child(self.character_key(',', cx))
            .child(
                Self::key("kobo-space", columns - 7.0, CHARACTER_FILL)
                    .text_size(rems(ACTION_TEXT_REM))
                    .text_color(rgb(MUTED))
                    .child(layout.name)
                    .on_click(|_, window, cx| Self::dispatch_key("space", window, cx)),
            )
            .child(self.character_key('.', cx))
            .child(
                Self::action_key("kobo-enter", "↲", 1.5, false)
                    .on_click(|_, window, cx| Self::dispatch_key("enter", window, cx)),
            )
            .child(
                // Blurring ends text input, which removes the keyboard;
                // tapping the field again brings it back.
                Self::action_key("kobo-hide-keyboard", "↓", 1.0, false)
                    .on_click(|_, window, _| window.blur()),
            )
    }

    fn layout_chooser(
        &self,
        keyboard: Stateful<Div>,
        current: &KoboKeyboardLayout,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let mut keyboard = keyboard;
        // Fifteen layouts and Cancel fit in four rows, keeping exactly the
        // same height as the keys so the focused input never moves.
        for (row_index, layouts) in KOBO_KEYBOARD_LAYOUTS.chunks(4).enumerate() {
            let mut row = Self::row();
            for layout in layouts {
                row = row.child(
                    Self::action_key(layout.code, layout.name, 1.0, layout.code == current.code)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.set_global(KoboKeyboardLayouts(vec![layout.code.to_owned()]));
                            this.choosing_layout = false;
                            this.shifted = false;
                            this.symbols = false;
                            cx.notify();
                        })),
                );
            }
            if row_index == 3 {
                row = row.child(
                    Self::action_key("kobo-layout-cancel", "Cancel", 1.0, false)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.choosing_layout = false;
                            cx.notify();
                        })),
                );
            }
            keyboard = keyboard.child(row);
        }
        keyboard
    }

    fn keyboard(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let layout = current_layout(cx);
        // Heights here must stay in step with `keyboard_height`.
        let keyboard = div()
            .id("kobo-virtual-keyboard")
            .flex_shrink_0()
            .w_full()
            .pt(px(PADDING_TOP))
            .px(px(PADDING_SIDE))
            .pb(px(PADDING_BOTTOM))
            .flex()
            .flex_col()
            .gap(px(ROW_GAP))
            .bg(rgb(PAPER))
            .border_t(px(BORDER_TOP))
            .border_color(rgb(INK))
            .font_family((self.font_family)(cx))
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            });

        if self.choosing_layout {
            return self.layout_chooser(keyboard, layout, cx);
        }

        if self.symbols {
            let [top, middle, bottom] = SYMBOL_ROWS;
            let backspace_units = SYMBOL_COLUMNS - bottom.chars().count() as f32;
            return keyboard
                .child(Self::row().children(self.character_keys(top, cx)))
                .child(Self::row().children(self.character_keys(middle, cx)))
                .child(
                    Self::row()
                        .children(self.character_keys(bottom, cx))
                        .child(Self::backspace_key(backspace_units)),
                )
                .child(self.bottom_row(layout, SYMBOL_COLUMNS, cx));
        }

        let columns = layout.columns();
        let [top, middle, bottom] = layout.rows;
        let side = (columns - bottom.chars().count() as f32) / 2.0;
        keyboard
            .child(self.centred_row(top, columns, cx))
            .child(self.centred_row(middle, columns, cx))
            .child(
                Self::row()
                    .child(
                        Self::action_key("kobo-shift", "↑", side, self.shifted).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.shifted = !this.shifted;
                                cx.notify();
                            },
                        )),
                    )
                    .children(self.character_keys(bottom, cx))
                    .child(Self::backspace_key(side)),
            )
            .child(self.bottom_row(layout, columns, cx))
    }
}

impl<V: Render + 'static> Render for KoboKeyboardRoot<V> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let input_active = window.is_text_input_active();
        if !input_active {
            self.shifted = false;
            self.symbols = false;
            self.choosing_layout = false;
        }

        // The content keeps the same place in the tree whether or not the
        // keyboard shows, so its element state survives the switch.
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.content.clone()),
            )
            .children(input_active.then(|| self.keyboard(cx)))
    }
}
