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
    /// Flash only the damaged region with GC16, keeping monochrome pixels.
    CleanMono,
    /// Flash a damaged region that contains grayscale artwork.
    CleanGray,
}

impl RefreshMode {
    pub fn waveform(self) -> &'static str {
        match self {
            Self::FastMono => "A2",
            Self::TextMono => "DU",
            Self::PartialGray | Self::FullGray | Self::FullMono | Self::CleanMono | Self::CleanGray => "GC16",
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

    pub fn decide_for_mode(
        &mut self,
        render_mode: KoboRenderMode,
        _scrolling: bool,
        _damage: PixelRect,
        _screen_area: u64,
    ) -> RefreshMode {
        // Reader frames use their final grayscale waveform immediately, including
        // page turns. Neither fixed mode accumulates fast-preview cleanup debt.
        match render_mode {
            KoboRenderMode::FastMonochrome => RefreshMode::TextMono,
            KoboRenderMode::QualityGrayscale => RefreshMode::PartialGray,
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
    text_input_cleanup: Option<(PixelRect, Instant)>,
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
            text_input_cleanup: None,
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
        self.text_input_cleanup = None;
    }

    /// A completed full presentation supersedes any pending cleanup image.
    pub fn record_full_presentation(&mut self, image: &GrayFrame) {
        self.policy.mark_cleanup();
        self.last_fast_update = None;
        self.fast_damage = None;
        self.text_input_cleanup = None;
        self.last_presented = Some(image.clone());
        self.pending = None;
        self.pending_scrolling = false;
    }

    /// Records explicit monochrome presentation outside the normal scheduler.
    /// Reader quality mode never accrues delayed cleanup from these hints.
    pub fn record_fast_presentation(&mut self, update: &FrameUpdate, now: Instant) {
        self.last_presented = Some(update.image.clone());
        if self.render_mode == KoboRenderMode::QualityGrayscale {
            return;
        }
        self.policy.record_fast_update(update.damage);
        self.remember_fast_damage(update.damage, now);
    }

    fn remember_fast_damage(&mut self, damage: PixelRect, now: Instant) {
        self.last_fast_update = Some(now);
        self.fast_damage = Some(self.fast_damage.map_or(damage, |previous| union(previous, damage)));
    }

    /// Moving a solid caret repeatedly with DU can leave old caret marks on
    /// the panel even though the framebuffer is correct. Clean the affected
    /// area once typing pauses, using the latest frame rather than a snapshot.
    pub(crate) fn record_text_input_presentation(&mut self, update: &FrameUpdate, now: Instant) {
        if self.render_mode != KoboRenderMode::FastMonochrome {
            return;
        }
        let damage = self.text_input_cleanup.map_or(update.damage, |(previous, _)| union(previous, update.damage));
        self.text_input_cleanup = Some((damage, now));
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
        let mode = if requires_grayscale(&update) {
            // Use the final image waveform directly. This also clears old gray
            // artwork when a cover moves or disappears, without a DU preview.
            RefreshMode::PartialGray
        } else {
            self.policy.decide_for_mode(
                self.render_mode,
                scrolling,
                update.damage,
                u64::from(update.image.width()) * u64::from(update.image.height()),
            )
        };
        self.last_presented = Some(update.image.clone());
        if mode.is_fast_partial() {
            self.remember_fast_damage(update.damage, now);
        } else if mode.is_full() {
            self.last_fast_update = None;
            self.fast_damage = None;
            self.text_input_cleanup = None;
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

    pub(crate) fn idle_cleanup_deadline(&self) -> Option<Instant> {
        let fast = self.last_fast_update.filter(|_| self.policy.has_fast_updates());
        let text = self.text_input_cleanup.map(|(_, last)| last);
        fast.into_iter().chain(text).min().map(|last| last + self.profile.idle_cleanup_after)
    }

    pub fn idle_cleanup(&mut self, now: Instant) -> Option<ScheduledRefresh> {
        let image = self.last_presented.clone()?;
        let (pending, mode) = if self.text_input_cleanup.is_some_and(|(_, last)| now.saturating_duration_since(last) >= self.profile.idle_cleanup_after) {
            let (damage, _) = self.text_input_cleanup.take()?;
            (damage, RefreshMode::CleanMono)
        } else {
            let last_fast_update = self.last_fast_update?;
            if now.saturating_duration_since(last_fast_update) < self.profile.idle_cleanup_after
                || !self.policy.has_fast_updates()
            {
                return None;
            }
            self.last_fast_update = None;
            self.policy.mark_cleanup();
            let damage = self.fast_damage.take()?;
            let mode = if self.render_mode == KoboRenderMode::FastMonochrome {
                self.text_input_cleanup = None;
                RefreshMode::FullMono
            } else {
                RefreshMode::PartialGray
            };
            (damage, mode)
        };
        let left = pending.x.min(image.width());
        let top = pending.y.min(image.height());
        let damage = PixelRect {
            x: left, y: top,
            width: pending.x.saturating_add(pending.width).min(image.width()).saturating_sub(left),
            height: pending.y.saturating_add(pending.height).min(image.height()).saturating_sub(top),
        };
        if damage.width == 0 || damage.height == 0 { return None; }
        let update = FrameUpdate { image, previous_image: None, damage };
        let mode = if mode == RefreshMode::CleanMono && requires_grayscale(&update) { RefreshMode::CleanGray } else { mode };
        Some(ScheduledRefresh {
            update,
            mode,
            reason: RefreshReason::IdleCleanup,
        })
    }

    pub fn fast_updates(&self) -> u32 {
        self.policy.fast_updates()
    }
}

pub(crate) fn requires_grayscale(update: &FrameUpdate) -> bool {
    let damage = update.damage;
    if damage.width == 0 || damage.height == 0 {
        return false;
    }
    update.image.grayscale_regions().iter()
        .chain(update.previous_image.iter().flat_map(|frame| frame.grayscale_regions()))
        .any(|region| {
            region.width > 0 && region.height > 0
                && damage.x < region.x.saturating_add(region.width)
                && region.x < damage.x.saturating_add(damage.width)
                && damage.y < region.y.saturating_add(region.height)
                && region.y < damage.y.saturating_add(damage.height)
        })
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
    fn reader_page_turns_and_late_ui_updates_use_gc16_without_cleanup() {
        let mut scheduler = RepaintScheduler::new(DeviceRefreshProfile::default());
        let now = Instant::now();
        assert!(scheduler.idle_cleanup_deadline().is_none());
        let content = PixelRect { x: 10, y: 20, width: 70, height: 60 };
        for page in 0..12 {
            let time = now + Duration::from_secs(page);
            scheduler.enqueue(update(content, 120), true);
            let refresh = scheduler.flush(time).unwrap();
            assert_eq!(refresh.mode, RefreshMode::PartialGray);
            assert_eq!(refresh.mode.waveform(), "GC16");
            assert_eq!(refresh.update.image.get_pixel(15, 25).0[0], 120);
            assert!(scheduler.idle_cleanup_deadline().is_none());
            assert!(scheduler.idle_cleanup(time + Duration::from_millis(800)).is_none());
        }
        scheduler.enqueue(update(PixelRect { x: 0, y: 0, width: 5, height: 5 }, 160), false);
        assert_eq!(scheduler.flush(now + Duration::from_secs(12)).unwrap().mode, RefreshMode::PartialGray);
        assert_eq!(scheduler.fast_updates(), 0);
        assert!(scheduler.idle_cleanup(now + Duration::from_secs(20)).is_none());
        assert!(scheduler.idle_cleanup_deadline().is_none());
    }

    #[test]
    fn full_presentation_cancels_old_scroll_cleanup() {
        let mut scheduler = RepaintScheduler::new_with_render_mode(DeviceRefreshProfile::default(), KoboRenderMode::FastMonochrome);
        let now = Instant::now();
        let damage = PixelRect { x: 0, y: 0, width: 100, height: 100 };
        scheduler.record_fast_presentation(&update(damage, 128), now);
        scheduler.record_full_presentation(&update(damage, 255).image);
        assert!(scheduler.idle_cleanup(now + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn reader_page_hints_cannot_schedule_a_second_grayscale_refresh() {
        let mut scheduler = RepaintScheduler::new(DeviceRefreshProfile::default());
        let now = Instant::now();
        assert!(scheduler.idle_cleanup_deadline().is_none());
        let content = PixelRect { x: 0, y: 10, width: 100, height: 90 };
        scheduler.record_fast_presentation(&update(content, 128), now);
        assert_eq!(scheduler.fast_updates(), 0);
        assert!(scheduler.idle_cleanup_deadline().is_none());
        assert!(scheduler.idle_cleanup(now + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn library_updates_keep_du_without_reader_cleanup() {
        let mut scheduler = RepaintScheduler::new_with_render_mode(DeviceRefreshProfile::default(), KoboRenderMode::FastMonochrome);
        let now = Instant::now();
        for scrolling in [false, true] {
            scheduler.enqueue(update(PixelRect { x: 0, y: 0, width: 100, height: 100 }, 128), scrolling);
            assert_eq!(scheduler.flush(now).unwrap().mode, RefreshMode::TextMono);
        }
        assert!(scheduler.idle_cleanup_deadline().is_none());
        assert!(scheduler.idle_cleanup(now + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn typing_cleans_all_old_caret_positions_once_after_the_last_key() {
        let mut scheduler = RepaintScheduler::new_with_render_mode(DeviceRefreshProfile::default(), KoboRenderMode::FastMonochrome);
        let now = Instant::now();
        for (delay, x) in [(0, 10), (400, 20)] {
            let time = now + Duration::from_millis(delay);
            scheduler.enqueue(update(PixelRect { x, y: 10, width: 3, height: 10 }, 0), false);
            let refresh = scheduler.flush(time).unwrap();
            assert_eq!(refresh.mode, RefreshMode::TextMono);
            scheduler.record_text_input_presentation(&refresh.update, time);
        }
        assert_eq!(scheduler.idle_cleanup_deadline(), Some(now + Duration::from_millis(1200)));
        assert!(scheduler.idle_cleanup(now + Duration::from_millis(800)).is_none());
        // An unrelated update must be retained, without extending the typing delay.
        scheduler.enqueue(update(PixelRect { x: 80, y: 80, width: 5, height: 5 }, 77), false);
        scheduler.flush(now + Duration::from_millis(900)).unwrap();
        let cleanup = scheduler.idle_cleanup(now + Duration::from_millis(1200)).unwrap();
        assert_eq!(cleanup.mode, RefreshMode::CleanMono);
        assert_eq!(cleanup.mode.waveform(), "GC16");
        assert!(!cleanup.mode.is_full());
        assert_eq!(cleanup.update.damage, PixelRect { x: 10, y: 10, width: 13, height: 10 });
        assert_eq!(cleanup.update.image.get_pixel(10, 10).0[0], 77);
        assert!(cleanup.update.previous_image.is_none(), "cleanup must repaint even unchanged framebuffer pixels");
        assert!(scheduler.idle_cleanup_deadline().is_none());
        assert!(scheduler.idle_cleanup(now + Duration::from_secs(5)).is_none());
    }

    #[test]
    fn full_refresh_or_mode_change_cancels_pending_typing_cleanup() {
        let now = Instant::now();
        let frame = update(PixelRect { x: 10, y: 10, width: 20, height: 10 }, 0);
        for change_mode in [false, true] {
            let mut scheduler = RepaintScheduler::new_with_render_mode(DeviceRefreshProfile::default(), KoboRenderMode::FastMonochrome);
            scheduler.enqueue(update(frame.damage, 0), false);
            scheduler.flush(now).unwrap();
            scheduler.record_text_input_presentation(&frame, now);
            if change_mode {
                scheduler.set_render_mode(KoboRenderMode::QualityGrayscale);
            } else {
                scheduler.record_full_presentation(&frame.image);
            }
            assert!(scheduler.idle_cleanup_deadline().is_none());
            assert!(scheduler.idle_cleanup(now + Duration::from_secs(1)).is_none());
        }
    }

    #[test]
    fn typing_cleanup_preserves_artwork_between_changed_regions() {
        let mut scheduler = RepaintScheduler::new_with_render_mode(DeviceRefreshProfile::default(), KoboRenderMode::FastMonochrome);
        let now = Instant::now();
        let image = GrayFrame::unpooled_with_grayscale_regions(GrayImage::from_pixel(100, 100, Luma([120])), vec![PixelRect { x: 20, y: 20, width: 60, height: 60 }]);
        for y in [5, 90] {
            let frame = FrameUpdate { image: image.clone(), previous_image: None, damage: PixelRect { x: 30, y, width: 10, height: 5 } };
            scheduler.enqueue(FrameUpdate { image: image.clone(), previous_image: None, damage: frame.damage }, false);
            scheduler.flush(now).unwrap();
            scheduler.record_text_input_presentation(&frame, now);
        }
        let cleanup = scheduler.idle_cleanup(now + Duration::from_secs(1)).unwrap();
        assert_eq!(cleanup.mode, RefreshMode::CleanGray);
        assert!(!cleanup.mode.is_full());
    }

    #[test]
    fn library_covers_use_gc16_while_updates_outside_artwork_keep_du() {
        let mut scheduler = RepaintScheduler::new_with_render_mode(DeviceRefreshProfile::default(), KoboRenderMode::FastMonochrome);
        let now = Instant::now();
        let cover = PixelRect { x: 20, y: 20, width: 60, height: 70 };
        let image = GrayFrame::unpooled_with_grayscale_regions(GrayImage::from_pixel(100, 100, Luma([120])), vec![cover]);
        for scrolling in [false, true] {
            scheduler.enqueue(FrameUpdate { image: image.clone(), previous_image: None, damage: cover }, scrolling);
            let refresh = scheduler.flush(now).unwrap();
            assert_eq!(refresh.mode, RefreshMode::PartialGray);
            assert_eq!(refresh.mode.waveform(), "GC16");
            assert_eq!(refresh.update.image.get_pixel(30, 30).0[0], 120);
        }
        let header = PixelRect { x: 0, y: 0, width: 100, height: 20 };
        scheduler.enqueue(FrameUpdate { image, previous_image: None, damage: header }, false);
        assert_eq!(scheduler.flush(now).unwrap().mode, RefreshMode::TextMono);
        assert_eq!(scheduler.fast_updates(), 0);
        assert!(scheduler.idle_cleanup_deadline().is_none());
        assert!(scheduler.idle_cleanup(now + Duration::from_secs(2)).is_none());
    }

    #[test]
    fn grayscale_only_cover_changes_are_not_sent_through_monochrome_damage_filtering() {
        let mut scheduler = RepaintScheduler::new_with_render_mode(DeviceRefreshProfile::default(), KoboRenderMode::FastMonochrome);
        let cover = PixelRect { x: 20, y: 20, width: 60, height: 70 };
        let before = GrayFrame::unpooled_with_grayscale_regions(GrayImage::from_pixel(100, 100, Luma([100])), vec![cover]);
        let after = GrayFrame::unpooled_with_grayscale_regions(GrayImage::from_pixel(100, 100, Luma([120])), vec![cover]);
        scheduler.enqueue(FrameUpdate { image: after, previous_image: Some(before), damage: cover }, false);
        let refresh = scheduler.flush(Instant::now()).unwrap();
        assert_eq!(refresh.mode, RefreshMode::PartialGray);
        assert_eq!(refresh.update.previous_image.unwrap().get_pixel(30, 30).0[0], 100);
        assert_eq!(refresh.update.image.get_pixel(30, 30).0[0], 120);
    }

    #[test]
    fn coalesced_cover_removal_keeps_the_old_artwork_area_on_gc16() {
        let mut scheduler = RepaintScheduler::new_with_render_mode(DeviceRefreshProfile::default(), KoboRenderMode::FastMonochrome);
        let cover = PixelRect { x: 20, y: 20, width: 60, height: 70 };
        let before = GrayFrame::unpooled_with_grayscale_regions(GrayImage::from_pixel(100, 100, Luma([120])), vec![cover]);
        scheduler.enqueue(FrameUpdate { image: GrayFrame::unpooled(GrayImage::from_pixel(100, 100, Luma([255]))), previous_image: Some(before), damage: cover }, true);
        scheduler.enqueue(update(PixelRect { x: 0, y: 0, width: 100, height: 10 }, 255), false);
        let refresh = scheduler.flush(Instant::now()).unwrap();
        assert!(refresh.update.image.grayscale_regions().is_empty());
        assert_eq!(refresh.mode, RefreshMode::PartialGray);
        assert!(scheduler.idle_cleanup_deadline().is_none());
    }
}
