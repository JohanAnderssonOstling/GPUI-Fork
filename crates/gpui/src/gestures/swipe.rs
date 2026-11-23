//! Visual feedback shared by paginated touch surfaces.

use std::time::Duration;
use web_time::Instant;

use crate::{
    App, Axis, Pixels, Point, Size, Window, point, px, touch_page_settle_duration,
    touch_page_settle_easing,
};

/// A finger-controlled translation which can ease back after a rejected turn.
#[derive(Default)]
pub struct SwipePreview {
    offset: Point<Pixels>,
    returning_at: Option<Instant>,
    axis: Option<Axis>,
    extent: Pixels,
    return_duration: Duration,
    redraw_pending: bool,
}

impl SwipePreview {
    /// Reports whether observers need invalidating after movement in the window.
    pub fn drag(&mut self, travel: Point<f32>, window: &Window) -> bool {
        self.drag_in_viewport(travel, window.viewport_size())
    }

    /// Updates displacement within the supplied content viewport, coalescing invalidations.
    pub fn drag_in_viewport(&mut self, travel: Point<f32>, viewport: Size<Pixels>) -> bool {
        if self.returning_at.is_some() {
            self.axis = None;
        }
        if self.axis.is_none() && travel.x.abs().max(travel.y.abs()) >= 12.0 {
            if travel.x.abs() >= travel.y.abs() * 1.25 {
                self.axis = Some(Axis::Horizontal);
            } else if travel.y.abs() >= travel.x.abs() * 1.25 {
                self.axis = Some(Axis::Vertical);
            }
        }
        // Keep the other axis stationary once a deliberate direction emerges.
        let offset = if self.axis.map_or(travel.x.abs() > travel.y.abs(), |axis| {
            axis == Axis::Horizontal
        }) {
            point(
                px(travel.x).clamp(-viewport.width * 0.85, viewport.width * 0.85),
                Pixels::ZERO,
            )
        } else {
            point(
                Pixels::ZERO,
                px(travel.y).clamp(-viewport.height * 0.85, viewport.height * 0.85),
            )
        };
        self.extent = if offset.x != Pixels::ZERO {
            viewport.width
        } else {
            viewport.height
        };
        let changed = self.offset != offset;
        self.offset = offset;
        self.returning_at = None;
        // State follows every event, but observers only need one invalidation
        // until the frame consumes the latest displacement.
        let notify = changed && !self.redraw_pending;
        self.redraw_pending |= changed;
        notify
    }

    /// Consume the current visual displacement when a page turn takes over.
    pub fn take_offset(&mut self) -> Point<Pixels> {
        let offset = if let Some(started_at) = self.returning_at {
            let progress = (started_at.elapsed().as_secs_f32()
                / self.return_duration.as_secs_f32())
            .clamp(0.0, 1.0);
            self.offset * (1.0 - touch_page_settle_easing(progress))
        } else {
            self.offset
        };
        self.clear();
        offset
    }

    /// Starts easing a rejected drag back to its origin.
    pub fn settle(&mut self) {
        self.return_duration =
            touch_page_settle_duration(self.offset.x.abs().max(self.offset.y.abs()), self.extent);
        self.returning_at = (self.offset != Point::default()).then(Instant::now);
    }

    /// Discards displacement and any pending return animation.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Consumes the latest displacement and schedules frames while returning.
    pub fn offset(&mut self, window: &mut Window, cx: &App) -> Point<Pixels> {
        self.redraw_pending = false;
        if cx.reduce_motion() {
            self.clear();
            return Point::default();
        }
        let Some(started_at) = self.returning_at else {
            return self.offset;
        };
        let progress = (started_at.elapsed().as_secs_f32() / self.return_duration.as_secs_f32())
            .clamp(0.0, 1.0);
        if progress >= 1.0 {
            self.clear();
            return Point::default();
        }
        window.request_animation_frame();
        self.offset * (1.0 - touch_page_settle_easing(progress))
    }
}

/// Retains the outgoing page while the incoming page is prepared, then slides
/// both by the same distance. No document layout is performed by the animation.
pub struct PageTurn<S> {
    /// Content retained until the incoming page is ready and the turn completes.
    pub outgoing: S,
    viewport: Size<Pixels>,
    travel: Point<Pixels>,
    drag: Point<Pixels>,
    started_at: Option<Instant>,
    duration: Duration,
}

/// Displacements of adjacent pages for one animation frame.
pub struct PageTurnFrame {
    /// Translation of the outgoing page from its original position.
    pub outgoing: Point<Pixels>,
    /// Translation of the incoming page from its destination.
    pub incoming: Point<Pixels>,
    /// Whether destination content is ready, allowing the animation clock to advance.
    pub ready: bool,
}

impl<S> PageTurn<S> {
    /// Whether application motion preferences allow a page animation.
    pub fn enabled(cx: &App) -> bool {
        !cx.reduce_motion()
    }

    /// Creates a turn along the dominant drag axis, or horizontally without a drag.
    pub fn new(
        outgoing: S,
        forward: bool,
        drag: Point<Pixels>,
        viewport: Size<Pixels>,
        cx: &App,
    ) -> Option<Self> {
        let axis = if drag.y.abs() > drag.x.abs() {
            Axis::Vertical
        } else {
            Axis::Horizontal
        };
        Self::with_axis(outgoing, forward, drag, viewport, axis, cx)
    }

    /// Creates a turn on the requested axis, discarding cross-axis displacement.
    pub fn new_on_axis(
        outgoing: S,
        forward: bool,
        drag: Point<Pixels>,
        viewport: Size<Pixels>,
        axis: Axis,
        cx: &App,
    ) -> Option<Self> {
        let drag = match axis {
            Axis::Vertical => point(Pixels::ZERO, drag.y),
            Axis::Horizontal => point(drag.x, Pixels::ZERO),
        };
        Self::with_axis(outgoing, forward, drag, viewport, axis, cx)
    }

    fn with_axis(
        outgoing: S,
        forward: bool,
        drag: Point<Pixels>,
        viewport: Size<Pixels>,
        axis: Axis,
        cx: &App,
    ) -> Option<Self> {
        if !Self::enabled(cx) {
            return None;
        }
        let direction = if forward { -1.0 } else { 1.0 };
        let travel = match axis {
            Axis::Vertical => point(Pixels::ZERO, viewport.height * direction),
            Axis::Horizontal => point(viewport.width * direction, Pixels::ZERO),
        };
        let extent = travel.x.abs().max(travel.y.abs());
        if extent <= Pixels::ZERO {
            return None;
        }
        // A stale displacement from a cancelled gesture must not start the
        // next keyboard turn on the wrong side of the page.
        let along = f32::from(match axis {
            Axis::Vertical => drag.y,
            Axis::Horizontal => drag.x,
        });
        let drag = if along == 0.0 || (along < 0.0) == forward {
            drag
        } else {
            Point::default()
        };
        let remaining = (travel - drag).x.abs().max((travel - drag).y.abs());
        let fraction = (f32::from(remaining) / f32::from(extent)).clamp(0.0, 1.0);
        let duration = Duration::from_secs_f32((80.0 + 100.0 * fraction) / 1000.0);
        Some(Self {
            outgoing,
            viewport,
            travel,
            drag,
            started_at: None,
            duration,
        })
    }

    fn offsets(&self, progress: f32, ready: bool) -> PageTurnFrame {
        let outgoing = self.drag + (self.travel - self.drag) * touch_page_settle_easing(progress);
        PageTurnFrame {
            outgoing,
            incoming: outgoing - self.travel,
            ready,
        }
    }

    /// Returns None when complete or when resizing/reduced motion cancels it.
    /// The clock starts only once the destination can actually be painted.
    pub fn frame(
        &mut self,
        viewport: Size<Pixels>,
        ready: bool,
        window: &mut Window,
        cx: &App,
    ) -> Option<PageTurnFrame> {
        let frame = self.advance(viewport, ready, cx)?;
        if frame.ready {
            window.request_animation_frame();
        }
        Some(frame)
    }

    fn advance(&mut self, viewport: Size<Pixels>, ready: bool, cx: &App) -> Option<PageTurnFrame> {
        if !Self::enabled(cx) || self.viewport != viewport {
            return None;
        }
        if !ready {
            return Some(self.offsets(0.0, false));
        }
        let started_at = *self.started_at.get_or_insert_with(Instant::now);
        let progress = started_at.elapsed().as_secs_f32() / self.duration.as_secs_f32();
        if progress >= 1.0 {
            return None;
        }
        Some(self.offsets(progress, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TestAppContext;
    use crate::prelude::*;
    use crate::{Context, IntoElement, Render};

    #[gpui::test]
    fn a_burst_of_moves_invalidates_once_but_keeps_the_latest_position(cx: &mut TestAppContext) {
        let (_, visual) = cx.add_window_view(|_, _| gpui::Empty);
        visual.update(|window, cx| {
            let mut preview = SwipePreview::default();
            assert!(preview.drag(point(-20.0, 0.0), window));
            for distance in 21..100 {
                assert!(!preview.drag(point(-(distance as f32), 0.0), window));
            }
            assert_eq!(preview.offset(window, cx), point(px(-99.0), Pixels::ZERO));
            assert!(
                preview.drag(point(-100.0, 0.0), window),
                "the next frame accepts another invalidation"
            );
        });
    }
    #[gpui::test]
    fn turns_continue_in_the_drag_direction_and_keep_pages_adjacent(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let viewport = gpui::size(px(600.0), px(800.0));
            for (forward, drag, travel) in [
                (true, point(px(-120.0), px(0.0)), point(px(-600.0), px(0.0))),
                (false, point(px(120.0), px(0.0)), point(px(600.0), px(0.0))),
                (true, point(px(0.0), px(-120.0)), point(px(0.0), px(-800.0))),
                (false, point(px(0.0), px(120.0)), point(px(0.0), px(800.0))),
                (true, Point::default(), point(px(-600.0), px(0.0))),
                (false, Point::default(), point(px(600.0), px(0.0))),
            ] {
                let turn = PageTurn::new((), forward, drag, viewport, cx).unwrap();
                assert_eq!(
                    turn.offsets(0.0, true).outgoing,
                    drag,
                    "release must not snap back"
                );
                let mut previous = drag;
                for progress in [0.0, 0.25, 0.5, 0.75, 1.0] {
                    let frame = turn.offsets(progress, true);
                    assert_eq!(
                        frame.outgoing - frame.incoming,
                        travel,
                        "the two pages must stay adjacent"
                    );
                    let movement = frame.outgoing - previous;
                    assert!(if forward {
                        movement.x <= Pixels::ZERO && movement.y <= Pixels::ZERO
                    } else {
                        movement.x >= Pixels::ZERO && movement.y >= Pixels::ZERO
                    });
                    previous = frame.outgoing;
                }
                assert_eq!(turn.offsets(1.0, true).incoming, Point::default());
            }
        });
    }

    #[gpui::test]
    fn explicit_vertical_turns_keep_keyboard_and_drag_motion_on_that_axis(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let viewport = gpui::size(px(600.0), px(800.0));
            for forward in [true, false] {
                let direction = if forward { -1.0 } else { 1.0 };
                for drag in [Point::default(), point(px(100.0), px(120.0 * direction))] {
                    let turn =
                        PageTurn::new_on_axis((), forward, drag, viewport, Axis::Vertical, cx)
                            .unwrap();
                    for progress in [0.0, 0.5, 1.0] {
                        let frame = turn.offsets(progress, true);
                        assert_eq!(frame.outgoing.x, Pixels::ZERO);
                        assert_eq!(frame.incoming.x, Pixels::ZERO);
                        assert_eq!(
                            frame.outgoing.y - frame.incoming.y,
                            viewport.height * direction
                        );
                    }
                }
            }
        });
    }

    #[gpui::test]
    fn drag_is_bounded_by_the_document_and_hands_off_without_snapping(cx: &mut TestAppContext) {
        let (_, visual) = cx.add_window_view(|_, _| gpui::Empty);
        visual.update(|window, cx| {
            let viewport = gpui::size(px(300.0), px(400.0));
            let mut preview = SwipePreview::default();
            preview.drag_in_viewport(point(0.0, 1000.0), viewport);
            let drag = preview.take_offset();
            assert_eq!(drag, point(px(0.0), px(340.0)));
            assert_eq!(preview.offset(window, cx), Point::default());
            let turn = PageTurn::new((), false, drag, viewport, cx).unwrap();
            assert_eq!(turn.offsets(0.0, true).outgoing, drag);
            assert!(
                turn.offsets(0.5, true).outgoing.y > drag.y,
                "completion must keep moving downward"
            );
        });
    }

    #[gpui::test]
    fn turns_wait_for_content_finish_and_cancel_for_resize_or_reduced_motion(
        cx: &mut TestAppContext,
    ) {
        let (_, visual) = cx.add_window_view(|_, _| gpui::Empty);
        visual.update(|window, cx| {
            let viewport = window.viewport_size();
            let mut turn = PageTurn::new((), true, Point::default(), viewport, cx).unwrap();
            assert!(!turn.advance(viewport, false, cx).unwrap().ready);
            assert!(
                turn.started_at.is_none(),
                "loading time must not consume animation time"
            );
            assert!(turn.advance(viewport, true, cx).unwrap().ready);
            turn.started_at = Some(Instant::now() - Duration::from_secs(1));
            assert!(turn.advance(viewport, true, cx).is_none());

            let mut turn = PageTurn::new((), false, Point::default(), viewport, cx).unwrap();
            assert!(
                turn.advance(
                    gpui::size(viewport.width + px(10.0), viewport.height),
                    true,
                    cx
                )
                .is_none()
            );
            cx.set_reduce_motion(true);
            assert!(turn.advance(viewport, true, cx).is_none());
            assert!(PageTurn::new((), true, Point::default(), viewport, cx).is_none());
        });
    }

    struct TurnView {
        turn: Option<PageTurn<()>>,
        renders: usize,
    }

    impl Render for TurnView {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.renders += 1;
            if self
                .turn
                .as_mut()
                .and_then(|turn| turn.frame(window.viewport_size(), true, window, cx))
                .is_none()
            {
                self.turn = None;
            }
            gpui::div().size_full()
        }
    }

    #[gpui::test]
    fn turns_schedule_frames_from_render_and_stop_when_complete(cx: &mut TestAppContext) {
        let (view, visual) = cx.add_window_view(|window, cx| TurnView {
            turn: PageTurn::new((), true, Point::default(), window.viewport_size(), cx),
            renders: 0,
        });
        visual.run_until_parked();
        let renders = view.read_with(visual, |view, _| {
            assert!(view.turn.as_ref().unwrap().started_at.is_some());
            view.renders
        });
        visual.update(|window, cx| {
            assert!(window.simulate_next_frame(cx) > 0);
        });
        visual.run_until_parked();
        view.update(visual, |view, cx| {
            assert!(
                view.renders > renders,
                "the scheduled callback must redraw the owning view"
            );
            view.turn.as_mut().unwrap().started_at = Some(Instant::now() - Duration::from_secs(1));
            cx.notify();
        });
        visual.run_until_parked();
        view.read_with(visual, |view, _| assert!(view.turn.is_none()));
    }
}
