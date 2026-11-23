//! Optional wall-clock timings for application stages.
//!
//! Each line also carries `t`, the time it was logged relative to the first
//! profiled stage, so events across different targets share a timeline,
//! and `rss`, the process's resident memory where the platform reports it.
use std::sync::OnceLock;
use web_time::Instant;

fn since_first_stage() -> f64 {
    static FIRST: OnceLock<Instant> = OnceLock::new();
    FIRST.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}

/// Resident memory from `/proc/self/statm`, counted in 4 KiB pages as on
/// every device this runs on.
#[cfg(target_os = "linux")]
fn resident() -> String {
    let pages = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|statm| statm.split_whitespace().nth(1)?.parse::<u64>().ok());
    pages
        .map(|pages| format!(" rss={} KiB", pages * 4))
        .unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
fn resident() -> String {
    String::new()
}

/// A wall-clock stage that remains inactive unless its log target enables debug output.
pub struct Stage {
    target: &'static str,
    name: &'static str,
    start: Option<Instant>,
    log_on_drop: bool,
}

impl Stage {
    /// Starts a stage which logs elapsed time when dropped.
    pub fn new(target: &'static str, name: &'static str) -> Self {
        let start = log::log_enabled!(target: target, log::Level::Debug).then(|| {
            since_first_stage();
            Instant::now()
        });
        Self {
            target,
            name,
            start,
            log_on_drop: true,
        }
    }

    /// Starts a stage which logs only when explicitly finished.
    pub fn deferred(target: &'static str, name: &'static str) -> Self {
        let mut stage = Self::new(target, name);
        stage.log_on_drop = false;
        stage
    }

    /// Logs elapsed time and a completion outcome once.
    pub fn finish(mut self, outcome: &'static str) {
        if let Some(start) = self.start.take() {
            log::debug!(target: self.target, "{} outcome={} {:.3} ms t={:.1} ms{}", self.name, outcome, start.elapsed().as_secs_f64() * 1000.0, since_first_stage(), resident());
        }
    }

    /// Discards the stage without logging a completion.
    pub fn cancel(mut self) {
        self.start = None;
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if let Some(start) = self.start.filter(|_| self.log_on_drop) {
            log::debug!(target: self.target, "{} {:.3} ms t={:.1} ms{}", self.name, start.elapsed().as_secs_f64() * 1000.0, since_first_stage(), resident());
        }
    }
}
