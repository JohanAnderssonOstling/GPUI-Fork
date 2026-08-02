use crate::PixelRect;

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
}
