use std::time::{Duration, Instant};

use image::RgbaImage;

use crate::{FrameUpdate, PixelRect};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshMode {
    FastMono,
    PartialGray,
    FullGray,
}

impl RefreshMode {
    pub fn waveform(self) -> &'static str {
        match self {
            Self::FastMono => "A2",
            Self::PartialGray | Self::FullGray => "GC16",
        }
    }
}

pub struct RefreshPolicy {
    fast_updates: u32,
    accumulated_damage: u64,
    cleanup_after_fast_updates: u32,
    cleanup_after_screen_areas: u64,
}

impl Default for RefreshPolicy {
    fn default() -> Self {
        Self {
            fast_updates: 0,
            accumulated_damage: 0,
            cleanup_after_fast_updates: 6,
            cleanup_after_screen_areas: 3,
        }
    }
}

impl RefreshPolicy {
    pub fn with_limits(fast_updates: u32, screen_areas: u64) -> Self {
        Self {
            fast_updates: 0,
            accumulated_damage: 0,
            cleanup_after_fast_updates: fast_updates.max(1),
            cleanup_after_screen_areas: screen_areas.max(1),
        }
    }

    pub fn decide(&mut self, scrolling: bool, damage: PixelRect, screen_area: u64) -> RefreshMode {
        if !scrolling {
            return RefreshMode::PartialGray;
        }

        self.fast_updates = self.fast_updates.saturating_add(1);
        self.accumulated_damage = self
            .accumulated_damage
            .saturating_add(u64::from(damage.width) * u64::from(damage.height));
        if self.fast_updates >= self.cleanup_after_fast_updates
            || self.accumulated_damage
                >= screen_area.saturating_mul(self.cleanup_after_screen_areas)
        {
            self.fast_updates = 0;
            self.accumulated_damage = 0;
            RefreshMode::FullGray
        } else {
            RefreshMode::FastMono
        }
    }

    pub fn fast_updates(&self) -> u32 {
        self.fast_updates
    }

    pub fn mark_cleanup(&mut self) {
        self.fast_updates = 0;
        self.accumulated_damage = 0;
    }

    pub fn has_fast_updates(&self) -> bool {
        self.fast_updates > 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceRefreshProfile {
    pub cleanup_after_fast_updates: u32,
    pub cleanup_after_screen_areas: u64,
    pub idle_cleanup_after: Duration,
}

impl DeviceRefreshProfile {
    pub fn for_device(codename: &str) -> Self {
        match codename.to_ascii_lowercase().as_str() {
            "trilogy" | "pixie" | "kraken" => Self {
                cleanup_after_fast_updates: 3,
                cleanup_after_screen_areas: 2,
                idle_cleanup_after: Duration::from_millis(500),
            },
            _ => Self::default(),
        }
    }
}

impl Default for DeviceRefreshProfile {
    fn default() -> Self {
        Self {
            cleanup_after_fast_updates: 6,
            cleanup_after_screen_areas: 3,
            idle_cleanup_after: Duration::from_millis(800),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshReason {
    ReleasedGesture,
    CoalescedInvalidation,
    IdleCleanup,
}

pub struct ScheduledRefresh {
    pub update: FrameUpdate,
    pub mode: RefreshMode,
    pub reason: RefreshReason,
}

/// Coalesces GPUI frame updates and schedules waveform cleanup independently
/// from the application view.
pub struct RepaintScheduler {
    policy: RefreshPolicy,
    profile: DeviceRefreshProfile,
    pending: Option<FrameUpdate>,
    pending_scrolling: bool,
    last_presented: Option<RgbaImage>,
    last_fast_update: Option<Instant>,
}

impl RepaintScheduler {
    pub fn new(profile: DeviceRefreshProfile) -> Self {
        Self {
            policy: RefreshPolicy::with_limits(
                profile.cleanup_after_fast_updates,
                profile.cleanup_after_screen_areas,
            ),
            profile,
            pending: None,
            pending_scrolling: false,
            last_presented: None,
            last_fast_update: None,
        }
    }

    pub fn enqueue(&mut self, update: FrameUpdate, scrolling: bool) {
        self.pending_scrolling |= scrolling;
        self.pending = Some(match self.pending.take() {
            Some(previous) => FrameUpdate {
                image: update.image,
                damage: union(previous.damage, update.damage),
            },
            None => update,
        });
    }

    pub fn flush(&mut self, now: Instant) -> Option<ScheduledRefresh> {
        let update = self.pending.take()?;
        let scrolling = std::mem::take(&mut self.pending_scrolling);
        let mode = self.policy.decide(
            scrolling,
            update.damage,
            u64::from(update.image.width()) * u64::from(update.image.height()),
        );
        self.last_presented = Some(update.image.clone());
        self.last_fast_update = (mode == RefreshMode::FastMono).then_some(now);
        Some(ScheduledRefresh {
            update,
            mode,
            reason: if scrolling {
                RefreshReason::ReleasedGesture
            } else {
                RefreshReason::CoalescedInvalidation
            },
        })
    }

    pub fn idle_cleanup(&mut self, now: Instant) -> Option<ScheduledRefresh> {
        let last_fast_update = self.last_fast_update?;
        if now.saturating_duration_since(last_fast_update) < self.profile.idle_cleanup_after
            || !self.policy.has_fast_updates()
        {
            return None;
        }
        let image = self.last_presented.clone()?;
        self.last_fast_update = None;
        self.policy.mark_cleanup();
        let damage = PixelRect { x: 0, y: 0, width: image.width(), height: image.height() };
        Some(ScheduledRefresh {
            update: FrameUpdate { image, damage },
            mode: RefreshMode::FullGray,
            reason: RefreshReason::IdleCleanup,
        })
    }

    pub fn fast_updates(&self) -> u32 {
        self.policy.fast_updates()
    }
}

fn union(left: PixelRect, right: PixelRect) -> PixelRect {
    let x = left.x.min(right.x);
    let y = left.y.min(right.y);
    let right_edge = (left.x + left.width).max(right.x + right.width);
    let bottom_edge = (left.y + left.height).max(right.y + right.height);
    PixelRect { x, y, width: right_edge - x, height: bottom_edge - y }
}
