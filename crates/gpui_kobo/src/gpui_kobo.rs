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
    AnyWindowHandle, App, AppContext, AtlasKey, AtlasTextureId, AtlasTextureKind, AtlasTile, Bounds,
    ContentMask, Context, DevicePixels, Entity, HeadlessAppContext, Hsla, InteractiveElement,
    IntoElement, MonochromeSprite, MouseButton, MouseUpEvent, ParentElement, PlatformAtlas,
    PlatformHeadlessRenderer, PlatformInput,
    PlatformTextSystem, PrimitiveBatch, Render, Rgba, Scene, ScrollDelta, ScrollWheelEvent, Size, StatefulInteractiveElement,
    Styled, SubpixelSprite, TileId, TouchPhase as GpuiTouchPhase, TransformationMatrix, Window, div,
    point, px, rgb, size,
};
use gpui_wgpu::CosmicTextSystem;
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

/// Render the deployable library feasibility scene through GPUI.
pub fn render_button_test_image() -> Result<RgbaImage> {
    ButtonRenderSession::new()?.capture()
}

/// A complete CPU-rendered frame update and the exact pixel bounds that changed.
pub struct FrameUpdate {
    pub image: RgbaImage,
    pub damage: PixelRect,
}

/// Retains the GPUI application, window, view, prior framebuffer, and touch-drag
/// state for the lifetime of the Kobo test.
pub struct ButtonRenderSession {
    window: AnyWindowHandle,
    view: Entity<ButtonTestView>,
    app: HeadlessAppContext,
    render_count: u64,
    previous_frame: Option<RgbaImage>,
    last_touch_position: Option<(f32, f32)>,
    pending_scroll_y: f32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ButtonRenderState {
    pub exit_requested: bool,
}

impl ButtonRenderSession {
    pub fn new() -> Result<Self> {
        let text_system = Arc::new(CosmicTextSystem::new_without_system_fonts("Lilex"));
        text_system.add_fonts(vec![
            Cow::Borrowed(include_bytes!("../../../assets/fonts/lilex/Lilex-Regular.ttf")),
            Cow::Borrowed(include_bytes!("../../../assets/fonts/lilex/Lilex-Bold.ttf")),
        ])?;

        let mut app = HeadlessAppContext::with_platform(
            text_system,
            Arc::new(()),
            || Some(Box::new(KoboRenderer::new())),
        );
        let mut view = None;
        let window = app.open_window(TEST_CANVAS, |_window, cx: &mut App| {
            let entity = cx.new(|_| ButtonTestView::default());
            view = Some(entity.clone());
            entity
        })?;
        app.run_until_parked();

        Ok(Self {
            window: window.into(),
            view: view.expect("GPUI window builder did not create its root view"),
            app,
            render_count: 0,
            previous_frame: None,
            last_touch_position: None,
            pending_scroll_y: 0.0,
        })
    }

    fn render_current(&mut self) -> Result<RgbaImage> {
        let image = self.app.capture_screenshot(self.window.clone())?;
        self.render_count = self.render_count.saturating_add(1);
        Ok(image)
    }

    pub fn capture(&mut self) -> Result<RgbaImage> {
        let image = self.render_current()?;
        self.previous_frame = Some(image.clone());
        Ok(image)
    }

    pub fn render_count(&self) -> u64 {
        self.render_count
    }

    pub fn state(&self) -> ButtonRenderState {
        self.app.read_entity(&self.view, |view, _| ButtonRenderState {
            exit_requested: view.exit_requested,
        })
    }

    pub fn dispatch_touch(
        &mut self,
        phase: TouchPhase,
        canvas_x: f32,
        canvas_y: f32,
    ) -> Result<Option<FrameUpdate>> {
        let logical_x = canvas_x / 2.0;
        let logical_y = canvas_y / 2.0;

        match phase {
            TouchPhase::Down => {
                self.last_touch_position = Some((logical_x, logical_y));
                self.pending_scroll_y = 0.0;
                return Ok(None);
            }
            TouchPhase::Move => {
                if let Some((_, previous_y)) = self.last_touch_position {
                    self.pending_scroll_y += logical_y - previous_y;
                }
                self.last_touch_position = Some((logical_x, logical_y));
                return Ok(None);
            }
            TouchPhase::Up => {}
        }

        if let Some((_, previous_y)) = self.last_touch_position {
            self.pending_scroll_y += logical_y - previous_y;
        }
        let scroll_y = self.pending_scroll_y;
        self.last_touch_position = None;
        self.pending_scroll_y = 0.0;

        let position = point(px(logical_x), px(logical_y));
        let scrolled = scroll_y.abs() >= 1.0;
        self.app.update_window(self.window.clone(), |_, window, cx| {
            if scrolled {
                window.dispatch_event(
                    PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position,
                        delta: ScrollDelta::Pixels(point(px(0.0), px(scroll_y))),
                        touch_phase: GpuiTouchPhase::Ended,
                        ..Default::default()
                    }),
                    cx,
                );
            }
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    click_count: 1,
                    ..Default::default()
                }),
                cx,
            );
        })?;
        self.app.run_until_parked();

        if self.state().exit_requested || !scrolled {
            return Ok(None);
        }

        let image = self.render_current()?;
        let damage = self
            .previous_frame
            .as_ref()
            .and_then(|previous| changed_pixel_bounds(previous, &image));
        self.previous_frame = Some(image.clone());
        Ok(damage.map(|damage| FrameUpdate { image, damage }))
    }
}

pub fn changed_pixel_bounds(before: &RgbaImage, after: &RgbaImage) -> Option<PixelRect> {
    if before.dimensions() != after.dimensions() {
        return Some(PixelRect {
            x: 0,
            y: 0,
            width: after.width(),
            height: after.height(),
        });
    }

    let mut min_x = after.width();
    let mut min_y = after.height();
    let mut max_x = 0;
    let mut max_y = 0;
    let mut changed = false;
    for y in 0..after.height() {
        for x in 0..after.width() {
            if before.get_pixel(x, y) != after.get_pixel(x, y) {
                changed = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    changed.then_some(PixelRect {
        x: min_x,
        y: min_y,
        width: max_x - min_x + 1,
        height: max_y - min_y + 1,
    })
}

pub fn damage_image(image: &RgbaImage, damage: PixelRect) -> RgbaImage {
    image::imageops::crop_imm(image, damage.x, damage.y, damage.width, damage.height).to_image()
}

/// Write an RGBA grayscale render as a binary PGM image.
pub fn write_pgm(image: &RgbaImage, path: impl AsRef<Path>) -> Result<()> {
    let mut writer = BufWriter::new(File::create(path.as_ref())?);
    write!(writer, "P5\n{} {}\n255\n", image.width(), image.height())?;
    let mut pixels = Vec::with_capacity((image.width() * image.height()) as usize);
    pixels.extend(image.pixels().map(|pixel| pixel.0[0]));
    writer.write_all(&pixels)?;
    writer.flush()?;
    Ok(())
}

const BOOK_TITLES: [&str; 16] = [
    "The Left Hand of Darkness",
    "A Wizard of Earthsea",
    "The Dispossessed",
    "Kindred",
    "The Fifth Season",
    "Invisible Cities",
    "The Name of the Rose",
    "The Book of Disquiet",
    "The Master and Margarita",
    "The Memory Police",
    "Drive Your Plow Over the Bones",
    "The City and the City",
    "The Remains of the Day",
    "Piranesi",
    "The Employees",
    "We Have Always Lived Here",
];

#[derive(Default)]
struct ButtonTestView {
    exit_requested: bool,
}

impl Render for ButtonTestView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let white = rgb(0xffffff);
        let black = rgb(0x181818);
        let paper = rgb(0xf3f0e8);
        let alternate = rgb(0xe4e0d6);

        div()
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .bg(paper)
            .font_family("Lilex")
            .text_color(black)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .h(px(54.0))
                    .px(px(14.0))
                    .bg(black)
                    .text_color(white)
                    .text_size(px(18.0))
                    .child("KOBO LIBRARY")
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(62.0))
                            .h(px(34.0))
                            .bg(black)
                            .text_color(white)
                            .text_size(px(14.0))
                            .child("EXIT")
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|view, _, _, cx| {
                                    view.exit_requested = true;
                                    cx.notify();
                                }),
                            ),
                    ),
            )
            .child(
                div()
                    .id("kobo-library-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .w_full()
                    .overflow_y_scroll()
                    .children(BOOK_TITLES.iter().enumerate().map(|(index, title)| {
                        div()
                            .flex()
                            .items_center()
                            .w_full()
                            .h(px(48.0))
                            .px(px(16.0))
                            .bg(if index % 2 == 0 { white } else { alternate })
                            .text_size(px(14.0))
                            .child(format!("{:02}  {title}", index + 1))
                    })),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w_full()
                    .h(px(28.0))
                    .bg(black)
                    .text_color(white)
                    .text_size(px(10.0))
                    .child("DRAG TO SCROLL | TAP EXIT"),
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
