//! Frame-based Kobo touch decoding. Type-A Kobo controllers report contacts in
//! each frame; treating tracking IDs as a Linux type-B lifetime leaves touches
//! stuck down on those devices. Keep only one primary contact for GPUI's mouse
//! interface and cancel multi-finger input rather than clicking a second finger.
use std::collections::{BTreeMap, VecDeque};
use crate::{TouchEvent, TouchPhase};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum KoboTouchProtocol {
    #[default]
    Auto,
    Single,
    MultitouchA,
    MultitouchB,
    MultitouchC,
}

#[derive(Clone, Copy, Debug, Default)]
struct Contact { x: i32, y: i32, pressure: i32 }

pub(crate) struct TouchDecoder {
    protocol: KoboTouchProtocol,
    current_id: i32,
    packets: BTreeMap<i32, Contact>,
    previous: BTreeMap<i32, Contact>,
    primary: Option<i32>,
    blocked: bool,
    output: VecDeque<TouchEvent>,
}

impl TouchDecoder {
    pub fn new(protocol: KoboTouchProtocol) -> Self {
        let mut decoder = Self { protocol, current_id: 0, packets: BTreeMap::new(), previous: BTreeMap::new(), primary: None, blocked: false, output: VecDeque::new() };
        if protocol == KoboTouchProtocol::Single { decoder.packets.insert(0, Contact::default()); }
        decoder
    }

    pub fn pop(&mut self) -> Option<TouchEvent> { self.output.pop_front() }

    pub fn reset(&mut self) { *self = Self::new(self.protocol); }

    pub fn feed(&mut self, kind: u16, code: u16, value: i32) {
        if kind == 0 && code == 3 {
            if let Some(contact) = self.primary.and_then(|id| self.previous.get(&id)).copied() {
                self.output.push_back(event(TouchPhase::Cancel, contact));
            }
            let output = std::mem::take(&mut self.output);
            self.reset();
            self.output = output;
            self.blocked = true;
            return;
        }
        if kind == 3 {
            if code == 0x39 && self.protocol != KoboTouchProtocol::Single {
                if value >= 0 {
                    self.current_id = value;
                    let old = self.previous.get(&value).copied().unwrap_or_default();
                    self.packets.entry(value).or_insert(Contact { pressure: 0, ..old });
                } else if let Some(contact) = self.packets.get_mut(&self.current_id) {
                    contact.pressure = 0;
                }
            } else if let Some(contact) = self.packets.get_mut(&self.current_id) {
                let single = self.protocol == KoboTouchProtocol::Single;
                let pressure = match self.protocol { KoboTouchProtocol::Single => 0x18, KoboTouchProtocol::MultitouchA => 0x30, _ => 0x3a };
                if code == if single { 0 } else { 0x35 } { contact.x = value; }
                else if code == if single { 1 } else { 0x36 } { contact.y = value; }
                else if code == pressure { contact.pressure = value; }
            }
        } else if kind == 0 && code == 0 {
            self.finish_frame();
        }
    }

    fn finish_frame(&mut self) {
        let contacts: BTreeMap<_, _> = self.packets.iter().filter(|(_, contact)| contact.pressure > 0).map(|(id, contact)| (*id, *contact)).collect();
        if contacts.len() > 1 {
            if let Some(contact) = self.primary.and_then(|id| self.previous.get(&id)).copied() {
                self.output.push_back(event(TouchPhase::Cancel, contact));
            }
            self.primary = None;
            self.blocked = true;
        } else if !self.blocked {
            if let Some(id) = self.primary {
                if let Some(contact) = contacts.get(&id) {
                    if self.previous.get(&id).is_some_and(|old| old.x != contact.x || old.y != contact.y) {
                        self.output.push_back(event(TouchPhase::Move, *contact));
                    }
                } else if let Some(old) = self.previous.get(&id).copied() {
                    let phase = if contacts.is_empty() { TouchPhase::Up } else { TouchPhase::Cancel };
                    let position = if self.protocol == KoboTouchProtocol::Single { old } else { self.packets.get(&id).copied().unwrap_or(old) };
                    self.output.push_back(event(phase, position));
                    self.primary = None;
                    self.blocked = !contacts.is_empty();
                }
            } else if let Some((id, contact)) = contacts.first_key_value() {
                self.primary = Some(*id);
                self.output.push_back(event(TouchPhase::Down, *contact));
            }
        }
        if contacts.is_empty() { self.blocked = false; }
        self.previous = contacts;
        if self.protocol != KoboTouchProtocol::Single { self.packets.clear(); }
    }
}

fn event(phase: TouchPhase, contact: Contact) -> TouchEvent { TouchEvent { phase, raw_x: contact.x, raw_y: contact.y } }

#[cfg(test)]
mod tests {
    use super::*;
    fn contact(decoder: &mut TouchDecoder, id: i32, x: i32, pressure: i32) {
        decoder.feed(3, 0x39, id);
        decoder.feed(3, 0x35, x);
        decoder.feed(3, 0x36, 200);
        decoder.feed(3, if decoder.protocol == KoboTouchProtocol::MultitouchA { 0x30 } else { 0x3a }, pressure);
    }
    fn frame(decoder: &mut TouchDecoder) { decoder.feed(0, 0, 0); }

    #[test]
    fn pressure_protocols_release_despite_positive_tracking_ids() {
        for protocol in [KoboTouchProtocol::MultitouchA, KoboTouchProtocol::MultitouchB, KoboTouchProtocol::MultitouchC] {
            let mut decoder = TouchDecoder::new(protocol);
            contact(&mut decoder, 7, 100, 50); frame(&mut decoder);
            assert_eq!(decoder.pop().unwrap().phase, TouchPhase::Down);
            contact(&mut decoder, 7, 120, 50); frame(&mut decoder);
            assert_eq!(decoder.pop().unwrap().phase, TouchPhase::Move);
            if protocol != KoboTouchProtocol::MultitouchB { contact(&mut decoder, 7, 120, 0); }
            frame(&mut decoder);
            assert_eq!(decoder.pop().unwrap().phase, TouchPhase::Up);
            assert!(decoder.pop().is_none());
        }
    }

    #[test]
    fn single_touch_and_multiple_fingers_do_not_produce_phantom_clicks() {
        let mut decoder = TouchDecoder::new(KoboTouchProtocol::Single);
        decoder.feed(3, 0, 100); decoder.feed(3, 1, 200); decoder.feed(3, 0x18, 20); frame(&mut decoder);
        assert_eq!(decoder.pop().unwrap().phase, TouchPhase::Down);
        decoder.feed(3, 0x18, 0); frame(&mut decoder);
        assert_eq!(decoder.pop().unwrap().phase, TouchPhase::Up);
        let mut decoder = TouchDecoder::new(KoboTouchProtocol::MultitouchB);
        contact(&mut decoder, 7, 100, 50); frame(&mut decoder); decoder.pop();
        contact(&mut decoder, 7, 100, 50); contact(&mut decoder, 8, 300, 50); frame(&mut decoder);
        assert_eq!(decoder.pop().unwrap().phase, TouchPhase::Cancel);
        contact(&mut decoder, 8, 300, 50); frame(&mut decoder);
        assert!(decoder.pop().is_none());
        frame(&mut decoder);
        contact(&mut decoder, 9, 200, 50); frame(&mut decoder);
        assert_eq!(decoder.pop().unwrap().phase, TouchPhase::Down);
    }
}
