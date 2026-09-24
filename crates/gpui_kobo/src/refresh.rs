use std::time::{Duration, Instant};

use crate::{FrameUpdate, GrayFrame, PixelRect};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum KoboRenderMode {
    FastMonochrome,
    #[default]
    QualityGrayscale,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshMode {
    FastMono,
    TextMono,
    PartialGray,
    FullGray,
    FullMono,
}

impl RefreshMode {
    pub fn waveform(self) -> &'static str {
        match self {
            Self::FastMono => "A2",
            Self::TextMono => "DU",
            Self::PartialGray | Self::FullGray | Self::FullMono => "GC16",
        }
    }

    pub fn is_full(self) -> bool {
        matches!(self, Self::FullGray | Self::FullMono)
    }

    fn is_fast_partial(self) -> bool {
        matches!(self, Self::FastMono)
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
        self.decide_for_mode(
            KoboRenderMode::QualityGrayscale,
            scrolling,
            damage,
            screen_area,
        )
    }

    pub fn decide_for_mode(
        &mut self,
        render_mode: KoboRenderMode,
        scrolling: bool,
        damage: PixelRect,
        screen_area: u64,
    ) -> RefreshMode {
        if render_mode == KoboRenderMode::FastMonochrome {
            // Library mode remains thresholded monochrome, but DU avoids the
            // severe ghosting produced by repeated A2 navigation updates.
            // Do not accumulate cleanup debt or schedule automatic GC16.
            return RefreshMode::TextMono;
        }

        if !scrolling {
            return RefreshMode::PartialGray;
        }

        self.record_fast_update(damage);
        if self.cleanup_due(screen_area) {
            self.mark_cleanup();
            RefreshMode::FullGray
        } else {
            RefreshMode::FastMono
        }
    }

    fn record_fast_update(&mut self, damage: PixelRect) {
        self.fast_updates = self.fast_updates.saturating_add(1);
        self.accumulated_damage = self
            .accumulated_damage
            .saturating_add(u64::from(damage.width) * u64::from(damage.height));
    }

    fn cleanup_due(&self, screen_area: u64) -> bool {
        self.fast_updates >= self.cleanup_after_fast_updates
            || self.accumulated_damage
                >= screen_area.saturating_mul(self.cleanup_after_screen_areas)
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
    render_mode: KoboRenderMode,
    pending: Option<FrameUpdate>,
    pending_scrolling: bool,
    last_presented: Option<GrayFrame>,
    last_fast_update: Option<Instant>,
    fast_damage: Option<PixelRect>,
}

impl RepaintScheduler {
    pub fn new(profile: DeviceRefreshProfile) -> Self {
        Self::new_with_render_mode(profile, KoboRenderMode::QualityGrayscale)
    }

    pub fn new_with_render_mode(
        profile: DeviceRefreshProfile,
        render_mode: KoboRenderMode,
    ) -> Self {
        Self {
            policy: RefreshPolicy::with_limits(
                profile.cleanup_after_fast_updates,
                profile.cleanup_after_screen_areas,
            ),
            profile,
            render_mode,
            pending: None,
            pending_scrolling: false,
            last_presented: None,
            last_fast_update: None,
            fast_damage: None,
        }
    }

    pub fn set_render_mode(&mut self, render_mode: KoboRenderMode) {
        if self.render_mode == render_mode {
            return;
        }
        self.render_mode = render_mode;
        self.policy = RefreshPolicy::with_limits(
            self.profile.cleanup_after_fast_updates,
            self.profile.cleanup_after_screen_areas,
        );
        self.pending = None;
        self.pending_scrolling = false;
        self.last_presented = None;
        self.last_fast_update = None;
        self.fast_damage = None;
    }

    /// A completed full presentation supersedes any pending cleanup image.
    pub fn record_full_presentation(&mut self, image: &GrayFrame) {
        self.policy.mark_cleanup();
        self.last_fast_update = None;
        self.fast_damage = None;
        self.last_presented = Some(image.clone());
        self.pending = None;
        self.pending_scrolling = false;
    }

    /// Records monochrome presentation performed outside the normal scheduler,
    /// such as the reader's fast page-turn path.
    pub fn record_fast_presentation(&mut self, update: &FrameUpdate, now: Instant) {
        self.policy.record_fast_update(update.damage);
        self.remember_fast_damage(update.damage, now);
        self.last_presented = Some(update.image.clone());
    }

    fn remember_fast_damage(&mut self, damage: PixelRect, now: Instant) {
        self.last_fast_update = Some(now);
        self.fast_damage = Some(self.fast_damage.map_or(damage, |previous| union(previous, damage)));
    }

    pub fn enqueue(&mut self, update: FrameUpdate, scrolling: bool) {
        self.pending_scrolling |= scrolling;
        self.pending = Some(match self.pending.take() {
            Some(previous) => FrameUpdate {
                image: update.image,
                previous_image: previous.previous_image,
                damage: union(previous.damage, update.damage),
            },
            None => update,
        });
    }

    pub fn flush(&mut self, now: Instant) -> Option<ScheduledRefresh> {
        let update = self.pending.take()?;
        let scrolling = std::mem::take(&mut self.pending_scrolling);
        let mode = self.policy.decide_for_mode(
            self.render_mode,
            scrolling,
            update.damage,
            u64::from(update.image.width()) * u64::from(update.image.height()),
        );
        self.last_presented = Some(update.image.clone());
        if mode.is_fast_partial() {
            self.remember_fast_damage(update.damage, now);
        } else if mode.is_full() {
            self.last_fast_update = None;
            self.fast_damage = None;
        }
        // A small grayscale UI update must not cancel outstanding scroll cleanup.
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
        let pending = self.fast_damage.take()?;
        let left = pending.x.min(image.width());
        let top = pending.y.min(image.height());
        let damage = PixelRect {
            x: left, y: top,
            width: pending.x.saturating_add(pending.width).min(image.width()).saturating_sub(left),
            height: pending.y.saturating_add(pending.height).min(image.height()).saturating_sub(top),
        };
        if damage.width == 0 || damage.height == 0 { return None; }
        Some(ScheduledRefresh {
            update: FrameUpdate {
                image,
                previous_image: None,
                damage,
            },
            mode: if self.render_mode == KoboRenderMode::FastMonochrome {
                RefreshMode::FullMono
            } else {
                RefreshMode::PartialGray
            },
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
    PixelRect {
        x,
        y,
        width: right_edge - x,
        height: bottom_edge - y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Luma};

    fn update(damage: PixelRect, gray: u8) -> FrameUpdate {
        FrameUpdate { image: GrayFrame::unpooled(GrayImage::from_pixel(100, 100, Luma([gray]))), previous_image: None, damage }
    }

    #[test]
    fn scroll_cleanup_survives_small_ui_updates_and_uses_latest_grayscale_frame() {
        let mut scheduler = RepaintScheduler::new(DeviceRefreshProfile::default());
        let now = Instant::now();
        let content = PixelRect { x: 10, y: 20, width: 70, height: 60 };
        scheduler.enqueue(update(content, 120), true);
        assert_eq!(scheduler.flush(now).unwrap().mode, RefreshMode::FastMono);
        scheduler.enqueue(update(PixelRect { x: 0, y: 0, width: 5, height: 5 }, 160), false);
        assert_eq!(scheduler.flush(now + Duration::from_millis(100)).unwrap().mode, RefreshMode::PartialGray);
        assert!(scheduler.idle_cleanup(now + Duration::from_millis(799)).is_none());
        let cleanup = scheduler.idle_cleanup(now + Duration::from_millis(800)).unwrap();
        assert_eq!(cleanup.mode, RefreshMode::PartialGray);
        assert_eq!(cleanup.update.damage, content);
        assert_eq!(cleanup.update.image.get_pixel(15, 25).0[0], 160);
        assert!(cleanup.update.previous_image.is_none());
        assert!(scheduler.idle_cleanup(now + Duration::from_secs(2)).is_none());
    }

    #[test]
    fn full_presentation_cancels_old_scroll_cleanup() {
        let mut scheduler = RepaintScheduler::new(DeviceRefreshProfile::default());
        let now = Instant::now();
        let damage = PixelRect { x: 0, y: 0, width: 100, height: 100 };
        scheduler.record_fast_presentation(&update(damage, 128), now);
        scheduler.record_full_presentation(&update(damage, 255).image);
        assert!(scheduler.idle_cleanup(now + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn fast_reader_page_turns_receive_grayscale_cleanup() {
        let mut scheduler = RepaintScheduler::new(DeviceRefreshProfile::default());
        let now = Instant::now();
        let content = PixelRect { x: 0, y: 10, width: 100, height: 90 };
        scheduler.record_fast_presentation(&update(content, 128), now);
        let cleanup = scheduler.idle_cleanup(now + Duration::from_secs(1)).unwrap();
        assert_eq!(cleanup.mode, RefreshMode::PartialGray);
        assert_eq!(cleanup.update.damage, content);
    }
}
