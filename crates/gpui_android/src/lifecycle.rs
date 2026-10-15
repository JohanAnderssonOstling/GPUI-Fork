use std::time::{Duration, Instant};

const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(16);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(1);

pub(crate) struct SurfaceLifecycle<Window> {
    window: Option<Window>,
    ready: bool,
    retry_not_before: Option<Instant>,
    retry_delay: Duration,
}

impl<Window> SurfaceLifecycle<Window> {
    pub(crate) fn new(window: Window) -> Self {
        Self {
            window: Some(window),
            ready: true,
            retry_not_before: None,
            retry_delay: INITIAL_RETRY_DELAY,
        }
    }

    pub(crate) fn window(&self) -> Option<&Window> {
        self.window.as_ref()
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready
    }

    pub(crate) fn retry_deadline(&self) -> Option<Instant> {
        self.retry_not_before
    }

    pub(crate) fn replace_window(&mut self, window: Window) {
        self.window = Some(window);
        self.invalidate();
    }

    pub(crate) fn invalidate(&mut self) {
        self.ready = false;
        self.retry_not_before = None;
        self.retry_delay = INITIAL_RETRY_DELAY;
    }

    pub(crate) fn clear(&mut self) {
        self.window = None;
        self.invalidate();
    }

    pub(crate) fn try_configure<Error>(
        &mut self,
        now: Instant,
        configure: impl FnOnce(&Window) -> Result<(), Error>,
    ) -> Result<bool, Error> {
        if self.ready || self.retry_not_before.is_some_and(|deadline| now < deadline) {
            return Ok(false);
        }
        let Some(window) = self.window.as_ref() else {
            return Ok(false);
        };
        match configure(window) {
            Ok(()) => {
                self.ready = true;
                self.retry_not_before = None;
                self.retry_delay = INITIAL_RETRY_DELAY;
                Ok(true)
            }
            Err(error) => {
                self.retry_not_before = Some(now + self.retry_delay);
                self.retry_delay = (self.retry_delay * 2).min(MAX_RETRY_DELAY);
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[test]
    fn failed_creation_retains_the_window_and_retries_until_ready() {
        let now = Instant::now();
        let mut surface = SurfaceLifecycle::new(1);
        surface.replace_window(2);
        let mut attempts = 0;
        let mut configure = |window: &usize| {
            assert_eq!(*window, 2);
            attempts += 1;
            if attempts < 3 {
                Err("transient failure")
            } else {
                Ok(())
            }
        };
        assert!(surface.try_configure(now, &mut configure).is_err());
        assert_eq!(surface.retry_deadline(), Some(now + INITIAL_RETRY_DELAY));
        assert_eq!(surface.window(), Some(&2));
        assert!(!surface.is_ready());
        assert_eq!(surface.try_configure(now, &mut configure), Ok(false));
        assert!(
            surface
                .try_configure(now + INITIAL_RETRY_DELAY, &mut configure)
                .is_err()
        );
        assert_eq!(
            surface.try_configure(now + INITIAL_RETRY_DELAY * 3, &mut configure),
            Ok(true)
        );
        assert!(surface.is_ready());
        assert_eq!(surface.retry_deadline(), None);
        assert_eq!(
            surface.try_configure(now + MAX_RETRY_DELAY, &mut configure),
            Ok(false)
        );
        assert_eq!(attempts, 3);
    }

    #[test]
    fn termination_cancels_retries_and_releases_the_window() {
        let now = Instant::now();
        let window = Rc::new(());
        let weak_window = Rc::downgrade(&window);
        let mut surface = SurfaceLifecycle::new(window);
        surface.invalidate();
        assert!(surface.try_configure(now, |_| Err("failure")).is_err());
        surface.clear();
        assert_eq!(surface.retry_deadline(), None);
        assert!(weak_window.upgrade().is_none());
        assert_eq!(
            surface.try_configure(now + MAX_RETRY_DELAY, |_| -> Result<(), ()> {
                panic!("a destroyed window must never be configured");
            }),
            Ok(false)
        );
        surface.replace_window(Rc::new(()));
        assert_eq!(surface.try_configure(now, |_| Ok::<_, ()>(())), Ok(true));
    }

    #[test]
    fn retry_delay_is_bounded_and_a_new_window_retries_immediately() {
        let mut now = Instant::now();
        let mut surface = SurfaceLifecycle::new(1);
        surface.invalidate();
        for _ in 0..20 {
            assert!(surface.try_configure(now, |_| Err("failure")).is_err());
            let deadline = surface
                .retry_not_before
                .expect("failed creation schedules a retry");
            assert!(deadline.duration_since(now) <= MAX_RETRY_DELAY);
            now = deadline;
        }
        surface.replace_window(2);
        assert_eq!(surface.try_configure(now, |_| Ok::<_, ()>(())), Ok(true));
    }
}
