#[cfg(target_os = "android")]
use crate::window::AndroidWindowInner;
#[cfg(target_os = "android")]
use android_activity::input::{
    InputEvent, KeyAction, KeyCharacterMap, KeyMapChar, Keycode, MetaState, MotionAction,
    TextInputAction, TextInputState, TextSpan,
};
#[cfg(target_os = "android")]
use android_activity::{AndroidApp, InputStatus};
use gpui::{
    KeyDownEvent, KeyLocation, Keystroke, LongPressEvent, Modifiers, PinchEvent, Pixels,
    PlatformInput, Point, ScrollDelta, ScrollWheelEvent, TouchEvent, TouchId, TouchPhase,
    UTF16Selection, point, px,
};
#[cfg(target_os = "android")]
use gpui::{
    KeyUpEvent, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
};
use std::collections::HashMap;
use std::ops::Range;
use std::time::Duration;
use std::time::Instant;

/// Distance (logical px) a touch may travel before it stops being a tap and
/// becomes a scroll.
const TOUCH_SLOP: f32 = 8.0;
const LONG_PRESS_DELAY: Duration = Duration::from_millis(500);
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

#[cfg(target_os = "android")]
fn span_range(span: TextSpan, text_length: usize) -> Range<usize> {
    let start = span.start.min(text_length);
    let end = span.end.min(text_length);
    start.min(end)..start.max(end)
}

fn selection_within(selection: &Range<usize>, outer: &Range<usize>) -> Option<Range<usize>> {
    (selection.start >= outer.start && selection.end <= outer.end)
        .then(|| selection.start - outer.start..selection.end - outer.start)
}

fn selection_from_endpoints(start: usize, end: usize, text_length: usize) -> UTF16Selection {
    let start = start.min(text_length);
    let end = end.min(text_length);
    UTF16Selection {
        range: start.min(end)..start.max(end),
        reversed: start > end,
    }
}

fn hardware_key_down(
    keystroke: Keystroke,
    key_location: KeyLocation,
    repeat_count: i32,
) -> KeyDownEvent {
    KeyDownEvent {
        keystroke,
        key_location,
        is_held: repeat_count > 0,
        prefer_character_input: false,
    }
}

#[cfg(target_os = "android")]
fn apply_text_input_state(window: &AndroidWindowInner, text_input_state: &TextInputState) -> bool {
    let handled = window.with_input_handler(|input_handler| {
        let Some(old_text) = AndroidWindowInner::input_handler_text(input_handler) else {
            return false;
        };

        let new_text_length = text_input_state.text.encode_utf16().count();
        let selection = selection_from_endpoints(
            text_input_state.selection.start,
            text_input_state.selection.end,
            new_text_length,
        );
        let compose_region = text_input_state
            .compose_region
            .map(|span| span_range(span, new_text_length))
            .filter(|range| !range.is_empty());

        if let Some(compose_region) = compose_region {
            let compose_text = utf16_slice(&text_input_state.text, compose_region.clone());
            let selected_range = selection_within(&selection.range, &compose_region);
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
                    .then(|| selection_within(&selection.range, &replacement.new_range))
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

#[cfg(target_os = "android")]
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

#[cfg(target_os = "android")]
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

#[cfg(target_os = "android")]
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

#[cfg(target_os = "android")]
pub(crate) fn dispatch_reader_chrome_reveal(window: &AndroidWindowInner) {
    let keystroke = Keystroke {
        modifiers: Modifiers::default(),
        key: "f24".to_owned(),
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
        started_at: Instant,
    },
    Scrolling {
        last: Point<Pixels>,
    },
    LongPressed,
    Captured,
    Pinching {
        pointer_ids: [i32; 2],
        last_distance: f32,
    },
}

#[derive(Default)]
pub(crate) struct TouchState {
    pub(crate) gesture: TouchGesture,
    contacts: HashMap<TouchId, TouchEvent>,
}

impl TouchState {
    /// Consume the remaining contact after a hold so it cannot become a click
    /// or a scroll. Keep ownership until all fingers are lifted or cancelled.
    fn consume_owned_gesture(&mut self, phase: TouchPhase) -> bool {
        if !matches!(
            self.gesture,
            TouchGesture::LongPressed | TouchGesture::Captured
        ) {
            return false;
        }
        if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) && self.contacts.is_empty() {
            self.gesture = TouchGesture::None;
        }
        true
    }

    pub(crate) fn long_press_deadline(&self) -> Option<Instant> {
        let TouchGesture::Pending {
            start, started_at, ..
        } = &self.gesture
        else {
            return None;
        };
        if self.contacts.len() != 1 {
            return None;
        }
        let contact = self.contacts.values().next()?;
        if f32::from(contact.position.x - start.x).hypot(f32::from(contact.position.y - start.y))
            > TOUCH_SLOP
        {
            return None;
        }
        Some(*started_at + LONG_PRESS_DELAY)
    }

    pub(crate) fn take_long_press(&mut self, now: Instant) -> Option<PlatformInput> {
        if now < self.long_press_deadline()? {
            return None;
        }
        let TouchGesture::Pending { start, .. } = &self.gesture else {
            return None;
        };
        let contact = self.contacts.values().next()?;
        let press = LongPressEvent {
            id: contact.id,
            position: *start,
        };
        self.gesture = TouchGesture::LongPressed;
        Some(PlatformInput::LongPress(press))
    }

    fn track(&mut self, event: &TouchEvent) -> bool {
        match event.phase {
            TouchPhase::Started => {
                self.contacts.insert(event.id, event.clone());
                true
            }
            TouchPhase::Moved => {
                if let Some(contact) = self.contacts.get_mut(&event.id) {
                    *contact = event.clone();
                    true
                } else {
                    false
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => self.contacts.remove(&event.id).is_some(),
        }
    }

    pub(crate) fn cancel(&mut self) -> Vec<PlatformInput> {
        let mut events = Vec::new();
        match std::mem::take(&mut self.gesture) {
            TouchGesture::Scrolling { last } => {
                events.push(PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position: last,
                    delta: ScrollDelta::Pixels(Point::default()),
                    modifiers: Modifiers::default(),
                    touch_phase: TouchPhase::Cancelled,
                }))
            }
            TouchGesture::Pinching { pointer_ids, .. } => {
                let positions = pointer_ids
                    .iter()
                    .filter_map(|pointer_id| {
                        self.contacts
                            .get(&TouchId((*pointer_id).max(0) as u64))
                            .map(|contact| contact.position)
                    })
                    .collect::<Vec<_>>();
                let position = match positions.as_slice() {
                    [first, second] => {
                        point((first.x + second.x) / 2.0, (first.y + second.y) / 2.0)
                    }
                    [position] => *position,
                    _ => Point::default(),
                };
                events.push(PlatformInput::Pinch(PinchEvent {
                    position,
                    delta: 0.0,
                    modifiers: Modifiers::default(),
                    phase: TouchPhase::Cancelled,
                }));
            }
            TouchGesture::Pending { .. }
            | TouchGesture::LongPressed
            | TouchGesture::Captured
            | TouchGesture::None => {}
        }
        let mut contacts = self
            .contacts
            .drain()
            .map(|(_, contact)| contact)
            .collect::<Vec<_>>();
        contacts.sort_by_key(|contact| contact.id);
        for mut contact in contacts {
            contact.phase = TouchPhase::Cancelled;
            events.push(PlatformInput::Touch(contact));
        }
        events
    }
}

#[cfg(target_os = "android")]
fn touch_pointer_indices(
    action: MotionAction,
    pointer_index: usize,
    pointer_count: usize,
) -> Range<usize> {
    match action {
        MotionAction::Down
        | MotionAction::PointerDown
        | MotionAction::Up
        | MotionAction::PointerUp
            if pointer_index < pointer_count =>
        {
            pointer_index..pointer_index + 1
        }
        MotionAction::Move | MotionAction::Cancel => 0..pointer_count,
        _ => 0..0,
    }
}

#[cfg(target_os = "android")]
pub(crate) fn handle_input_event(
    event: &InputEvent<'_>,
    window: &AndroidWindowInner,
    touches: &mut TouchState,
    key_maps: &mut HashMap<i32, KeyCharacterMap>,
    app: &AndroidApp,
    finish_activity: &mut bool,
) -> InputStatus {
    match event {
        InputEvent::MotionEvent(motion_event) => {
            if gpui::profiler::input_frame::enabled() {
                let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
                // MotionEvent uses System.nanoTime's monotonic time base.
                if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } == 0 {
                    let now_ns = (now.tv_sec as i64).saturating_mul(1_000_000_000).saturating_add(now.tv_nsec as i64);
                    let age_ns = now_ns.saturating_sub(motion_event.event_time()).max(0) as u64;
                    gpui::profiler::input_frame::platform_event_age(Duration::from_nanos(age_ns));
                }
            }
            let scale = window.state.borrow().scale_factor;
            let action = motion_event.action();
            let pointer_count = motion_event.pointer_count();
            let pointer_index = if matches!(action, MotionAction::Move | MotionAction::Cancel) {
                0
            } else {
                motion_event.pointer_index()
            };
            if pointer_index >= pointer_count {
                return InputStatus::Unhandled;
            }
            let pointer = motion_event.pointer_at_index(pointer_index);
            let position = point(px(pointer.x() / scale), px(pointer.y() / scale));
            window.state.borrow_mut().mouse_position = position;

            let phase = match action {
                MotionAction::Down | MotionAction::PointerDown => TouchPhase::Started,
                MotionAction::Move => TouchPhase::Moved,
                MotionAction::Up | MotionAction::PointerUp => TouchPhase::Ended,
                MotionAction::Cancel => TouchPhase::Cancelled,
                _ => return InputStatus::Unhandled,
            };
            let raw_touches = touch_pointer_indices(action, pointer_index, pointer_count)
                .map(|pointer_index| {
                    let pointer = motion_event.pointer_at_index(pointer_index);
                    TouchEvent {
                        id: TouchId(pointer.pointer_id().max(0) as u64),
                        phase,
                        position: point(px(pointer.x() / scale), px(pointer.y() / scale)),
                        force: None,
                    }
                })
                .filter(|event| touches.track(event))
                .collect::<Vec<_>>();
            let dispatch_touches = || {
                let mut claimed = false;
                for event in &raw_touches {
                    claimed |= window
                        .dispatch_input(PlatformInput::Touch(event.clone()))
                        .is_some_and(|result| result.default_prevented);
                }
                claimed
            };
            if raw_touches.is_empty() {
                return InputStatus::Handled;
            }
            // A recognized hold owns the rest of this contact. Releasing it
            // must not also click, and moving it must not turn into a swipe.
            if touches.consume_owned_gesture(phase) {
                dispatch_touches();
                return InputStatus::Handled;
            }
            let gesture = &mut touches.gesture;
            if !matches!(action, MotionAction::Up | MotionAction::Cancel) && dispatch_touches() {
                // A view (for example a selection handle) owns this contact.
                // Do not synthesize a tap, scroll, or another long press.
                *gesture = TouchGesture::Captured;
                return InputStatus::Handled;
            }

            match motion_event.action() {
                MotionAction::Down => {
                    // A new gesture supersedes any tap handoff that did not find
                    // a matching input handler in the preceding registrations.
                    window.pending_ime_tap.set(None);
                    window.pending_ime_tap_had_handler.set(false);
                    window.pending_ime_previous_bounds.set(None);
                    window.pending_ime_registrations.set(0);
                    *gesture = TouchGesture::Pending {
                        start: position,
                        last: position,
                        started_at: Instant::now(),
                    };
                }
                MotionAction::Move => match gesture {
                    TouchGesture::Pending { start, last, .. } => {
                        let moved = ((f32::from(position.x) - f32::from(start.x)).powi(2)
                            + (f32::from(position.y) - f32::from(start.y)).powi(2))
                        .sqrt();
                        if moved > TOUCH_SLOP {
                            let delta = point(position.x - start.x, position.y - start.y);
                            let anchor = *start;
                            *gesture = TouchGesture::Scrolling { last: position };
                            window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                                position: anchor,
                                delta: ScrollDelta::Pixels(delta),
                                modifiers: Modifiers::default(),
                                touch_phase: TouchPhase::Started,
                            }));
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
                    TouchGesture::Pinching {
                        pointer_ids,
                        last_distance,
                    } => {
                        let mut pointers = pointer_ids.iter().filter_map(|pointer_id| {
                            (0..pointer_count)
                                .map(|index| motion_event.pointer_at_index(index))
                                .find(|pointer| pointer.pointer_id() == *pointer_id)
                        });
                        if let (Some(first), Some(second)) = (pointers.next(), pointers.next()) {
                            let first = point(px(first.x() / scale), px(first.y() / scale));
                            let second = point(px(second.x() / scale), px(second.y() / scale));
                            let center =
                                point((first.x + second.x) / 2.0, (first.y + second.y) / 2.0);
                            let distance =
                                f32::from(second.x - first.x).hypot(f32::from(second.y - first.y));
                            if *last_distance > 0.0 && distance > 0.0 {
                                window.dispatch_input(PlatformInput::Pinch(PinchEvent {
                                    position: center,
                                    delta: distance / *last_distance - 1.0,
                                    modifiers: Modifiers::default(),
                                    phase: TouchPhase::Moved,
                                }));
                            }
                            *last_distance = distance;
                        }
                    }
                    TouchGesture::LongPressed | TouchGesture::Captured | TouchGesture::None => {}
                },
                MotionAction::PointerDown => {
                    if motion_event.pointer_count() >= 2
                        && !matches!(gesture, TouchGesture::Pinching { .. })
                    {
                        if matches!(gesture, TouchGesture::Scrolling { .. }) {
                            window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                                position,
                                delta: ScrollDelta::Pixels(Point::default()),
                                modifiers: Modifiers::default(),
                                touch_phase: TouchPhase::Cancelled,
                            }));
                        }
                        let first = motion_event.pointer_at_index(0);
                        let second = motion_event.pointer_at_index(1);
                        let distance = ((second.x() - first.x()) / scale)
                            .hypot((second.y() - first.y()) / scale);
                        let center = point(
                            px((first.x() + second.x()) / (2.0 * scale)),
                            px((first.y() + second.y()) / (2.0 * scale)),
                        );
                        *gesture = TouchGesture::Pinching {
                            pointer_ids: [first.pointer_id(), second.pointer_id()],
                            last_distance: distance,
                        };
                        window.dispatch_input(PlatformInput::Pinch(PinchEvent {
                            position: center,
                            delta: 0.0,
                            modifiers: Modifiers::default(),
                            phase: TouchPhase::Started,
                        }));
                    }
                }
                MotionAction::PointerUp => {
                    if let TouchGesture::Pinching { pointer_ids, .. } = gesture
                        && pointer_ids.contains(&pointer.pointer_id())
                    {
                        window.dispatch_input(PlatformInput::Pinch(PinchEvent {
                            position,
                            delta: 0.0,
                            modifiers: Modifiers::default(),
                            phase: TouchPhase::Ended,
                        }));
                        let mut remaining = (0..pointer_count)
                            .filter(|index| *index != pointer_index)
                            .map(|index| motion_event.pointer_at_index(index));
                        match (remaining.next(), remaining.next()) {
                            (Some(first), Some(second)) => {
                                let distance = ((second.x() - first.x()) / scale)
                                    .hypot((second.y() - first.y()) / scale);
                                let center = point(
                                    px((first.x() + second.x()) / (2.0 * scale)),
                                    px((first.y() + second.y()) / (2.0 * scale)),
                                );
                                *gesture = TouchGesture::Pinching {
                                    pointer_ids: [first.pointer_id(), second.pointer_id()],
                                    last_distance: distance,
                                };
                                window.dispatch_input(PlatformInput::Pinch(PinchEvent {
                                    position: center,
                                    delta: 0.0,
                                    modifiers: Modifiers::default(),
                                    phase: TouchPhase::Started,
                                }));
                            }
                            (Some(remaining), None) => {
                                let position =
                                    point(px(remaining.x() / scale), px(remaining.y() / scale));
                                *gesture = TouchGesture::Scrolling { last: position };
                                window.dispatch_input(PlatformInput::ScrollWheel(
                                    ScrollWheelEvent {
                                        position,
                                        delta: ScrollDelta::Pixels(Point::default()),
                                        modifiers: Modifiers::default(),
                                        touch_phase: TouchPhase::Started,
                                    },
                                ));
                            }
                            _ => *gesture = TouchGesture::None,
                        }
                    }
                }
                MotionAction::Up => {
                    match std::mem::take(gesture) {
                        TouchGesture::Pending {
                            start, started_at, ..
                        } => {
                            // Long presses are emitted by the polling timer,
                            // not synthesized as a mouse click on release.
                            if started_at.elapsed() >= LONG_PRESS_DELAY {
                                dispatch_touches();
                                return InputStatus::Handled;
                            }
                            let button = MouseButton::Left;
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
                                button,
                                position: start,
                                modifiers: Modifiers::default(),
                                click_count,
                                first_mouse: false,
                            }));
                            window.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
                                button,
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
                        TouchGesture::Scrolling { last } => {
                            window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                                position,
                                delta: ScrollDelta::Pixels(position - last),
                                modifiers: Modifiers::default(),
                                touch_phase: TouchPhase::Ended,
                            }));
                        }
                        TouchGesture::Pinching { .. } => {
                            window.dispatch_input(PlatformInput::Pinch(PinchEvent {
                                position,
                                delta: 0.0,
                                modifiers: Modifiers::default(),
                                phase: TouchPhase::Ended,
                            }));
                        }
                        TouchGesture::LongPressed | TouchGesture::Captured | TouchGesture::None => {
                        }
                    }
                }
                MotionAction::Cancel => {
                    if matches!(gesture, TouchGesture::Scrolling { .. }) {
                        window.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position,
                            delta: ScrollDelta::Pixels(Point::default()),
                            modifiers: Modifiers::default(),
                            touch_phase: TouchPhase::Cancelled,
                        }));
                    }
                    if matches!(gesture, TouchGesture::Pinching { .. }) {
                        window.dispatch_input(PlatformInput::Pinch(PinchEvent {
                            position,
                            delta: 0.0,
                            modifiers: Modifiers::default(),
                            phase: TouchPhase::Cancelled,
                        }));
                    }
                    *gesture = TouchGesture::None;
                }
                _ => return InputStatus::Unhandled,
            }
            if matches!(action, MotionAction::Up | MotionAction::Cancel) {
                dispatch_touches();
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
                    let result = window.dispatch_input(PlatformInput::KeyDown(hardware_key_down(
                        keystroke,
                        key_location,
                        key_event.repeat_count(),
                    )));

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
                window.accept_text_input_state(text_input_state);
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
    use gpui::{
        Bounds, Context, ElementInputHandler, EntityInputHandler, IntoElement,
        PlatformInputHandler, Render, TestAppContext, Window, div,
    };

    fn contact(id: u64, phase: TouchPhase, position: Point<Pixels>) -> TouchEvent {
        TouchEvent {
            id: TouchId(id),
            phase,
            position,
            force: None,
        }
    }

    #[test]
    fn stationary_hold_emits_one_explicit_long_press_before_release() {
        let now = Instant::now();
        let position = point(px(30.0), px(40.0));
        let mut touches = TouchState::default();
        touches.track(&contact(7, TouchPhase::Started, position));
        touches.gesture = TouchGesture::Pending {
            start: position,
            last: position,
            started_at: now,
        };
        assert_eq!(touches.long_press_deadline(), Some(now + LONG_PRESS_DELAY));
        assert!(
            touches
                .take_long_press(now + LONG_PRESS_DELAY - Duration::from_millis(1))
                .is_none()
        );
        assert!(
            matches!(touches.take_long_press(now + LONG_PRESS_DELAY), Some(PlatformInput::LongPress(LongPressEvent { id: TouchId(7), position: held })) if held == position)
        );
        assert!(matches!(touches.gesture, TouchGesture::LongPressed));
        assert_eq!(touches.long_press_deadline(), None);
        assert!(
            touches
                .take_long_press(now + LONG_PRESS_DELAY * 2)
                .is_none()
        );
        let cancelled = touches.cancel();
        assert!(
            matches!(cancelled.as_slice(), [PlatformInput::Touch(event)] if event.phase == TouchPhase::Cancelled)
        );
        assert!(
            touches
                .take_long_press(now + LONG_PRESS_DELAY * 3)
                .is_none()
        );
    }

    #[test]
    fn long_press_is_cancelled_by_movement_multiple_contacts_and_release() {
        let now = Instant::now();
        let position = point(px(30.0), px(40.0));
        let mut touches = TouchState::default();
        touches.track(&contact(7, TouchPhase::Started, position));
        touches.gesture = TouchGesture::Pending {
            start: position,
            last: position,
            started_at: now,
        };
        touches.track(&contact(7, TouchPhase::Moved, point(px(50.0), px(40.0))));
        assert_eq!(touches.long_press_deadline(), None);
        assert!(touches.take_long_press(now + LONG_PRESS_DELAY).is_none());
        touches.track(&contact(7, TouchPhase::Moved, position));
        touches.track(&contact(8, TouchPhase::Started, position));
        assert_eq!(touches.long_press_deadline(), None);
        assert!(touches.take_long_press(now + LONG_PRESS_DELAY).is_none());
        touches.track(&contact(8, TouchPhase::Ended, position));
        touches.track(&contact(7, TouchPhase::Ended, position));
        assert!(touches.take_long_press(now + LONG_PRESS_DELAY).is_none());
    }

    #[test]
    fn long_press_consumes_movement_and_release_until_all_contacts_end() {
        let position = point(px(30.0), px(40.0));
        let mut touches = TouchState::default();
        touches.track(&contact(7, TouchPhase::Started, position));
        touches.gesture = TouchGesture::LongPressed;
        touches.track(&contact(7, TouchPhase::Moved, point(px(90.0), px(40.0))));
        assert!(touches.consume_owned_gesture(TouchPhase::Moved));
        assert!(matches!(touches.gesture, TouchGesture::LongPressed));
        touches.track(&contact(8, TouchPhase::Started, position));
        assert!(touches.consume_owned_gesture(TouchPhase::Started));
        touches.track(&contact(7, TouchPhase::Ended, position));
        assert!(touches.consume_owned_gesture(TouchPhase::Ended));
        assert!(matches!(touches.gesture, TouchGesture::LongPressed));
        touches.track(&contact(8, TouchPhase::Ended, position));
        assert!(touches.consume_owned_gesture(TouchPhase::Ended));
        assert!(matches!(touches.gesture, TouchGesture::None));
        assert!(!touches.consume_owned_gesture(TouchPhase::Ended));
    }

    #[test]
    fn captured_handle_drag_suppresses_platform_gestures_and_finishes_on_release() {
        let now = Instant::now();
        let position = point(px(30.0), px(40.0));
        let mut touches = TouchState::default();
        touches.track(&contact(7, TouchPhase::Started, position));
        touches.gesture = TouchGesture::Captured;
        assert!(
            touches
                .take_long_press(now + LONG_PRESS_DELAY * 2)
                .is_none()
        );
        touches.track(&contact(7, TouchPhase::Moved, point(px(300.0), px(400.0))));
        assert!(touches.consume_owned_gesture(TouchPhase::Moved));
        touches.track(&contact(7, TouchPhase::Ended, position));
        assert!(touches.consume_owned_gesture(TouchPhase::Ended));
        assert!(matches!(touches.gesture, TouchGesture::None));
        assert!(touches.cancel().is_empty());
    }

    #[test]
    fn interrupted_tap_cancels_contacts_without_clicking_and_ignores_late_release() {
        let mut touches = TouchState::default();
        let position = point(px(10.0), px(20.0));
        assert!(touches.track(&contact(7, TouchPhase::Started, position)));
        touches.gesture = TouchGesture::Pending {
            start: position,
            last: position,
            started_at: Instant::now(),
        };
        let events = touches.cancel();
        assert!(matches!(touches.gesture, TouchGesture::None));
        assert!(matches!(events.as_slice(), [PlatformInput::Touch(event)]
            if event.id == TouchId(7)
                && event.phase == TouchPhase::Cancelled
                && event.position == position));
        assert!(touches.cancel().is_empty());
        assert!(!touches.track(&contact(7, TouchPhase::Ended, position)));
        assert!(!touches.track(&contact(7, TouchPhase::Moved, position)));
        assert!(touches.track(&contact(7, TouchPhase::Started, position)));
        assert!(touches.track(&contact(7, TouchPhase::Ended, position)));
        assert!(touches.cancel().is_empty());
    }

    #[test]
    fn interrupted_scroll_sends_a_terminal_scroll_and_touch_at_the_last_position() {
        let mut touches = TouchState::default();
        let start = point(px(10.0), px(20.0));
        let last = point(px(10.0), px(80.0));
        assert!(touches.track(&contact(1, TouchPhase::Started, start)));
        assert!(touches.track(&contact(1, TouchPhase::Moved, last)));
        touches.gesture = TouchGesture::Scrolling { last };
        let events = touches.cancel();
        assert!(
            matches!(events.as_slice(), [PlatformInput::ScrollWheel(scroll), PlatformInput::Touch(touch)]
            if scroll.touch_phase == TouchPhase::Cancelled
                && scroll.position == last
                && touch.phase == TouchPhase::Cancelled
                && touch.position == last)
        );
        assert!(touches.cancel().is_empty());
    }

    #[test]
    fn interrupted_pinch_cancels_all_contacts_including_an_extra_finger() {
        let mut touches = TouchState::default();
        for (id, horizontal) in [(3, 30.0), (2, 20.0), (1, 10.0)] {
            assert!(touches.track(&contact(
                id,
                TouchPhase::Started,
                point(px(horizontal), px(40.0)),
            )));
        }
        touches.gesture = TouchGesture::Pinching {
            pointer_ids: [1, 3],
            last_distance: 20.0,
        };
        let events = touches.cancel();
        assert!(matches!(events.first(), Some(PlatformInput::Pinch(event))
            if event.phase == TouchPhase::Cancelled
                && event.delta == 0.0
                && event.position == point(px(20.0), px(40.0))));
        let cancelled = events
            .iter()
            .skip(1)
            .map(|event| {
                let PlatformInput::Touch(touch) = event else {
                    panic!("each contact must receive a terminal touch event");
                };
                assert_eq!(touch.phase, TouchPhase::Cancelled);
                touch.id
            })
            .collect::<Vec<_>>();
        assert_eq!(cancelled, vec![TouchId(1), TouchId(2), TouchId(3)]);
        assert!(touches.cancel().is_empty());
    }

    #[test]
    fn hardware_key_repeats_are_held_without_changing_the_keystroke() {
        for repeat_count in [0, 1, 20] {
            let keystroke = Keystroke {
                modifiers: Modifiers::shift(),
                key: "left".to_owned(),
                key_char: None,
            };
            let event = hardware_key_down(keystroke.clone(), KeyLocation::Standard, repeat_count);
            assert_eq!(event.keystroke, keystroke);
            assert_eq!(event.is_held, repeat_count > 0);
        }
    }

    struct SelectionField {
        selection: UTF16Selection,
    }

    impl Render for SelectionField {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    impl EntityInputHandler for SelectionField {
        fn text_for_range(
            &mut self,
            range: Range<usize>,
            adjusted_range: &mut Option<Range<usize>>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<String> {
            *adjusted_range = Some(range.clone());
            Some(utf16_slice("a😀ö", range))
        }

        fn selected_text_range(
            &mut self,
            _ignore_disabled_input: bool,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<UTF16Selection> {
            Some(UTF16Selection {
                range: self.selection.range.clone(),
                reversed: self.selection.reversed,
            })
        }

        fn marked_text_range(
            &self,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<Range<usize>> {
            None
        }

        fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

        fn replace_text_in_range(
            &mut self,
            _range: Option<Range<usize>>,
            _text: &str,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
        }

        fn replace_and_mark_text_in_range(
            &mut self,
            _range: Option<Range<usize>>,
            _new_text: &str,
            _new_selected_range: Option<Range<usize>>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
        }

        fn bounds_for_range(
            &mut self,
            _range_utf16: Range<usize>,
            _element_bounds: Bounds<Pixels>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<Bounds<Pixels>> {
            None
        }

        fn character_index_for_point(
            &mut self,
            _point: Point<Pixels>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) -> Option<usize> {
            None
        }

        fn set_selected_text_range(
            &mut self,
            selection_utf16: UTF16Selection,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
            self.selection = selection_utf16;
        }
    }

    #[gpui::test]
    fn selection_direction_round_trips_through_platform_and_entity_handlers(
        cx: &mut TestAppContext,
    ) {
        let window = cx.add_window(|_, _| SelectionField {
            selection: selection_from_endpoints(0, 0, 4),
        });
        let mut handler = cx.update(|cx| {
            window
                .update(cx, |_, window, cx| {
                    PlatformInputHandler::new(
                        window.to_async(cx),
                        Box::new(ElementInputHandler::new(Bounds::default(), cx.entity())),
                    )
                })
                .expect("test window exists")
        });
        for (start, end, expected_range, reversed) in [
            (4, 1, 1..4, true),
            (1, 3, 1..3, false),
            (3, 3, 3..3, false),
            (100, 1, 1..4, true),
            (100, 200, 4..4, false),
        ] {
            handler.set_selected_text_range(selection_from_endpoints(start, end, 4));
            let selection = handler
                .selected_text_range(true)
                .expect("selection was stored by the entity");
            assert_eq!(selection.range, expected_range);
            assert_eq!(selection.reversed, reversed);
        }
    }

    #[cfg(target_os = "android")]
    #[test]
    fn raw_touch_events_cover_each_pointer_lifecycle() {
        for action in [
            MotionAction::Down,
            MotionAction::PointerDown,
            MotionAction::Up,
            MotionAction::PointerUp,
        ] {
            assert_eq!(
                touch_pointer_indices(action, 1, 3).collect::<Vec<_>>(),
                vec![1]
            );
        }
        for action in [MotionAction::Move, MotionAction::Cancel] {
            assert_eq!(
                touch_pointer_indices(action, 0, 3).collect::<Vec<_>>(),
                vec![0, 1, 2]
            );
        }
        assert!(touch_pointer_indices(MotionAction::PointerDown, 3, 3).is_empty());
    }

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
                old_range: 5..9,
                new_range: 5..5,
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

#[cfg(target_os = "android")]
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

#[cfg(target_os = "android")]
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

#[cfg(target_os = "android")]
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
#[cfg(target_os = "android")]
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
