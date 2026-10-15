use scheduler::Instant;
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static ORIGIN: OnceLock<Instant> = OnceLock::new();

/// Opt-in startup stages. Nested scope durations overlap their parents.
pub struct StartupProfile {
    scope: &'static str,
    started: Option<Instant>,
    previous: Option<Instant>,
}

impl StartupProfile {
    /// Enables info-level timing for hosts that cannot configure log filters.
    pub fn enable() {
        ENABLED.store(true, Ordering::Relaxed);
    }

    /// Starts a scope when enabled explicitly or through the startup debug log filter.
    pub fn new(scope: &'static str) -> Self {
        let enabled = ENABLED.load(Ordering::Relaxed)
            || log::log_enabled!(target: "startup_profile", log::Level::Debug);
        let started = enabled.then(Instant::now);
        if let Some(started) = started {
            ORIGIN.get_or_init(|| started);
        }
        Self {
            scope,
            started,
            previous: started,
        }
    }

    /// Logs the time since the preceding stage, or since the scope began.
    pub fn mark(&mut self, stage: &str) {
        if let (Some(started), Some(previous), Some(origin)) =
            (self.started, self.previous, ORIGIN.get())
        {
            let now = Instant::now();
            self.previous = Some(now);
            let level = if ENABLED.load(Ordering::Relaxed) {
                log::Level::Info
            } else {
                log::Level::Debug
            };
            log::log!(target: "startup_profile", level,
                "scope={} stage={} duration_ms={:.3} elapsed_ms={:.3} since_start_ms={:.3}",
                self.scope, stage,
                (now - previous).as_secs_f64() * 1000.0,
                (now - started).as_secs_f64() * 1000.0,
                (now - *origin).as_secs_f64() * 1000.0);
        }
    }
}
