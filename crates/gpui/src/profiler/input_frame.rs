//! Opt-in input-to-frame timings without enabling the full profiler.
//! Submission timings stop at the renderer handoff, not display scanout.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use web_time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Enables info-level measurements for hosts such as Android.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Whether input/frame instrumentation is enabled.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed) || log::log_enabled!(target: "input_frame", log::Level::Debug)
}

fn level() -> log::Level {
    if ENABLED.load(Ordering::Relaxed) {
        log::Level::Info
    } else {
        log::Level::Debug
    }
}

/// Logs the platform queue age, measured against its own monotonic clock.
pub fn platform_event_age(age: Duration) {
    log::log!(target: "input_frame", level(), "stage=platform event_age_ms={:.3}", age.as_secs_f64() * 1000.0);
}

struct PendingInput {
    first: Instant,
    latest: Instant,
    latest_dispatched: Instant,
    kind: &'static str,
    events: usize,
    dispatch_ms: f64,
}

struct Frame {
    input: PendingInput,
    started: Instant,
    layout_started: Option<Instant>,
    layout_ms: f64,
    draw_ms: f64,
}

#[derive(Default)]
pub(crate) struct InputFrameProfile {
    pending: Option<PendingInput>,
    frame: Option<Frame>,
}

impl InputFrameProfile {
    pub(crate) fn begin_input(&self) -> Option<Instant> {
        enabled().then(Instant::now)
    }

    pub(crate) fn end_input(
        &mut self,
        started: Option<Instant>,
        kind: &'static str,
        changed: bool,
    ) {
        let Some(started) = started.filter(|_| changed) else {
            return;
        };
        let dispatched = Instant::now();
        let dispatch_ms = (dispatched - started).as_secs_f64() * 1000.0;
        if let Some(pending) = self.pending.as_mut() {
            pending.latest = started;
            pending.latest_dispatched = dispatched;
            pending.kind = kind;
            pending.events += 1;
            pending.dispatch_ms += dispatch_ms;
        } else {
            self.pending = Some(PendingInput {
                first: started,
                latest: started,
                latest_dispatched: dispatched,
                kind,
                events: 1,
                dispatch_ms,
            });
        }
    }

    pub(crate) fn begin_draw(&mut self) {
        self.frame = self.pending.take().map(|input| Frame {
            input,
            started: Instant::now(),
            layout_started: None,
            layout_ms: 0.0,
            draw_ms: 0.0,
        });
    }

    pub(crate) fn begin_layout(&mut self) {
        if let Some(frame) = self.frame.as_mut() {
            frame.layout_started = Some(Instant::now());
        }
    }

    pub(crate) fn end_layout(&mut self) {
        if let Some(frame) = self.frame.as_mut() {
            if let Some(started) = frame.layout_started.take() {
                frame.layout_ms = started.elapsed().as_secs_f64() * 1000.0;
            }
        }
    }

    pub(crate) fn end_draw(&mut self) {
        if let Some(frame) = self.frame.as_mut() {
            frame.draw_ms = frame.started.elapsed().as_secs_f64() * 1000.0;
        }
    }

    pub(crate) fn begin_present(&self) -> Option<Instant> {
        self.frame.as_ref().map(|_| Instant::now())
    }

    pub(crate) fn end_present(&mut self, started: Option<Instant>) {
        let Some(frame) = self.frame.take() else {
            return;
        };
        let submit_ms = started.map_or(0.0, |start| start.elapsed().as_secs_f64() * 1000.0);
        let input = frame.input;
        log::log!(target: "input_frame", level(),
            "stage=frame kind={} events={} dispatch_ms={:.3} queue_ms={:.3} layout_ms={:.3} paint_and_finish_ms={:.3} submit_ms={:.3} first_input_to_submit_ms={:.3} latest_input_to_submit_ms={:.3}",
            input.kind, input.events, input.dispatch_ms, (frame.started - input.latest_dispatched).as_secs_f64() * 1000.0,
            frame.layout_ms, (frame.draw_ms - frame.layout_ms).max(0.0), submit_ms,
            input.first.elapsed().as_secs_f64() * 1000.0, input.latest.elapsed().as_secs_f64() * 1000.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coalesced_frame_preserves_both_the_oldest_and_latest_input() {
        let now = Instant::now();
        let first = now - Duration::from_millis(3);
        let latest = now - Duration::from_millis(1);
        let mut profile = InputFrameProfile::default();
        profile.end_input(Some(first), "scroll", true);
        profile.end_input(Some(latest), "scroll", true);
        profile.begin_draw();
        let frame = profile.frame.as_ref().unwrap();
        assert_eq!(frame.input.first, first);
        assert_eq!(frame.input.latest, latest);
        assert_eq!(frame.input.events, 2);
        assert!(profile.pending.is_none());
        profile.end_draw();
        let present = profile.begin_present();
        profile.end_present(present);
        assert!(profile.frame.is_none());
    }

    #[test]
    fn inputs_without_visual_work_do_not_get_attributed_to_a_future_frame() {
        let mut profile = InputFrameProfile::default();
        profile.end_input(Some(Instant::now()), "tap", false);
        profile.end_input(None, "scroll", true);
        profile.begin_draw();
        assert!(profile.frame.is_none());
    }
}
