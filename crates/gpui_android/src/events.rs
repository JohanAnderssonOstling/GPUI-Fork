use crate::window::AndroidWindowInner;
use android_activity::input::{
    InputEvent, KeyAction, KeyCharacterMap, KeyMapChar, Keycode, MetaState, MotionAction,
    TextInputAction, TextInputState, TextSpan,
};
use android_activity::{AndroidApp, InputStatus};
use gpui::{
    KeyDownEvent, KeyLocation, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, PlatformInput, Point,
    ScrollDelta, ScrollWheelEvent, TouchEvent, TouchId, TouchPhase, point, px,
};
use std::collections::HashMap;
use std::ops::Range;
use std::time::Instant;

/// Distance (logical px) a touch may travel before it stops being a tap and
/// becomes a scroll.
const TOUCH_SLOP: f32 = 8.0;
const DOUBLE_TAP_MILLIS: u128 = 400;
const DOUBLE_TAP_DISTANCE: f32 = 16.0;

#[derive(Debug, Eq, PartialEq)]
struct TextReplacement {
    old_range: Range<usize>,
    new_range: Range<usize>,
    replacement: String,
}

fn text_replacement(old_text: &str, new_text: &str) -> TextReplacement {
    let old_characters = old_text.chars().collect::<Vec<_>>();
    let new_characters = new_text.chars().collect::<Vec<_>>();
    let prefix_characters = old_characters
        .iter()
        .zip(&new_characters)
        .take_while(|(old, new)| old == new)
        .count();
    let suffix_characters = old_characters[prefix_characters..]
        .iter()
        .rev()
        .zip(new_characters[prefix_characters..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();

    let prefix_utf16 = old_characters[..prefix_characters]
        .iter()
        .map(|character| character.len_utf16())
        .sum::<usize>();
    let old_suffix_utf16 = old_characters[old_characters.len() - suffix_characters..]
        .iter()
        .map(|character| character.len_utf16())
        .sum::<usize>();
    let new_suffix_utf16 = new_characters[new_characters.len() - suffix_characters..]
        .iter()
        .map(|character| character.len_utf16())
        .sum::<usize>();

    TextReplacement {
        old_range: prefix_utf16..old_text.encode_utf16().count() - old_suffix_utf16,
        new_range: prefix_utf16..new_text.encode_utf16().count() - new_suffix_utf16,
        replacement: new_characters[prefix_characters..new_characters.len() - suffix_characters]
            .iter()
            .collect(),
    }
}

fn utf16_offset_to_byte(text: &str, utf16_offset: usize) -> usize {
    let mut current_utf16 = 0;
    for (byte_offset, character) in text.char_indices() {
        if current_utf16 >= utf16_offset {
            return byte_offset;
        }
        let next_utf16 = current_utf16 + character.len_utf16();
        if utf16_offset < next_utf16 {
            return byte_offset;
        }
        current_utf16 = next_utf16;
    }
    text.len()
}

fn utf16_slice(text: &str, range: Range<usize>) -> String {
    let start = utf16_offset_to_byte(text, range.start);
    let end = utf16_offset_to_byte(text, range.end).max(start);
    text[start..end].to_owned()
}

fn replace_utf16(text: &str, range: Range<usize>, replacement: &str) -> String {
    let start = utf16_offset_to_byte(text, range.start);
    let end = utf16_offset_to_byte(text, range.end).max(start);
    format!("{}{}{}", &text[..start], replacement, &text[end..])
}

fn span_range(span: TextSpan, text_length: usize) -> Range<usize> {
    let start = span.start.min(text_length);
    let end = span.end.min(text_length);
    start.min(end)..start.max(end)
}

fn selection_within(selection: &Range<usize>, outer: &Range<usize>) -> Option<Range<usize>> {
    (selection.start >= outer.start && selection.end <= outer.end)
        .then(|| selection.start - outer.start..selection.end - outer.start)
}

fn apply_text_input_state(window: &AndroidWindowInner, text_input_state: &TextInputState) -> bool {
    let handled = window.with_input_handler(|input_handler| {
        let Some(old_text) = AndroidWindowInner::input_handler_text(input_handler) else {
            return false;
        };

        let new_text_length = text_input_state.text.encode_utf16().count();
        let selection = span_range(text_input_state.selection, new_text_length);
        let compose_region = text_input_state
            .compose_region
            .map(|span| span_range(span, new_text_length))
            .filter(|range| !range.is_empty());

        if let Some(compose_region) = compose_region {
            let compose_text = utf16_slice(&text_input_state.text, compose_region.clone());
            let selected_range = selection_within(&selection, &compose_region);
            let marked_range = input_handler.marked_text_range();
            let replaced_existing_composition = marked_range.as_ref().is_some_and(|marked_range| {
                replace_utf16(&old_text, marked_range.clone(), &compose_text)
                    == text_input_state.text
            });

            if replaced_existing_composition {
                input_handler.replace_and_mark_text_in_range(
                    marked_range,
                    &compose_text,
                    selected_range,
                );
            } else {
                let replacement = text_replacement(&old_text, &text_input_state.text);
                let replacement_selection = (replacement.new_range == compose_region)
                    .then(|| selection_within(&selection, &replacement.new_range))
                    .flatten();
                input_handler.replace_and_mark_text_in_range(
                    Some(replacement.old_range),
                    &replacement.replacement,
                    replacement_selection,
                );
                if replacement.new_range != compose_region {
                    input_handler.replace_and_mark_text_in_range(
                        Some(compose_region.clone()),
                        &compose_text,
                        selected_range,
                    );
                }
            }
        } else {
            if old_text != text_input_state.text {
                let replacement = text_replacement(&old_text, &text_input_state.text);
                input_handler
                    .replace_text_in_range(Some(replacement.old_range), &replacement.replacement);
            }
            input_handler.unmark_text();
        }
        input_handler.set_selected_text_range(selection);
        true
    });

    handled == Some(true)
}

fn dispatch_ime_key(window: &AndroidWindowInner, key: &str, modifiers: Modifiers) {
    let keystroke = Keystroke {
        modifiers,
        key: key.to_owned(),
        key_char: None,
    };
    window.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
        keystroke: keystroke.clone(),
        key_location: KeyLocation::Standard,
        is_held: false,
        prefer_character_input: false,
    }));
    window.dispatch_input(PlatformInput::KeyUp(KeyUpEvent {
        keystroke,
        key_location: KeyLocation::Standard,
    }));
}

fn handle_text_action(window: &AndroidWindowInner, action: TextInputAction) -> InputStatus {
    match action {
        TextInputAction::Next => dispatch_ime_key(window, "tab", Modifiers::default()),
        TextInputAction::Previous => dispatch_ime_key(window, "tab", Modifiers::shift()),
        TextInputAction::Unspecified
        | TextInputAction::None
        | TextInputAction::Go
        | TextInputAction::Search
        | TextInputAction::Send
        | TextInputAction::Done => {
            dispatch_ime_key(window, "enter", Modifiers::default());
        }
        _ => return InputStatus::Unhandled,
    }
    InputStatus::Handled
}

pub(crate) fn dispatch_reader_page_key(window: &AndroidWindowInner, next: bool) {
    let keystroke = Keystroke {
        modifiers: Modifiers::default(),
        key: if next { "pagedown" } else { "pageup" }.to_owned(),
        key_char: None,
    };
    window.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
        keystroke: keystroke.clone(),
        key_location: KeyLocation::Standard,
        is_held: false,
        prefer_character_input: false,
    }));
    window.dispatch_input(PlatformInput::KeyUp(KeyUpEvent {
        keystroke,
        key_location: KeyLocation::Standard,
    }));
}

pub(crate) fn dispatch_reader_chrome_reveal(window: &AndroidWindowInner) {
    let keystroke = Keystroke { modifiers: Modifiers::default(), key: "f24".to_owned(), key_char: None };
    window.dispatch_input(PlatformInput::KeyDown(KeyDownEvent { keystroke: keystroke.clone(), key_location: KeyLocation::Standard, is_held: false, prefer_character_input: false }));
    window.dispatch_input(PlatformInput::KeyUp(KeyUpEvent { keystroke, key_location: KeyLocation::Standard }));
}

pub(crate) struct ClickState {
    last_position: Point<Pixels>,
    last_time: Option<Instant>,
    current_count: usize,
}

impl Default for ClickState {
    fn default() -> Self {
        Self {
            last_position: Point::default(),
            last_time: None,
            current_count: 0,
        }
    }
}

impl ClickState {
    fn register_click(&mut self, position: Point<Pixels>) -> usize {
        let now = Instant::now();
        let distance = ((f32::from(position.x) - f32::from(self.last_position.x)).powi(2)
            + (f32::from(position.y) - f32::from(self.last_position.y)).powi(2))
        .sqrt();

        let within_double_tap = self
            .last_time
            .is_some_and(|last| now.duration_since(last).as_millis() < DOUBLE_TAP_MILLIS);
        if within_double_tap && distance < DOUBLE_TAP_DISTANCE {
            self.current_count += 1;
        } else {
            self.current_count = 1;
        }

        self.last_position = position;
        self.last_time = Some(now);
        self.current_count
    }
}

/// Single-finger gesture recognizer. Raw touch events preserve input modality;
/// a tap additionally becomes MouseDown + MouseUp for GPUI's existing click
/// handlers, and a drag past the slop becomes a ScrollWheel stream with touch
/// phases so content follows the finger.
#[derive(Default)]
pub(crate) enum TouchGesture {
    #[default]
    None,
    Pending {
        start: Point<Pixels>,
        last: Point<Pixels>,
    },
    Scrolling {
        last: Point<Pixels>,
    },
}

pub(crate) fn handle_input_event(
    event: &InputEvent<'_>,
    window: &AndroidWindowInner,
    gesture: &mut TouchGesture,
    key_maps: &mut HashMap<i32, KeyCharacterMap>,
    app: &AndroidApp,
    finish_activity: &mut bool,
) -> InputStatus {
    match event {
        InputEvent::MotionEvent(motion_event) => {
            let scale = window.state.borrow().scale_factor;
            let pointer_index = motion_event.pointer_index();
            let pointer = motion_event.pointer_at_index(pointer_index);
            let position = point(px(pointer.x() / scale), px(pointer.y() / scale));
            let touch_id = TouchId(pointer.pointer_id().max(0) as u64);
            window.state.borrow_mut().mouse_position = position;

            match motion_event.action() {
                MotionAction::Down => {
                    // A new gesture supersedes any tap handoff that did not find
                    // a matching input handler in the preceding registrations.
                    window.pending_ime_tap.set(None);
                    window.pending_ime_tap_had_handler.set(false);
                    window.pending_ime_previous_bounds.set(None);
                    window.pending_ime_registrations.set(0);
                    window.dispatch_input(PlatformInput::Touch(TouchEvent {
                        id: touch_id,
                        phase: TouchPhase::Started,
                        position,
                        force: None,
                    }));
                    *gesture = TouchGesture::Pending {
                        start: position,
                        last: position,
                    };
                }
                MotionAction::Move => {
                    window.dispatch_input(PlatformInput::Touch(TouchEvent {
                        id: touch_id,
                        phase: TouchPhase::Moved,
                        position,
                        force: None,
                    }));
                    match gesture {
                        TouchGesture::Pending { start, last } => {
                            let moved = ((f32::from(position.x) - f32::from(start.x)).powi(2)
                                + (f32::from(position.y) - f32::from(start.y)).powi(2))
                            .sqrt();
                            if moved > TOUCH_SLOP {
                                let delta = point(position.x - last.x, position.y - last.y);
                                let anchor = *start;
                                *gesture = TouchGesture::Scrolling { last: position };
                                window.dispatch_input(PlatformInput::ScrollWheel(
                                    ScrollWheelEvent {
                                        position: anchor,
                                        delta: ScrollDelta::Pixels(delta),
                                        modifiers: Modifiers::default(),
                                        touch_phase: TouchPhase::Started,
                                    },
                                ));
                            } else {
                                *last = position;
                            }
                        }
                        TouchGesture::Scrolling { last } => {
                            let delta = point(position.x - last.x, position.y - last.y);
                            *last = position;
                            window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                                position,
                                delta: ScrollDelta::Pixels(delta),
                                modifiers: Modifiers::default(),
                                touch_phase: TouchPhase::Moved,
                            }));
                        }
                        TouchGesture::None => {}
                    }
                }
                MotionAction::Up => {
                    match std::mem::take(gesture) {
                        TouchGesture::Pending { start, .. } => {
                            let click_count = window.click_state.borrow_mut().register_click(start);
                            // Snapshot the handler before dispatching the click.
                            // MouseDown/MouseUp may synchronously move focus and
                            // replace it, so sampling afterward loses the field
                            // that was focused when the tap began.
                            let input_handler_bounds = window
                                .with_input_handler(|handler| handler.element_bounds())
                                .flatten();
                            let had_input_handler = window.state.borrow().input_handler.is_some();
                            let tapped_input =
                                input_handler_bounds.is_some_and(|bounds| bounds.contains(&start));
                            if !tapped_input {
                                window.pending_ime_tap_had_handler.set(had_input_handler);
                                window.pending_ime_previous_bounds.set(input_handler_bounds);
                                window.pending_ime_tap.set(Some(start));
                                window.pending_ime_registrations.set(3);
                                app.hide_soft_input(false);
                            }
                            window.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
                                position: start,
                                pressed_button: None,
                                modifiers: Modifiers::default(),
                            }));
                            window.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
                                button: MouseButton::Left,
                                position: start,
                                modifiers: Modifiers::default(),
                                click_count,
                                first_mouse: false,
                            }));
                            window.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
                                button: MouseButton::Left,
                                position: start,
                                modifiers: Modifiers::default(),
                                click_count,
                            }));
                            // A focused input handler remains registered while the
                            // cursor is visible, even when the user taps elsewhere.
                            // Only a tap inside that handler's element bounds is an
                            // explicit request to show the keyboard.
                            if tapped_input {
                                window.sync_text_input_state();
                                app.show_soft_input(false);
                            }
                        }
                        TouchGesture::Scrolling { .. } => {
                            window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                                position,
                                delta: ScrollDelta::Pixels(Point::default()),
                                modifiers: Modifiers::default(),
                                touch_phase: TouchPhase::Ended,
                            }));
                        }
                        TouchGesture::None => {}
                    }
                    window.dispatch_input(PlatformInput::Touch(TouchEvent {
                        id: touch_id,
                        phase: TouchPhase::Ended,
                        position,
                        force: None,
                    }));
                }
                MotionAction::Cancel => {
                    if matches!(gesture, TouchGesture::Scrolling { .. }) {
                        window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position,
                            delta: ScrollDelta::Pixels(Point::default()),
                            modifiers: Modifiers::default(),
                            touch_phase: TouchPhase::Ended,
                        }));
                    }
                    *gesture = TouchGesture::None;
                    window.dispatch_input(PlatformInput::Touch(TouchEvent {
                        id: touch_id,
                        phase: TouchPhase::Cancelled,
                        position,
                        force: None,
                    }));
                }
                _ => return InputStatus::Unhandled,
            }
            InputStatus::Handled
        }
        InputEvent::KeyEvent(key_event) => {
            let keycode = key_event.key_code();
            let Some(key) = keycode_to_key(keycode) else {
                return InputStatus::Unhandled;
            };
            let meta_state = key_event.meta_state();
            let modifiers = modifiers_from_meta_state(meta_state);

            {
                let mut state = window.state.borrow_mut();
                state.modifiers = modifiers;
                state.capslock = gpui::Capslock {
                    on: meta_state.caps_lock_on(),
                };
            }
            window.dispatch_input(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                modifiers,
                capslock: gpui::Capslock {
                    on: meta_state.caps_lock_on(),
                },
            }));

            if key.is_empty() {
                return InputStatus::Handled;
            }

            let key_char = key_char_for(key_event.device_id(), keycode, meta_state, key_maps, app);
            let keystroke = Keystroke {
                modifiers,
                key: key.to_owned(),
                key_char: key_char.clone(),
            };
            let key_location = key_location(keycode);

            match key_event.action() {
                KeyAction::Down => {
                    let result = window.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
                        keystroke,
                        key_location,
                        is_held: false,
                        prefer_character_input: false,
                    }));

                    let propagate = result.as_ref().is_none_or(|result| result.propagate);
                    if keycode == Keycode::Back && propagate {
                        *finish_activity = true;
                    }

                    if propagate
                        && modifiers.is_subset_of(&Modifiers::shift())
                        && let Some(text) = key_char
                    {
                        window.with_input_handler(|handler| {
                            handler.replace_text_in_range(None, &text);
                        });
                    }
                    InputStatus::Handled
                }
                KeyAction::Up => {
                    window.dispatch_input(PlatformInput::KeyUp(KeyUpEvent {
                        keystroke,
                        key_location,
                    }));
                    InputStatus::Handled
                }
                _ => InputStatus::Unhandled,
            }
        }
        InputEvent::TextEvent(text_input_state) => {
            if apply_text_input_state(window, text_input_state) {
                InputStatus::Handled
            } else {
                InputStatus::Unhandled
            }
        }
        InputEvent::TextAction(action) => handle_text_action(window, *action),
        _ => InputStatus::Unhandled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_replacement_appends_ascii_text() {
        assert_eq!(
            text_replacement("mail@example.co", "mail@example.com"),
            TextReplacement {
                old_range: 15..15,
                new_range: 15..16,
                replacement: "m".to_owned(),
            }
        );
    }

    #[test]
    fn text_replacement_deletes_a_selection() {
        assert_eq!(
            text_replacement("one two three", "one three"),
            TextReplacement {
                old_range: 4..8,
                new_range: 4..4,
                replacement: String::new(),
            }
        );
    }

    #[test]
    fn text_replacement_does_not_split_surrogate_pairs() {
        assert_eq!(
            text_replacement("A😀Z", "A😃Z"),
            TextReplacement {
                old_range: 1..3,
                new_range: 1..3,
                replacement: "😃".to_owned(),
            }
        );
    }

    #[test]
    fn utf16_helpers_preserve_non_ascii_text() {
        assert_eq!(utf16_slice("a😀ö", 1..3), "😀");
        assert_eq!(replace_utf16("a😀ö", 1..3, "界"), "a界ö");
    }
}

fn key_location(keycode: Keycode) -> KeyLocation {
    match keycode {
        Keycode::Numpad0
        | Keycode::Numpad1
        | Keycode::Numpad2
        | Keycode::Numpad3
        | Keycode::Numpad4
        | Keycode::Numpad5
        | Keycode::Numpad6
        | Keycode::Numpad7
        | Keycode::Numpad8
        | Keycode::Numpad9
        | Keycode::NumpadDivide
        | Keycode::NumpadMultiply
        | Keycode::NumpadSubtract
        | Keycode::NumpadAdd
        | Keycode::NumpadDot
        | Keycode::NumpadComma
        | Keycode::NumpadEnter
        | Keycode::NumpadEquals
        | Keycode::NumpadLeftParen
        | Keycode::NumpadRightParen => KeyLocation::Numpad,
        _ => KeyLocation::Standard,
    }
}

fn key_char_for(
    device_id: i32,
    keycode: Keycode,
    meta_state: MetaState,
    key_maps: &mut HashMap<i32, KeyCharacterMap>,
    app: &AndroidApp,
) -> Option<String> {
    const VIRTUAL_KEYBOARD_DEVICE_ID: i32 = -1;

    if !key_maps.contains_key(&device_id) {
        // Some devices (e.g. the emulator's forwarded host keyboard, id 0)
        // have no per-device map; the built-in virtual keyboard map always
        // exists. Cache whichever we got under the original id.
        let map = app.device_key_character_map(device_id).or_else(|error| {
            log::info!(
                "no key character map for device {device_id} ({error:?}); \
                 falling back to the virtual keyboard map"
            );
            app.device_key_character_map(VIRTUAL_KEYBOARD_DEVICE_ID)
        });
        match map {
            Ok(map) => {
                key_maps.insert(device_id, map);
            }
            Err(error) => {
                log::warn!("failed to load key character map for device {device_id}: {error:?}");
                return None;
            }
        }
    }
    let map = key_maps.get(&device_id)?;
    match map.get(keycode, meta_state) {
        Ok(KeyMapChar::Unicode(character)) => Some(character.to_string()),
        Ok(_) => None,
        Err(error) => {
            log::warn!("KeyCharacterMap.get failed: {error:?}");
            None
        }
    }
}

fn modifiers_from_meta_state(meta_state: MetaState) -> Modifiers {
    Modifiers {
        control: meta_state.ctrl_on(),
        alt: meta_state.alt_on(),
        shift: meta_state.shift_on(),
        platform: meta_state.meta_on(),
        function: meta_state.function_on(),
    }
}

/// Maps an Android keycode to GPUI's key names (see `Keystroke::parse`).
/// Returns `Some("")` for modifier keys (handled via ModifiersChanged) and
/// `None` for keys we don't handle so the OS can apply default behavior
/// (volume, media controls, etc.).
fn keycode_to_key(keycode: Keycode) -> Option<&'static str> {
    use Keycode::*;
    Some(match keycode {
        A => "a",
        B => "b",
        C => "c",
        D => "d",
        E => "e",
        F => "f",
        G => "g",
        H => "h",
        I => "i",
        J => "j",
        K => "k",
        L => "l",
        M => "m",
        N => "n",
        O => "o",
        P => "p",
        Q => "q",
        R => "r",
        S => "s",
        T => "t",
        U => "u",
        V => "v",
        W => "w",
        X => "x",
        Y => "y",
        Z => "z",
        Keycode0 => "0",
        Keycode1 => "1",
        Keycode2 => "2",
        Keycode3 => "3",
        Keycode4 => "4",
        Keycode5 => "5",
        Keycode6 => "6",
        Keycode7 => "7",
        Keycode8 => "8",
        Keycode9 => "9",
        Space => "space",
        Enter | NumpadEnter => "enter",
        Tab => "tab",
        Del => "backspace",
        ForwardDel => "delete",
        Back => "back",
        Escape => "escape",
        DpadUp => "up",
        DpadDown => "down",
        DpadLeft => "left",
        DpadRight => "right",
        PageUp => "pageup",
        PageDown => "pagedown",
        MoveHome => "home",
        MoveEnd => "end",
        Comma => ",",
        Period => ".",
        Minus => "-",
        Equals => "=",
        LeftBracket => "[",
        RightBracket => "]",
        Backslash => "\\",
        Semicolon => ";",
        Apostrophe => "'",
        Slash => "/",
        Grave => "`",
        ShiftLeft | ShiftRight | CtrlLeft | CtrlRight | AltLeft | AltRight | MetaLeft
        | MetaRight | CapsLock => "",
        _ => return None,
    })
}
