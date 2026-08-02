//! A production-oriented, single-window GPUI backend for Kobo e-readers.
//!
//! This crate proves that a GPUI scene containing a simple button can be
//! rasterized without a GPU and handed to FBInk on a Kobo device. It remains a
//! deliberately e-ink-specific backend rather than a desktop compatibility layer.

mod platform;
pub use platform::{KoboPlatform, KoboPlatformOptions, KoboWindow};

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use gpui::{
    App, AppContext, AssetSource, AtlasKey, AtlasTextureId, AtlasTextureKind,
    AtlasTile, Background, Bounds, Corners,
    ContentMask, Context, DevicePixels, Hsla, InteractiveElement,
    IntoElement, MonochromeSprite, MouseButton, ParentElement, PlatformAtlas,
    Path as GpuiPath, PolychromeSprite, PrimitiveBatch, Quad, Render, Rgba,
    ScaledPixels, Scene, Shadow,
    Size, StatefulInteractiveElement, Styled, SubpixelSprite, TileId,
    TransformationMatrix, Underline, Window, WindowBounds,
    WindowOptions, div, point, px,
    linear_color_stop, linear_gradient, rgb, size, SharedString,
};
use image::{Rgba as ImageRgba, RgbaImage};
use parking_lot::Mutex;

pub struct KoboAssets;

impl AssetSource for KoboAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(match path {
            "kobo-cover.ppm" => Some(Cow::Borrowed(include_bytes!("../assets/kobo-cover.ppm"))),
            _ => None,
        })
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(if path.is_empty() {
            vec!["kobo-cover.ppm".into()]
        } else {
            Vec::new()
        })
    }
}

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

    pub(crate) fn render_to_grayscale(&self, scene: &Scene, size: Size<DevicePixels>) -> Result<RgbaImage> {
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
                        draw_quad(&mut target, quad);
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
                PrimitiveBatch::Shadows(range) => {
                    for shadow in &scene.shadows[range] {
                        draw_shadow(&mut target, shadow);
                    }
                }
                PrimitiveBatch::Paths(range) => {
                    for path in &scene.paths[range] {
                        draw_path(&mut target, path);
                    }
                }
                PrimitiveBatch::Underlines(range) => {
                    for underline in &scene.underlines[range] {
                        self.draw_underline(&mut target, underline)?;
                    }
                }
                PrimitiveBatch::PolychromeSprites { range, .. } => {
                    for sprite in &scene.polychrome_sprites[range] {
                        self.draw_polychrome_sprite(&mut target, sprite)?;
                    }
                }
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
            sprite.transformation,
        )
    }

    fn draw_subpixel_sprite(
        &self,
        target: &mut RgbaImage,
        sprite: &SubpixelSprite,
    ) -> Result<()> {
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
            sprite.transformation,
        )
    }

    fn draw_underline(&self, target: &mut RgbaImage, underline: &Underline) -> Result<()> {
        if underline.wavy == 0 {
            fill_rect(target, underline.bounds, underline.content_mask, underline.color);
            return Ok(());
        }
        let Some((x0, y0, x1, y1)) = clipped_rect(target, underline.bounds, underline.content_mask) else {
            return Ok(());
        };
        let (gray, alpha) = grayscale(underline.color);
        let center = underline.bounds.origin.y.0 + underline.bounds.size.height.0 / 2.0;
        let amplitude = underline.thickness.0.max(1.0);
        for x in x0..x1 {
            let wave = ((x as f32 - underline.bounds.origin.x.0) / (amplitude * 3.0)).sin();
            let y = (center + wave * amplitude).round() as i32;
            for offset in 0..underline.thickness.0.ceil().max(1.0) as i32 {
                let sample_y = y + offset;
                if sample_y >= y0 as i32 && sample_y < y1 as i32 {
                    blend_gray(target.get_pixel_mut(x, sample_y as u32), gray, alpha);
                }
            }
        }
        Ok(())
    }

    fn draw_polychrome_sprite(
        &self,
        target: &mut RgbaImage,
        sprite: &PolychromeSprite,
    ) -> Result<()> {
        let tile = self
            .atlas
            .pixels(sprite.tile.tile_id)
            .ok_or_else(|| anyhow::anyhow!("image references a missing CPU atlas tile"))?;
        let source_width = usize::try_from(tile.size.width.0.max(0))?;
        let source_height = usize::try_from(tile.size.height.0.max(0))?;
        if tile.bytes.len() != source_width.saturating_mul(source_height).saturating_mul(4) {
            bail!("polychrome atlas tile is not RGBA8");
        }

        let left = sprite.bounds.origin.x.0.floor().max(0.0) as u32;
        let top = sprite.bounds.origin.y.0.floor().max(0.0) as u32;
        let right = (sprite.bounds.origin.x.0 + sprite.bounds.size.width.0)
            .ceil()
            .min(target.width() as f32) as u32;
        let bottom = (sprite.bounds.origin.y.0 + sprite.bounds.size.height.0)
            .ceil()
            .min(target.height() as f32) as u32;
        let mask_left = sprite.content_mask.bounds.origin.x.0.floor().max(0.0) as u32;
        let mask_top = sprite.content_mask.bounds.origin.y.0.floor().max(0.0) as u32;
        let mask_right = (sprite.content_mask.bounds.origin.x.0
            + sprite.content_mask.bounds.size.width.0)
            .ceil()
            .min(target.width() as f32) as u32;
        let mask_bottom = (sprite.content_mask.bounds.origin.y.0
            + sprite.content_mask.bounds.size.height.0)
            .ceil()
            .min(target.height() as f32) as u32;
        let width = sprite.bounds.size.width.0.max(1.0);
        let height = sprite.bounds.size.height.0.max(1.0);

        for y in top.max(mask_top)..bottom.min(mask_bottom) {
            for x in left.max(mask_left)..right.min(mask_right) {
                let source_x = (((x as f32 - sprite.bounds.origin.x.0) / width)
                    * source_width as f32)
                    .floor()
                    .clamp(0.0, source_width.saturating_sub(1) as f32)
                    as usize;
                let source_y = (((y as f32 - sprite.bounds.origin.y.0) / height)
                    * source_height as f32)
                    .floor()
                    .clamp(0.0, source_height.saturating_sub(1) as f32)
                    as usize;
                let offset = (source_y * source_width + source_x) * 4;
                let rgba = &tile.bytes[offset..offset + 4];
                let (gray, alpha) = rgba8_to_grayscale(
                    [rgba[0], rgba[1], rgba[2], rgba[3]],
                    sprite.opacity,
                );
                if point_in_rounded_rect(x as f32 + 0.5, y as f32 + 0.5, sprite.bounds, sprite.corner_radii) {
                    blend_gray(target.get_pixel_mut(x, y), gray, alpha);
                }
            }
        }
        Ok(())
    }
}

impl KoboRenderer {
    pub(crate) fn render_scene_to_image(
        &mut self,
        scene: &Scene,
        size: Size<DevicePixels>,
    ) -> Result<RgbaImage> {
        self.render_to_grayscale(scene, size)
    }

    pub(crate) fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.atlas.clone()
    }
}

/// Open the smoke-test view through a normal production GPUI application.
pub fn open_button_test_window(cx: &mut App) -> Result<()> {
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.0), px(0.0)),
                size: TEST_CANVAS,
            })),
            focus: true,
            show: true,
            ..Default::default()
        },
        |_window, cx| cx.new(|_| ButtonTestView::default()),
    )?;
    Ok(())
}

/// A complete CPU-rendered frame update and the exact pixel bounds that changed.
pub struct FrameUpdate {
    pub image: RgbaImage,
    pub damage: PixelRect,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RendererCoverage {
    pub painted_pixels: usize,
    pub rounded_corner_pixels: usize,
}

/// Exercise renderer paths that the minimal library view does not naturally
/// emit, using the same raster functions used for GPUI scene batches.
pub fn extended_renderer_self_test() -> Result<RendererCoverage> {
    let mut target = RgbaImage::from_pixel(64, 64, ImageRgba([255, 255, 255, 255]));
    let bounds = Bounds {
        origin: point(ScaledPixels(8.0), ScaledPixels(8.0)),
        size: size(ScaledPixels(48.0), ScaledPixels(30.0)),
    };
    let mask = ContentMask { bounds: Bounds {
        origin: point(ScaledPixels(0.0), ScaledPixels(0.0)),
        size: size(ScaledPixels(64.0), ScaledPixels(64.0)),
    }};
    let mut quad = Quad::default();
    quad.bounds = bounds;
    quad.content_mask = mask;
    quad.background = linear_gradient(
        90.0,
        linear_color_stop(rgb(0x202020), 0.0),
        linear_color_stop(rgb(0xd0d0d0), 1.0),
    );
    quad.border_color = rgb(0x101010).into();
    quad.border_widths.top = ScaledPixels(2.0);
    quad.border_widths.right = ScaledPixels(2.0);
    quad.border_widths.bottom = ScaledPixels(2.0);
    quad.border_widths.left = ScaledPixels(2.0);
    quad.corner_radii = Corners::all(ScaledPixels(8.0));
    draw_quad(&mut target, &quad);
    let rounded_corner_pixels = [target.get_pixel(8, 8), target.get_pixel(55, 8)]
        .into_iter()
        .filter(|pixel| pixel.0[0] == 255)
        .count();

    let shadow = Shadow {
        order: Default::default(),
        blur_radius: ScaledPixels(6.0),
        bounds: Bounds {
            origin: point(ScaledPixels(4.0), ScaledPixels(4.0)),
            size: size(ScaledPixels(56.0), ScaledPixels(42.0)),
        },
        corner_radii: Corners::all(ScaledPixels(10.0)),
        content_mask: mask,
        color: gpui::rgba(0x00000066).into(),
        element_bounds: bounds,
        element_corner_radii: Corners::all(ScaledPixels(8.0)),
        inset: 0,
        pad: 0,
    };
    draw_shadow(&mut target, &shadow);

    let mut path = GpuiPath::new(point(px(14.0), px(44.0)));
    path.line_to(point(px(32.0), px(58.0)));
    path.line_to(point(px(50.0), px(44.0)));
    path.color = rgb(0x303030).into();
    path.content_mask = ContentMask { bounds: Bounds {
        origin: point(px(0.0), px(0.0)),
        size: size(px(64.0), px(64.0)),
    }};
    draw_path(&mut target, &path.scale(1.0));

    let underline = Underline {
        order: Default::default(),
        pad: 0,
        bounds: Bounds {
            origin: point(ScaledPixels(10.0), ScaledPixels(60.0)),
            size: size(ScaledPixels(44.0), ScaledPixels(3.0)),
        },
        content_mask: mask,
        color: rgb(0x181818).into(),
        thickness: ScaledPixels(1.0),
        wavy: 1,
    };
    KoboRenderer::new().draw_underline(&mut target, &underline)?;

    let painted_pixels = target.pixels().filter(|pixel| pixel.0[0] != 255).count();
    if painted_pixels < 500 || rounded_corner_pixels != 2 {
        bail!("extended renderer coverage failed: painted={painted_pixels} corners={rounded_corner_pixels}");
    }
    Ok(RendererCoverage { painted_pixels, rounded_corner_pixels })
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
                                    cx.quit();
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

fn draw_quad(target: &mut RgbaImage, quad: &Quad) {
    let Some((x0, y0, x1, y1)) = clipped_rect(target, quad.bounds, quad.content_mask) else {
        return;
    };
    let has_any_border = quad.border_widths.top.0 > 0.0
        || quad.border_widths.right.0 > 0.0
        || quad.border_widths.bottom.0 > 0.0
        || quad.border_widths.left.0 > 0.0;
    let has_rounded_corners = quad.corner_radii.top_left.0 > 0.0
        || quad.corner_radii.top_right.0 > 0.0
        || quad.corner_radii.bottom_right.0 > 0.0
        || quad.corner_radii.bottom_left.0 > 0.0;
    if !has_any_border && !has_rounded_corners {
        let (gray, alpha) = background_gray(quad.background);
        fill_clipped_gray(target, x0, y0, x1, y1, gray, alpha);
        return;
    }
    let inner = Bounds {
        origin: point(
            ScaledPixels(quad.bounds.origin.x.0 + quad.border_widths.left.0),
            ScaledPixels(quad.bounds.origin.y.0 + quad.border_widths.top.0),
        ),
        size: size(
            ScaledPixels((quad.bounds.size.width.0 - quad.border_widths.left.0 - quad.border_widths.right.0).max(0.0)),
            ScaledPixels((quad.bounds.size.height.0 - quad.border_widths.top.0 - quad.border_widths.bottom.0).max(0.0)),
        ),
    };
    let border_inset = quad.border_widths.left.0.max(quad.border_widths.right.0)
        .max(quad.border_widths.top.0)
        .max(quad.border_widths.bottom.0);
    let inner_radii = Corners {
        top_left: ScaledPixels((quad.corner_radii.top_left.0 - border_inset).max(0.0)),
        top_right: ScaledPixels((quad.corner_radii.top_right.0 - border_inset).max(0.0)),
        bottom_right: ScaledPixels((quad.corner_radii.bottom_right.0 - border_inset).max(0.0)),
        bottom_left: ScaledPixels((quad.corner_radii.bottom_left.0 - border_inset).max(0.0)),
    };
    let has_border = border_inset > 0.0;
    let (border_gray, border_alpha) = grayscale(quad.border_color);
    for y in y0..y1 {
        for x in x0..x1 {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            if !point_in_rounded_rect(px, py, quad.bounds, quad.corner_radii) {
                continue;
            }
            if has_border && !point_in_rounded_rect(px, py, inner, inner_radii) {
                blend_gray(target.get_pixel_mut(x, y), border_gray, border_alpha);
            } else {
                let (gray, alpha) = background_gray(quad.background);
                blend_gray(target.get_pixel_mut(x, y), gray, alpha);
            }
        }
    }
}

fn draw_shadow(_target: &mut RgbaImage, _shadow: &Shadow) {
    // Shadows consume a large amount of CPU and add little value on e-ink.
    // Accept the primitive so GPUI scenes remain compatible, but omit its paint.
}

fn draw_path(target: &mut RgbaImage, path: &GpuiPath<ScaledPixels>) {
    for triangle in path.vertices.chunks_exact(3) {
        let a = triangle[0].xy_position;
        let b = triangle[1].xy_position;
        let c = triangle[2].xy_position;
        let min_x = a.x.0.min(b.x.0).min(c.x.0).floor().max(0.0) as u32;
        let min_y = a.y.0.min(b.y.0).min(c.y.0).floor().max(0.0) as u32;
        let max_x = a.x.0.max(b.x.0).max(c.x.0).ceil().min(target.width() as f32) as u32;
        let max_y = a.y.0.max(b.y.0).max(c.y.0).ceil().min(target.height() as f32) as u32;
        let denominator = (b.y.0 - c.y.0) * (a.x.0 - c.x.0)
            + (c.x.0 - b.x.0) * (a.y.0 - c.y.0);
        if denominator.abs() <= f32::EPSILON {
            continue;
        }
        for y in min_y..max_y {
            for x in min_x..max_x {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                if !path.content_mask.bounds.contains(&point(ScaledPixels(px), ScaledPixels(py))) {
                    continue;
                }
                let wa = ((b.y.0 - c.y.0) * (px - c.x.0) + (c.x.0 - b.x.0) * (py - c.y.0)) / denominator;
                let wb = ((c.y.0 - a.y.0) * (px - c.x.0) + (a.x.0 - c.x.0) * (py - c.y.0)) / denominator;
                let wc = 1.0 - wa - wb;
                if wa < 0.0 || wb < 0.0 || wc < 0.0 {
                    continue;
                }
                let st_x = wa * triangle[0].st_position.x + wb * triangle[1].st_position.x + wc * triangle[2].st_position.x;
                let st_y = wa * triangle[0].st_position.y + wb * triangle[1].st_position.y + wc * triangle[2].st_position.y;
                if st_x * st_x > st_y {
                    continue;
                }
                let (gray, alpha) = background_gray(path.color);
                blend_gray(target.get_pixel_mut(x, y), gray, alpha);
            }
        }
    }
}

fn background_gray(background: Background) -> (u8, f32) {
    if let Some(color) = background.as_solid() {
        return grayscale(color);
    }
    if let Some((_angle, stops, _)) = background.as_linear_gradient() {
        let from = stops[0];
        let to = stops[1];
        let amount = 0.5;
        let from: Rgba = from.color.into();
        let to: Rgba = to.color.into();
        let red = from.r + (to.r - from.r) * amount;
        let green = from.g + (to.g - from.g) * amount;
        let blue = from.b + (to.b - from.b) * amount;
        let alpha = from.a + (to.a - from.a) * amount;
        return (((0.2126 * red + 0.7152 * green + 0.0722 * blue) * 255.0).round() as u8, alpha);
    }
    (255, 0.0)
}

fn point_in_rounded_rect(x: f32, y: f32, bounds: Bounds<ScaledPixels>, radii: Corners<ScaledPixels>) -> bool {
    let left = bounds.origin.x.0;
    let top = bounds.origin.y.0;
    let right = left + bounds.size.width.0;
    let bottom = top + bounds.size.height.0;
    if x < left || x >= right || y < top || y >= bottom {
        return false;
    }
    let radius = if x < left + bounds.size.width.0 / 2.0 {
        if y < top + bounds.size.height.0 / 2.0 { radii.top_left.0 } else { radii.bottom_left.0 }
    } else if y < top + bounds.size.height.0 / 2.0 {
        radii.top_right.0
    } else {
        radii.bottom_right.0
    }.min(bounds.size.width.0 / 2.0).min(bounds.size.height.0 / 2.0).max(0.0);
    if radius == 0.0 {
        return true;
    }
    let center_x = if x < left + radius { left + radius } else if x > right - radius { right - radius } else { x };
    let center_y = if y < top + radius { top + radius } else if y > bottom - radius { bottom - radius } else { y };
    (x - center_x).powi(2) + (y - center_y).powi(2) <= radius.powi(2)
}

fn fill_clipped_gray(
    target: &mut RgbaImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    gray: u8,
    alpha: f32,
) {
    for y in y0..y1 {
        for x in x0..x1 {
            blend_gray(target.get_pixel_mut(x, y), gray, alpha);
        }
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
    transformation: TransformationMatrix,
) -> Result<()> {
    let transformed = transform_bounds(bounds, transformation);
    let Some((x0, y0, x1, y1)) = clipped_rect(target, transformed, mask) else {
        return Ok(());
    };
    let source_width = usize::try_from(tile.size.width.0.max(0))?;
    let source_height = usize::try_from(tile.size.height.0.max(0))?;
    if source_width == 0 || source_height == 0 {
        return Ok(());
    }
    let inverse = invert_transform(transformation).ok_or_else(|| anyhow::anyhow!("glyph transformation is singular"))?;
    let target_width = bounds.size.width.0.max(1.0);
    let target_height = bounds.size.height.0.max(1.0);
    let (gray, color_alpha) = grayscale(color);

    for y in y0..y1 {
        for x in x0..x1 {
            let (sample_x, sample_y) = apply_transform(inverse, x as f32 + 0.5, y as f32 + 0.5);
            if sample_x < bounds.origin.x.0
                || sample_y < bounds.origin.y.0
                || sample_x >= bounds.origin.x.0 + bounds.size.width.0
                || sample_y >= bounds.origin.y.0 + bounds.size.height.0
            {
                continue;
            }
            let source_x = (((sample_x - bounds.origin.x.0) / target_width)
                * source_width as f32)
                .floor()
                .clamp(0.0, (source_width - 1) as f32) as usize;
            let source_y = (((sample_y - bounds.origin.y.0) / target_height)
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

fn transform_bounds(bounds: Bounds<ScaledPixels>, transform: TransformationMatrix) -> Bounds<ScaledPixels> {
    let points = [
        apply_transform(transform, bounds.origin.x.0, bounds.origin.y.0),
        apply_transform(transform, bounds.origin.x.0 + bounds.size.width.0, bounds.origin.y.0),
        apply_transform(transform, bounds.origin.x.0, bounds.origin.y.0 + bounds.size.height.0),
        apply_transform(transform, bounds.origin.x.0 + bounds.size.width.0, bounds.origin.y.0 + bounds.size.height.0),
    ];
    let min_x = points.iter().map(|point| point.0).fold(f32::INFINITY, f32::min);
    let min_y = points.iter().map(|point| point.1).fold(f32::INFINITY, f32::min);
    let max_x = points.iter().map(|point| point.0).fold(f32::NEG_INFINITY, f32::max);
    let max_y = points.iter().map(|point| point.1).fold(f32::NEG_INFINITY, f32::max);
    Bounds {
        origin: point(ScaledPixels(min_x), ScaledPixels(min_y)),
        size: size(ScaledPixels(max_x - min_x), ScaledPixels(max_y - min_y)),
    }
}

fn apply_transform(transform: TransformationMatrix, x: f32, y: f32) -> (f32, f32) {
    (
        transform.translation[0] + transform.rotation_scale[0][0] * x + transform.rotation_scale[0][1] * y,
        transform.translation[1] + transform.rotation_scale[1][0] * x + transform.rotation_scale[1][1] * y,
    )
}

fn invert_transform(transform: TransformationMatrix) -> Option<TransformationMatrix> {
    let [[a, b], [c, d]] = transform.rotation_scale;
    let determinant = a * d - b * c;
    if determinant.abs() <= f32::EPSILON {
        return None;
    }
    let inverse = determinant.recip();
    let rotation_scale = [[d * inverse, -b * inverse], [-c * inverse, a * inverse]];
    let translation = [
        -(rotation_scale[0][0] * transform.translation[0] + rotation_scale[0][1] * transform.translation[1]),
        -(rotation_scale[1][0] * transform.translation[0] + rotation_scale[1][1] * transform.translation[1]),
    ];
    Some(TransformationMatrix { rotation_scale, translation })
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

/// Convert an RGBA8 sprite sample into the grayscale intensity and effective alpha
/// consumed by the Kobo CPU renderer.
pub fn rgba8_to_grayscale(rgba: [u8; 4], opacity: f32) -> (u8, f32) {
    let luminance = 0.2126 * rgba[0] as f32
        + 0.7152 * rgba[1] as f32
        + 0.0722 * rgba[2] as f32;
    (
        luminance.round().clamp(0.0, 255.0) as u8,
        rgba[3] as f32 / 255.0 * opacity.clamp(0.0, 1.0),
    )
}

fn blend_gray(pixel: &mut ImageRgba<u8>, gray: u8, alpha: f32) {
    let alpha = alpha.clamp(0.0, 1.0);
    let current = f32::from(pixel.0[0]);
    let blended = (f32::from(gray) * alpha + current * (1.0 - alpha)).round() as u8;
    *pixel = ImageRgba([blended, blended, blended, 255]);
}
mod display;
mod input;
mod presenter;
mod refresh;
mod runtime;

pub use display::*;
pub use input::*;
pub use presenter::*;
pub use refresh::*;
pub use runtime::*;
