//! Touch gesture recognition vocabulary.
//!
//! GPUI recognizes gestures from raw [`TouchEvent`](crate::TouchEvent)s in a
//! single, portable arena in gpui core: recognizers compete for in-flight
//! touches, winners claim them, and losers are cancelled. Recognized gestures
//! are surfaced through *existing* semantic events wherever possible, a tap
//! becomes [`ClickEvent::Touch`](crate::ClickEvent), a pan becomes
//! [`ScrollWheelEvent`](crate::ScrollWheelEvent)s carrying a
//! [`TouchPhase`](crate::TouchPhase), and a pinch becomes
//! [`PinchEvent`](crate::PinchEvent)s — so components written against
//! `on_click` and scroll containers work untouched on mobile.

use std::time::Duration;
use web_time::Instant;

use crate::{
    Axis, IsZero, Pixels, Point, ScrollDelta, ScrollWheelEvent, Size, TouchPhase, ease_in_out,
    point, px,
};

const SCROLL_EVENT_SEPARATION: Duration = Duration::from_millis(28);

/// Tracks the dominant axis across the events in a scroll gesture.
#[derive(Clone, Copy, Debug, Default)]
pub struct OngoingScroll {
    last_event: Option<Instant>,
    axis: Option<Axis>,
    pub(crate) page_origin: Option<Point<Pixels>>,
}

impl OngoingScroll {
    /// Filters the given delta to the dominant axis of the current scroll gesture.
    ///
    /// Gestures are delimited by their touch phase when available, with a timeout
    /// fallback for platforms that only emit [`TouchPhase::Moved`].
    pub fn filter(&mut self, delta: &mut Point<Pixels>, touch_phase: TouchPhase) {
        self.filter_at(delta, touch_phase, Instant::now())
    }

    fn filter_at(&mut self, delta: &mut Point<Pixels>, touch_phase: TouchPhase, now: Instant) {
        const UNLOCK_PERCENT: f32 = 1.9;
        const UNLOCK_LOWER_BOUND: Pixels = px(6.);

        if matches!(touch_phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            self.last_event = None;
            self.axis = None;
            return;
        }

        let x = delta.x.abs();
        let y = delta.y.abs();
        if x.is_zero() && y.is_zero() {
            if touch_phase == TouchPhase::Started {
                self.last_event = None;
                self.axis = None;
            }
            return;
        }

        let starts_new_gesture = touch_phase == TouchPhase::Started
            || self
                .last_event
                .is_none_or(|last_event| now.duration_since(last_event) >= SCROLL_EVENT_SEPARATION);
        let mut axis = self.axis;
        if starts_new_gesture {
            axis = if x <= y {
                Some(Axis::Vertical)
            } else {
                Some(Axis::Horizontal)
            };
        } else if x.max(y) >= UNLOCK_LOWER_BOUND {
            match axis {
                Some(Axis::Vertical) if x > y && x >= y * UNLOCK_PERCENT => {
                    axis = None;
                }
                Some(Axis::Horizontal) if y > x && y >= x * UNLOCK_PERCENT => {
                    axis = None;
                }
                _ => {}
            }
        }

        self.last_event = Some(now);
        self.axis = axis;
        match axis {
            Some(Axis::Vertical) => delta.x = Pixels::ZERO,
            Some(Axis::Horizontal) => delta.y = Pixels::ZERO,
            None => {}
        }
    }
}

/// Duration of the eased pan used when scrolling by a complete viewport.
pub const PAGE_SCROLL_DURATION: Duration = Duration::from_millis(140);

/// Duration of a touch settle, proportional to the fraction of a page left.
/// A short correction finishes quickly; a full turn stays within 120 ms.
pub fn touch_page_settle_duration(remaining: Pixels, extent: Pixels) -> Duration {
    let fraction = (f32::from(remaining.abs()) / f32::from(extent).max(1.0)).clamp(0.0, 1.0);
    Duration::from_secs_f32((60.0 + 60.0 * fraction) / 1000.0)
}

/// Touch settles begin moving immediately and slow down as they reach the target.
pub fn touch_page_settle_easing(progress: f32) -> f32 {
    1.0 - (1.0 - progress.clamp(0.0, 1.0)).powi(3)
}

/// A viewport pan that can be interrupted by direct scrolling.
#[derive(Debug)]
pub(crate) struct PageScrollAnimation {
    from: Point<Pixels>,
    pub(crate) target: Point<Pixels>,
    pub(crate) last_offset: Point<Pixels>,
    started_at: Instant,
    touch_duration: Option<Duration>,
}

impl PageScrollAnimation {
    pub(crate) fn new(from: Point<Pixels>, target: Point<Pixels>) -> Self {
        Self {
            from,
            target,
            last_offset: from,
            started_at: Instant::now(),
            touch_duration: None,
        }
    }

    pub(crate) fn touch(
        from: Point<Pixels>,
        target: Point<Pixels>,
        viewport: Size<Pixels>,
    ) -> Self {
        let remaining = target - from;
        let (distance, extent) = if remaining.x.abs() > remaining.y.abs() {
            (remaining.x, viewport.width)
        } else {
            (remaining.y, viewport.height)
        };
        Self {
            touch_duration: Some(touch_page_settle_duration(distance, extent)),
            ..Self::new(from, target)
        }
    }

    pub(crate) fn advance(&mut self, reduce_motion: bool) -> (Point<Pixels>, bool) {
        let progress = if reduce_motion {
            1.0
        } else {
            (self.started_at.elapsed().as_secs_f32()
                / self
                    .touch_duration
                    .unwrap_or(PAGE_SCROLL_DURATION)
                    .as_secs_f32())
            .clamp(0.0, 1.0)
        };
        let eased = if self.touch_duration.is_some() {
            touch_page_settle_easing(progress)
        } else {
            ease_in_out(progress)
        };
        self.last_offset = self.from + (self.target - self.from) * eased;
        if progress >= 1.0 {
            self.last_offset = self.target;
        }
        (self.last_offset, progress >= 1.0)
    }
}

/// Streams touch movement to the receiving surface, then requests a page or
/// a return to the starting offset on release. Positions stay anchored to the
/// first contact so moving across another surface cannot transfer the gesture.
#[derive(Default)]
pub(crate) struct PageScrollGesture {
    start: Option<ScrollWheelEvent>,
    last_sample: Option<Instant>,
    velocity: Point<f32>,
    from_edge: bool,
    axis: Option<Axis>,
}

impl PageScrollGesture {
    pub(crate) fn active(&self) -> bool {
        self.start.is_some() && !self.from_edge
    }

    pub(crate) fn filter(
        &mut self,
        event: ScrollWheelEvent,
        viewport: Size<Pixels>,
        bottom_inset: Pixels,
    ) -> Option<ScrollWheelEvent> {
        self.filter_at(event, viewport, bottom_inset, Instant::now())
    }

    fn lock_axis(&mut self, travel: Point<Pixels>) {
        if self.axis.is_none() {
            let x = f32::from(travel.x).abs();
            let y = f32::from(travel.y).abs();
            if x.max(y) >= 12.0 {
                if x >= y * 1.25 {
                    self.axis = Some(Axis::Horizontal);
                } else if y >= x * 1.25 {
                    self.axis = Some(Axis::Vertical);
                }
            }
        }
    }

    fn filter_at(
        &mut self,
        mut event: ScrollWheelEvent,
        viewport: Size<Pixels>,
        bottom_inset: Pixels,
        now: Instant,
    ) -> Option<ScrollWheelEvent> {
        let ScrollDelta::Pixels(delta) = event.delta else {
            *self = Self::default();
            return Some(event);
        };
        if !f32::from(delta.x).is_finite() || !f32::from(delta.y).is_finite() {
            let start = self.start.take();
            *self = Self::default();
            return start.map(|start| ScrollWheelEvent {
                position: start.position,
                delta: ScrollDelta::Pages(Point::default()),
                touch_phase: TouchPhase::Cancelled,
                ..event
            });
        }
        match event.touch_phase {
            TouchPhase::Started => {
                self.start = Some(event.clone());
                self.last_sample = Some(now);
                self.velocity = Point::default();
                self.axis = None;
                self.lock_axis(delta);
                self.from_edge = event.position.y >= viewport.height - bottom_inset.max(px(48.0));
                (!self.from_edge).then_some(event)
            }
            TouchPhase::Moved => {
                let Some(start) = self.start.as_mut() else {
                    return Some(event);
                };
                let ScrollDelta::Pixels(travel) = &mut start.delta else {
                    unreachable!()
                };
                *travel += delta;
                if let Some(last) = self.last_sample {
                    let seconds = now.duration_since(last).as_secs_f32().max(0.001);
                    self.velocity =
                        point(f32::from(delta.x) / seconds, f32::from(delta.y) / seconds);
                }
                self.last_sample = Some(now);
                event.position = start.position;
                let travel = *travel;
                self.lock_axis(travel);
                (!self.from_edge).then_some(event)
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                let Some(start) = self.start.take() else {
                    return Some(event);
                };
                if self.from_edge {
                    *self = Self::default();
                    return None;
                }
                let ScrollDelta::Pixels(travel) = start.delta else {
                    unreachable!()
                };
                let travel = travel + delta;
                let horizontal = self.axis.map_or(travel.x.abs() > travel.y.abs(), |axis| {
                    axis == Axis::Horizontal
                });
                let (along, extent, velocity) = if horizontal {
                    (f32::from(travel.x), viewport.width, self.velocity.x)
                } else {
                    (f32::from(travel.y), viewport.height, self.velocity.y)
                };
                let threshold = (f32::from(extent) * 0.12).clamp(48.0, 120.0);
                // A short flick must be deliberate and still moving at release;
                // holding a short drag must not turn it into a page request.
                let flick = along.abs() >= 24.0
                    && velocity.abs() >= 600.0
                    && velocity.signum() == along.signum()
                    && self
                        .last_sample
                        .is_some_and(|last| now.duration_since(last) <= Duration::from_millis(80));
                let commit =
                    event.touch_phase == TouchPhase::Ended && (along.abs() >= threshold || flick);
                event.position = start.position;
                event.delta = ScrollDelta::Pages(if !commit {
                    Point::default()
                } else if horizontal {
                    point(along.signum(), 0.0)
                } else {
                    point(0.0, along.signum())
                });
                *self = Self::default();
                Some(event)
            }
        }
    }
}

/// Feel constants consumed by gesture recognizers. Provided on a best-effort
/// basis, depending on each platform's support, defaulting to GPUI's own
/// (iOS flavored) values
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GestureTuning {
    /// Distance a touch may travel before it stops being a potential tap and
    /// becomes a pan/drag.
    pub touch_slop: Pixels,
    /// Maximum interval between taps for them to accumulate a tap count.
    pub multi_tap_interval: Duration,
    /// Maximum distance between taps for them to accumulate a tap count.
    pub multi_tap_slop: Pixels,
    /// How long a touch must remain within [`Self::touch_slop`] to be
    /// recognized as a long press.
    pub long_press_duration: Duration,
    /// Per-millisecond decay factor applied to scroll momentum after a fling.
    /// (`UIScrollView` uses `0.998` per millisecond for its normal
    /// deceleration rate.)
    pub momentum_decay_per_ms: f32,
    /// Minimum release velocity, in pixels per second, required to start
    /// scroll momentum.
    pub min_fling_velocity: f32,
}

impl Default for GestureTuning {
    fn default() -> Self {
        Self {
            touch_slop: px(8.),
            multi_tap_interval: Duration::from_millis(400),
            multi_tap_slop: px(16.),
            long_press_duration: Duration::from_millis(500),
            momentum_decay_per_ms: 0.998,
            min_fling_velocity: 50.,
        }
    }
}

/// The set of gesture kinds that participate in recognition.
///
/// Used by [`PlatformGestures::native_recognizers`] to declare which gestures
/// the platform recognizes natively rather than leaving to gpui core's
/// portable recognizers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GestureKinds {
    /// Tap (and multi-tap), surfaced as [`ClickEvent::Touch`](crate::ClickEvent).
    pub tap: bool,
    /// Long press, surfaced as [`LongPressEvent`](crate::LongPressEvent).
    pub long_press: bool,
    /// Pan/scroll (including fling momentum), surfaced as
    /// [`ScrollWheelEvent`](crate::ScrollWheelEvent)s.
    pub pan: bool,
    /// Pinch to zoom, surfaced as [`PinchEvent`](crate::PinchEvent)s.
    pub pinch: bool,
}

impl GestureKinds {
    /// No gestures; gpui core's portable recognizers handle everything.
    pub const NONE: Self = Self {
        tap: false,
        long_press: false,
        pan: false,
        pinch: false,
    };

    /// All gesture kinds.
    pub const ALL: Self = Self {
        tap: true,
        long_press: true,
        pan: true,
        pinch: true,
    };
}

/// Platform gesture recognition services.
///
/// If your mobile platform supports native gesture recognition, use this
/// to share it with GPUI.
pub trait PlatformGestures {
    /// Feel constants for the portable recognizers on this platform.
    fn tuning(&self) -> GestureTuning {
        GestureTuning::default()
    }

    /// The gesture kinds this platform recognizes natively.
    fn native_recognizers(&self) -> GestureKinds {
        GestureKinds::NONE
    }
}

/// A no-op [`PlatformGestures`] implementation: no native recognizers and
/// default tuning. Suitable for desktop platforms and tests.
pub struct NullPlatformGestures;

impl PlatformGestures for NullPlatformGestures {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::point;

    #[test]
    fn touch_settles_respond_early_and_shorten_with_the_remaining_distance() {
        let short = touch_page_settle_duration(px(10.0), px(100.0));
        let long = touch_page_settle_duration(px(100.0), px(100.0));
        assert!(short < long);
        assert!(long <= Duration::from_millis(120));
        assert!(
            touch_page_settle_easing(0.1) > 0.25,
            "settling should respond in its first frames"
        );
        assert_eq!(touch_page_settle_easing(0.0), 0.0);
        assert_eq!(touch_page_settle_easing(1.0), 1.0);
    }

    fn page_event(dy: f32, phase: TouchPhase) -> ScrollWheelEvent {
        ScrollWheelEvent {
            position: point(px(100.0), px(100.0)),
            delta: ScrollDelta::Pixels(point(px(0.0), px(dy))),
            touch_phase: phase,
            ..Default::default()
        }
    }

    fn page_filter(
        gesture: &mut PageScrollGesture,
        event: ScrollWheelEvent,
        now: Instant,
    ) -> Option<ScrollWheelEvent> {
        gesture.filter_at(event, crate::size(px(400.0), px(800.0)), px(0.0), now)
    }

    #[test]
    fn page_swipe_streams_movement_and_settles_one_page_at_the_start_position() {
        let now = Instant::now();
        let mut gesture = PageScrollGesture::default();
        let start = page_filter(&mut gesture, page_event(-20.0, TouchPhase::Started), now).unwrap();
        assert!(
            matches!(start.delta, ScrollDelta::Pixels(delta) if delta == point(px(0.0), px(-20.0)))
        );
        assert!(gesture.active());
        let moved = page_filter(
            &mut gesture,
            ScrollWheelEvent {
                position: point(px(300.0), px(500.0)),
                ..page_event(-1000.0, TouchPhase::Moved)
            },
            now + Duration::from_millis(100),
        )
        .unwrap();
        assert_eq!(moved.position, start.position);
        assert!(
            matches!(moved.delta, ScrollDelta::Pixels(delta) if delta == point(px(0.0), px(-1000.0)))
        );
        let end = page_filter(
            &mut gesture,
            page_event(0.0, TouchPhase::Ended),
            now + Duration::from_millis(120),
        )
        .unwrap();
        assert!(matches!(end.delta, ScrollDelta::Pages(delta) if delta == point(0.0, -1.0)));
        assert_eq!(end.position, start.position);
        assert!(!gesture.active());
    }

    #[test]
    fn page_swipe_short_flicks_commit_but_paused_drags_and_jitter_return() {
        let now = Instant::now();
        for (distance, pause_ms, expected) in [(24.0, 10, -1.0), (24.0, 150, 0.0), (10.0, 10, 0.0)]
        {
            let mut gesture = PageScrollGesture::default();
            page_filter(&mut gesture, page_event(-8.0, TouchPhase::Started), now);
            page_filter(
                &mut gesture,
                page_event(-distance, TouchPhase::Moved),
                now + Duration::from_millis(20),
            );
            let end = page_filter(
                &mut gesture,
                page_event(0.0, TouchPhase::Ended),
                now + Duration::from_millis(20 + pause_ms),
            )
            .unwrap();
            assert!(
                matches!(end.delta, ScrollDelta::Pages(delta) if delta == point(0.0, expected))
            );
        }
    }

    #[test]
    fn page_swipe_counts_release_movement_and_keeps_the_locked_axis() {
        let now = Instant::now();
        let mut gesture = PageScrollGesture::default();
        page_filter(&mut gesture, page_event(-20.0, TouchPhase::Started), now);
        let end = page_filter(
            &mut gesture,
            page_event(-80.0, TouchPhase::Ended),
            now + Duration::from_millis(200),
        )
        .unwrap();
        assert!(matches!(end.delta, ScrollDelta::Pages(delta) if delta == point(0.0, -1.0)));

        let mut gesture = PageScrollGesture::default();
        page_filter(
            &mut gesture,
            ScrollWheelEvent {
                delta: ScrollDelta::Pixels(point(px(-60.0), px(0.0))),
                ..page_event(0.0, TouchPhase::Started)
            },
            now,
        );
        page_filter(
            &mut gesture,
            page_event(-200.0, TouchPhase::Moved),
            now + Duration::from_millis(100),
        );
        let end = page_filter(
            &mut gesture,
            page_event(0.0, TouchPhase::Ended),
            now + Duration::from_millis(200),
        )
        .unwrap();
        assert!(matches!(end.delta, ScrollDelta::Pages(delta) if delta == point(-1.0, 0.0)));
    }

    #[test]
    fn page_swipe_cancellation_and_invalid_input_return_the_preview() {
        let now = Instant::now();
        for terminal in [
            page_event(0.0, TouchPhase::Cancelled),
            page_event(f32::NAN, TouchPhase::Moved),
        ] {
            let mut gesture = PageScrollGesture::default();
            page_filter(&mut gesture, page_event(-160.0, TouchPhase::Started), now);
            let end = page_filter(&mut gesture, terminal, now + Duration::from_millis(20)).unwrap();
            assert!(matches!(end.delta, ScrollDelta::Pages(delta) if delta == Point::default()));
            assert_eq!(end.touch_phase, TouchPhase::Cancelled);
            assert!(!gesture.active());
        }
    }

    #[test]
    fn page_swipe_system_edge_is_suppressed_and_hardware_pages_pass_through() {
        let now = Instant::now();
        let mut gesture = PageScrollGesture::default();
        for phase in [TouchPhase::Started, TouchPhase::Moved, TouchPhase::Ended] {
            let event = ScrollWheelEvent {
                position: point(px(100.0), px(799.0)),
                ..page_event(-160.0, phase)
            };
            assert!(page_filter(&mut gesture, event, now).is_none());
            assert!(!gesture.active());
        }
        let event = ScrollWheelEvent {
            delta: ScrollDelta::Pages(point(-1.0, 0.0)),
            ..page_event(0.0, TouchPhase::Ended)
        };
        assert!(
            matches!(page_filter(&mut gesture, event, now).unwrap().delta, ScrollDelta::Pages(delta) if delta == point(-1.0, 0.0))
        );
    }

    #[test]
    fn ongoing_scroll_locks_to_dominant_axis() {
        let now = Instant::now();
        let mut ongoing_scroll = OngoingScroll::default();
        let mut horizontal_delta = point(px(10.), px(2.));
        ongoing_scroll.filter_at(&mut horizontal_delta, TouchPhase::Started, now);
        assert_eq!(ongoing_scroll.axis, Some(Axis::Horizontal));
        assert_eq!(horizontal_delta, point(px(10.), px(0.)));

        let mut continued_delta = point(px(3.), px(2.));
        ongoing_scroll.filter_at(
            &mut continued_delta,
            TouchPhase::Moved,
            now + Duration::from_millis(1),
        );
        assert_eq!(ongoing_scroll.axis, Some(Axis::Horizontal));
        assert_eq!(continued_delta, point(px(3.), px(0.)));
    }

    #[test]
    fn ongoing_scroll_unlocks_when_direction_changes() {
        let now = Instant::now();
        let mut ongoing_scroll = OngoingScroll::default();
        let mut horizontal_delta = point(px(10.), px(2.));
        ongoing_scroll.filter_at(&mut horizontal_delta, TouchPhase::Started, now);

        let mut vertical_delta = point(px(2.), px(10.));
        ongoing_scroll.filter_at(
            &mut vertical_delta,
            TouchPhase::Moved,
            now + Duration::from_millis(1),
        );
        assert_eq!(ongoing_scroll.axis, None);
        assert_eq!(vertical_delta, point(px(2.), px(10.)));
    }

    #[test]
    fn ongoing_scroll_starts_new_gesture_at_timeout_boundary() {
        let now = Instant::now();
        let mut ongoing_scroll = OngoingScroll::default();
        let mut horizontal_delta = point(px(10.), px(2.));
        ongoing_scroll.filter_at(&mut horizontal_delta, TouchPhase::Moved, now);

        let mut vertical_delta = point(px(2.), px(10.));
        ongoing_scroll.filter_at(
            &mut vertical_delta,
            TouchPhase::Moved,
            now + SCROLL_EVENT_SEPARATION,
        );
        assert_eq!(ongoing_scroll.axis, Some(Axis::Vertical));
        assert_eq!(vertical_delta, point(px(0.), px(10.)));
    }

    #[test]
    fn ongoing_scroll_ignores_zero_delta_and_resets_when_ended() {
        let now = Instant::now();
        let mut ongoing_scroll = OngoingScroll::default();
        let mut horizontal_delta = point(px(10.), px(2.));
        ongoing_scroll.filter_at(&mut horizontal_delta, TouchPhase::Started, now);

        let mut zero_delta = Point::default();
        ongoing_scroll.filter_at(
            &mut zero_delta,
            TouchPhase::Ended,
            now + Duration::from_millis(1),
        );
        assert_eq!(ongoing_scroll.axis, None);

        let mut vertical_delta = point(px(2.), px(3.));
        ongoing_scroll.filter_at(
            &mut vertical_delta,
            TouchPhase::Moved,
            now + Duration::from_millis(2),
        );
        assert_eq!(ongoing_scroll.axis, Some(Axis::Vertical));
        assert_eq!(vertical_delta, point(px(0.), px(3.)));
    }

    #[test]
    fn ongoing_scroll_ignores_zero_delta_movement() {
        let now = Instant::now();
        let mut ongoing_scroll = OngoingScroll::default();
        let mut horizontal_delta = point(px(10.), px(2.));
        ongoing_scroll.filter_at(&mut horizontal_delta, TouchPhase::Started, now);

        let mut zero_delta = Point::default();
        ongoing_scroll.filter_at(
            &mut zero_delta,
            TouchPhase::Moved,
            now + SCROLL_EVENT_SEPARATION,
        );

        let mut vertical_delta = point(px(2.), px(10.));
        ongoing_scroll.filter_at(
            &mut vertical_delta,
            TouchPhase::Moved,
            now + SCROLL_EVENT_SEPARATION,
        );
        assert_eq!(ongoing_scroll.axis, Some(Axis::Vertical));
        assert_eq!(vertical_delta, point(px(0.), px(10.)));
    }

    #[test]
    fn ongoing_scroll_supports_moved_only_platforms() {
        let now = Instant::now();
        let mut ongoing_scroll = OngoingScroll::default();
        let mut horizontal_delta = point(px(10.), px(2.));
        ongoing_scroll.filter_at(&mut horizontal_delta, TouchPhase::Moved, now);
        assert_eq!(ongoing_scroll.axis, Some(Axis::Horizontal));
        assert_eq!(horizontal_delta, point(px(10.), px(0.)));
    }
}
