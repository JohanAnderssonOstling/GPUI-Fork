use anyhow::{Context as _, Result, bail};
use image::GrayImage;

use crate::{FrameUpdate, KoboRenderMode, PixelRect, RefreshMode, ScreenGeometry};

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

    #[link(name = "gpui_fbink_shim", kind = "static")]
    unsafe extern "C" {
        pub fn gpui_fbink_open(state: *mut State) -> i32;
        pub fn gpui_fbink_reinit(fd: i32, state: *mut State) -> i32;
        pub fn gpui_fbink_set_rotation(fd: i32, rotation: u8) -> i32;
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
    scratch_pixels: Vec<u8>,
    render_mode: KoboRenderMode,
    monochrome_threshold: u8,
}

impl FbInkPresenter {
    pub fn open() -> Result<Self> {
        Self::open_with_render_mode(KoboRenderMode::QualityGrayscale, 160)
    }

    #[cfg(target_arch = "arm")]
    pub fn open_with_render_mode(
        render_mode: KoboRenderMode,
        monochrome_threshold: u8,
    ) -> Result<Self> {
        let mut state = native::State::default();
        let fd = unsafe { native::gpui_fbink_open(&mut state) };
        if fd < 0 {
            bail!("fbink_open/init failed with {fd}");
        }
        Ok(Self {
            fd,
            geometry: geometry_from_native(&state),
            presentations: 0,
            scratch_pixels: Vec::new(),
            render_mode,
            monochrome_threshold,
        })
    }

    #[cfg(not(target_arch = "arm"))]
    pub fn open_with_render_mode(
        _render_mode: KoboRenderMode,
        _monochrome_threshold: u8,
    ) -> Result<Self> {
        bail!("native FBInk presentation is only available in ARM Kobo builds")
    }

    pub fn set_render_mode(&mut self, render_mode: KoboRenderMode) {
        self.render_mode = render_mode;
    }

    pub fn geometry(&self) -> &ScreenGeometry {
        &self.geometry
    }

    pub fn set_current_rotation(&mut self, current_rotation: u8) {
        let current_rotation = current_rotation % 4;
        #[cfg(target_arch = "arm")]
        {
            let result = unsafe { native::gpui_fbink_set_rotation(self.fd, current_rotation) };
            if result < 0 {
                return;
            }

            let mut state = native::State::default();
            let result = unsafe { native::gpui_fbink_reinit(self.fd, &mut state) };
            if result < 0 {
                return;
            }

            self.geometry = geometry_from_native(&state);
            self.geometry.current_rotation = current_rotation;
            return;
        }

        #[cfg(not(target_arch = "arm"))]
        {
            self.geometry.current_rotation = current_rotation;
        }
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

    pub fn present_full(&mut self, image: &GrayImage) -> Result<()> {
        let mode = if self.presentations == 0 {
            println!("GPUI startup refresh: forcing full GC16");
            RefreshMode::FullGray
        } else if self.render_mode == KoboRenderMode::FastMonochrome {
            RefreshMode::FullMono
        } else {
            RefreshMode::FullGray
        };
        self.present_pixels(
            image,
            PixelRect {
                x: 0,
                y: 0,
                width: image.width(),
                height: image.height(),
            },
            mode,
        )?;
        Ok(())
    }

    pub fn present_full_gc16(&mut self, image: &GrayImage) -> Result<()> {
        println!("GPUI full GC16 refresh");
        self.present_pixels(
            image,
            PixelRect {
                x: 0,
                y: 0,
                width: image.width(),
                height: image.height(),
            },
            RefreshMode::FullGray,
        )?;
        Ok(())
    }

    pub fn present_update(&mut self, update: &FrameUpdate, mode: RefreshMode) -> Result<()> {
        let mode = if self.presentations == 0 {
            println!("GPUI startup refresh: forcing full GC16");
            RefreshMode::FullGray
        } else {
            mode
        };
        let damage = if mode.is_full() {
            PixelRect {
                x: 0,
                y: 0,
                width: update.image.width(),
                height: update.image.height(),
            }
        } else {
            update.damage
        };
        let mut regions = if mode.is_full() {
            vec![damage]
        } else if let Some(previous) = update.previous_image.as_deref() {
            tiled_damage_regions(
                previous,
                &update.image,
                damage,
                (self.render_mode == KoboRenderMode::FastMonochrome)
                    .then_some(self.monochrome_threshold),
            )
        } else {
            vec![damage]
        };
        if regions.len() > 1 {
            let bounding_area = rect_area(&self.geometry.scale_damage(
                damage,
                update.image.width(),
                update.image.height(),
            ));
            let region_area: u64 = regions
                .iter()
                .map(|region| {
                    rect_area(&self.geometry.scale_damage(
                        *region,
                        update.image.width(),
                        update.image.height(),
                    ))
                })
                .sum();
            let call_penalty = (regions.len() as u64 - 1).saturating_mul(4096);
            if region_area.saturating_add(call_penalty) >= bounding_area {
                regions.clear();
                regions.push(damage);
            }
        }
        let region_area: u64 = regions.iter().map(rect_area).sum();
        println!(
            "GPUI damage regions: count={} pixels={} bounding_pixels={}",
            regions.len(),
            region_area,
            rect_area(&damage)
        );
        for region in regions {
            self.present_pixels(&update.image, region, mode)?;
        }
        Ok(())
    }

    #[cfg(target_arch = "arm")]
    fn present_pixels(
        &mut self,
        image: &GrayImage,
        damage: PixelRect,
        mode: RefreshMode,
    ) -> Result<()> {
        let target = self
            .geometry
            .scale_damage(damage, image.width(), image.height());
        let capacity = usize::try_from(damage.width.saturating_mul(damage.height))?;
        self.scratch_pixels.clear();
        self.scratch_pixels.reserve(capacity);
        let source_width = usize::try_from(image.width())?;
        let left = usize::try_from(damage.x)?;
        let width = usize::try_from(damage.width)?;
        for y in damage.y..damage.y + damage.height {
            let row_start = usize::try_from(y)?
                .saturating_mul(source_width)
                .saturating_add(left);
            let row_end = row_start.saturating_add(width);
            self.scratch_pixels
                .extend_from_slice(&image.as_raw()[row_start..row_end]);
        }
        if self.render_mode == KoboRenderMode::FastMonochrome {
            for pixel in &mut self.scratch_pixels {
                *pixel = monochrome_value(*pixel, self.monochrome_threshold);
            }
        }
        let mode_number = match mode {
            RefreshMode::FastMono => 0,
            RefreshMode::PartialGray => 1,
            RefreshMode::FullGray => 2,
            RefreshMode::TextMono => 3,
            RefreshMode::FullMono => 4,
        };
        let result = unsafe {
            native::gpui_fbink_present_gray(
                self.fd,
                self.scratch_pixels.as_mut_ptr(),
                i32::try_from(damage.width)?,
                i32::try_from(damage.height)?,
                self.scratch_pixels.len(),
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
    fn present_pixels(
        &mut self,
        _image: &GrayImage,
        _damage: PixelRect,
        _mode: RefreshMode,
    ) -> Result<()> {
        bail!("native FBInk presentation is only available in ARM Kobo builds")
    }
}

const DAMAGE_TILE_SIZE: u32 = 32;
const MAX_DAMAGE_REGIONS: usize = 6;

fn rect_area(rect: &PixelRect) -> u64 {
    u64::from(rect.width) * u64::from(rect.height)
}

fn tiled_damage_regions(
    before: &GrayImage,
    after: &GrayImage,
    damage: PixelRect,
    monochrome_threshold: Option<u8>,
) -> Vec<PixelRect> {
    if before.dimensions() != after.dimensions() || damage.width == 0 || damage.height == 0 {
        return vec![damage];
    }

    let columns = damage.width.div_ceil(DAMAGE_TILE_SIZE);
    let rows = damage.height.div_ceil(DAMAGE_TILE_SIZE);
    let tile_count = usize::try_from(columns.saturating_mul(rows)).unwrap_or(usize::MAX);
    if tile_count == usize::MAX {
        return vec![damage];
    }

    let mut dirty = vec![false; tile_count];
    for row in 0..rows {
        for column in 0..columns {
            let x = damage.x + column * DAMAGE_TILE_SIZE;
            let y = damage.y + row * DAMAGE_TILE_SIZE;
            let width = DAMAGE_TILE_SIZE.min(damage.x + damage.width - x);
            let height = DAMAGE_TILE_SIZE.min(damage.y + damage.height - y);
            let stride = after.width() as usize;
            let left = x as usize;
            let width = width as usize;
            let changed = (y..y + height).any(|pixel_y| {
                let start = pixel_y as usize * stride + left;
                let end = start + width;
                match monochrome_threshold {
                    Some(threshold) => before.as_raw()[start..end]
                        .iter()
                        .zip(&after.as_raw()[start..end])
                        .any(|(&old, &new)| {
                            monochrome_value(old, threshold) != monochrome_value(new, threshold)
                        }),
                    None => before.as_raw()[start..end] != after.as_raw()[start..end],
                }
            });
            dirty[(row * columns + column) as usize] = changed;
        }
    }

    let mut visited = vec![false; dirty.len()];
    let mut regions = Vec::new();
    for row in 0..rows {
        for column in 0..columns {
            let index = (row * columns + column) as usize;
            if !dirty[index] || visited[index] {
                continue;
            }

            let mut stack = vec![(column, row)];
            visited[index] = true;
            let mut min_column = column;
            let mut max_column = column;
            let mut min_row = row;
            let mut max_row = row;
            while let Some((current_column, current_row)) = stack.pop() {
                min_column = min_column.min(current_column);
                max_column = max_column.max(current_column);
                min_row = min_row.min(current_row);
                max_row = max_row.max(current_row);
                let neighbors = [
                    current_column
                        .checked_sub(1)
                        .map(|value| (value, current_row)),
                    (current_column + 1 < columns).then_some((current_column + 1, current_row)),
                    current_row
                        .checked_sub(1)
                        .map(|value| (current_column, value)),
                    (current_row + 1 < rows).then_some((current_column, current_row + 1)),
                ];
                for (neighbor_column, neighbor_row) in neighbors.into_iter().flatten() {
                    let neighbor_index = (neighbor_row * columns + neighbor_column) as usize;
                    if dirty[neighbor_index] && !visited[neighbor_index] {
                        visited[neighbor_index] = true;
                        stack.push((neighbor_column, neighbor_row));
                    }
                }
            }

            let x = damage.x + min_column * DAMAGE_TILE_SIZE;
            let y = damage.y + min_row * DAMAGE_TILE_SIZE;
            let right =
                (damage.x + (max_column + 1) * DAMAGE_TILE_SIZE).min(damage.x + damage.width);
            let bottom =
                (damage.y + (max_row + 1) * DAMAGE_TILE_SIZE).min(damage.y + damage.height);
            regions.push(PixelRect {
                x,
                y,
                width: right - x,
                height: bottom - y,
            });
            if regions.len() > MAX_DAMAGE_REGIONS {
                return vec![damage];
            }
        }
    }

    let region_area: u64 = regions.iter().map(rect_area).sum();
    if regions.is_empty() {
        Vec::new()
    } else if region_area.saturating_mul(2) >= rect_area(&damage) {
        vec![damage]
    } else {
        regions
    }
}

fn monochrome_value(value: u8, threshold: u8) -> u8 {
    if value < threshold { 0 } else { 255 }
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
pub fn presenter_self_test(image: &GrayImage, damage: PixelRect) -> Result<usize> {
    if damage.x + damage.width > image.width() || damage.y + damage.height > image.height() {
        bail!("presenter self-test damage lies outside the image");
    }
    usize::try_from(damage.width.saturating_mul(damage.height))
        .context("presenter self-test damage length overflow")
}
