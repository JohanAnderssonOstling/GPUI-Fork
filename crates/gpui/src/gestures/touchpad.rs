//! Discrete page navigation from smooth touchpad scroll events.

use crate::{Point, ScrollDelta, ScrollWheelEvent, TouchPhase, point};
use std::time::Duration;
use web_time::Instant;

/// Default minimum interval between discrete page turns from a physical wheel.
pub const PAGE_WHEEL_REPEAT_INTERVAL: Duration = Duration::from_millis(200);

/// Physical wheel paging consumes fresh input only, never queues it behind an
/// animation. Closely spaced notches therefore cannot schedule a second turn.
pub fn page_wheel_ready(last_turn: Option<Instant>, animating: bool, now: Instant) -> bool {
    !animating
        && last_turn.is_none_or(|last| now.duration_since(last) >= PAGE_WHEEL_REPEAT_INTERVAL)
}

const IDLE_TIMEOUT: Duration = Duration::from_millis(200);
const TURN_DISTANCE: f32 = 48.0;

/// By default, emits at most one turn per scroll burst, including its momentum tail.
/// Linux and browser backends can send only `Moved`, so an idle gap also
/// starts a new gesture. Phased gestures commit on release, allowing
/// cancellation to discard their travel. Repeating navigation commits during
/// movement instead. Touchscreens use their own handlers.
#[derive(Default)]
pub struct TouchpadNavigation {
    last_event: Option<Instant>,
    travel: Point<f32>,
    consumed: bool,
    wait_for_end: bool,
    repeat_interval: Option<Duration>,
    last_turn: Option<Instant>,
}

impl TouchpadNavigation {
    /// Repeats while movement events arrive, without scheduling future turns.
    /// Release and cancellation stop repeats and suppress the momentum tail.
    pub fn repeating(interval: Duration) -> Self {
        Self {
            repeat_interval: Some(interval),
            ..Self::default()
        }
    }

    /// Keeps minor cross-axis drift in the same burst, including on X11.
    pub fn is_active(&self) -> bool {
        self.wait_for_end
            || self
                .last_event
                .is_some_and(|last| last.elapsed() < IDLE_TIMEOUT)
    }

    /// Returns signed travel on the dominant axis once the threshold is met.
    pub fn update(&mut self, event: &ScrollWheelEvent) -> Option<Point<f32>> {
        self.update_at(event, Instant::now())
    }

    /// Panning or zooming consumes this burst so its tail cannot turn a page.
    pub fn consume(&mut self) {
        self.consumed = true;
        self.wait_for_end = false;
        self.last_event = Some(Instant::now());
    }

    fn update_at(&mut self, event: &ScrollWheelEvent, now: Instant) -> Option<Point<f32>> {
        let delta = match event.delta {
            ScrollDelta::Pixels(delta) => point(f32::from(delta.x), f32::from(delta.y)),
            // X11 reports smooth touchpad scrolling in lines as well.
            ScrollDelta::Lines(delta) => delta * 32.0,
            ScrollDelta::Pages(_) => return None,
        };
        if event.touch_phase == TouchPhase::Started
            || (!self.wait_for_end
                && self
                    .last_event
                    .is_none_or(|last| now.duration_since(last) >= IDLE_TIMEOUT))
        {
            self.travel = point(0.0, 0.0);
            self.consumed = false;
            self.wait_for_end = event.touch_phase == TouchPhase::Started;
        }
        self.last_event = Some(now);
        if event.touch_phase == TouchPhase::Cancelled {
            self.consumed = true;
            self.wait_for_end = false;
            return None;
        }
        if self.repeat_interval.is_some() && event.touch_phase == TouchPhase::Ended {
            self.consumed = true;
            self.wait_for_end = false;
            return None;
        }
        if self.consumed || !delta.x.is_finite() || !delta.y.is_finite() {
            return None;
        }
        if self.repeat_interval.is_some() {
            // A reversal must not spend the next interval working through
            // movement accumulated in the old direction.
            if delta.x * self.travel.x < 0.0 {
                self.travel.x = 0.0;
            }
            if delta.y * self.travel.y < 0.0 {
                self.travel.y = 0.0;
            }
        }
        self.travel += delta;
        if self.repeat_interval.is_none()
            && self.wait_for_end
            && event.touch_phase != TouchPhase::Ended
        {
            return None;
        }
        if event.touch_phase == TouchPhase::Ended {
            self.wait_for_end = false;
            self.consumed = true;
        }
        let x = self.travel.x.abs();
        let y = self.travel.y.abs();
        let travel = if x >= TURN_DISTANCE && x >= y * 1.25 {
            point(self.travel.x, 0.0)
        } else if y >= TURN_DISTANCE && y >= x * 1.25 {
            point(0.0, self.travel.y)
        } else {
            return None;
        };
        if let Some(interval) = self.repeat_interval {
            if (delta.x == 0.0 && delta.y == 0.0)
                || self
                    .last_turn
                    .is_some_and(|last| now.duration_since(last) < interval)
            {
                return None;
            }
            self.last_turn = Some(now);
            self.travel = point(0.0, 0.0);
        } else {
            self.consumed = true;
        }
        Some(travel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::px;

    fn scroll(x: f32, y: f32, touch_phase: TouchPhase) -> ScrollWheelEvent {
        ScrollWheelEvent {
            delta: ScrollDelta::Pixels(point(px(x), px(y))),
            touch_phase,
            ..Default::default()
        }
    }

    #[test]
    fn accumulates_small_movements_and_ignores_release_and_momentum() {
        let mut navigation = TouchpadNavigation::default();
        let now = Instant::now();
        assert_eq!(
            navigation.update_at(&scroll(-20.0, 2.0, TouchPhase::Started), now),
            None
        );
        assert_eq!(
            navigation.update_at(&scroll(-30.0, 2.0, TouchPhase::Moved), now),
            None
        );
        assert_eq!(
            navigation.update_at(&scroll(0.0, 0.0, TouchPhase::Ended), now),
            Some(point(-50.0, 0.0))
        );
        assert_eq!(
            navigation.update_at(
                &scroll(-100.0, 0.0, TouchPhase::Moved),
                now + Duration::from_millis(20)
            ),
            None
        );
        assert_eq!(
            navigation.update_at(
                &scroll(0.0, 60.0, TouchPhase::Started),
                now + Duration::from_millis(30)
            ),
            None
        );
        assert_eq!(
            navigation.update_at(
                &scroll(0.0, 0.0, TouchPhase::Ended),
                now + Duration::from_millis(40)
            ),
            Some(point(0.0, 60.0))
        );
    }

    #[test]
    fn unphased_backends_restart_after_idle_and_cancel_discards_travel() {
        let mut navigation = TouchpadNavigation::default();
        let now = Instant::now();
        assert_eq!(
            navigation.update_at(&scroll(0.0, -60.0, TouchPhase::Moved), now),
            Some(point(0.0, -60.0))
        );
        assert_eq!(
            navigation.update_at(&scroll(0.0, -60.0, TouchPhase::Moved), now + IDLE_TIMEOUT),
            Some(point(0.0, -60.0))
        );
        assert_eq!(
            navigation.update_at(&scroll(-20.0, 0.0, TouchPhase::Started), now + IDLE_TIMEOUT),
            None
        );
        assert_eq!(
            navigation.update_at(
                &scroll(-100.0, 0.0, TouchPhase::Cancelled),
                now + IDLE_TIMEOUT
            ),
            None
        );
        assert_eq!(
            navigation.update_at(&scroll(-100.0, 0.0, TouchPhase::Ended), now + IDLE_TIMEOUT),
            None
        );
        assert_eq!(
            navigation.update_at(
                &scroll(-100.0, 0.0, TouchPhase::Started),
                now + IDLE_TIMEOUT
            ),
            None
        );
        assert_eq!(
            navigation.update_at(&scroll(0.0, 0.0, TouchPhase::Cancelled), now + IDLE_TIMEOUT),
            None
        );
        assert_eq!(
            navigation.update_at(&scroll(-100.0, 0.0, TouchPhase::Moved), now + IDLE_TIMEOUT),
            None
        );
    }

    #[test]
    fn panning_consumes_the_burst_and_invalid_or_diagonal_input_does_not_turn() {
        let mut navigation = TouchpadNavigation::default();
        let now = Instant::now();
        assert_eq!(
            navigation.update_at(&scroll(f32::NAN, 0.0, TouchPhase::Started), now),
            None
        );
        assert_eq!(
            navigation.update_at(&scroll(50.0, 50.0, TouchPhase::Moved), now),
            None
        );
        navigation.consume();
        assert_eq!(
            navigation.update_at(&scroll(0.0, 200.0, TouchPhase::Moved), now),
            None
        );
        assert_eq!(
            navigation.update_at(
                &ScrollWheelEvent {
                    delta: ScrollDelta::Pages(point(0.0, -1.0)),
                    ..Default::default()
                },
                now
            ),
            None
        );
    }

    #[test]
    fn x11_horizontal_line_deltas_accumulate_like_pixel_deltas() {
        let mut navigation = TouchpadNavigation::default();
        let now = Instant::now();
        let event = ScrollWheelEvent {
            delta: ScrollDelta::Lines(point(-1.0, 0.0)),
            ..Default::default()
        };
        assert_eq!(navigation.update_at(&event, now), None);
        assert_eq!(navigation.update_at(&event, now), Some(point(-64.0, 0.0)));
        assert_eq!(navigation.update_at(&event, now), None);
    }

    #[test]
    fn continuous_movement_turns_five_times_per_second_with_or_without_phases() {
        let interval = Duration::from_millis(200);
        for phased in [false, true] {
            let mut navigation = TouchpadNavigation::repeating(interval);
            let now = Instant::now();
            let mut turns = Vec::new();
            for milliseconds in (0..1_000).step_by(20) {
                let phase = if phased && milliseconds == 0 {
                    TouchPhase::Started
                } else {
                    TouchPhase::Moved
                };
                if let Some(travel) = navigation.update_at(
                    &scroll(0.0, -48.0, phase),
                    now + Duration::from_millis(milliseconds),
                ) {
                    assert!(travel.y < 0.0);
                    turns.push(milliseconds);
                }
            }
            assert_eq!(turns, [0, 200, 400, 600, 800]);
            // Accumulated travel cannot generate a delayed turn on a zero delta.
            assert_eq!(
                navigation.update_at(
                    &scroll(0.0, 0.0, TouchPhase::Moved),
                    now + Duration::from_millis(1_000)
                ),
                None
            );
            assert_eq!(
                navigation.update_at(
                    &scroll(0.0, 0.0, TouchPhase::Ended),
                    now + Duration::from_millis(1_020)
                ),
                None
            );
            assert_eq!(
                navigation.update_at(
                    &scroll(0.0, -100.0, TouchPhase::Moved),
                    now + Duration::from_millis(1_040)
                ),
                None
            );
        }
    }

    #[test]
    fn repeating_navigation_filters_jitter_and_discards_release_and_cancelled_travel() {
        let interval = Duration::from_millis(200);
        let mut navigation = TouchpadNavigation::repeating(interval);
        let now = Instant::now();
        assert_eq!(
            navigation.update_at(&scroll(-20.0, 0.0, TouchPhase::Started), now),
            None
        );
        assert_eq!(
            navigation.update_at(&scroll(0.0, 0.0, TouchPhase::Ended), now),
            None
        );
        assert_eq!(
            navigation.update_at(&scroll(-100.0, 0.0, TouchPhase::Moved), now),
            None
        );
        assert_eq!(
            navigation.update_at(&scroll(-20.0, 0.0, TouchPhase::Started), now),
            None
        );
        assert_eq!(
            navigation.update_at(&scroll(-28.0, 0.0, TouchPhase::Moved), now),
            Some(point(-48.0, 0.0))
        );
        assert_eq!(
            navigation.update_at(&scroll(-100.0, 0.0, TouchPhase::Moved), now + interval / 2),
            None
        );
        assert_eq!(
            navigation.update_at(&scroll(48.0, 0.0, TouchPhase::Moved), now + interval),
            Some(point(48.0, 0.0))
        );
        assert_eq!(
            navigation.update_at(
                &scroll(100.0, 0.0, TouchPhase::Cancelled),
                now + interval * 2
            ),
            None
        );
        assert_eq!(
            navigation.update_at(&scroll(100.0, 0.0, TouchPhase::Moved), now + interval * 2),
            None
        );
        assert_eq!(
            navigation.update_at(&scroll(0.0, 60.0, TouchPhase::Started), now + interval * 2),
            Some(point(0.0, 60.0))
        );
    }
}
