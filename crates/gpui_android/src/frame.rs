use std::time::{Duration, Instant};

pub(crate) const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Coalesces frame demand without keeping an idle activity awake. Requests
/// made during a frame remain pending for the next frame.
#[derive(Default)]
pub(crate) struct FrameScheduler {
    pending: Option<bool>,
    last_frame: Option<Instant>,
    retry_not_before: Option<Instant>,
}

impl FrameScheduler {
    pub(crate) fn request(&mut self, force_render: bool) {
        self.pending = Some(self.pending.unwrap_or(false) || force_render);
    }

    pub(crate) fn defer_until(&mut self, deadline: Instant) {
        self.request(true);
        self.retry_not_before = Some(deadline);
    }

    pub(crate) fn deadline(&self, now: Instant) -> Option<Instant> {
        self.pending?;
        let deadline = self.last_frame.map_or(now, |last| last + FRAME_INTERVAL);
        Some(
            self.retry_not_before
                .map_or(deadline, |retry| retry.max(deadline)),
        )
    }

    pub(crate) fn take_due(&mut self, now: Instant) -> Option<bool> {
        if self.deadline(now)? > now {
            return None;
        }
        let force_render = self.pending.take()?;
        self.last_frame = Some(now);
        self.retry_not_before = None;
        Some(force_render)
    }
}

pub(crate) fn poll_timeout(
    now: Instant,
    frame_deadline: Option<Instant>,
    long_press_deadline: Option<Instant>,
) -> Option<Duration> {
    frame_deadline
        .into_iter()
        .chain(long_press_deadline)
        .min()
        .map(|deadline| {
            // Android's looper truncates its timeout to milliseconds. Round
            // up so the final fraction of a millisecond cannot busy-poll.
            let wait = deadline.saturating_duration_since(now);
            let millis = wait.as_nanos().div_ceil(1_000_000);
            Duration::from_millis(millis.try_into().unwrap_or(u64::MAX))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_activity_has_no_poll_deadline() {
        let now = Instant::now();
        let mut frames = FrameScheduler::default();
        assert_eq!(poll_timeout(now, frames.deadline(now), None), None);
        frames.request(false);
        assert_eq!(
            poll_timeout(now, frames.deadline(now), None),
            Some(Duration::ZERO)
        );
        assert_eq!(frames.take_due(now), Some(false));
        assert_eq!(poll_timeout(now, frames.deadline(now), None), None);
        assert_eq!(frames.take_due(now + FRAME_INTERVAL), None);
    }

    #[test]
    fn animation_requests_are_coalesced_and_rearmed_during_a_frame() {
        let now = Instant::now();
        let mut frames = FrameScheduler::default();
        frames.request(false);
        assert_eq!(frames.take_due(now), Some(false));
        // A next-frame callback can request another frame while drawing.
        frames.request(false);
        frames.request(false);
        assert_eq!(frames.take_due(now), None);
        assert_eq!(frames.deadline(now), Some(now + FRAME_INTERVAL));
        assert_eq!(frames.take_due(now + FRAME_INTERVAL), Some(false));
        assert_eq!(frames.deadline(now + FRAME_INTERVAL), None);
    }

    #[test]
    fn forced_redraw_survives_coalescing_and_surface_retry() {
        let now = Instant::now();
        let mut frames = FrameScheduler::default();
        frames.request(true);
        frames.request(false);
        assert_eq!(frames.take_due(now), Some(true));
        let retry = now + Duration::from_millis(100);
        frames.defer_until(retry);
        frames.request(false);
        assert_eq!(frames.deadline(now), Some(retry));
        assert_eq!(frames.take_due(retry - Duration::from_millis(1)), None);
        assert_eq!(frames.take_due(retry), Some(true));
        assert_eq!(frames.deadline(retry), None);
    }

    #[test]
    fn long_press_wakes_without_continuous_frames() {
        let now = Instant::now();
        let press = now + Duration::from_millis(500);
        assert_eq!(
            poll_timeout(now, None, Some(press)),
            Some(Duration::from_millis(500))
        );
        let frame = now + FRAME_INTERVAL;
        assert_eq!(
            poll_timeout(now, Some(frame), Some(press)),
            Some(FRAME_INTERVAL)
        );
        assert_eq!(poll_timeout(press, None, Some(press)), Some(Duration::ZERO));
        assert_eq!(poll_timeout(press, None, None), None);
    }

    #[test]
    fn submillisecond_deadlines_do_not_busy_poll() {
        let now = Instant::now();
        assert_eq!(
            poll_timeout(now, Some(now + Duration::from_micros(100)), None),
            Some(Duration::from_millis(1)),
        );
        assert_eq!(
            poll_timeout(now, Some(now + Duration::from_micros(1_100)), None),
            Some(Duration::from_millis(2)),
        );
    }
}
