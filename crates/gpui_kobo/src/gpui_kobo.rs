//! Experimental CPU rendering pieces for a future Kobo GPUI backend.
//!
//! This crate proves that a GPUI scene containing a simple button can be
//! rasterized without a GPU and handed to FBInk on a Kobo device. It remains a
//! deliberately narrow feasibility backend, not a production GPUI platform.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use gpui::{
    App, AppContext, AtlasKey, AtlasTextureId, AtlasTextureKind, AtlasTile, Bounds, ContentMask,
    Context, DevicePixels, HeadlessAppContext, Hsla, IntoElement, MonochromeSprite,
    NoopTextSystem, ParentElement, PlatformAtlas, PlatformHeadlessRenderer, PrimitiveBatch, Render,
    Rgba, Scene, Size, Styled, SubpixelSprite, TileId, TransformationMatrix, Window, div, px, rgb, size,
};
use image::{Rgba as ImageRgba, RgbaImage};
use parking_lot::Mutex;

#[derive(Clone)]
struct CpuTile {
    size: Size<DevicePixels>,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct CpuAtlasState {
    next_tile_id: u32,
    tiles_by_key: HashMap<AtlasKey, AtlasTile>,
    pixels_by_tile_id: HashMap<u32, CpuTile>,
}

/// CPU-owned atlas used by the experimental renderer.
#[derive(Default)]
pub struct KoboAtlas {
    state: Mutex<CpuAtlasState>,
}

impl KoboAtlas {
    fn pixels(&self, tile_id: TileId) -> Option<CpuTile> {
        self.state.lock().pixels_by_tile_id.get(&tile_id.0).cloned()
    }
}

impl PlatformAtlas for KoboAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: &AtlasKey,
        build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> Result<Option<AtlasTile>> {
        if let Some(tile) = self.state.lock().tiles_by_key.get(key).copied() {
            return Ok(Some(tile));
        }

        let Some((size, bytes)) = build()? else {
            return Ok(None);
        };
        let width = usize::try_from(size.width.0.max(0))?;
        let height = usize::try_from(size.height.0.max(0))?;
        let pixel_count = width.saturating_mul(height);
        let kind = match key {
            AtlasKey::Glyph(params) if params.subpixel_rendering => AtlasTextureKind::Subpixel,
            AtlasKey::Glyph(_) => AtlasTextureKind::Monochrome,
            AtlasKey::Svg(_) | AtlasKey::Image(_) => AtlasTextureKind::Polychrome,
        };
        let expected_len = match kind {
            AtlasTextureKind::Monochrome => pixel_count,
            AtlasTextureKind::Subpixel | AtlasTextureKind::Polychrome => pixel_count.saturating_mul(4),
        };
        if bytes.len() != expected_len {
            bail!(
                "atlas tile has {} bytes, expected {} for {:?}",
                bytes.len(),
                expected_len,
                kind
            );
        }

        let mut state = self.state.lock();
        if let Some(tile) = state.tiles_by_key.get(key).copied() {
            return Ok(Some(tile));
        }
        let tile_id = TileId(state.next_tile_id);
        state.next_tile_id = state.next_tile_id.saturating_add(1);
        let tile = AtlasTile {
            texture_id: AtlasTextureId { index: 0, kind },
            tile_id,
            padding: 0,
            bounds: Bounds { origin: Default::default(), size },
        };
        state.pixels_by_tile_id.insert(
            tile_id.0,
            CpuTile {
                size,
                bytes: bytes.into_owned(),
            },
        );
        state.tiles_by_key.insert(key.clone(), tile);
        Ok(Some(tile))
    }

    fn remove(&self, key: &AtlasKey) {
        let mut state = self.state.lock();
        if let Some(tile) = state.tiles_by_key.remove(key) {
            state.pixels_by_tile_id.remove(&tile.tile_id.0);
        }
    }
}

/// CPU scene renderer for the minimal Kobo feasibility test.
pub struct KoboRenderer {
    atlas: Arc<KoboAtlas>,
}

pub const CANVAS_WIDTH: u32 = 600;
pub const CANVAS_HEIGHT: u32 = 800;

const TEST_CANVAS: Size<gpui::Pixels> = size(px(300.0), px(400.0));

#[derive(Clone, Copy, Debug)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PixelRect {
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x as f32
            && y >= self.y as f32
            && x < (self.x + self.width) as f32
            && y < (self.y + self.height) as f32
    }
}

pub const BUTTON_DAMAGE: PixelRect = PixelRect {
    x: 120,
    y: 336,
    width: 360,
    height: 128,
};

impl Default for KoboRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl KoboRenderer {
    /// Create a renderer with an empty CPU atlas.
    pub fn new() -> Self {
        Self { atlas: Arc::new(KoboAtlas::default()) }
    }

    fn render_to_grayscale(&self, scene: &Scene, size: Size<DevicePixels>) -> Result<RgbaImage> {
        let width = u32::try_from(size.width.0.max(0))?;
        let height = u32::try_from(size.height.0.max(0))?;
        if width == 0 || height == 0 {
            bail!("Kobo render target must be non-empty");
        }
        let mut target =
            RgbaImage::from_pixel(width, height, ImageRgba([255, 255, 255, 255]));

        for batch in scene.batches() {
            match batch {
                PrimitiveBatch::Quads(range) => {
                    for quad in &scene.quads[range] {
                        let Some(color) = quad.background.as_solid() else {
                            bail!("Kobo spike only supports solid quad backgrounds");
                        };
                        fill_rect(&mut target, quad.bounds, quad.content_mask, color);
                    }
                }
                PrimitiveBatch::MonochromeSprites { range, .. } => {
                    for sprite in &scene.monochrome_sprites[range] {
                        self.draw_monochrome_sprite(&mut target, sprite)?;
                    }
                }
                PrimitiveBatch::SubpixelSprites { range, .. } => {
                    for sprite in &scene.subpixel_sprites[range] {
                        self.draw_subpixel_sprite(&mut target, sprite)?;
                    }
                }
                PrimitiveBatch::Shadows(range) if range.is_empty() => {}
                PrimitiveBatch::Paths(range) if range.is_empty() => {}
                PrimitiveBatch::Underlines(range) if range.is_empty() => {}
                PrimitiveBatch::PolychromeSprites { range, .. } if range.is_empty() => {}
                PrimitiveBatch::Surfaces(range) if range.is_empty() => {}
                unsupported => bail!("unsupported GPUI primitive batch in Kobo spike: {unsupported:?}"),
            }
        }

        Ok(target)
    }

    fn draw_monochrome_sprite(
        &self,
        target: &mut RgbaImage,
        sprite: &MonochromeSprite,
    ) -> Result<()> {
        if sprite.transformation != TransformationMatrix::unit() {
            bail!("Kobo spike does not support transformed glyph sprites");
        }
        let tile = self
            .atlas
            .pixels(sprite.tile.tile_id)
            .ok_or_else(|| anyhow::anyhow!("glyph references a missing CPU atlas tile"))?;
        draw_mask(
            target,
            sprite.bounds,
            sprite.content_mask,
            sprite.color,
            &tile,
            false,
        )
    }

    fn draw_subpixel_sprite(
        &self,
        target: &mut RgbaImage,
        sprite: &SubpixelSprite,
    ) -> Result<()> {
        if sprite.transformation != TransformationMatrix::unit() {
            bail!("Kobo spike does not support transformed glyph sprites");
        }
        let tile = self
            .atlas
            .pixels(sprite.tile.tile_id)
            .ok_or_else(|| anyhow::anyhow!("glyph references a missing CPU atlas tile"))?;
        draw_mask(
            target,
            sprite.bounds,
            sprite.content_mask,
            sprite.color,
            &tile,
            true,
        )
    }
}

impl PlatformHeadlessRenderer for KoboRenderer {
    fn render_scene_to_image(
        &mut self,
        scene: &Scene,
        size: Size<DevicePixels>,
    ) -> Result<RgbaImage> {
        self.render_to_grayscale(scene, size)
    }

    fn render_scene(&mut self, scene: &Scene, size: Size<DevicePixels>) -> Result<()> {
        self.render_to_grayscale(scene, size).map(|_| ())
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.atlas.clone()
    }
}

/// Render the deployable one-button feasibility scene through GPUI.
///
/// The logical 300 by 400 canvas is rendered at GPUI's test display scale,
/// producing a 600 by 800 grayscale image that FBInk can scale to the device's
/// current viewport. The label is made from quads so the device binary does not
/// depend on a desktop font stack or GPU renderer.
pub fn render_button_test_image() -> Result<RgbaImage> {
    let mut app = HeadlessAppContext::with_platform(
        Arc::new(NoopTextSystem::new()),
        Arc::new(()),
        || Some(Box::new(KoboRenderer::new())),
    );
    let window = app.open_window(TEST_CANVAS, |_window, cx: &mut App| {
        cx.new(|_| ButtonTestView)
    })?;
    app.run_until_parked();
    app.capture_screenshot(window.into())
}

/// Retains the GPUI-rendered base frame and exposes the two visual states used by
/// the Kobo input smoke test. Keeping this state in-process avoids restarting the
/// application between touch events.
pub struct ButtonRenderSession {
    base: RgbaImage,
    pressed: bool,
    activated: bool,
}

impl ButtonRenderSession {
    pub fn new() -> Result<Self> {
        Ok(Self {
            base: render_button_test_image()?,
            pressed: false,
            activated: false,
        })
    }

    pub fn capture(&self) -> Result<RgbaImage> {
        let mut image = self.base.clone();
        if self.activated {
            flip_damage_vertically(&mut image, BUTTON_DAMAGE);
        }
        if self.pressed {
            invert_damage(&mut image, BUTTON_DAMAGE);
        }
        Ok(image)
    }

    pub fn set_state(&mut self, pressed: bool, activated: bool) -> Result<RgbaImage> {
        self.pressed = pressed;
        self.activated = activated;
        self.capture()
    }
}

pub fn button_damage_image(image: &RgbaImage) -> RgbaImage {
    image::imageops::crop_imm(
        image,
        BUTTON_DAMAGE.x,
        BUTTON_DAMAGE.y,
        BUTTON_DAMAGE.width,
        BUTTON_DAMAGE.height,
    )
    .to_image()
}

fn invert_damage(image: &mut RgbaImage, rect: PixelRect) {
    for y in rect.y..rect.y + rect.height {
        for x in rect.x..rect.x + rect.width {
            let pixel = image.get_pixel_mut(x, y);
            pixel.0[0] = 255 - pixel.0[0];
            pixel.0[1] = 255 - pixel.0[1];
            pixel.0[2] = 255 - pixel.0[2];
        }
    }
}

fn flip_damage_vertically(image: &mut RgbaImage, rect: PixelRect) {
    for offset in 0..rect.height / 2 {
        let top = rect.y + offset;
        let bottom = rect.y + rect.height - 1 - offset;
        for x in rect.x..rect.x + rect.width {
            let top_pixel = *image.get_pixel(x, top);
            let bottom_pixel = *image.get_pixel(x, bottom);
            image.put_pixel(x, top, bottom_pixel);
            image.put_pixel(x, bottom, top_pixel);
        }
    }
}

/// Write an RGBA grayscale render as a binary PGM image.
///
/// PGM keeps the on-device executable independent from an image encoder and is
/// one of the image formats accepted by image-enabled FBInk builds.
pub fn write_pgm(image: &RgbaImage, path: impl AsRef<Path>) -> Result<()> {
    let mut writer = BufWriter::new(File::create(path.as_ref())?);
    write!(writer, "P5\n{} {}\n255\n", image.width(), image.height())?;
    let mut pixels = Vec::with_capacity((image.width() * image.height()) as usize);
    pixels.extend(image.pixels().map(|pixel| pixel.0[0]));
    writer.write_all(&pixels)?;
    writer.flush()?;
    Ok(())
}

struct ButtonTestView;

impl Render for ButtonTestView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let white = rgb(0xffffff);
        div()
            .flex()
            .items_center()
            .justify_center()
            .w_full()
            .h_full()
            .bg(white)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(180.0))
                    .h(px(64.0))
                    .bg(rgb(0x181818))
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap(px(5.0))
                            .child(div().w(px(10.0)).h(px(16.0)).bg(white))
                            .child(div().w(px(10.0)).h(px(25.0)).bg(white))
                            .child(div().w(px(10.0)).h(px(34.0)).bg(white)),
                    ),
            )
    }
}

fn fill_rect(
    target: &mut RgbaImage,
    bounds: Bounds<gpui::ScaledPixels>,
    mask: ContentMask<gpui::ScaledPixels>,
    color: Hsla,
) {
    let Some((x0, y0, x1, y1)) = clipped_rect(target, bounds, mask) else {
        return;
    };
    let (gray, alpha) = grayscale(color);
    for y in y0..y1 {
        for x in x0..x1 {
            blend_gray(target.get_pixel_mut(x, y), gray, alpha);
        }
    }
}

fn draw_mask(
    target: &mut RgbaImage,
    bounds: Bounds<gpui::ScaledPixels>,
    mask: ContentMask<gpui::ScaledPixels>,
    color: Hsla,
    tile: &CpuTile,
    rgba_mask: bool,
) -> Result<()> {
    let Some((x0, y0, x1, y1)) = clipped_rect(target, bounds, mask) else {
        return Ok(());
    };
    let source_width = usize::try_from(tile.size.width.0.max(0))?;
    let source_height = usize::try_from(tile.size.height.0.max(0))?;
    if source_width == 0 || source_height == 0 {
        return Ok(());
    }
    let bounds_x0 = bounds.origin.x.0.floor() as i32;
    let bounds_y0 = bounds.origin.y.0.floor() as i32;
    let target_width = bounds.size.width.0.max(1.0);
    let target_height = bounds.size.height.0.max(1.0);
    let (gray, color_alpha) = grayscale(color);

    for y in y0..y1 {
        for x in x0..x1 {
            let source_x = (((x as i32 - bounds_x0) as f32 / target_width)
                * source_width as f32)
                .floor()
                .clamp(0.0, (source_width - 1) as f32) as usize;
            let source_y = (((y as i32 - bounds_y0) as f32 / target_height)
                * source_height as f32)
                .floor()
                .clamp(0.0, (source_height - 1) as f32) as usize;
            let pixel_index = source_y * source_width + source_x;
            let coverage = if rgba_mask {
                let offset = pixel_index * 4;
                let channels = &tile.bytes[offset..offset + 4];
                (u16::from(channels[0]) + u16::from(channels[1]) + u16::from(channels[2]))
                    as f32
                    / (3.0 * 255.0)
            } else {
                f32::from(tile.bytes[pixel_index]) / 255.0
            };
            blend_gray(
                target.get_pixel_mut(x, y),
                gray,
                color_alpha * coverage,
            );
        }
    }
    Ok(())
}

fn clipped_rect(
    target: &RgbaImage,
    bounds: Bounds<gpui::ScaledPixels>,
    mask: ContentMask<gpui::ScaledPixels>,
) -> Option<(u32, u32, u32, u32)> {
    let clipped = bounds.intersect(&mask.bounds);
    let x0 = clipped.origin.x.0.floor().max(0.0) as u32;
    let y0 = clipped.origin.y.0.floor().max(0.0) as u32;
    let x1 = (clipped.origin.x.0 + clipped.size.width.0)
        .ceil()
        .clamp(0.0, target.width() as f32) as u32;
    let y1 = (clipped.origin.y.0 + clipped.size.height.0)
        .ceil()
        .clamp(0.0, target.height() as f32) as u32;
    (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
}

fn grayscale(color: Hsla) -> (u8, f32) {
    let rgba: Rgba = color.into();
    let luminance = 0.2126 * rgba.r + 0.7152 * rgba.g + 0.0722 * rgba.b;
    ((luminance.clamp(0.0, 1.0) * 255.0).round() as u8, rgba.a)
}

fn blend_gray(pixel: &mut ImageRgba<u8>, gray: u8, alpha: f32) {
    let alpha = alpha.clamp(0.0, 1.0);
    let current = f32::from(pixel.0[0]);
    let blended = (f32::from(gray) * alpha + current * (1.0 - alpha)).round() as u8;
    *pixel = ImageRgba([blended, blended, blended, 255]);
}
mod input;

pub use input::*;
