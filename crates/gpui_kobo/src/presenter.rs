use anyhow::{Context as _, Result, bail};
use image::RgbaImage;

use crate::{CANVAS_HEIGHT, CANVAS_WIDTH, FrameUpdate, PixelRect, RefreshMode, ScreenGeometry};

#[cfg(target_arch = "arm")]
mod native {
    use std::ffi::c_char;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct State {
        pub view_width: u32,
        pub view_height: u32,
        pub screen_width: u32,
        pub screen_height: u32,
        pub view_x: u32,
        pub view_y: u32,
        pub current_rotation: u8,
        pub can_rotate: u8,
        pub is_sunxi: u8,
        pub reserved: u8,
        pub device_name: [c_char; 16],
        pub device_codename: [c_char; 16],
        pub device_platform: [c_char; 16],
    }

    unsafe extern "C" {
        pub fn gpui_fbink_open(state: *mut State) -> i32;
        pub fn gpui_fbink_reinit(fd: i32, state: *mut State) -> i32;
        pub fn gpui_fbink_present_gray(
            fd: i32,
            data: *mut u8,
            source_width: i32,
            source_height: i32,
            length: usize,
            target_x: i32,
            target_y: i32,
            target_width: i32,
            target_height: i32,
            refresh_mode: i32,
        ) -> i32;
        pub fn gpui_fbink_close(fd: i32) -> i32;
    }
}

/// A persistent, in-process FBInk connection. It avoids temporary image files
/// and process creation for every e-ink update.
pub struct FbInkPresenter {
    #[cfg(target_arch = "arm")]
    fd: i32,
    geometry: ScreenGeometry,
    presentations: u64,
}

impl FbInkPresenter {
    #[cfg(target_arch = "arm")]
    pub fn open() -> Result<Self> {
        let mut state = native::State::default();
        let fd = unsafe { native::gpui_fbink_open(&mut state) };
        if fd < 0 {
            bail!("fbink_open/init failed with {fd}");
        }
        Ok(Self {
            fd,
            geometry: geometry_from_native(&state),
            presentations: 0,
        })
    }

    #[cfg(not(target_arch = "arm"))]
    pub fn open() -> Result<Self> {
        bail!("native FBInk presentation is only available in ARM Kobo builds")
    }

    pub fn geometry(&self) -> &ScreenGeometry {
        &self.geometry
    }

    pub fn presentations(&self) -> u64 {
        self.presentations
    }

    #[cfg(target_arch = "arm")]
    pub fn reinitialize(&mut self) -> Result<bool> {
        let previous = self.geometry.clone();
        let mut state = native::State::default();
        let result = unsafe { native::gpui_fbink_reinit(self.fd, &mut state) };
        if result < 0 {
            bail!("fbink_reinit failed with {result}");
        }
        self.geometry = geometry_from_native(&state);
        Ok(result > 0 || self.geometry != previous)
    }

    #[cfg(not(target_arch = "arm"))]
    pub fn reinitialize(&mut self) -> Result<bool> {
        Ok(false)
    }

    pub fn present_full(&mut self, image: &RgbaImage) -> Result<()> {
        self.present_pixels(
            image,
            PixelRect { x: 0, y: 0, width: CANVAS_WIDTH, height: CANVAS_HEIGHT },
            RefreshMode::FullGray,
        )
    }

    pub fn present_update(&mut self, update: &FrameUpdate, mode: RefreshMode) -> Result<()> {
        let damage = if mode == RefreshMode::FullGray {
            PixelRect { x: 0, y: 0, width: CANVAS_WIDTH, height: CANVAS_HEIGHT }
        } else {
            update.damage
        };
        self.present_pixels(&update.image, damage, mode)
    }

    #[cfg(target_arch = "arm")]
    fn present_pixels(&mut self, image: &RgbaImage, damage: PixelRect, mode: RefreshMode) -> Result<()> {
        let target = self.geometry.scale_damage(damage, CANVAS_WIDTH, CANVAS_HEIGHT);
        let capacity = usize::try_from(damage.width.saturating_mul(damage.height))?;
        let mut pixels = Vec::with_capacity(capacity);
        for y in damage.y..damage.y + damage.height {
            for x in damage.x..damage.x + damage.width {
                pixels.push(image.get_pixel(x, y).0[0]);
            }
        }
        let mode_number = match mode {
            RefreshMode::FastMono => 0,
            RefreshMode::PartialGray => 1,
            RefreshMode::FullGray => 2,
        };
        let result = unsafe {
            native::gpui_fbink_present_gray(
                self.fd,
                pixels.as_mut_ptr(),
                i32::try_from(damage.width)?,
                i32::try_from(damage.height)?,
                pixels.len(),
                i32::try_from(target.x)?,
                i32::try_from(target.y)?,
                i32::try_from(target.width)?,
                i32::try_from(target.height)?,
                mode_number,
            )
        };
        if result < 0 {
            bail!("fbink_print_raw_data failed with {result}");
        }
        self.presentations = self.presentations.saturating_add(1);
        Ok(())
    }

    #[cfg(not(target_arch = "arm"))]
    fn present_pixels(&mut self, _image: &RgbaImage, _damage: PixelRect, _mode: RefreshMode) -> Result<()> {
        bail!("native FBInk presentation is only available in ARM Kobo builds")
    }
}

#[cfg(target_arch = "arm")]
impl Drop for FbInkPresenter {
    fn drop(&mut self) {
        unsafe {
            native::gpui_fbink_close(self.fd);
        }
    }
}

#[cfg(target_arch = "arm")]
fn geometry_from_native(state: &native::State) -> ScreenGeometry {
    fn text(bytes: &[std::ffi::c_char]) -> String {
        let bytes: Vec<u8> = bytes
            .iter()
            .copied()
            .take_while(|byte| *byte != 0)
            .map(|byte| byte as u8)
            .collect();
        String::from_utf8(bytes).unwrap_or_else(|_| "unknown".into())
    }
    ScreenGeometry {
        view_width: state.view_width,
        view_height: state.view_height,
        screen_width: state.screen_width,
        screen_height: state.screen_height,
        view_x: state.view_x,
        view_y: state.view_y,
        current_rotation: state.current_rotation,
        device_name: text(&state.device_name),
        device_codename: text(&state.device_codename),
    }
}

/// Validate the damage-to-buffer conversion used by native presentation.
pub fn presenter_self_test(image: &RgbaImage, damage: PixelRect) -> Result<usize> {
    if damage.x + damage.width > image.width() || damage.y + damage.height > image.height() {
        bail!("presenter self-test damage lies outside the image");
    }
    usize::try_from(damage.width.saturating_mul(damage.height))
        .context("presenter self-test damage length overflow")
}
