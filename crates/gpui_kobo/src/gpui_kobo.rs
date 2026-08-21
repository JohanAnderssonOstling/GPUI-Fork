//! A production-oriented, single-window GPUI backend for Kobo e-readers.
//!
//! This crate proves that a GPUI scene containing a simple button can be
//! rasterized without a GPU and handed to FBInk on a Kobo device. It remains a
//! deliberately e-ink-specific backend rather than a desktop compatibility layer.

mod platform;
mod virtual_keyboard;
pub use platform::{
    KoboExperienceMode, KoboPlatform, KoboPlatformOptions, KoboWindow, PageButtonBehavior,
    begin_application_startup_profile, begin_book_launch_profile, current_experience_mode,
    current_render_mode, request_auto_rotation, request_experience_mode, request_full_repaint,
    request_reader_page_repaint, request_render_mode,
};
pub use virtual_keyboard::KoboKeyboardRoot;

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Instant;

use anyhow::{Result, bail};
use gpui::{
    App, AppContext, AssetSource, AtlasKey, AtlasTextureId, AtlasTextureKind, AtlasTile,
    Background, Bounds, ContentMask, Context, Corners, DevicePixels, Hsla, InteractiveElement,
    IntoElement, MonochromeSprite, MouseButton, ParentElement, Path as GpuiPath, PlatformAtlas,
    PolychromeSprite, PrimitiveBatch, Quad, Render, Rgba, ScaledPixels, Scene, Shadow,
    SharedString, Size, StatefulInteractiveElement, Styled, SubpixelSprite, TileId,
    TransformationMatrix, Underline, Window, WindowBounds, WindowOptions, div, linear_color_stop,
    linear_gradient, point, px, rgb, size,
};
use image::{GrayImage, Luma};
use parking_lot::Mutex;

/// Enables high-volume device diagnostics without imposing release log I/O by default.
pub fn verbose_logging_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("GPUI_KOBO_VERBOSE")
            .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
    })
}

/// Enables per-frame CPU renderer timings in the regular Kobo application.
/// Kept separate from verbose input/presentation diagnostics so profiling can
/// be enabled without flooding the device log with unrelated events.
pub(crate) fn render_profiling_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("GPUI_KOBO_PROFILE")
            .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
    })
}

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
    pixels: CpuTilePixels,
    opaque: bool,
}

#[derive(Clone)]
enum CpuTilePixels {
    Mask(Vec<u8>),
    LumaAlpha(Vec<u8>),
}

#[derive(Default)]
struct CpuAtlasState {
    next_tile_id: u32,
    tiles_by_key: HashMap<AtlasKey, AtlasTile>,
    pixels_by_tile_id: HashMap<u32, Arc<CpuTile>>,
}

/// CPU-owned atlas used by the experimental renderer.
#[derive(Default)]
pub struct KoboAtlas {
    state: Mutex<CpuAtlasState>,
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
        let kind = key.texture_kind();
        let expected_len = match kind {
            AtlasTextureKind::Monochrome => pixel_count,
            AtlasTextureKind::Subpixel | AtlasTextureKind::Polychrome => {
                pixel_count.saturating_mul(4)
            }
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
            bounds: Bounds {
                origin: Default::default(),
                size,
            },
        };
        let pixels = match kind {
            AtlasTextureKind::Monochrome => CpuTilePixels::Mask(bytes.into_owned()),
            AtlasTextureKind::Subpixel => CpuTilePixels::Mask(
                bytes
                    .chunks_exact(4)
                    .map(|channels| {
                        ((u16::from(channels[0]) + u16::from(channels[1]) + u16::from(channels[2]))
                            / 3) as u8
                    })
                    .collect(),
            ),
            AtlasTextureKind::Polychrome => {
                let mut converted = Vec::with_capacity(pixel_count.saturating_mul(2));
                for channels in bytes.chunks_exact(4) {
                    converted.push(
                        (0.2126 * f32::from(channels[0])
                            + 0.7152 * f32::from(channels[1])
                            + 0.0722 * f32::from(channels[2]))
                        .round() as u8,
                    );
                    converted.push(channels[3]);
                }
                CpuTilePixels::LumaAlpha(converted)
            }
        };
        let opaque = match &pixels {
            CpuTilePixels::LumaAlpha(pixels) => pixels
                .chunks_exact(2)
                .all(|channels| channels[1] == u8::MAX),
            CpuTilePixels::Mask(_) => false,
        };
        state.pixels_by_tile_id.insert(
            tile_id.0,
            Arc::new(CpuTile {
                size,
                pixels,
                opaque,
            }),
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
    previous_scene: Option<SceneSnapshot>,
    frame_pool: Arc<Mutex<Vec<FramePoolEntry>>>,
    frame_generation: u64,
    damage_history: VecDeque<FrameDamage>,
    candidate_marks: Vec<u32>,
    candidate_generation: u32,
    candidate_heap: BinaryHeap<Reverse<(usize, usize, usize)>>,
    candidate_indices: Vec<usize>,
}

struct SceneSnapshot {
    size: (u32, u32),
    primitives: Vec<PrimitiveStamp>,
    spatial_index: SceneSpatialIndex,
}

#[derive(Clone, Copy)]
struct PrimitiveStamp {
    fingerprint: u64,
    bounds: PixelRect,
    primitive: ScenePrimitive,
}

#[derive(Clone, Copy)]
enum ScenePrimitive {
    Quad(usize),
    MonochromeSprite { index: usize, tile_id: u32 },
    SubpixelSprite { index: usize, tile_id: u32 },
    Path(usize),
    Underline(usize),
    PolychromeSprite { index: usize, tile_id: u32 },
    Unsupported,
}

struct SceneSpatialIndex {
    columns: u32,
    rows: u32,
    cells: Vec<Vec<usize>>,
}

pub const CANVAS_WIDTH: u32 = 600;
pub const CANVAS_HEIGHT: u32 = 800;

const TEST_CANVAS: Size<gpui::Pixels> = size(px(300.0), px(400.0));
const SCENE_TILE_SIZE: u32 = 80;
const FRAME_DAMAGE_HISTORY_LIMIT: usize = 12;

struct FramePoolEntry {
    image: GrayImage,
    generation: u64,
}

struct FrameDamage {
    generation: u64,
    regions: Vec<PixelRect>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl SceneSpatialIndex {
    fn new(primitives: &[PrimitiveStamp], width: u32, height: u32) -> Self {
        let columns = width.div_ceil(SCENE_TILE_SIZE).max(1);
        let rows = height.div_ceil(SCENE_TILE_SIZE).max(1);
        let mut cells = vec![Vec::new(); (columns * rows) as usize];
        for (primitive_index, primitive) in primitives.iter().enumerate() {
            let left = (primitive.bounds.x / SCENE_TILE_SIZE).min(columns - 1);
            let top = (primitive.bounds.y / SCENE_TILE_SIZE).min(rows - 1);
            let right = (primitive.bounds.x + primitive.bounds.width - 1)
                .div_euclid(SCENE_TILE_SIZE)
                .min(columns - 1);
            let bottom = (primitive.bounds.y + primitive.bounds.height - 1)
                .div_euclid(SCENE_TILE_SIZE)
                .min(rows - 1);
            for tile_y in top..=bottom {
                for tile_x in left..=right {
                    cells[(tile_y * columns + tile_x) as usize].push(primitive_index);
                }
            }
        }
        Self {
            columns,
            rows,
            cells,
        }
    }

    fn collect_candidates(
        &self,
        region: PixelRect,
        primitives: &[PrimitiveStamp],
        marks: &mut Vec<u32>,
        generation: &mut u32,
        heap: &mut BinaryHeap<Reverse<(usize, usize, usize)>>,
        candidates: &mut Vec<usize>,
    ) {
        let left = (region.x / SCENE_TILE_SIZE).min(self.columns - 1);
        let top = (region.y / SCENE_TILE_SIZE).min(self.rows - 1);
        let right = (region.x + region.width - 1)
            .div_euclid(SCENE_TILE_SIZE)
            .min(self.columns - 1);
        let bottom = (region.y + region.height - 1)
            .div_euclid(SCENE_TILE_SIZE)
            .min(self.rows - 1);
        if marks.len() < primitives.len() {
            marks.resize(primitives.len(), 0);
        }
        *generation = generation.wrapping_add(1);
        if *generation == 0 {
            marks.fill(0);
            *generation = 1;
        }
        candidates.clear();
        heap.clear();
        for tile_y in top..=bottom {
            for tile_x in left..=right {
                let cell_index = (tile_y * self.columns + tile_x) as usize;
                if let Some(&primitive_index) = self.cells[cell_index].first() {
                    heap.push(Reverse((primitive_index, cell_index, 0)));
                }
            }
        }
        while let Some(Reverse((primitive_index, cell_index, position))) = heap.pop() {
            if marks[primitive_index] != *generation {
                marks[primitive_index] = *generation;
                if pixel_rects_intersect(primitives[primitive_index].bounds, region) {
                    candidates.push(primitive_index);
                }
            }
            let next_position = position + 1;
            if let Some(&next_index) = self.cells[cell_index].get(next_position) {
                heap.push(Reverse((next_index, cell_index, next_position)));
            }
        }
    }

    fn update_primitive_bounds(
        &mut self,
        primitive_index: usize,
        before: PixelRect,
        after: PixelRect,
    ) {
        let before_left = (before.x / SCENE_TILE_SIZE).min(self.columns - 1);
        let before_top = (before.y / SCENE_TILE_SIZE).min(self.rows - 1);
        let before_right = (before.x + before.width - 1)
            .div_euclid(SCENE_TILE_SIZE)
            .min(self.columns - 1);
        let before_bottom = (before.y + before.height - 1)
            .div_euclid(SCENE_TILE_SIZE)
            .min(self.rows - 1);
        for tile_y in before_top..=before_bottom {
            for tile_x in before_left..=before_right {
                let cell = &mut self.cells[(tile_y * self.columns + tile_x) as usize];
                if let Ok(position) = cell.binary_search(&primitive_index) {
                    cell.remove(position);
                }
            }
        }

        let after_left = (after.x / SCENE_TILE_SIZE).min(self.columns - 1);
        let after_top = (after.y / SCENE_TILE_SIZE).min(self.rows - 1);
        let after_right = (after.x + after.width - 1)
            .div_euclid(SCENE_TILE_SIZE)
            .min(self.columns - 1);
        let after_bottom = (after.y + after.height - 1)
            .div_euclid(SCENE_TILE_SIZE)
            .min(self.rows - 1);
        for tile_y in after_top..=after_bottom {
            for tile_x in after_left..=after_right {
                let cell = &mut self.cells[(tile_y * self.columns + tile_x) as usize];
                if let Err(position) = cell.binary_search(&primitive_index) {
                    cell.insert(position, primitive_index);
                }
            }
        }
    }
}

struct Fingerprint(u64);

impl Fingerprint {
    fn new() -> Self {
        Self(0x9e37_79b9_7f4a_7c15)
    }

    fn write_u64(&mut self, value: u64) {
        self.0 ^= value.wrapping_add(0x9e37_79b9_7f4a_7c15);
        self.0 = self.0.rotate_left(27).wrapping_mul(0x3c79_ac49_2ba7_b653);
        self.0 ^= self.0 >> 33;
    }

    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }

    fn write_u8(&mut self, value: u8) {
        self.write_u64(u64::from(value));
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    fn finish(self) -> u64 {
        self.0 ^ (self.0 >> 29)
    }
}

fn hash_f32(hasher: &mut Fingerprint, value: f32) {
    hasher.write_u32(value.to_bits());
}

fn hash_scaled_bounds(hasher: &mut Fingerprint, bounds: Bounds<ScaledPixels>) {
    hash_f32(hasher, bounds.origin.x.0);
    hash_f32(hasher, bounds.origin.y.0);
    hash_f32(hasher, bounds.size.width.0);
    hash_f32(hasher, bounds.size.height.0);
}

fn hash_color(hasher: &mut Fingerprint, color: Hsla) {
    let (gray, alpha) = grayscale(color);
    hasher.write_u8(gray);
    hash_f32(hasher, alpha);
}

fn hash_background(hasher: &mut Fingerprint, background: Background) {
    let (gray, alpha) = background_gray(background);
    hasher.write_u8(gray);
    hash_f32(hasher, alpha);
}

fn hash_transform(hasher: &mut Fingerprint, transform: TransformationMatrix) {
    for row in transform.rotation_scale {
        for value in row {
            hash_f32(hasher, value);
        }
    }
    for value in transform.translation {
        hash_f32(hasher, value);
    }
}

fn quad_fingerprint(quad: &Quad) -> u64 {
    let mut hasher = Fingerprint::new();
    hasher.write_u8(0);
    hash_scaled_bounds(&mut hasher, quad.bounds);
    hash_scaled_bounds(&mut hasher, quad.content_mask.bounds);
    hash_background(&mut hasher, quad.background);
    hash_color(&mut hasher, quad.border_color);
    hash_f32(&mut hasher, quad.border_widths.top.0);
    hash_f32(&mut hasher, quad.border_widths.right.0);
    hash_f32(&mut hasher, quad.border_widths.bottom.0);
    hash_f32(&mut hasher, quad.border_widths.left.0);
    hasher.finish()
}

fn monochrome_fingerprint(sprite: &MonochromeSprite) -> u64 {
    let mut hasher = Fingerprint::new();
    hasher.write_u8(1);
    hash_scaled_bounds(&mut hasher, sprite.bounds);
    hash_scaled_bounds(&mut hasher, sprite.content_mask.bounds);
    hash_color(&mut hasher, sprite.color);
    hasher.write_u32(sprite.tile.tile_id.0);
    hash_transform(&mut hasher, sprite.transformation);
    hasher.finish()
}

fn subpixel_fingerprint(sprite: &SubpixelSprite) -> u64 {
    let mut hasher = Fingerprint::new();
    hasher.write_u8(2);
    hash_scaled_bounds(&mut hasher, sprite.bounds);
    hash_scaled_bounds(&mut hasher, sprite.content_mask.bounds);
    hash_color(&mut hasher, sprite.color);
    hasher.write_u32(sprite.tile.tile_id.0);
    hash_transform(&mut hasher, sprite.transformation);
    hasher.finish()
}

fn path_fingerprint(path: &GpuiPath<ScaledPixels>) -> u64 {
    let mut hasher = Fingerprint::new();
    hasher.write_u8(3);
    hash_scaled_bounds(&mut hasher, path.bounds);
    hash_scaled_bounds(&mut hasher, path.content_mask.bounds);
    hash_background(&mut hasher, path.color);
    hasher.write_usize(path.vertices.len());
    for vertex in &path.vertices {
        hash_f32(&mut hasher, vertex.xy_position.x.0);
        hash_f32(&mut hasher, vertex.xy_position.y.0);
        hash_f32(&mut hasher, vertex.st_position.x);
        hash_f32(&mut hasher, vertex.st_position.y);
    }
    hasher.finish()
}

fn underline_fingerprint(underline: &Underline) -> u64 {
    let mut hasher = Fingerprint::new();
    hasher.write_u8(4);
    hash_scaled_bounds(&mut hasher, underline.bounds);
    hash_scaled_bounds(&mut hasher, underline.content_mask.bounds);
    hash_color(&mut hasher, underline.color);
    hash_f32(&mut hasher, underline.thickness.0);
    hasher.write_u32(underline.wavy.get() as u32);
    hasher.finish()
}

fn polychrome_fingerprint(sprite: &PolychromeSprite) -> u64 {
    let mut hasher = Fingerprint::new();
    hasher.write_u8(5);
    hash_scaled_bounds(&mut hasher, sprite.bounds);
    hash_scaled_bounds(&mut hasher, sprite.content_mask.bounds);
    hash_f32(&mut hasher, sprite.opacity);
    hasher.write_u32(sprite.tile.tile_id.0);
    hasher.finish()
}

fn pixel_rect_bounds(rect: PixelRect) -> Bounds<ScaledPixels> {
    Bounds {
        origin: point(ScaledPixels(rect.x as f32), ScaledPixels(rect.y as f32)),
        size: size(
            ScaledPixels(rect.width as f32),
            ScaledPixels(rect.height as f32),
        ),
    }
}

fn bounds_empty(bounds: Bounds<ScaledPixels>) -> bool {
    bounds.size.width.0 <= 0.0 || bounds.size.height.0 <= 0.0
}

fn bounds_pixel_rect(bounds: Bounds<ScaledPixels>, width: u32, height: u32) -> Option<PixelRect> {
    let left = bounds.origin.x.0.floor().max(0.0).min(width as f32) as u32;
    let top = bounds.origin.y.0.floor().max(0.0).min(height as f32) as u32;
    let right = (bounds.origin.x.0 + bounds.size.width.0)
        .ceil()
        .max(0.0)
        .min(width as f32) as u32;
    let bottom = (bounds.origin.y.0 + bounds.size.height.0)
        .ceil()
        .max(0.0)
        .min(height as f32) as u32;
    (right > left && bottom > top).then_some(PixelRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

fn scene_primitive_stamps(scene: &Scene, width: u32, height: u32) -> Vec<PrimitiveStamp> {
    let mut stamps = Vec::new();
    for batch in scene.batches() {
        match batch {
            PrimitiveBatch::Quads(range) => {
                for index in range {
                    let primitive = &scene.quads[index];
                    if let Some(bounds) = bounds_pixel_rect(
                        primitive.bounds.intersect(&primitive.content_mask.bounds),
                        width,
                        height,
                    ) {
                        stamps.push(PrimitiveStamp {
                            fingerprint: quad_fingerprint(primitive),
                            bounds,
                            primitive: ScenePrimitive::Quad(index),
                        });
                    }
                }
            }
            PrimitiveBatch::MonochromeSprites { range, .. } => {
                for index in range {
                    let primitive = &scene.monochrome_sprites[index];
                    if let Some(bounds) = bounds_pixel_rect(
                        transform_bounds(primitive.bounds, primitive.transformation)
                            .intersect(&primitive.content_mask.bounds),
                        width,
                        height,
                    ) {
                        stamps.push(PrimitiveStamp {
                            fingerprint: monochrome_fingerprint(primitive),
                            bounds,
                            primitive: ScenePrimitive::MonochromeSprite {
                                index,
                                tile_id: primitive.tile.tile_id.0,
                            },
                        });
                    }
                }
            }
            PrimitiveBatch::SubpixelSprites { range, .. } => {
                for index in range {
                    let primitive = &scene.subpixel_sprites[index];
                    if let Some(bounds) = bounds_pixel_rect(
                        transform_bounds(primitive.bounds, primitive.transformation)
                            .intersect(&primitive.content_mask.bounds),
                        width,
                        height,
                    ) {
                        stamps.push(PrimitiveStamp {
                            fingerprint: subpixel_fingerprint(primitive),
                            bounds,
                            primitive: ScenePrimitive::SubpixelSprite {
                                index,
                                tile_id: primitive.tile.tile_id.0,
                            },
                        });
                    }
                }
            }
            PrimitiveBatch::Paths(range) => {
                for index in range {
                    let primitive = &scene.paths[index];
                    let Some(first) = primitive.vertices.first() else {
                        continue;
                    };
                    let mut min_x = first.xy_position.x.0;
                    let mut min_y = first.xy_position.y.0;
                    let mut max_x = min_x;
                    let mut max_y = min_y;
                    for vertex in &primitive.vertices[1..] {
                        min_x = min_x.min(vertex.xy_position.x.0);
                        min_y = min_y.min(vertex.xy_position.y.0);
                        max_x = max_x.max(vertex.xy_position.x.0);
                        max_y = max_y.max(vertex.xy_position.y.0);
                    }
                    let bounds = Bounds {
                        origin: point(ScaledPixels(min_x), ScaledPixels(min_y)),
                        size: size(ScaledPixels(max_x - min_x), ScaledPixels(max_y - min_y)),
                    }
                    .intersect(&primitive.content_mask.bounds);
                    if let Some(bounds) = bounds_pixel_rect(bounds, width, height) {
                        stamps.push(PrimitiveStamp {
                            fingerprint: path_fingerprint(primitive),
                            bounds,
                            primitive: ScenePrimitive::Path(index),
                        });
                    }
                }
            }
            PrimitiveBatch::Underlines(range) => {
                for index in range {
                    let primitive = &scene.underlines[index];
                    if let Some(bounds) = bounds_pixel_rect(
                        primitive.bounds.intersect(&primitive.content_mask.bounds),
                        width,
                        height,
                    ) {
                        stamps.push(PrimitiveStamp {
                            fingerprint: underline_fingerprint(primitive),
                            bounds,
                            primitive: ScenePrimitive::Underline(index),
                        });
                    }
                }
            }
            PrimitiveBatch::PolychromeSprites { range, .. } => {
                for index in range {
                    let primitive = &scene.polychrome_sprites[index];
                    if let Some(bounds) = bounds_pixel_rect(
                        primitive.bounds.intersect(&primitive.content_mask.bounds),
                        width,
                        height,
                    ) {
                        stamps.push(PrimitiveStamp {
                            fingerprint: polychrome_fingerprint(primitive),
                            bounds,
                            primitive: ScenePrimitive::PolychromeSprite {
                                index,
                                tile_id: primitive.tile.tile_id.0,
                            },
                        });
                    }
                }
            }
            PrimitiveBatch::Shadows(_) => {}
            PrimitiveBatch::Surfaces(range) if range.is_empty() => {}
            _ => stamps.push(PrimitiveStamp {
                fingerprint: u64::MAX,
                bounds: PixelRect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                primitive: ScenePrimitive::Unsupported,
            }),
        }
    }
    stamps
}

fn changed_primitive_regions(
    before: &[PrimitiveStamp],
    after: &[PrimitiveStamp],
    canvas: PixelRect,
    changed_indices: &mut Vec<usize>,
) -> Vec<PixelRect> {
    let mut regions = Vec::new();
    for index in 0..before.len().max(after.len()) {
        match (before.get(index), after.get(index)) {
            (Some(before), Some(after))
                if before.fingerprint == after.fingerprint && before.bounds == after.bounds => {}
            (Some(before), Some(after)) => {
                changed_indices.push(index);
                add_dirty_region(&mut regions, before.bounds, canvas);
                add_dirty_region(&mut regions, after.bounds, canvas);
            }
            (Some(before), None) => {
                changed_indices.push(index);
                add_dirty_region(&mut regions, before.bounds, canvas);
            }
            (None, Some(after)) => {
                changed_indices.push(index);
                add_dirty_region(&mut regions, after.bounds, canvas);
            }
            (None, None) => {}
        }
    }
    regions
}

fn add_dirty_region(regions: &mut Vec<PixelRect>, rect: PixelRect, canvas: PixelRect) {
    let left = rect.x.saturating_sub(1);
    let top = rect.y.saturating_sub(1);
    let right = (rect.x + rect.width + 1).min(canvas.width);
    let bottom = (rect.y + rect.height + 1).min(canvas.height);
    let mut merged = PixelRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    };
    let mut index = 0;
    while index < regions.len() {
        if pixel_rects_touch(regions[index], merged) {
            merged = union_pixel_rect(regions.swap_remove(index), merged);
        } else {
            index += 1;
        }
    }
    regions.push(merged);
}

fn pixel_rects_touch(left: PixelRect, right: PixelRect) -> bool {
    left.x <= right.x + right.width
        && right.x <= left.x + left.width
        && left.y <= right.y + right.height
        && right.y <= left.y + left.height
}

fn pixel_rects_intersect(left: PixelRect, right: PixelRect) -> bool {
    left.x < right.x + right.width
        && right.x < left.x + left.width
        && left.y < right.y + right.height
        && right.y < left.y + left.height
}

fn union_pixel_rect(left: PixelRect, right: PixelRect) -> PixelRect {
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

fn rect_area_u64(rect: &PixelRect) -> u64 {
    u64::from(rect.width) * u64::from(rect.height)
}

fn clear_region(target: &mut GrayImage, region: PixelRect) {
    let stride = target.width() as usize;
    for y in region.y..region.y + region.height {
        let start = y as usize * stride + region.x as usize;
        target.as_mut()[start..start + region.width as usize].fill(255);
    }
}

fn copy_region(source: &GrayImage, target: &mut GrayImage, region: PixelRect) {
    let stride = source.width() as usize;
    for y in region.y..region.y + region.height {
        let start = y as usize * stride + region.x as usize;
        let end = start + region.width as usize;
        target.as_mut()[start..end].copy_from_slice(&source.as_raw()[start..end]);
    }
}

fn synchronize_recycled_frame(
    target: &mut GrayImage,
    target_generation: u64,
    current_generation: u64,
    previous: &GrayImage,
    history: &VecDeque<FrameDamage>,
) -> bool {
    if target_generation == current_generation {
        return true;
    }
    if target_generation > current_generation {
        return false;
    }
    let mut expected = target_generation.saturating_add(1);
    for damage in history {
        if damage.generation < expected {
            continue;
        }
        if damage.generation != expected {
            return false;
        }
        for region in &damage.regions {
            copy_region(previous, target, *region);
        }
        expected = expected.saturating_add(1);
    }
    expected == current_generation.saturating_add(1)
}

impl Default for KoboRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl KoboRenderer {
    /// Create a renderer with an empty CPU atlas.
    pub fn new() -> Self {
        Self {
            atlas: Arc::new(KoboAtlas::default()),
            previous_scene: None,
            frame_pool: Arc::new(Mutex::new(Vec::with_capacity(3))),
            frame_generation: 0,
            damage_history: VecDeque::with_capacity(FRAME_DAMAGE_HISTORY_LIMIT),
            candidate_marks: Vec::new(),
            candidate_generation: 0,
            candidate_heap: BinaryHeap::new(),
            candidate_indices: Vec::new(),
        }
    }

    /// Drop framebuffer-size-dependent render state while retaining the CPU
    /// atlas shared with GPUI's window. Retained scenes keep atlas tile IDs, so
    /// replacing the atlas during a scale transition makes their glyph and
    /// image references invalid.
    pub(crate) fn reset_surface(&mut self) {
        self.previous_scene = None;
        self.frame_pool.lock().clear();
        self.frame_generation = 0;
        self.damage_history.clear();
        self.candidate_marks.clear();
        self.candidate_generation = 0;
        self.candidate_heap.clear();
        self.candidate_indices.clear();
    }

    pub(crate) fn render_to_grayscale(
        &mut self,
        scene: &Scene,
        size: Size<DevicePixels>,
        previous: Option<&GrayImage>,
    ) -> Result<Option<(GrayFrame, PixelRect)>> {
        let profile_enabled = render_profiling_enabled();
        let total_started = profile_enabled.then(Instant::now);
        let width = u32::try_from(size.width.0.max(0))?;
        let height = u32::try_from(size.height.0.max(0))?;
        if width == 0 || height == 0 {
            bail!("Kobo render target must be non-empty");
        }
        let full = PixelRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let stamps_started = profile_enabled.then(Instant::now);
        let primitives = scene_primitive_stamps(scene, width, height);
        let stamps_us = stamps_started.map_or(0, |started| started.elapsed().as_micros());
        let reuse_spatial_index = previous
            .is_some_and(|image| image.dimensions() == (width, height))
            && self.previous_scene.as_ref().is_some_and(|old| {
                old.size == (width, height) && old.primitives.len() == primitives.len()
            });
        let diff_started = profile_enabled.then(Instant::now);
        let mut changed_indices = Vec::new();
        let mut regions = match (&self.previous_scene, previous) {
            (Some(old), Some(previous))
                if old.size == (width, height) && previous.dimensions() == (width, height) =>
            {
                changed_primitive_regions(&old.primitives, &primitives, full, &mut changed_indices)
            }
            _ => vec![full],
        };
        let diff_us = diff_started.map_or(0, |started| started.elapsed().as_micros());
        if regions.is_empty() {
            if let Some(total_started) = total_started {
                println!(
                    "GPUI_KOBO_CPU_RENDER changed=false primitives={} stamps_us={stamps_us} diff_us={diff_us} spatial_us=0 buffer_us=0 raster_us=0 total_us={}",
                    primitives.len(),
                    total_started.elapsed().as_micros(),
                );
            }
            return Ok(None);
        }
        let total_area: u64 = regions.iter().map(rect_area_u64).sum();
        if regions.len() > 6
            || total_area.saturating_mul(5) >= rect_area_u64(&full).saturating_mul(3)
        {
            regions.clear();
            regions.push(full);
        }
        let changed_primitive_count = changed_indices.len();
        let spatial_started = profile_enabled.then(Instant::now);
        let spatial_index = if reuse_spatial_index {
            let old = self.previous_scene.take().expect("checked above");
            let mut spatial_index = old.spatial_index;
            for index in changed_indices {
                let before = old.primitives[index].bounds;
                let after = primitives[index].bounds;
                if before != after {
                    spatial_index.update_primitive_bounds(index, before, after);
                }
            }
            spatial_index
        } else {
            SceneSpatialIndex::new(&primitives, width, height)
        };
        let spatial_us = spatial_started.map_or(0, |started| started.elapsed().as_micros());
        let snapshot = SceneSnapshot {
            size: (width, height),
            spatial_index,
            primitives,
        };

        let buffer_started = profile_enabled.then(Instant::now);
        let recycled = {
            let mut pool = self.frame_pool.lock();
            pool.iter()
                .position(|entry| entry.image.dimensions() == (width, height))
                .map(|index| pool.swap_remove(index))
        };
        let recycled_generation = recycled.as_ref().map(|entry| entry.generation);
        let mut target = recycled
            .map(|entry| entry.image)
            .unwrap_or_else(|| GrayImage::new(width, height));
        if let Some(previous) = previous.filter(|image| image.dimensions() == (width, height)) {
            let synchronized = recycled_generation.is_some_and(|generation| {
                synchronize_recycled_frame(
                    &mut target,
                    generation,
                    self.frame_generation,
                    previous,
                    &self.damage_history,
                )
            });
            if !synchronized {
                target.as_mut().copy_from_slice(previous.as_raw());
            }
        } else {
            target.as_mut().fill(255);
        }
        let buffer_us = buffer_started.map_or(0, |started| started.elapsed().as_micros());
        let raster_started = profile_enabled.then(Instant::now);
        let atlas = self.atlas.clone();
        let atlas_state = atlas.state.lock();
        for region in &regions {
            clear_region(&mut target, *region);
            self.render_scene_region(
                scene,
                &snapshot,
                &atlas_state.pixels_by_tile_id,
                &mut target,
                *region,
            )?;
        }
        let raster_us = raster_started.map_or(0, |started| started.elapsed().as_micros());
        let history_regions = regions.clone();
        let region_count = history_regions.len();
        let damage = regions.into_iter().reduce(union_pixel_rect).unwrap_or(full);
        let next_generation = self.frame_generation.wrapping_add(1).max(1);
        self.frame_generation = next_generation;
        self.damage_history.push_back(FrameDamage {
            generation: next_generation,
            regions: history_regions,
        });
        while self.damage_history.len() > FRAME_DAMAGE_HISTORY_LIMIT {
            self.damage_history.pop_front();
        }
        self.previous_scene = Some(snapshot);
        if let Some(total_started) = total_started {
            println!(
                "GPUI_KOBO_CPU_RENDER changed=true primitives={} changed_primitives={changed_primitive_count} regions={} damage_pixels={} stamps_us={stamps_us} diff_us={diff_us} spatial_us={spatial_us} buffer_us={buffer_us} raster_us={raster_us} total_us={}",
                self.previous_scene
                    .as_ref()
                    .map_or(0, |scene| scene.primitives.len()),
                region_count,
                u64::from(damage.width) * u64::from(damage.height),
                total_started.elapsed().as_micros(),
            );
        }
        Ok(Some((
            GrayFrame::pooled(target, next_generation, &self.frame_pool),
            damage,
        )))
    }

    fn render_scene_region(
        &mut self,
        scene: &Scene,
        snapshot: &SceneSnapshot,
        tiles: &HashMap<u32, Arc<CpuTile>>,
        target: &mut GrayImage,
        region: PixelRect,
    ) -> Result<()> {
        let clip = pixel_rect_bounds(region);
        snapshot.spatial_index.collect_candidates(
            region,
            &snapshot.primitives,
            &mut self.candidate_marks,
            &mut self.candidate_generation,
            &mut self.candidate_heap,
            &mut self.candidate_indices,
        );
        for &primitive_index in &self.candidate_indices {
            match &snapshot.primitives[primitive_index].primitive {
                ScenePrimitive::Quad(index) => {
                    let mut quad = scene.quads[*index];
                    quad.content_mask.bounds = quad.content_mask.bounds.intersect(&clip);
                    if !bounds_empty(quad.content_mask.bounds) {
                        draw_quad(target, &quad);
                    }
                }
                ScenePrimitive::MonochromeSprite { index, tile_id } => {
                    let mut sprite = scene.monochrome_sprites[*index];
                    sprite.content_mask.bounds = sprite.content_mask.bounds.intersect(&clip);
                    if !bounds_empty(sprite.content_mask.bounds) {
                        let tile = tiles.get(tile_id).ok_or_else(|| {
                            anyhow::anyhow!("glyph references a missing CPU atlas tile")
                        })?;
                        Self::draw_monochrome_sprite(target, &sprite, tile)?;
                    }
                }
                ScenePrimitive::SubpixelSprite { index, tile_id } => {
                    let mut sprite = scene.subpixel_sprites[*index];
                    sprite.content_mask.bounds = sprite.content_mask.bounds.intersect(&clip);
                    if !bounds_empty(sprite.content_mask.bounds) {
                        let tile = tiles.get(tile_id).ok_or_else(|| {
                            anyhow::anyhow!("glyph references a missing CPU atlas tile")
                        })?;
                        Self::draw_subpixel_sprite(target, &sprite, tile)?;
                    }
                }
                ScenePrimitive::Path(index) => {
                    draw_path(target, &scene.paths[*index], Some(clip));
                }
                ScenePrimitive::Underline(index) => {
                    let mut underline = scene.underlines[*index];
                    underline.content_mask.bounds = underline.content_mask.bounds.intersect(&clip);
                    if !bounds_empty(underline.content_mask.bounds) {
                        Self::draw_underline(target, &underline)?;
                    }
                }
                ScenePrimitive::PolychromeSprite { index, tile_id } => {
                    let mut sprite = scene.polychrome_sprites[*index];
                    sprite.content_mask.bounds = sprite.content_mask.bounds.intersect(&clip);
                    if !bounds_empty(sprite.content_mask.bounds) {
                        let tile = tiles.get(tile_id).ok_or_else(|| {
                            anyhow::anyhow!("image references a missing CPU atlas tile")
                        })?;
                        Self::draw_polychrome_sprite(target, &sprite, tile)?;
                    }
                }
                ScenePrimitive::Unsupported => {
                    bail!("unsupported GPUI primitive batch in Kobo renderer")
                }
            }
        }

        Ok(())
    }

    fn draw_monochrome_sprite(
        target: &mut GrayImage,
        sprite: &MonochromeSprite,
        tile: &CpuTile,
    ) -> Result<()> {
        draw_mask(
            target,
            sprite.bounds,
            sprite.content_mask,
            sprite.color,
            tile,
            false,
            sprite.transformation,
        )
    }

    fn draw_subpixel_sprite(
        target: &mut GrayImage,
        sprite: &SubpixelSprite,
        tile: &CpuTile,
    ) -> Result<()> {
        draw_mask(
            target,
            sprite.bounds,
            sprite.content_mask,
            sprite.color,
            tile,
            true,
            sprite.transformation,
        )
    }

    fn draw_underline(target: &mut GrayImage, underline: &Underline) -> Result<()> {
        if !underline.wavy.get() {
            fill_rect(
                target,
                underline.bounds,
                underline.content_mask,
                underline.color,
            );
            return Ok(());
        }
        let Some((x0, y0, x1, y1)) = clipped_rect(target, underline.bounds, underline.content_mask)
        else {
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
        target: &mut GrayImage,
        sprite: &PolychromeSprite,
        tile: &CpuTile,
    ) -> Result<()> {
        let source_width = usize::try_from(tile.size.width.0.max(0))?;
        let source_height = usize::try_from(tile.size.height.0.max(0))?;
        let CpuTilePixels::LumaAlpha(pixels) = &tile.pixels else {
            bail!("polychrome atlas tile is not luminance-alpha");
        };
        if pixels.len() != source_width.saturating_mul(source_height).saturating_mul(2) {
            bail!("polychrome atlas tile has an invalid luminance-alpha length");
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

        let draw_left = left.max(mask_left);
        let draw_top = top.max(mask_top);
        let draw_right = right.min(mask_right);
        let draw_bottom = bottom.min(mask_bottom);
        let unscaled = sprite.bounds.origin.x.0.fract() == 0.0
            && sprite.bounds.origin.y.0.fract() == 0.0
            && sprite.bounds.size.width.0 == source_width as f32
            && sprite.bounds.size.height.0 == source_height as f32;
        if tile.opaque && sprite.opacity >= 1.0 && unscaled {
            let origin_x = sprite.bounds.origin.x.0 as i64;
            let origin_y = sprite.bounds.origin.y.0 as i64;
            for y in draw_top..draw_bottom {
                let source_y = (i64::from(y) - origin_y) as usize;
                for x in draw_left..draw_right {
                    let source_x = (i64::from(x) - origin_x) as usize;
                    let offset = (source_y * source_width + source_x) * 2;
                    target.get_pixel_mut(x, y).0[0] = pixels[offset];
                }
            }
            return Ok(());
        }

        const FIXED_ONE: i64 = 1 << 16;
        let source_x_step =
            ((source_width as f64 * FIXED_ONE as f64) / width as f64).round() as i64;
        let source_y_step =
            ((source_height as f64 * FIXED_ONE as f64) / height as f64).round() as i64;
        let source_x_start = ((((draw_left as f64 - sprite.bounds.origin.x.0 as f64)
            / width as f64)
            * source_width as f64)
            * FIXED_ONE as f64)
            .floor() as i64;
        let mut source_y_fixed = ((((draw_top as f64 - sprite.bounds.origin.y.0 as f64)
            / height as f64)
            * source_height as f64)
            * FIXED_ONE as f64)
            .floor() as i64;
        for y in draw_top..draw_bottom {
            let source_y = source_y_fixed
                .div_euclid(FIXED_ONE)
                .clamp(0, source_height.saturating_sub(1) as i64)
                as usize;
            let mut source_x_fixed = source_x_start;
            for x in draw_left..draw_right {
                let source_x = source_x_fixed
                    .div_euclid(FIXED_ONE)
                    .clamp(0, source_width.saturating_sub(1) as i64)
                    as usize;
                let offset = (source_y * source_width + source_x) * 2;
                let gray = pixels[offset];
                let alpha = f32::from(pixels[offset + 1]) / 255.0 * sprite.opacity.clamp(0.0, 1.0);
                blend_gray(target.get_pixel_mut(x, y), gray, alpha);
                source_x_fixed += source_x_step;
            }
            source_y_fixed += source_y_step;
        }
        Ok(())
    }
}

impl KoboRenderer {
    pub(crate) fn render_scene_to_image(
        &mut self,
        scene: &Scene,
        size: Size<DevicePixels>,
        previous: Option<&GrayImage>,
    ) -> Result<Option<(GrayFrame, PixelRect)>> {
        self.render_to_grayscale(scene, size, previous)
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
    pub image: GrayFrame,
    pub previous_image: Option<GrayFrame>,
    pub damage: PixelRect,
}

#[derive(Clone)]
pub struct GrayFrame(Arc<GrayFrameInner>);

struct GrayFrameInner {
    image: GrayImage,
    generation: u64,
    pool: Option<Weak<Mutex<Vec<FramePoolEntry>>>>,
}

impl GrayFrame {
    fn pooled(image: GrayImage, generation: u64, pool: &Arc<Mutex<Vec<FramePoolEntry>>>) -> Self {
        Self(Arc::new(GrayFrameInner {
            image,
            generation,
            pool: Some(Arc::downgrade(pool)),
        }))
    }

    pub fn unpooled(image: GrayImage) -> Self {
        Self(Arc::new(GrayFrameInner {
            image,
            generation: 0,
            pool: None,
        }))
    }

    pub(crate) fn image_mut(&mut self) -> &mut GrayImage {
        &mut Arc::get_mut(&mut self.0)
            .expect("a newly rendered Kobo frame must be uniquely owned")
            .image
    }
}

impl Deref for GrayFrame {
    type Target = GrayImage;

    fn deref(&self) -> &Self::Target {
        &self.0.image
    }
}

impl AsRef<GrayImage> for GrayFrame {
    fn as_ref(&self) -> &GrayImage {
        self
    }
}

impl Drop for GrayFrameInner {
    fn drop(&mut self) {
        let Some(pool) = self.pool.as_ref().and_then(Weak::upgrade) else {
            return;
        };
        let image = std::mem::replace(&mut self.image, GrayImage::new(0, 0));
        let mut pool = pool.lock();
        if pool.len() < 3 {
            pool.push(FramePoolEntry {
                image,
                generation: self.generation,
            });
        }
    }
}

pub fn changed_pixel_bounds(before: &GrayImage, after: &GrayImage) -> Option<PixelRect> {
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
    let width = after.width();
    for (index, (&old, &new)) in before.as_raw().iter().zip(after.as_raw()).enumerate() {
        if old != new {
            let index = index as u32;
            let x = index % width;
            let y = index / width;
            changed = true;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    changed.then_some(PixelRect {
        x: min_x,
        y: min_y,
        width: max_x - min_x + 1,
        height: max_y - min_y + 1,
    })
}

pub fn changed_pixel_bounds_in(
    before: &GrayImage,
    after: &GrayImage,
    candidate: PixelRect,
) -> Option<PixelRect> {
    if before.dimensions() != after.dimensions() {
        return Some(candidate);
    }
    let stride = after.width() as usize;
    let mut damage: Option<PixelRect> = None;
    for y in candidate.y..candidate.y + candidate.height {
        let start = y as usize * stride + candidate.x as usize;
        let end = start + candidate.width as usize;
        for (offset, (&old, &new)) in before.as_raw()[start..end]
            .iter()
            .zip(&after.as_raw()[start..end])
            .enumerate()
        {
            if old == new {
                continue;
            }
            let pixel = PixelRect {
                x: candidate.x + offset as u32,
                y,
                width: 1,
                height: 1,
            };
            damage = Some(damage.map_or(pixel, |damage| union_pixel_rect(damage, pixel)));
        }
    }
    damage
}

pub fn damage_image(image: &GrayImage, damage: PixelRect) -> GrayImage {
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
    let mut target = GrayImage::from_pixel(64, 64, Luma([255]));
    let bounds = Bounds {
        origin: point(ScaledPixels(8.0), ScaledPixels(8.0)),
        size: size(ScaledPixels(48.0), ScaledPixels(30.0)),
    };
    let mask = ContentMask {
        bounds: Bounds {
            origin: point(ScaledPixels(0.0), ScaledPixels(0.0)),
            size: size(ScaledPixels(64.0), ScaledPixels(64.0)),
        },
    };
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
    path.content_mask = ContentMask {
        bounds: Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(64.0), px(64.0)),
        },
    };
    draw_path(&mut target, &path.scale(1.0), None);

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
        wavy: true.into(),
    };
    KoboRenderer::draw_underline(&mut target, &underline)?;

    let painted_pixels = target.pixels().filter(|pixel| pixel.0[0] != 255).count();
    if painted_pixels < 500 || rounded_corner_pixels != 0 {
        bail!(
            "rectangular renderer coverage failed: painted={painted_pixels} unpainted_corners={rounded_corner_pixels}"
        );
    }
    Ok(RendererCoverage {
        painted_pixels,
        rounded_corner_pixels,
    })
}

/// Write a grayscale render as a binary PGM image.
pub fn write_pgm(image: &GrayImage, path: impl AsRef<Path>) -> Result<()> {
    let mut writer = BufWriter::new(File::create(path.as_ref())?);
    write!(writer, "P5\n{} {}\n255\n", image.width(), image.height())?;
    writer.write_all(image.as_raw())?;
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

fn draw_quad(target: &mut GrayImage, quad: &Quad) {
    let Some((x0, y0, x1, y1)) = clipped_rect(target, quad.bounds, quad.content_mask) else {
        return;
    };
    let (background_gray, background_alpha) = background_gray(quad.background);
    fill_clipped_gray(target, x0, y0, x1, y1, background_gray, background_alpha);
    let has_any_border = quad.border_widths.top.0 > 0.0
        || quad.border_widths.right.0 > 0.0
        || quad.border_widths.bottom.0 > 0.0
        || quad.border_widths.left.0 > 0.0;
    if !has_any_border {
        return;
    }
    let (border_gray, border_alpha) = grayscale(quad.border_color);
    let bounds_left = quad.bounds.origin.x.0;
    let bounds_top = quad.bounds.origin.y.0;
    let bounds_right = bounds_left + quad.bounds.size.width.0;
    let bounds_bottom = bounds_top + quad.bounds.size.height.0;
    let top_end = (bounds_top + quad.border_widths.top.0)
        .ceil()
        .clamp(y0 as f32, y1 as f32) as u32;
    let bottom_start = (bounds_bottom - quad.border_widths.bottom.0)
        .floor()
        .clamp(y0 as f32, y1 as f32) as u32;
    let left_end = (bounds_left + quad.border_widths.left.0)
        .ceil()
        .clamp(x0 as f32, x1 as f32) as u32;
    let right_start = (bounds_right - quad.border_widths.right.0)
        .floor()
        .clamp(x0 as f32, x1 as f32) as u32;

    fill_clipped_gray(target, x0, y0, x1, top_end, border_gray, border_alpha);
    fill_clipped_gray(
        target,
        x0,
        bottom_start.max(top_end),
        x1,
        y1,
        border_gray,
        border_alpha,
    );
    let middle_top = top_end;
    let middle_bottom = bottom_start.max(middle_top);
    fill_clipped_gray(
        target,
        x0,
        middle_top,
        left_end,
        middle_bottom,
        border_gray,
        border_alpha,
    );
    fill_clipped_gray(
        target,
        right_start.max(left_end),
        middle_top,
        x1,
        middle_bottom,
        border_gray,
        border_alpha,
    );
}

fn draw_shadow(_target: &mut GrayImage, _shadow: &Shadow) {
    // Shadows consume a large amount of CPU and add little value on e-ink.
    // Accept the primitive so GPUI scenes remain compatible, but omit its paint.
}

fn draw_path(
    target: &mut GrayImage,
    path: &GpuiPath<ScaledPixels>,
    extra_clip: Option<Bounds<ScaledPixels>>,
) {
    let content_bounds = extra_clip
        .map(|clip| path.content_mask.bounds.intersect(&clip))
        .unwrap_or(path.content_mask.bounds);
    for triangle in path.vertices.chunks_exact(3) {
        let a = triangle[0].xy_position;
        let b = triangle[1].xy_position;
        let c = triangle[2].xy_position;
        let min_x =
            a.x.0
                .min(b.x.0)
                .min(c.x.0)
                .floor()
                .max(content_bounds.origin.x.0)
                .max(0.0) as u32;
        let min_y =
            a.y.0
                .min(b.y.0)
                .min(c.y.0)
                .floor()
                .max(content_bounds.origin.y.0)
                .max(0.0) as u32;
        let max_x =
            a.x.0
                .max(b.x.0)
                .max(c.x.0)
                .ceil()
                .min(content_bounds.origin.x.0 + content_bounds.size.width.0)
                .min(target.width() as f32) as u32;
        let max_y =
            a.y.0
                .max(b.y.0)
                .max(c.y.0)
                .ceil()
                .min(content_bounds.origin.y.0 + content_bounds.size.height.0)
                .min(target.height() as f32) as u32;
        let denominator = (b.y.0 - c.y.0) * (a.x.0 - c.x.0) + (c.x.0 - b.x.0) * (a.y.0 - c.y.0);
        if denominator.abs() <= f32::EPSILON {
            continue;
        }
        for y in min_y..max_y {
            for x in min_x..max_x {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                if !content_bounds.contains(&point(ScaledPixels(px), ScaledPixels(py))) {
                    continue;
                }
                let wa =
                    ((b.y.0 - c.y.0) * (px - c.x.0) + (c.x.0 - b.x.0) * (py - c.y.0)) / denominator;
                let wb =
                    ((c.y.0 - a.y.0) * (px - c.x.0) + (a.x.0 - c.x.0) * (py - c.y.0)) / denominator;
                let wc = 1.0 - wa - wb;
                if wa < 0.0 || wb < 0.0 || wc < 0.0 {
                    continue;
                }
                let st_x = wa * triangle[0].st_position.x
                    + wb * triangle[1].st_position.x
                    + wc * triangle[2].st_position.x;
                let st_y = wa * triangle[0].st_position.y
                    + wb * triangle[1].st_position.y
                    + wc * triangle[2].st_position.y;
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
        return (
            ((0.2126 * red + 0.7152 * green + 0.0722 * blue) * 255.0).round() as u8,
            alpha,
        );
    }
    (255, 0.0)
}

fn fill_clipped_gray(
    target: &mut GrayImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    gray: u8,
    alpha: f32,
) {
    if alpha >= 1.0 {
        let stride = target.width() as usize;
        let width = (x1 - x0) as usize;
        for y in y0..y1 {
            let start = y as usize * stride + x0 as usize;
            target.as_mut()[start..start + width].fill(gray);
        }
        return;
    }
    for y in y0..y1 {
        for x in x0..x1 {
            blend_gray(target.get_pixel_mut(x, y), gray, alpha);
        }
    }
}

fn fill_rect(
    target: &mut GrayImage,
    bounds: Bounds<gpui::ScaledPixels>,
    mask: ContentMask<gpui::ScaledPixels>,
    color: Hsla,
) {
    let Some((x0, y0, x1, y1)) = clipped_rect(target, bounds, mask) else {
        return;
    };
    let (gray, alpha) = grayscale(color);
    fill_clipped_gray(target, x0, y0, x1, y1, gray, alpha);
}

fn draw_mask(
    target: &mut GrayImage,
    bounds: Bounds<gpui::ScaledPixels>,
    mask: ContentMask<gpui::ScaledPixels>,
    color: Hsla,
    tile: &CpuTile,
    _rgba_mask: bool,
    transformation: TransformationMatrix,
) -> Result<()> {
    const GLYPH_COVERAGE_THRESHOLD: u8 = 96;

    // GPUI may position glyph sprites on fractional scaled pixels. Fractional
    // placement is useful with antialiasing, but produces unstable gray edges
    // and wider damage on an e-ink display. Snap translation-only glyphs to
    // the CPU framebuffer before sampling their atlas mask.
    let mut transformation = transformation;
    let translation_only = transformation.rotation_scale == [[1.0, 0.0], [0.0, 1.0]];
    if translation_only {
        transformation.translation[0] =
            (bounds.origin.x.0 + transformation.translation[0]).round() - bounds.origin.x.0;
        transformation.translation[1] =
            (bounds.origin.y.0 + transformation.translation[1]).round() - bounds.origin.y.0;
    }
    let transformed = transform_bounds(bounds, transformation);
    let Some((x0, y0, x1, y1)) = clipped_rect(target, transformed, mask) else {
        return Ok(());
    };
    let source_width = usize::try_from(tile.size.width.0.max(0))?;
    let source_height = usize::try_from(tile.size.height.0.max(0))?;
    if source_width == 0 || source_height == 0 {
        return Ok(());
    }
    let CpuTilePixels::Mask(mask_pixels) = &tile.pixels else {
        bail!("glyph atlas tile is not a coverage mask");
    };
    let translated_x = bounds.origin.x.0 + transformation.translation[0];
    let translated_y = bounds.origin.y.0 + transformation.translation[1];
    let aligned_x = translated_x.round();
    let aligned_y = translated_y.round();
    let one_to_one = (bounds.size.width.0 - source_width as f32).abs() < 0.001
        && (bounds.size.height.0 - source_height as f32).abs() < 0.001;
    if translation_only
        && one_to_one
        && (translated_x - aligned_x).abs() < 0.001
        && (translated_y - aligned_y).abs() < 0.001
    {
        let origin_x = aligned_x as i32;
        let origin_y = aligned_y as i32;
        let (gray, color_alpha) = grayscale(color);
        let color_alpha = (color_alpha.clamp(0.0, 1.0) * 255.0).round() as u16;
        let target_stride = target.width() as usize;
        for y in y0..y1 {
            let source_y = (y as i32 - origin_y) as usize;
            let source_x = (x0 as i32 - origin_x) as usize;
            let source_start = source_y * source_width + source_x;
            let target_start = y as usize * target_stride + x0 as usize;
            let width = (x1 - x0) as usize;
            let source_row = &mask_pixels[source_start..source_start + width];
            let target_row = &mut target.as_mut()[target_start..target_start + width];
            for (pixel, &coverage) in target_row.iter_mut().zip(source_row) {
                let coverage: u8 = if coverage >= GLYPH_COVERAGE_THRESHOLD {
                    255
                } else {
                    0
                };
                let alpha = ((u16::from(coverage) * color_alpha + 127) / 255) as u8;
                blend_gray_channel(pixel, gray, alpha);
            }
        }
        return Ok(());
    }
    let inverse = invert_transform(transformation)
        .ok_or_else(|| anyhow::anyhow!("glyph transformation is singular"))?;
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
            let source_x = (((sample_x - bounds.origin.x.0) / target_width) * source_width as f32)
                .floor()
                .clamp(0.0, (source_width - 1) as f32) as usize;
            let source_y = (((sample_y - bounds.origin.y.0) / target_height) * source_height as f32)
                .floor()
                .clamp(0.0, (source_height - 1) as f32) as usize;
            let pixel_index = source_y * source_width + source_x;
            let coverage = if mask_pixels[pixel_index] >= GLYPH_COVERAGE_THRESHOLD {
                1.0
            } else {
                0.0
            };
            blend_gray(target.get_pixel_mut(x, y), gray, color_alpha * coverage);
        }
    }
    Ok(())
}

fn transform_bounds(
    bounds: Bounds<ScaledPixels>,
    transform: TransformationMatrix,
) -> Bounds<ScaledPixels> {
    let points = [
        apply_transform(transform, bounds.origin.x.0, bounds.origin.y.0),
        apply_transform(
            transform,
            bounds.origin.x.0 + bounds.size.width.0,
            bounds.origin.y.0,
        ),
        apply_transform(
            transform,
            bounds.origin.x.0,
            bounds.origin.y.0 + bounds.size.height.0,
        ),
        apply_transform(
            transform,
            bounds.origin.x.0 + bounds.size.width.0,
            bounds.origin.y.0 + bounds.size.height.0,
        ),
    ];
    let min_x = points
        .iter()
        .map(|point| point.0)
        .fold(f32::INFINITY, f32::min);
    let min_y = points
        .iter()
        .map(|point| point.1)
        .fold(f32::INFINITY, f32::min);
    let max_x = points
        .iter()
        .map(|point| point.0)
        .fold(f32::NEG_INFINITY, f32::max);
    let max_y = points
        .iter()
        .map(|point| point.1)
        .fold(f32::NEG_INFINITY, f32::max);
    Bounds {
        origin: point(ScaledPixels(min_x), ScaledPixels(min_y)),
        size: size(ScaledPixels(max_x - min_x), ScaledPixels(max_y - min_y)),
    }
}

fn apply_transform(transform: TransformationMatrix, x: f32, y: f32) -> (f32, f32) {
    (
        transform.translation[0]
            + transform.rotation_scale[0][0] * x
            + transform.rotation_scale[0][1] * y,
        transform.translation[1]
            + transform.rotation_scale[1][0] * x
            + transform.rotation_scale[1][1] * y,
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
        -(rotation_scale[0][0] * transform.translation[0]
            + rotation_scale[0][1] * transform.translation[1]),
        -(rotation_scale[1][0] * transform.translation[0]
            + rotation_scale[1][1] * transform.translation[1]),
    ];
    Some(TransformationMatrix {
        rotation_scale,
        translation,
    })
}

fn clipped_rect(
    target: &GrayImage,
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
    let luminance = 0.2126 * rgba[0] as f32 + 0.7152 * rgba[1] as f32 + 0.0722 * rgba[2] as f32;
    (
        luminance.round().clamp(0.0, 255.0) as u8,
        rgba[3] as f32 / 255.0 * opacity.clamp(0.0, 1.0),
    )
}

fn blend_gray(pixel: &mut Luma<u8>, gray: u8, alpha: f32) {
    let alpha = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    blend_gray_channel(&mut pixel.0[0], gray, alpha);
}

fn blend_gray_channel(pixel: &mut u8, gray: u8, alpha: u8) {
    let alpha = u16::from(alpha);
    if alpha == 0 {
        return;
    }
    if alpha == 255 {
        *pixel = gray;
        return;
    }
    let inverse = 255 - alpha;
    *pixel = ((u16::from(gray) * alpha + u16::from(*pixel) * inverse + 127) / 255) as u8;
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
