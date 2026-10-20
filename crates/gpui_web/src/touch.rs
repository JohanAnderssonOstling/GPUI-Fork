//! Browser gesture recognition. Raw contacts are dispatched before synthesis so
//! selection handles can claim a gesture without also scrolling or clicking.

use gpui::{
    LongPressEvent, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PinchEvent, Pixels, PlatformInput, Point, ScrollDelta, ScrollWheelEvent, TouchEvent, TouchId,
    TouchPhase, point,
};
use std::collections::BTreeMap;

pub(crate) const LONG_PRESS_MS: i32 = 500;
const TOUCH_SLOP: f32 = 8.0;

#[derive(Default)]
enum Gesture {
    #[default]
    None,
    Pending {
        id: TouchId,
        start: Point<Pixels>,
        started_at: f64,
    },
    Scrolling {
        last: Point<Pixels>,
    },
    Pinching {
        ids: [TouchId; 2],
        distance: f32,
        center: Point<Pixels>,
    },
    Owned,
}

#[derive(Default)]
pub(crate) struct TouchState {
    contacts: BTreeMap<TouchId, Point<Pixels>>,
    gesture: Gesture,
}

fn distance(a: Point<Pixels>, b: Point<Pixels>) -> f32 {
    f32::from(a.x - b.x).hypot(f32::from(a.y - b.y))
}
fn scroll(position: Point<Pixels>, delta: Point<Pixels>, phase: TouchPhase) -> PlatformInput {
    PlatformInput::ScrollWheel(ScrollWheelEvent {
        position,
        delta: ScrollDelta::Pixels(delta),
        modifiers: Modifiers::default(),
        touch_phase: phase,
    })
}
fn pinch(position: Point<Pixels>, delta: f32, phase: TouchPhase) -> PlatformInput {
    PlatformInput::Pinch(PinchEvent {
        position,
        delta,
        modifiers: Modifiers::default(),
        phase,
    })
}

impl TouchState {
    pub(crate) fn contains(&self, id: TouchId) -> bool {
        self.contacts.contains_key(&id)
    }

    pub(crate) fn process(
        &mut self,
        event: &TouchEvent,
        claimed: bool,
        now: f64,
        click_count: usize,
    ) -> Vec<PlatformInput> {
        let mut output = Vec::new();
        if event.phase != TouchPhase::Started && !self.contains(event.id) {
            return output;
        }
        match event.phase {
            TouchPhase::Started | TouchPhase::Moved => {
                self.contacts.insert(event.id, event.position);
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                self.contacts.remove(&event.id);
            }
        }
        if claimed {
            self.end_gesture(TouchPhase::Cancelled, &mut output);
            self.gesture = Gesture::Owned;
        }
        if matches!(self.gesture, Gesture::Owned) {
            if self.contacts.is_empty() {
                self.gesture = Gesture::None;
            }
            return output;
        }
        if event.phase == TouchPhase::Cancelled {
            self.end_gesture(TouchPhase::Cancelled, &mut output);
            // Remaining fingers cannot become taps after cancellation.
            self.gesture = if self.contacts.is_empty() {
                Gesture::None
            } else {
                Gesture::Owned
            };
            return output;
        }
        match event.phase {
            TouchPhase::Started if self.contacts.len() == 1 => {
                self.gesture = Gesture::Pending {
                    id: event.id,
                    start: event.position,
                    started_at: now,
                };
            }
            TouchPhase::Started if self.contacts.len() == 2 => {
                self.end_gesture(TouchPhase::Cancelled, &mut output);
                let mut contacts = self.contacts.iter();
                let (&a_id, &a) = contacts.next().unwrap();
                let (&b_id, &b) = contacts.next().unwrap();
                let center = point((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
                self.gesture = Gesture::Pinching {
                    ids: [a_id, b_id],
                    distance: distance(a, b),
                    center,
                };
                output.push(pinch(center, 0.0, TouchPhase::Started));
            }
            TouchPhase::Moved => match &mut self.gesture {
                Gesture::Pending { start, .. } if distance(event.position, *start) > TOUCH_SLOP => {
                    output.push(scroll(*start, event.position - *start, TouchPhase::Started));
                    self.gesture = Gesture::Scrolling {
                        last: event.position,
                    };
                }
                Gesture::Scrolling { last } => {
                    output.push(scroll(
                        event.position,
                        event.position - *last,
                        TouchPhase::Moved,
                    ));
                    *last = event.position;
                }
                Gesture::Pinching {
                    ids,
                    distance: previous,
                    center,
                } => {
                    if let (Some(a), Some(b)) =
                        (self.contacts.get(&ids[0]), self.contacts.get(&ids[1]))
                    {
                        *center = point((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
                        let current = distance(*a, *b);
                        if *previous > 0.0 && current > 0.0 {
                            output.push(pinch(
                                *center,
                                current / *previous - 1.0,
                                TouchPhase::Moved,
                            ));
                        }
                        *previous = current;
                    }
                }
                _ => {}
            },
            TouchPhase::Ended => {
                // An unrelated third finger lifting must not terminate a pinch.
                if matches!(&self.gesture, Gesture::Pinching { ids, .. } if !ids.contains(&event.id))
                {
                    return output;
                }
                match std::mem::take(&mut self.gesture) {
                    Gesture::Pending {
                        start, started_at, ..
                    } if now - started_at < f64::from(LONG_PRESS_MS)
                        && distance(event.position, start) <= TOUCH_SLOP =>
                    {
                        let modifiers = Modifiers::default();
                        output.push(PlatformInput::MouseMove(MouseMoveEvent {
                            position: start,
                            pressed_button: None,
                            modifiers,
                        }));
                        output.push(PlatformInput::MouseDown(MouseDownEvent {
                            button: MouseButton::Left,
                            position: start,
                            modifiers,
                            click_count,
                            first_mouse: false,
                        }));
                        output.push(PlatformInput::MouseUp(MouseUpEvent {
                            button: MouseButton::Left,
                            position: start,
                            modifiers,
                            click_count,
                        }));
                    }
                    Gesture::Scrolling { last } => output.push(scroll(
                        event.position,
                        event.position - last,
                        TouchPhase::Ended,
                    )),
                    Gesture::Pinching { center, .. } => {
                        output.push(pinch(center, 0.0, TouchPhase::Ended));
                        // Consume the remaining contacts; lifting after zoom is not a click.
                        if !self.contacts.is_empty() {
                            self.gesture = Gesture::Owned;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        output
    }

    pub(crate) fn long_press(&mut self, now: f64) -> Option<PlatformInput> {
        let Gesture::Pending {
            id,
            start,
            started_at,
        } = self.gesture
        else {
            return None;
        };
        if self.contacts.len() != 1 || now - started_at < f64::from(LONG_PRESS_MS) {
            return None;
        }
        self.gesture = Gesture::Owned;
        Some(PlatformInput::LongPress(LongPressEvent {
            id,
            position: start,
        }))
    }

    fn end_gesture(&mut self, phase: TouchPhase, output: &mut Vec<PlatformInput>) {
        match std::mem::take(&mut self.gesture) {
            Gesture::Scrolling { last } => output.push(scroll(last, Point::default(), phase)),
            Gesture::Pinching { center, .. } => output.push(pinch(center, 0.0, phase)),
            _ => {}
        }
    }

    pub(crate) fn cancel(&mut self) -> Vec<PlatformInput> {
        let mut output = Vec::new();
        self.end_gesture(TouchPhase::Cancelled, &mut output);
        for (id, position) in std::mem::take(&mut self.contacts) {
            output.push(PlatformInput::Touch(TouchEvent {
                id,
                position,
                phase: TouchPhase::Cancelled,
                force: None,
            }));
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::px;
    fn contact(id: u64, phase: TouchPhase, x: f32, y: f32) -> TouchEvent {
        TouchEvent {
            id: TouchId(id),
            phase,
            position: point(px(x), px(y)),
            force: None,
        }
    }
    #[test]
    fn tap_clicks_once_but_drag_and_cancel_never_click() {
        let mut state = TouchState::default();
        state.process(&contact(1, TouchPhase::Started, 10.0, 20.0), false, 0.0, 1);
        let tap = state.process(&contact(1, TouchPhase::Ended, 11.0, 20.0), false, 50.0, 1);
        assert!(matches!(
            tap.as_slice(),
            [
                PlatformInput::MouseMove(_),
                PlatformInput::MouseDown(_),
                PlatformInput::MouseUp(_)
            ]
        ));
        state.process(
            &contact(2, TouchPhase::Started, 10.0, 20.0),
            false,
            100.0,
            1,
        );
        let moved = state.process(&contact(2, TouchPhase::Moved, 10.0, 60.0), false, 120.0, 1);
        assert!(
            matches!(moved.as_slice(), [PlatformInput::ScrollWheel(e)] if e.touch_phase == TouchPhase::Started && matches!(e.delta, ScrollDelta::Pixels(delta) if delta == point(px(0.0), px(40.0))))
        );
        let cancelled = state.cancel();
        assert!(
            matches!(cancelled.as_slice(), [PlatformInput::ScrollWheel(e), PlatformInput::Touch(t)] if e.touch_phase == TouchPhase::Cancelled && t.phase == TouchPhase::Cancelled)
        );
        assert!(
            state
                .process(&contact(2, TouchPhase::Ended, 10.0, 60.0), false, 140.0, 1)
                .is_empty()
        );
    }
    #[test]
    fn claimed_contacts_and_holds_do_not_scroll_or_click_on_release() {
        for claimed in [false, true] {
            let mut state = TouchState::default();
            state.process(
                &contact(1, TouchPhase::Started, 10.0, 20.0),
                claimed,
                0.0,
                1,
            );
            if !claimed {
                assert!(state.long_press(499.0).is_none());
                assert!(matches!(
                    state.long_press(500.0),
                    Some(PlatformInput::LongPress(_))
                ));
                assert!(state.long_press(600.0).is_none());
            }
            assert!(
                state
                    .process(&contact(1, TouchPhase::Moved, 10.0, 80.0), false, 650.0, 1)
                    .is_empty()
            );
            assert!(
                state
                    .process(&contact(1, TouchPhase::Ended, 10.0, 80.0), false, 700.0, 1)
                    .is_empty()
            );
        }
    }
    #[test]
    fn pinch_cancels_scroll_and_consumes_the_remaining_finger() {
        let mut state = TouchState::default();
        state.process(&contact(1, TouchPhase::Started, 0.0, 0.0), false, 0.0, 1);
        state.process(&contact(1, TouchPhase::Moved, 0.0, 20.0), false, 20.0, 1);
        let begin = state.process(&contact(2, TouchPhase::Started, 0.0, 120.0), false, 30.0, 1);
        assert!(
            matches!(begin.as_slice(), [PlatformInput::ScrollWheel(e), PlatformInput::Pinch(p)] if e.touch_phase == TouchPhase::Cancelled && p.phase == TouchPhase::Started)
        );
        let moved = state.process(&contact(2, TouchPhase::Moved, 0.0, 220.0), false, 40.0, 1);
        assert!(matches!(moved.as_slice(), [PlatformInput::Pinch(p)] if p.delta == 1.0));
        let end = state.process(&contact(2, TouchPhase::Ended, 0.0, 220.0), false, 50.0, 1);
        assert!(
            matches!(end.as_slice(), [PlatformInput::Pinch(p)] if p.phase == TouchPhase::Ended)
        );
        assert!(
            state
                .process(&contact(1, TouchPhase::Ended, 0.0, 20.0), false, 60.0, 1)
                .is_empty()
        );
    }
    #[test]
    fn claiming_a_scroll_cancels_it_and_suppresses_the_release() {
        let mut state = TouchState::default();
        state.process(&contact(1, TouchPhase::Started, 0.0, 0.0), false, 0.0, 1);
        state.process(&contact(1, TouchPhase::Moved, 0.0, 30.0), false, 10.0, 1);
        assert!(state.long_press(600.0).is_none());
        let claimed = state.process(&contact(1, TouchPhase::Moved, 0.0, 40.0), true, 610.0, 1);
        assert!(
            matches!(claimed.as_slice(), [PlatformInput::ScrollWheel(e)] if e.touch_phase == TouchPhase::Cancelled)
        );
        assert!(
            state
                .process(&contact(1, TouchPhase::Ended, 0.0, 40.0), false, 620.0, 1)
                .is_empty()
        );
        state.process(
            &contact(2, TouchPhase::Started, 10.0, 20.0),
            false,
            700.0,
            1,
        );
        assert!(matches!(
            state
                .process(&contact(2, TouchPhase::Ended, 10.0, 20.0), false, 750.0, 1)
                .last(),
            Some(PlatformInput::MouseUp(_))
        ));
    }

    #[test]
    fn a_release_outside_tap_slop_and_a_late_hold_do_not_click() {
        let mut state = TouchState::default();
        state.process(&contact(1, TouchPhase::Started, 0.0, 0.0), false, 0.0, 1);
        assert!(
            state
                .process(&contact(1, TouchPhase::Ended, 0.0, 100.0), false, 10.0, 1)
                .is_empty()
        );
        state.process(&contact(2, TouchPhase::Started, 0.0, 0.0), false, 20.0, 1);
        assert!(
            state
                .process(&contact(2, TouchPhase::Ended, 0.0, 0.0), false, 620.0, 1)
                .is_empty()
        );
    }

    #[test]
    fn lifting_an_unrelated_third_finger_preserves_the_pinch() {
        let mut state = TouchState::default();
        state.process(&contact(1, TouchPhase::Started, 0.0, 0.0), false, 0.0, 1);
        state.process(&contact(2, TouchPhase::Started, 0.0, 100.0), false, 10.0, 1);
        state.process(&contact(3, TouchPhase::Started, 100.0, 0.0), false, 20.0, 1);
        assert!(
            state
                .process(&contact(3, TouchPhase::Ended, 100.0, 0.0), false, 30.0, 1)
                .is_empty()
        );
        assert!(
            matches!(state.process(&contact(2, TouchPhase::Moved, 0.0, 200.0), false, 40.0, 1).as_slice(), [PlatformInput::Pinch(p)] if p.delta == 1.0)
        );
    }
}
