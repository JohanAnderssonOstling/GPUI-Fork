use anyhow::{Context as _, Ok, Result};
use collections::HashMap;
use cosmic_text::{
    Attrs, AttrsList, Ellipsize, Family, Font as CosmicTextFont,
    FontFeatures as CosmicFontFeatures, FontSystem, FontVariations as CosmicFontVariations,
    GlyphDecorationData, LayoutLine as CosmicLayoutLine, ShapeBuffer, ShapeGlyph, ShapeLine,
    ShapeSpan, ShapeWord, Style, Weight,
    fontdb::{Database, ID as DatabaseFontId, Source as FontSource},
};
use gpui::{
    Bounds, DevicePixels, Font, FontFallbacks, FontFeatures, FontId, FontMetrics, FontRun,
    FontPalette, FontPaletteBase, FontStyle, GlyphId, LineLayout, Pixels, PlatformTextSystem, RenderGlyphParams,
    SUBPIXEL_VARIANTS_X, SUBPIXEL_VARIANTS_Y, ShapedCluster, ShapedGlyph, ShapedRun, SharedString,
    Size, TextRenderingMode, point, px, size,
};

#[cfg(target_os = "linux")]
use libc::{madvise, sysconf, MADV_DONTNEED, _SC_PAGESIZE};
use parking_lot::RwLock;
use smallvec::SmallVec;
use std::{
    borrow::Cow,
    mem::{size_of, take},
    ops::Range,
    sync::Arc,
};
use swash::{
    scale::{Render, ScaleContext, Source, StrikeWith},
    zeno::{Format, Transform, Vector},
};
use unicode_segmentation::UnicodeSegmentation;

const MAX_LAYOUT_SCRATCH_GLYPHS: usize = 4096;
const MAX_SHAPE_SCRATCH_BYTES: usize = 256 * 1024;

pub struct CosmicTextSystem(RwLock<CosmicTextSystemState>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FontKey {
    family: SharedString,
    features: FontFeatures,
    fallbacks: Option<FontFallbacks>,
}

impl FontKey {
    fn new(family: SharedString, features: FontFeatures, fallbacks: Option<FontFallbacks>) -> Self {
        Self {
            family,
            features,
            fallbacks,
        }
    }
}

struct CosmicTextSystemState {
    font_system: FontSystem,
    scratch: ShapeBuffer,
    layout_lines: Vec<CosmicLayoutLine>,
    cluster_scratch: Vec<ShapedCluster>,
    shape_line: Option<ShapeLine>,
    swash_scale_context: ScaleContext,
    // Preserve IDs held by layouts when the corresponding font data is released.
    loaded_fonts: Vec<LoadedFont>,
    /// Caches the `FontId`s associated with a specific family to avoid iterating the font database
    /// for every font face in a family.
    font_ids_by_family_cache: HashMap<FontKey, SmallVec<[FontId; 4]>>,
    font_instances: HashMap<(FontId, Weight, CosmicFontVariations, Option<u32>, Option<FontPalette>, bool), FontId>,
    system_font_fallback: String,
    allow_system_fonts: bool,
    system_fonts_loaded: bool,
}

#[derive(Clone)]
struct LoadedFont {
    database_id: DatabaseFontId,
    weight: Weight,
    style: Style,
    variations: CosmicFontVariations,
    oblique_angle: Option<f32>,
    slant_axis: bool,
    metrics: Option<FontMetrics>,
    features: CosmicFontFeatures,
    is_known_emoji_font: bool,
    is_color_font: bool,
    palette: Option<FontPalette>,
    /// resolved at load time so `layout_line` shares one chain across faces.
    /// `Arc` keeps clone cheap on the per-run hot path.
    user_fallback_chain: Arc<[(FontId, SharedString)]>,
}

impl CosmicTextSystem {
    pub fn new(system_font_fallback: &str) -> Self {
        let system = Self::new_without_system_fonts(system_font_fallback);
        {
            let mut state = system.0.write();
            let mut database = Database::new();
            database.set_monospace_family("Noto Sans Mono");
            database.set_sans_serif_family("Open Sans");
            database.set_serif_family("DejaVu Serif");
            let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".to_owned());
            state.font_system = FontSystem::new_with_locale_and_db(locale, database);
            state.allow_system_fonts = true;
        }
        system
    }

    pub fn new_without_system_fonts(system_font_fallback: &str) -> Self {
        let font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), Database::new());

        Self(RwLock::new(CosmicTextSystemState {
            font_system,
            scratch: ShapeBuffer::default(),
            layout_lines: Vec::new(),
            cluster_scratch: Vec::new(),
            shape_line: None,
            swash_scale_context: ScaleContext::new(),
            loaded_fonts: Vec::new(),
            font_instances: HashMap::default(),
            font_ids_by_family_cache: HashMap::default(),
            system_font_fallback: system_font_fallback.to_string(),
            allow_system_fonts: false,
            system_fonts_loaded: false,
        }))
    }
}

impl PlatformTextSystem for CosmicTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.write().add_fonts(fonts)
    }

    fn all_font_names(&self) -> Vec<String> {
        let mut names = self.registered_font_names();
        let allow_system_fonts = self.0.read().allow_system_fonts;
        if allow_system_fonts {
            let mut catalog = Database::new();
            catalog.load_system_fonts();
            names.extend(
                catalog
                    .faces()
                    .filter_map(|face| face.families.first().map(|family| family.0.clone())),
            );
        }
        names.sort_unstable();
        names.dedup();
        names
    }

    fn registered_font_names(&self) -> Vec<String> {
        self.0
            .read()
            .font_system
            .db()
            .faces()
            .filter_map(|face| face.families.first().map(|family| family.0.clone()))
            .collect()
    }

    fn release_font_data(&self) {
        self.0.write().release_font_data();
    }

    fn release_shaping_scratch(&self) {
        self.0.write().release_shaping_scratch();
    }

    fn recycle_line_clusters(&self, clusters: Vec<ShapedCluster>) {
        self.0.write().recycle_line_clusters(clusters);
    }

    fn font_supports_any_text(&self, descriptor: &Font, samples: &[&str]) -> bool {
        let mut state = self.0.write();
        let name = gpui::font_name_with_fallbacks(&descriptor.family, &state.system_font_fallback)
            .to_owned();
        if !state
            .font_system
            .db()
            .faces()
            .any(|face| face.families.iter().any(|family| family.0 == name))
        {
            state.ensure_system_fonts();
        }
        let families = [Family::Name(&name)];
        let query = cosmic_text::fontdb::Query {
            families: &families,
            weight: cosmic_text::Weight(descriptor.weight.0 as u16),
            stretch: closest_stretch(descriptor.variations.stretch()),
            style: match descriptor.style {
                gpui::FontStyle::Normal => cosmic_text::Style::Normal,
                gpui::FontStyle::Italic => cosmic_text::Style::Italic,
                gpui::FontStyle::Oblique => cosmic_text::Style::Oblique,
            },
        };
        let database = state.font_system.db();
        database
            .query(&query)
            .and_then(|id| {
                database.with_face_data(id, |data, index| {
                    swash::FontRef::from_index(data, index as usize).is_some_and(|font| {
                        let charmap = font.charmap();
                        samples.iter().any(|sample| {
                            sample.chars().all(|character| charmap.map(character) != 0)
                        })
                    })
                })
            })
            .unwrap_or(false)
    }

    fn font_id(&self, font: &Font) -> Result<FontId> {
        let mut state = self.0.write();
        let key = FontKey::new(
            font.family.clone(),
            font.features.clone(),
            font.fallbacks.clone(),
        );
        let candidates = if let Some(font_ids) = state.font_ids_by_family_cache.get(&key) {
            font_ids.as_slice()
        } else {
            let font_ids =
                state.load_family(&font.family, &font.features, font.fallbacks.as_ref())?;
            state.font_ids_by_family_cache.insert(key.clone(), font_ids);
            state.font_ids_by_family_cache[&key].as_ref()
        };

        let ix = find_best_match(font, candidates, &state)?;

        let base_id = candidates[ix];
        let font_id = state.font_instance(base_id, font);
        state.font(font_id)?;
        Ok(font_id)
    }

    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        let mut state = self.0.write();
        if let Some(metrics) = state.loaded_font(font_id).metrics {
            return metrics;
        }
        match state.font(font_id) {
            std::result::Result::Ok(font) => {
                metrics_for_font(&font, state.loaded_font(font_id).oblique_angle, state.loaded_font(font_id).slant_axis)
            }
            Err(error) => {
                log::warn!("failed to reload font metrics: {error:#}");
                FontMetrics {
                    units_per_em: 1,
                    ascent: 0.0,
                    descent: 0.0,
                    line_gap: 0.0,
                    underline_position: 0.0,
                    underline_thickness: 0.0,
                    cap_height: 0.0,
                    x_height: 0.0,
                    bounding_box: Bounds::default(),
                }
            }
        }
    }

    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        let font = self.0.write().font(font_id)?;
        let glyph_metrics = font.as_swash().glyph_metrics(font.normalized_coords());
        let glyph_id = glyph_id.0 as u16;
        Ok(Bounds {
            origin: point(0.0, 0.0),
            size: size(
                glyph_metrics.advance_width(glyph_id),
                glyph_metrics.advance_height(glyph_id),
            ),
        })
    }

    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        self.0.write().advance(font_id, glyph_id)
    }

    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.write().glyph_for_char(font_id, ch)
    }

    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        self.0.write().raster_bounds(params)
    }

    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        self.0.write().rasterize_glyph(params, raster_bounds)
    }

    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        self.0.write().layout_line(text, font_size, runs)
    }

    fn layout_line_reusing_clusters(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        let mut state = self.0.write();
        let clusters = take(&mut state.cluster_scratch);
        state.layout_line_with_cluster_buffer(text, font_size, runs, clusters)
    }

    fn recommended_rendering_mode(
        &self,
        _font_id: FontId,
        _font_size: Pixels,
    ) -> TextRenderingMode {
        TextRenderingMode::Subpixel
    }
}

impl CosmicTextSystemState {
    fn loaded_font(&self, font_id: FontId) -> &LoadedFont {
        &self.loaded_fonts[font_id.0]
    }

    fn font(&mut self, font_id: FontId) -> Result<Arc<CosmicTextFont>> {
        let loaded = self.loaded_font(font_id);
        let (database_id, weight, variations, angle, slant_axis) = (
            loaded.database_id,
            loaded.weight,
            loaded.variations,
            loaded.oblique_angle,
            loaded.slant_axis,
        );
        let font = self
            .font_system
            .get_font_with_variations(database_id, weight, variations)
            .context("could not load font data")?;
        self.loaded_fonts[font_id.0].is_color_font = is_color_font(&font);
        self.loaded_fonts[font_id.0]
            .metrics
            .get_or_insert_with(|| metrics_for_font(&font, angle, slant_axis));
        Ok(font)
    }

    fn font_instance(&mut self, base_id: FontId, descriptor: &Font) -> FontId {
        let weight = Weight(descriptor.weight.0 as u16);
        let slant_axis = descriptor.variations.slant_axis();
        let angle = match descriptor.style {
            FontStyle::Oblique => Some(descriptor.variations.oblique_angle().unwrap_or(14.0)),
            FontStyle::Italic if self.loaded_font(base_id).style == Style::Normal => Some(14.0),
            _ => None,
        };
        let variations = CosmicFontVariations::new(
            Some(descriptor.variations.stretch()),
            if !slant_axis { Some(0.0) } else { match descriptor.style {
                FontStyle::Normal => Some(0.0),
                FontStyle::Oblique => angle.map(|angle| -angle),
                FontStyle::Italic => None,
            } },
            Some(if slant_axis && descriptor.style == FontStyle::Italic {
                1.0
            } else {
                0.0
            }),
        );
        let angle = angle.filter(|_| descriptor.variations.synthesis());
        let key = (base_id, weight, variations, angle.map(f32::to_bits), descriptor.palette.clone(), slant_axis);
        if let Some(id) = self.font_instances.get(&key) {
            return *id;
        }
        let mut loaded = self.loaded_font(base_id).clone();
        loaded.weight = weight;
        loaded.style = match descriptor.style {
            FontStyle::Normal => Style::Normal,
            FontStyle::Italic => Style::Italic,
            FontStyle::Oblique => Style::Oblique,
        };
        loaded.variations = variations;
        loaded.oblique_angle = angle;
        loaded.slant_axis = slant_axis;
        loaded.metrics = None;
        loaded.palette = descriptor.palette.clone();
        let id = FontId(self.loaded_fonts.len());
        self.loaded_fonts.push(loaded);
        self.font_instances.insert(key, id);
        id
    }

    fn ensure_system_fonts(&mut self) {
        if !self.allow_system_fonts || self.system_fonts_loaded {
            return;
        }
        self.font_system.db_mut().load_system_fonts();
        self.system_fonts_loaded = true;
        self.rebuild_font_system();
    }

    fn rebuild_font_system(&mut self) {
        let locale = self.font_system.locale().to_owned();
        // Database clones preserve IDs referenced by shaped lines and the GPUI caches.
        let database = self.font_system.db().clone();
        self.font_system = FontSystem::new_with_locale_and_db(locale, database);
    }

    fn release_font_data(&mut self) {
        self.shape_line = None;
        self.scratch = ShapeBuffer::default();
        self.layout_lines.clear();
        self.swash_scale_context = ScaleContext::new();
        let database = self.font_system.db_mut();
        let shared: Vec<_> = database
            .faces()
            .filter_map(|face| matches!(face.source, FontSource::SharedFile(..)).then_some(face.id))
            .collect();
        for id in shared {
            database.make_face_data_unshared(id);
        }
        self.rebuild_font_system();
    }

    #[profiling::function]
    fn add_fonts(&mut self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        let db = self.font_system.db_mut();
        for bytes in fonts {
            db.load_font_source(FontSource::Binary(Arc::new(bytes)));
        }
        Ok(())
    }

    #[profiling::function]
    fn load_family(
        &mut self,
        name: &str,
        features: &FontFeatures,
        fallbacks: Option<&FontFallbacks>,
    ) -> Result<SmallVec<[FontId; 4]>> {
        // recurse with `fallbacks = None` so a fallback family cannot pull in
        // another chain. missing fallback families are dropped so a typo in
        // settings still lets the primary family load.
        let user_fallback_chain: Arc<[(FontId, SharedString)]> = match fallbacks {
            Some(fallbacks) if !fallbacks.fallback_list().is_empty() => {
                let mut chain: Vec<(FontId, SharedString)> = Vec::new();
                for fallback_name in fallbacks.fallback_list() {
                    let fb_key = FontKey::new(
                        SharedString::from(fallback_name.clone()),
                        features.clone(),
                        None,
                    );
                    let fb_ids = if let Some(cached) = self.font_ids_by_family_cache.get(&fb_key) {
                        cached.clone()
                    } else {
                        let loaded = self.load_family(fallback_name, features, None)?;
                        self.font_ids_by_family_cache
                            .insert(fb_key.clone(), loaded.clone());
                        loaded
                    };
                    let Some(&fb_id) = fb_ids.first() else {
                        continue;
                    };
                    let db_id = self.loaded_fonts[fb_id.0].database_id;
                    if let Some(face) = self.font_system.db().face(db_id)
                        && let Some(family) = face.families.first()
                    {
                        chain.push((fb_id, SharedString::from(family.0.clone())));
                    }
                }
                Arc::from(chain)
            }
            _ => Arc::from(Vec::new()),
        };

        let name = gpui::font_name_with_fallbacks(name, &self.system_font_fallback).to_owned();
        if !self
            .font_system
            .db()
            .faces()
            .any(|face| face.families.iter().any(|family| name == family.0))
        {
            self.ensure_system_fonts();
        }

        let families = self
            .font_system
            .db()
            .faces()
            .filter(|face| face.families.iter().any(|family| name == family.0))
            .map(|face| {
                (
                    face.id,
                    face.post_script_name.clone(),
                    face.weight,
                    face.style,
                )
            })
            .collect::<SmallVec<[_; 4]>>();

        let cosmic_features = cosmic_font_features(features)?;

        let mut loaded_font_ids = SmallVec::new();
        for (font_id, postscript_name, weight, style) in families {
            let database_id = font_id;
            let font_id = FontId(self.loaded_fonts.len());
            loaded_font_ids.push(font_id);
            self.loaded_fonts.push(LoadedFont {
                database_id,
                weight,
                style,
                variations: CosmicFontVariations::default(),
                oblique_angle: None,
                slant_axis: true,
                metrics: None,
                features: cosmic_features.clone(),
                is_known_emoji_font: check_is_known_emoji_font(&postscript_name),
                is_color_font: false,
                palette: None,
                user_fallback_chain: Arc::clone(&user_fallback_chain),
            });
        }

        Ok(loaded_font_ids)
    }

    fn advance(&mut self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let font = self.font(font_id)?;
        let glyph_metrics = font.as_swash().glyph_metrics(font.normalized_coords());
        Ok(Size {
            width: glyph_metrics.advance_width(glyph_id.0 as u16),
            height: glyph_metrics.advance_height(glyph_id.0 as u16),
        })
    }

    fn glyph_for_char(&mut self, font_id: FontId, ch: char) -> Option<GlyphId> {
        let font = self.font(font_id).ok()?;
        let glyph_id = font.as_swash().charmap().map(ch);
        if glyph_id == 0 {
            None
        } else {
            Some(GlyphId(glyph_id.into()))
        }
    }

    fn raster_bounds(&mut self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let image = self.render_glyph_image(params)?;
        Ok(Bounds {
            origin: point(image.placement.left.into(), (-image.placement.top).into()),
            size: size(image.placement.width.into(), image.placement.height.into()),
        })
    }

    #[profiling::function]
    fn rasterize_glyph(
        &mut self,
        params: &RenderGlyphParams,
        glyph_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        if glyph_bounds.size.width.0 == 0 || glyph_bounds.size.height.0 == 0 {
            anyhow::bail!("glyph bounds are empty");
        }

        let mut image = self.render_glyph_image(params)?;
        let bitmap_size = glyph_bounds.size;
        match image.content {
            swash::scale::image::Content::Color | swash::scale::image::Content::SubpixelMask => {
                // Convert from RGBA to BGRA.
                for pixel in image.data.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
                Ok((bitmap_size, image.data))
            }
            swash::scale::image::Content::Mask => {
                if params.is_emoji {
                    let [r, g, b, alpha] = params.foreground_color.unwrap_or([0, 0, 0, 255]);
                    let expanded = image.data.iter().flat_map(|coverage| {
                        let a = (u16::from(*coverage) * u16::from(alpha) + 127) / 255;
                        [(u16::from(b) * a / 255) as u8, (u16::from(g) * a / 255) as u8,
                            (u16::from(r) * a / 255) as u8, a as u8]
                    }).collect();
                    Ok((bitmap_size, expanded))
                } else if params.subpixel_rendering {
                    // We must always return RGBA data when subpixel rendering is requested.
                    let expanded = image.data.iter().flat_map(|&a| [a, a, a, a]).collect();
                    Ok((bitmap_size, expanded))
                } else {
                    Ok((bitmap_size, image.data))
                }
            }
        }
    }

    fn render_glyph_image(
        &mut self,
        params: &RenderGlyphParams,
    ) -> Result<swash::scale::image::Image> {
        let font = self.font(params.font_id)?;
        let font_ref = font.as_swash();
        let pixel_size = f32::from(params.font_size);
        let loaded = self.loaded_font(params.font_id);
        let shear = oblique_shear(&font, loaded.oblique_angle, loaded.slant_axis);
        let palette = loaded.palette.clone();
        let family_palette = palette.as_ref().and_then(|palette| {
            let face = self.font_system.db().face(loaded.database_id)?;
            palette.families.iter().rev().find(|palette| {
                face.families.iter().any(|(family, _)| {
                    family.eq_ignore_ascii_case(palette.family.as_ref())
                })
            })
        });
        let base = family_palette.map_or_else(
            || palette.as_ref().map_or(FontPaletteBase::Normal, |palette| palette.base),
            |palette| palette.base,
        );
        let colors = family_palette.map_or_else(
            || palette.as_ref().map_or(&[][..], |palette| palette.colors.as_ref()),
            |palette| palette.colors.as_ref(),
        );
        let palette_index = palette_index(&font, base);

        let subpixel_offset = Vector::new(
            params.subpixel_variant.x as f32 / SUBPIXEL_VARIANTS_X as f32 / params.scale_factor,
            params.subpixel_variant.y as f32 / SUBPIXEL_VARIANTS_Y as f32 / params.scale_factor,
        );

        let mut scaler = self
            .swash_scale_context
            .builder(font_ref)
            .size(pixel_size * params.scale_factor)
            // Color layers share authored geometry. Automatic hinting derives
            // alignment zones from the cmap, so Unicode subsets of the same
            // color font can otherwise move their layers by different amounts.
            .hint(!params.is_emoji)
            .normalized_coords(font.normalized_coords().iter().copied())
            .build();

        let sources: &[Source] = if shear != 0.0 {
            // Bitmap strikes cannot be transformed by Swash.
            if params.is_emoji {
                &[Source::ColorOutline(palette_index), Source::Outline]
            } else {
                &[Source::Outline]
            }
        } else if params.is_emoji {
            &[
                Source::ColorOutline(palette_index),
                Source::ColorBitmap(StrikeWith::BestFit),
                Source::Outline,
            ]
        } else {
            &[Source::Bitmap(StrikeWith::ExactSize), Source::Outline]
        };

        let mut renderer = Render::new(sources);
        renderer.default_color(params.foreground_color.unwrap_or([0, 0, 0, 255]));
        renderer.palette_overrides(colors);
        if shear != 0.0 {
            renderer.transform(Some(Transform {
                xx: 1.0,
                xy: 0.0,
                yx: shear,
                yy: 1.0,
                x: 0.0,
                y: 0.0,
            }));
        }
        if params.subpixel_rendering {
            // There seems to be a bug in Swash where the B and R values are swapped.
            renderer
                .format(Format::subpixel_bgra())
                .offset(subpixel_offset);
        } else {
            renderer.format(Format::Alpha).offset(subpixel_offset);
        }

        let glyph_id: u16 = params.glyph_id.0.try_into()?;
        renderer
            .render(&mut scaler, glyph_id)
            .with_context(|| format!("unable to render glyph via swash for {params:?}"))
    }

    /// This is used when cosmic_text has chosen a fallback font instead of using the requested
    /// font, typically to handle some unicode characters. When this happens, `loaded_fonts` may not
    /// yet have an entry for this fallback font, and so one is added.
    ///
    /// Note that callers shouldn't use this `FontId` somewhere that will retrieve the corresponding
    /// `LoadedFont.features`, as it will have an arbitrarily chosen or empty value. The only
    /// current use of this field is for the *input* of `layout_line`, and so it's fine to use
    /// `font_id_for_cosmic_id` when computing the *output* of `layout_line`.
    fn font_id_for_cosmic_id(
        &mut self,
        id: DatabaseFontId,
        weight: Weight,
        variations: CosmicFontVariations,
        angle: Option<f32>,
        palette: Option<FontPalette>,
        slant_axis: bool,
    ) -> Result<FontId> {
        if let Some(ix) = self.loaded_fonts.iter().position(|loaded| {
            loaded.database_id == id
                && loaded.weight == weight
                && loaded.variations == variations
                && loaded.oblique_angle == angle
                && loaded.palette == palette
                && loaded.slant_axis == slant_axis
        }) {
            Ok(FontId(ix))
        } else {
            let font = self
                .font_system
                .get_font_with_variations(id, weight, variations)
                .context("failed to get fallback font from cosmic-text font system")?;
            let face = self
                .font_system
                .db()
                .face(id)
                .context("fallback font face not found in cosmic-text database")?;

            let font_id = FontId(self.loaded_fonts.len());
            self.loaded_fonts.push(LoadedFont {
                database_id: id,
                weight,
                style: face.style,
                variations,
                oblique_angle: angle,
                slant_axis,
                metrics: Some(metrics_for_font(&font, angle, slant_axis)),
                features: CosmicFontFeatures::new(),
                is_known_emoji_font: check_is_known_emoji_font(&face.post_script_name),
                is_color_font: is_color_font(&font),
                palette,
                user_fallback_chain: Arc::from(Vec::new()),
            });

            Ok(font_id)
        }
    }

    #[profiling::function]
    fn layout_line(&mut self, text: &str, font_size: Pixels, font_runs: &[FontRun]) -> LineLayout {
        self.layout_line_with_cluster_buffer(text, font_size, font_runs, Vec::new())
    }

    fn layout_line_with_cluster_buffer(
        &mut self,
        text: &str,
        font_size: Pixels,
        font_runs: &[FontRun],
        mut clusters: Vec<ShapedCluster>,
    ) -> LineLayout {
        clusters.clear();
        let mut layout = if contains_paragraph_separator(text) {
            self.layout_line_with_separators(text, font_size, font_runs, clusters)
        } else {
            self.layout_line_no_separators(text, font_size, font_runs, clusters)
        };
        if self.allow_system_fonts
            && !self.system_fonts_loaded
            && layout
                .runs
                .iter()
                .any(|run| run.glyphs.iter().any(|glyph| glyph.id.0 == 0))
        {
            self.ensure_system_fonts();
            let mut clusters = take(&mut layout.clusters);
            clusters.clear();
            layout = if contains_paragraph_separator(text) {
                self.layout_line_with_separators(text, font_size, font_runs, clusters)
            } else {
                self.layout_line_no_separators(text, font_size, font_runs, clusters)
            };
        }
        layout
    }

    fn layout_line_with_separators(
        &mut self,
        text: &str,
        font_size: Pixels,
        font_runs: &[FontRun],
        clusters: Vec<ShapedCluster>,
    ) -> LineLayout {
        let mut layout = LineLayout {
            font_size,
            clusters,
            len: text.len(),
            ..Default::default()
        };
        let mut paragraph_start = 0;
        let mut segment_clusters = Vec::new();

        for (separator_start, separator) in text
            .char_indices()
            .filter(|(_, character)| is_paragraph_separator(*character))
        {
            let separator_end = separator_start + separator.len_utf8();
            self.shape_segment(
                text,
                paragraph_start..separator_start,
                font_size,
                font_runs,
                &mut layout,
                &mut segment_clusters,
            );
            // Paragraph delimiters have byte positions but no ink or advance.
            // Shaping them can trigger unrelated missing-glyph font fallback.
            layout.clusters.push(ShapedCluster {
                bytes: separator_start..separator_end,
                x: layout.width,
                advance: Pixels::ZERO,
                is_rtl: false,
            });
            paragraph_start = separator_end;
        }

        self.shape_segment(
            text,
            paragraph_start..text.len(),
            font_size,
            font_runs,
            &mut layout,
            &mut segment_clusters,
        );

        layout
    }

    fn shape_segment(
        &mut self,
        text: &str,
        range: Range<usize>,
        font_size: Pixels,
        font_runs: &[FontRun],
        layout: &mut LineLayout,
        segment_clusters: &mut Vec<ShapedCluster>,
    ) {
        if range.is_empty() {
            return;
        }

        let segment_font_runs = clip_font_runs(font_runs, range.clone());
        let mut segment = self.layout_line_no_separators(
            &text[range.clone()], font_size, &segment_font_runs, take(segment_clusters),
        );

        layout
            .clusters
            .extend(segment.clusters.drain(..).map(|mut cluster| {
                cluster.bytes = cluster.bytes.start + range.start..cluster.bytes.end + range.start;
                cluster.x += layout.width;
                cluster
            }));
        *segment_clusters = segment.clusters;

        let mut segment_runs = segment.runs;
        for run in &mut segment_runs {
            for glyph in &mut run.glyphs {
                glyph.index += range.start;
                glyph.position.x += layout.width;
            }
        }

        for mut run in segment_runs {
            if let Some(same_run) = layout
                .runs
                .last_mut()
                .filter(|last| last.font_id == run.font_id)
            {
                same_run.glyphs.append(&mut run.glyphs);
            } else {
                layout.runs.push(run);
            }
        }

        layout.width += segment.width;
        layout.ascent = layout.ascent.max(segment.ascent);
        layout.descent = layout.descent.max(segment.descent);
    }

    fn layout_line_no_separators(
        &mut self,
        text: &str,
        font_size: Pixels,
        font_runs: &[FontRun],
        mut clusters: Vec<ShapedCluster>,
    ) -> LineLayout {
        clusters.clear();
        let mut attrs_list = AttrsList::new(&Attrs::new());
        let mut offs = 0;
        for run in font_runs {
            let run_end = offs + run.len;

            let fallback_chain = Arc::clone(&self.loaded_font(run.font_id).user_fallback_chain);
            let mut coverage_fonts = SmallVec::<[(FontId, Arc<CosmicTextFont>); 4]>::new();
            if !fallback_chain.is_empty() {
                for id in
                    std::iter::once(run.font_id).chain(fallback_chain.iter().map(|(id, _)| *id))
                {
                    match self.font(id) {
                        std::result::Result::Ok(font) => coverage_fonts.push((id, font)),
                        Err(error) => log::warn!("failed to reload fallback font: {error:#}"),
                    }
                }
            }
            let loaded_font = self.loaded_font(run.font_id);
            let Some(face) = self.font_system.db().face(loaded_font.database_id) else {
                log::warn!(
                    "font face not found in database for font_id {:?}",
                    run.font_id
                );
                offs = run_end;
                continue;
            };
            let Some(first_family) = face.families.first() else {
                log::warn!(
                    "font face has no family names for font_id {:?}",
                    run.font_id
                );
                offs = run_end;
                continue;
            };

            let primary_family_name = &first_family.0;
            let primary_stretch = loaded_font
                .variations
                .settings()
                .find_map(|(tag, value)| (tag == *b"wdth").then_some(value))
                .map(closest_stretch)
                .unwrap_or(face.stretch);
            let primary_style = loaded_font.style;
            let primary_weight = loaded_font.weight;
            let fallback_chain = Arc::clone(&loaded_font.user_fallback_chain);

            // Borrow features while constructing spans; AttrsList owns the copy
            // it needs after these temporary views and font borrows expire.
            let primary_attrs = Attrs {
                font_features: Cow::Borrowed(&loaded_font.features),
                ..Attrs::new()
                    .metadata(run.font_id.0)
                    .family(Family::Name(primary_family_name))
                    .stretch(primary_stretch)
                    .style(primary_style)
                    .weight(primary_weight)
                    .font_variations(loaded_font.variations)
            };
            let fallback_attrs: SmallVec<[Attrs<'_>; 4]> = fallback_chain
                .iter()
                .map(|(_, fb_name)| Attrs {
                    font_features: Cow::Borrowed(&loaded_font.features),
                    ..Attrs::new()
                        .metadata(run.font_id.0)
                        .family(Family::Name(fb_name))
                        .stretch(primary_stretch)
                        .style(primary_style)
                        .weight(primary_weight)
                        .font_variations(loaded_font.variations)
                })
                .collect();

            let spans = if fallback_chain.is_empty() {
                let mut spans = SmallVec::<[RunSpan; 4]>::new();
                spans.push(RunSpan {
                    start: offs,
                    end: run_end,
                    slot: None,
                    font_id: run.font_id,
                });
                spans
            } else {
                let covers = |id: FontId, ch: char| {
                    coverage_fonts
                        .iter()
                        .find(|(font_id, _)| *font_id == id)
                        .is_some_and(|(_, font)| font.as_swash().charmap().map(ch) != 0)
                };
                compute_run_spans(text, offs, run.len, run.font_id, &fallback_chain, &covers)
            };

            for span in spans {
                let attrs = match span.slot {
                    None => &primary_attrs,
                    Some(ix) => &fallback_attrs[ix],
                };
                // A single style covering the whole line needs no range-map entry.
                if span.start == 0 && span.end == text.len() && !text.is_empty() {
                    attrs_list = AttrsList::new(attrs);
                } else {
                    attrs_list.add_span(span.start..span.end, attrs);
                }
            }
            offs = run_end;
        }

        let line = if let Some(mut line) = self.shape_line.take() {
            line.build_without_ellipsis(
                &mut self.font_system,
                text,
                &attrs_list,
                cosmic_text::Shaping::Advanced,
                4,
            );
            line
        } else {
            ShapeLine::new_without_ellipsis(
                &mut self.font_system,
                text,
                &attrs_list,
                cosmic_text::Shaping::Advanced,
                4,
            )
        };
        // Cosmic text drains the previous lines and reuses their glyph buffers.
        // Taking the vector also permits font resolution while reading its output.
        let mut layout_lines = take(&mut self.layout_lines);
        line.layout_to_buffer(
            &mut self.scratch,
            f32::from(font_size),
            None, // We do our own wrapping
            cosmic_text::Wrap::None,
            Ellipsize::None,
            None,
            &mut layout_lines,
            None,
            cosmic_text::Hinting::Disabled,
        );

        let Some(layout) = layout_lines.first() else {
            self.retain_shaping_scratch(line, layout_lines);
            return LineLayout {
                font_size,
                width: Pixels::ZERO,
                ascent: Pixels::ZERO,
                descent: Pixels::ZERO,
                runs: Vec::new(),
                clusters,
                len: text.len(),
            };
        };

        let mut runs: Vec<ShapedRun> = Vec::new();
        for (index, glyph) in layout.glyphs.iter().enumerate() {
            let mut font_id = FontId(glyph.metadata);
            let mut loaded_font = self.loaded_font(font_id);
            if loaded_font.database_id != glyph.font_id
                || loaded_font.weight != glyph.font_weight
                || loaded_font.variations != glyph.font_variations
            {
                let angle = loaded_font.oblique_angle;
                match self.font_id_for_cosmic_id(
                    glyph.font_id,
                    glyph.font_weight,
                    glyph.font_variations,
                    angle,
                    loaded_font.palette.clone(),
                    loaded_font.slant_axis,
                ) {
                    std::result::Result::Ok(resolved_id) => {
                        font_id = resolved_id;
                        loaded_font = self.loaded_font(font_id);
                    }
                    Err(error) => {
                        log::warn!(
                            "failed to resolve cosmic font id {:?}: {error:#}",
                            glyph.font_id
                        );
                        continue;
                    }
                }
            }
            let is_emoji = loaded_font.is_color_font;

            // HACK: Prevent crash caused by variation selectors.
            if glyph.glyph_id == 3 && loaded_font.is_known_emoji_font {
                continue;
            }

            let shaped_glyph = ShapedGlyph {
                id: GlyphId(glyph.glyph_id as u32),
                position: point(glyph.x.into(), glyph.y.into()),
                index: glyph.start,
                is_emoji,
            };

            if let Some(last_run) = runs
                .last_mut()
                .filter(|last_run| last_run.font_id == font_id)
            {
                last_run.glyphs.push(shaped_glyph);
            } else {
                let count = 1 + layout.glyphs[index + 1..]
                    .iter()
                    .take_while(|next| {
                        next.font_id == glyph.font_id && next.metadata == glyph.metadata
                    })
                    .count();
                let mut glyphs = Vec::with_capacity(count);
                glyphs.push(shaped_glyph);
                runs.push(ShapedRun { font_id, glyphs });
            }
        }

        let result = LineLayout {
            font_size,
            width: layout.w.into(),
            ascent: layout.max_ascent.into(),
            descent: layout.max_descent.into(),
            runs,
            clusters: shaped_clusters(layout, clusters),
            len: text.len(),
        };
        self.retain_shaping_scratch(line, layout_lines);
        result
    }

    fn recycle_line_clusters(&mut self, mut clusters: Vec<ShapedCluster>) {
        // Contents have been converted into the caller's persistent geometry.
        // Keep one empty allocation, with the same bound as layout scratch.
        clusters.clear();
        if clusters.capacity() <= MAX_LAYOUT_SCRATCH_GLYPHS
            && clusters.capacity() > self.cluster_scratch.capacity()
        {
            self.cluster_scratch = clusters;
        }
    }

    fn release_shaping_scratch(&mut self) {
        self.font_system.release_shaping_scratch();
        discard_spare_pages_at_least(&mut self.cluster_scratch, 16 * 1024);
        // These values are rebuilt before reuse. Discard their pages only at
        // batch boundaries so consecutive layouts can still reuse warm memory.
        for layout in &mut self.layout_lines {
            layout.glyphs.clear();
            layout.decorations.clear();
            discard_spare_pages(&mut layout.glyphs);
            discard_spare_pages(&mut layout.decorations);
        }
        if let Some(line) = &mut self.shape_line {
            for span in &mut line.spans {
                span.words.clear();
                span.glyphs.clear();
                span.decoration_spans.clear();
                discard_spare_pages(&mut span.glyphs);
                discard_spare_pages(&mut span.words);
                discard_spare_pages(&mut span.decoration_spans);
            }
            discard_spare_pages(&mut line.spans);
        }
    }

    fn retain_shaping_scratch(&mut self, mut line: ShapeLine, mut layout_lines: Vec<CosmicLayoutLine>) {
        // Large runs release their buffers instead of permanently raising the
        // platform's retained high-water mark. ShapeLine::build can then reuse
        // spans, words and glyphs from the previous ordinary run.
        let retain_layout = layout_lines
            .iter()
            .map(|line| line.glyphs.capacity())
            .sum::<usize>()
            <= MAX_LAYOUT_SCRATCH_GLYPHS;
        for layout in &mut layout_lines {
            if !retain_layout {
                layout.glyphs.clear();
                layout.decorations.clear();
            }
            discard_spare_pages(&mut layout.glyphs);
            discard_spare_pages(&mut layout.decorations);
        }
        if retain_layout {
            self.layout_lines = layout_lines;
        } else {
            // Preserve parameter drop order when both scratch sets are rejected.
            drop(layout_lines);
        }
        let retain_shape = shape_line_buffer_bytes(&line) <= MAX_SHAPE_SCRATCH_BYTES;
        for span in &mut line.spans {
            if !retain_shape {
                span.words.clear();
                span.glyphs.clear();
                span.decoration_spans.clear();
            }
            discard_spare_pages(&mut span.glyphs);
            discard_spare_pages(&mut span.words);
            discard_spare_pages(&mut span.decoration_spans);
        }
        if !retain_shape {
            line.spans.clear();
        }
        discard_spare_pages(&mut line.spans);
        if retain_shape {
            self.shape_line = Some(line);
        }
    }
}

fn discard_spare_pages<T>(buffer: &mut Vec<T>) {
    discard_spare_pages_at_least(buffer, 64 * 1024);
}

fn discard_spare_pages_at_least<T>(buffer: &mut Vec<T>, minimum_bytes: usize) {
    #[cfg(target_os = "linux")]
    {
        let spare = buffer.spare_capacity_mut();
        let bytes = std::mem::size_of_val(spare);
        if bytes < minimum_bytes {
            return;
        }
        let page_size = unsafe { sysconf(_SC_PAGESIZE) };
        if page_size <= 0 {
            return;
        }
        let page_size = page_size as usize;
        let pointer = spare.as_mut_ptr().cast::<u8>();
        let offset = pointer.align_offset(page_size);
        if offset >= bytes {
            return;
        }
        let length = (bytes - offset) / page_size * page_size;
        if length > 0 {
            // SAFETY: only whole pages inside uninitialized, exclusively owned
            // spare capacity are discarded. Live values and neighboring
            // allocations are outside this range.
            let result = unsafe { madvise(pointer.add(offset).cast(), length, MADV_DONTNEED) };
            if result != 0 {
                log::debug!("could not discard unused shaping pages");
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (buffer, minimum_bytes);
}

fn shaped_clusters(layout: &CosmicLayoutLine, mut clusters: Vec<ShapedCluster>) -> Vec<ShapedCluster> {
    clusters.reserve_exact(layout.glyphs.len());
    for glyph in &layout.glyphs {
        if let Some(cluster) = clusters.last_mut().filter(|cluster| {
            cluster.bytes == (glyph.start..glyph.end) && cluster.is_rtl == glyph.level.is_rtl()
        }) {
            cluster.x = cluster.x.min(px(glyph.x));
            cluster.advance += px(glyph.w);
        } else {
            clusters.push(ShapedCluster {
                bytes: glyph.start..glyph.end,
                x: px(glyph.x),
                advance: px(glyph.w),
                is_rtl: glyph.level.is_rtl(),
            });
        }
    }
    if !clusters
        .windows(2)
        .all(|pair| pair[0].bytes.start <= pair[1].bytes.start)
    {
        clusters.sort_unstable_by_key(|cluster| cluster.bytes.start);
    }
    clusters
}

fn shape_line_buffer_bytes(line: &ShapeLine) -> usize {
    line.spans.capacity() * size_of::<ShapeSpan>()
        + line
            .spans
            .iter()
            .map(|span| {
                span.words.capacity() * size_of::<ShapeWord>()
                    + span.decoration_spans.capacity()
                        * size_of::<(Range<usize>, GlyphDecorationData)>()
                    + span.glyphs.capacity() * size_of::<ShapeGlyph>()
            })
            .sum::<usize>()
}

#[inline(always)]
fn is_paragraph_separator(character: char) -> bool {
    unicode_bidi::bidi_class(character) == unicode_bidi::BidiClass::B
}

fn contains_paragraph_separator(text: &str) -> bool {
    if text
        .bytes()
        .any(|byte| matches!(byte, b'\n' | b'\r' | 0x1c | 0x1d | 0x1e))
    {
        return true;
    }

    !text.is_ascii() && text.chars().any(is_paragraph_separator)
}

fn clip_font_runs(font_runs: &[FontRun], range: Range<usize>) -> SmallVec<[FontRun; 4]> {
    let mut clipped = SmallVec::new();
    let mut offs = 0;
    for run in font_runs {
        let run_start = offs;
        offs += run.len;
        if offs <= range.start {
            continue;
        }
        if run_start >= range.end {
            break;
        }
        let start = run_start.max(range.start);
        let end = offs.min(range.end);
        if start < end {
            clipped.push(FontRun {
                len: end - start,
                font_id: run.font_id,
            });
        }
    }
    clipped
}

#[cfg(feature = "font-kit")]
fn find_best_match(
    font: &Font,
    candidates: &[FontId],
    state: &CosmicTextSystemState,
) -> Result<usize> {
    let candidate_properties = candidates
        .iter()
        .map(|font_id| {
            let database_id = state.loaded_font(*font_id).database_id;
            let face_info = state
                .font_system
                .db()
                .face(database_id)
                .context("font face not found in database")?;
            Ok(face_info_into_properties(face_info))
        })
        .collect::<Result<SmallVec<[_; 4]>>>()?;

    let ix =
        font_kit::matching::find_best_match(&candidate_properties, &font_into_properties(font))
            .context("requested font family contains no font matching the other parameters")?;

    Ok(ix)
}

#[cfg(not(feature = "font-kit"))]
fn find_best_match(
    font: &Font,
    candidates: &[FontId],
    state: &CosmicTextSystemState,
) -> Result<usize> {
    if candidates.is_empty() {
        anyhow::bail!("requested font family contains no font matching the other parameters");
    }
    if candidates.len() == 1 {
        return Ok(0);
    }

    let target_weight = font.weight.0;
    let target_italic = matches!(
        font.style,
        gpui::FontStyle::Italic | gpui::FontStyle::Oblique
    );

    let mut best_index = 0;
    let mut best_score = (u8::MAX, f32::MAX, u32::MAX);

    for (index, font_id) in candidates.iter().enumerate() {
        let database_id = state.loaded_font(*font_id).database_id;
        let face_info = state
            .font_system
            .db()
            .face(database_id)
            .context("font face not found in database")?;

        let is_italic = matches!(
            face_info.style,
            cosmic_text::Style::Italic | cosmic_text::Style::Oblique
        );
        let style_penalty: u32 = if is_italic == target_italic { 0 } else { 1000 };
        let weight_diff = (face_info.weight.0 as i32 - target_weight as i32).unsigned_abs();
        let width = stretch_percentage(face_info.stretch);
        let target = font.variations.stretch();
        let preferred_side = if target <= 100.0 {
            width <= target
        } else {
            width >= target
        };
        let score = (
            u8::from(!preferred_side),
            (width - target).abs(),
            style_penalty + weight_diff,
        );

        if score < best_score {
            best_score = score;
            best_index = index;
        }
    }

    Ok(best_index)
}

/// one contiguous slice of a `FontRun` that maps to a single slot. `slot` is
/// `None` for the primary font and `Some(ix)` for `fallback_chain[ix]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunSpan {
    start: usize,
    end: usize,
    slot: Option<usize>,
    font_id: FontId,
}

/// walks `text[run_offset..run_offset + run_len]` and groups codepoints into
/// spans. inheriting codepoints stay in the current span so shaping clusters
/// like emoji zwj sequences and combining marks are not torn apart.
fn compute_run_spans(
    text: &str,
    run_offset: usize,
    run_len: usize,
    primary: FontId,
    fallback_chain: &[(FontId, SharedString)],
    covers: &impl Fn(FontId, char) -> bool,
) -> SmallVec<[RunSpan; 4]> {
    let mut spans = SmallVec::new();
    let run_end = run_offset + run_len;
    if run_end <= run_offset {
        return spans;
    }
    if fallback_chain.is_empty() {
        spans.push(RunSpan {
            start: run_offset,
            end: run_end,
            slot: None,
            font_id: primary,
        });
        return spans;
    }
    let run_text = &text[run_offset..run_end];
    let mut span_start = run_offset;
    let mut span_slot: Option<usize> = None;
    let mut span_font_id = primary;
    for (grapheme_idx, grapheme) in run_text.grapheme_indices(true) {
        let abs = run_offset + grapheme_idx;
        let ch = grapheme.chars().next().unwrap_or('\0');
        let next_slot = pick_covering_slot(ch, span_slot, primary, fallback_chain, covers);
        if next_slot == span_slot {
            continue;
        }
        if abs > span_start {
            spans.push(RunSpan {
                start: span_start,
                end: abs,
                slot: span_slot,
                font_id: span_font_id,
            });
        }
        span_start = abs;
        span_slot = next_slot;
        span_font_id = slot_font_id(next_slot, primary, fallback_chain);
    }
    if span_start < run_end {
        spans.push(RunSpan {
            start: span_start,
            end: run_end,
            slot: span_slot,
            font_id: span_font_id,
        });
    }
    spans
}

fn slot_font_id(
    slot: Option<usize>,
    primary: FontId,
    fallback_chain: &[(FontId, SharedString)],
) -> FontId {
    match slot {
        None => primary,
        Some(ix) => fallback_chain[ix].0,
    }
}

fn pick_covering_slot(
    ch: char,
    current: Option<usize>,
    primary: FontId,
    fallback_chain: &[(FontId, SharedString)],
    covers: &impl Fn(FontId, char) -> bool,
) -> Option<usize> {
    if covers(primary, ch) {
        return None;
    }
    let current_id = slot_font_id(current, primary, fallback_chain);
    if covers(current_id, ch) {
        return current;
    }

    fallback_chain
        .iter()
        .position(|(fb_id, _)| covers(*fb_id, ch))
}

fn oblique_shear(font: &CosmicTextFont, angle: Option<f32>, slant_axis: bool) -> f32 {
    // Variable slant is clamped by the font's capabilities, without extra synthesis.
    if (slant_axis && font.has_slant_axis()) || font.is_italic_instance() {
        return 0.0;
    }
    angle.map_or(0.0, |angle| {
        // At ±90° the shear is singular. Keep synthesized outlines finite.
        angle.clamp(-89.0, 89.0).to_radians().tan()
            - font.oblique_angle().clamp(-89.0, 89.0).to_radians().tan()
    })
}

fn stretch_percentage(stretch: cosmic_text::Stretch) -> f32 {
    match stretch {
        cosmic_text::Stretch::UltraCondensed => 50.0,
        cosmic_text::Stretch::ExtraCondensed => 62.5,
        cosmic_text::Stretch::Condensed => 75.0,
        cosmic_text::Stretch::SemiCondensed => 87.5,
        cosmic_text::Stretch::Normal => 100.0,
        cosmic_text::Stretch::SemiExpanded => 112.5,
        cosmic_text::Stretch::Expanded => 125.0,
        cosmic_text::Stretch::ExtraExpanded => 150.0,
        cosmic_text::Stretch::UltraExpanded => 200.0,
    }
}

fn closest_stretch(stretch: f32) -> cosmic_text::Stretch {
    use cosmic_text::Stretch;
    [
        Stretch::UltraCondensed,
        Stretch::ExtraCondensed,
        Stretch::Condensed,
        Stretch::SemiCondensed,
        Stretch::Normal,
        Stretch::SemiExpanded,
        Stretch::Expanded,
        Stretch::ExtraExpanded,
        Stretch::UltraExpanded,
    ]
    .into_iter()
    .min_by(|a, b| {
        (stretch_percentage(*a) - stretch)
            .abs()
            .total_cmp(&(stretch_percentage(*b) - stretch).abs())
    })
    .unwrap_or(Stretch::Normal)
}

fn metrics_for_font(font: &CosmicTextFont, angle: Option<f32>, slant_axis: bool) -> FontMetrics {
    let metrics = font.as_swash().metrics(font.normalized_coords());
    let shear = oblique_shear(font, angle, slant_axis);
    let left = (shear * metrics.ascent)
        .min(-shear * metrics.descent)
        .min(0.0);
    let right = (shear * metrics.ascent)
        .max(-shear * metrics.descent)
        .max(0.0);
    FontMetrics {
        units_per_em: metrics.units_per_em as u32,
        ascent: metrics.ascent,
        descent: -metrics.descent,
        line_gap: metrics.leading,
        underline_position: metrics.underline_offset,
        underline_thickness: metrics.stroke_size,
        cap_height: metrics.cap_height,
        x_height: metrics.x_height,
        bounding_box: Bounds {
            origin: point(left, 0.0),
            size: size(
                metrics.max_width + right - left,
                metrics.ascent + metrics.descent,
            ),
        },
    }
}

fn cosmic_font_features(features: &FontFeatures) -> Result<CosmicFontFeatures> {
    let mut result = CosmicFontFeatures::new();
    for feature in features.0.iter() {
        let name_bytes: [u8; 4] = feature
            .0
            .as_bytes()
            .try_into()
            .context("Incorrect feature flag format")?;

        let tag = cosmic_text::FeatureTag::new(&name_bytes);

        result.set(tag, feature.1);
    }
    Ok(result)
}

#[cfg(feature = "font-kit")]
fn font_into_properties(font: &gpui::Font) -> font_kit::properties::Properties {
    font_kit::properties::Properties {
        style: match font.style {
            gpui::FontStyle::Normal => font_kit::properties::Style::Normal,
            gpui::FontStyle::Italic => font_kit::properties::Style::Italic,
            gpui::FontStyle::Oblique => font_kit::properties::Style::Oblique,
        },
        weight: font_kit::properties::Weight(font.weight.0),
        stretch: font_kit::properties::Stretch(font.variations.stretch() / 100.0),
    }
}

#[cfg(feature = "font-kit")]
fn face_info_into_properties(
    face_info: &cosmic_text::fontdb::FaceInfo,
) -> font_kit::properties::Properties {
    font_kit::properties::Properties {
        style: match face_info.style {
            cosmic_text::Style::Normal => font_kit::properties::Style::Normal,
            cosmic_text::Style::Italic => font_kit::properties::Style::Italic,
            cosmic_text::Style::Oblique => font_kit::properties::Style::Oblique,
        },
        weight: font_kit::properties::Weight(face_info.weight.0.into()),
        stretch: match face_info.stretch {
            cosmic_text::Stretch::Condensed => font_kit::properties::Stretch::CONDENSED,
            cosmic_text::Stretch::Expanded => font_kit::properties::Stretch::EXPANDED,
            cosmic_text::Stretch::ExtraCondensed => font_kit::properties::Stretch::EXTRA_CONDENSED,
            cosmic_text::Stretch::ExtraExpanded => font_kit::properties::Stretch::EXTRA_EXPANDED,
            cosmic_text::Stretch::Normal => font_kit::properties::Stretch::NORMAL,
            cosmic_text::Stretch::SemiCondensed => font_kit::properties::Stretch::SEMI_CONDENSED,
            cosmic_text::Stretch::SemiExpanded => font_kit::properties::Stretch::SEMI_EXPANDED,
            cosmic_text::Stretch::UltraCondensed => font_kit::properties::Stretch::ULTRA_CONDENSED,
            cosmic_text::Stretch::UltraExpanded => font_kit::properties::Stretch::ULTRA_EXPANDED,
        },
    }
}

fn check_is_known_emoji_font(postscript_name: &str) -> bool {
    // TODO: Include other common emoji fonts
    postscript_name == "NotoColorEmoji"
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{font, px};

    fn fid(i: usize) -> FontId {
        FontId(i)
    }

    fn chain(ids: &[usize]) -> SmallVec<[(FontId, SharedString); 4]> {
        ids.iter()
            .map(|&i| (fid(i), SharedString::from(format!("fb{i}"))))
            .collect()
    }

    fn span(start: usize, end: usize, slot: Option<usize>, font_id: FontId) -> RunSpan {
        RunSpan {
            start,
            end,
            slot,
            font_id,
        }
    }

    const IBM_PLEX: &[u8] =
        include_bytes!("../../../assets/fonts/ibm-plex-sans/IBMPlexSans-Regular.ttf");

    /// Every code point of `Bidi_Class=B`, each of which starts a new bidi
    /// paragraph and so can split one line into mixed-direction paragraphs.
    const SEPARATORS: &[char] = &[
        '\u{000a}', '\u{000d}', '\u{001c}', '\u{001d}', '\u{001e}', '\u{0085}', '\u{2029}',
    ];

    fn text_system() -> Result<CosmicTextSystem> {
        let text_system = CosmicTextSystem::new_without_system_fonts("IBM Plex Sans");
        text_system.add_fonts(vec![Cow::Borrowed(IBM_PLEX)])?;
        Ok(text_system)
    }

    #[test]
    fn non_latin_face_loads_and_remains_available_for_glyph_fallback() -> Result<()> {
        let text_system = text_system()?;
        text_system.add_fonts(vec![Cow::Borrowed(include_bytes!("../fixtures/cjk.ttf"))])?;
        let cjk = text_system.font_id(&font("GPUI CJK Test"))?;
        assert!(text_system.glyph_for_char(cjk, 'm').is_none());
        assert!(text_system.glyph_for_char(cjk, '漢').is_some());
        assert_eq!(text_system.font_id(&font("GPUI CJK Test"))?, cjk);
        assert!(
            text_system
                .all_font_names()
                .iter()
                .any(|name| name == "GPUI CJK Test")
        );

        let mut descriptor = font("IBM Plex Sans");
        descriptor.fallbacks = Some(FontFallbacks::from_fonts(vec!["GPUI CJK Test".into()]));
        let primary = text_system.font_id(&descriptor)?;
        let text = "Latin 漢";
        let layout = text_system.layout_line(
            text,
            px(14.0),
            &[FontRun {
                len: text.len(),
                font_id: primary,
            }],
        );
        assert!(
            layout
                .runs
                .iter()
                .any(|run| run.font_id == cjk && run.glyphs.iter().any(|glyph| glyph.id.0 != 0))
        );

        let primary = text_system.font_id(&font("IBM Plex Sans"))?;
        let layout = text_system.layout_line(
            text,
            px(14.0),
            &[FontRun {
                len: text.len(),
                font_id: primary,
            }],
        );
        assert!(
            layout
                .runs
                .iter()
                .any(|run| run.font_id == cjk && run.glyphs.iter().any(|glyph| glyph.id.0 != 0))
        );
        Ok(())
    }

    #[test]
    fn shared_text_system_reports_a_missing_family_without_falling_back() -> Result<()> {
        let text_system = gpui::TextSystem::new(Arc::new(text_system()?));
        assert!(text_system.font_id(&font("Missing CJK family")).is_err());
        let latin = text_system.font_id(&font("IBM Plex Sans"))?;
        assert!(text_system.advance(latin, px(14.0), '漢').is_err());
        Ok(())
    }

    #[test]
    fn registering_non_latin_fallback_preserves_latin_layout() -> Result<()> {
        let text_system = text_system()?;
        let text = "Office AV ffi café Ångström 123 — a\u{301}";
        let before = layout_text(&text_system, text)?;
        text_system.add_fonts(vec![Cow::Borrowed(include_bytes!("../fixtures/cjk.ttf"))])?;
        let after = layout_text(&text_system, text)?;
        assert_eq!(
            (before.width, before.ascent, before.descent, before.len),
            (after.width, after.ascent, after.descent, after.len)
        );
        let glyphs = |line: &LineLayout| {
            line.runs
                .iter()
                .flat_map(|run| {
                    run.glyphs.iter().map(move |glyph| {
                        (
                            run.font_id,
                            glyph.id,
                            glyph.position,
                            glyph.index,
                            glyph.is_emoji,
                        )
                    })
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(glyphs(&before), glyphs(&after));
        Ok(())
    }

    fn layout_text(text_system: &CosmicTextSystem, text: &str) -> Result<LineLayout> {
        let font_id = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        let runs = [FontRun {
            len: text.len(),
            font_id,
        }];
        Ok(text_system.layout_line(text, gpui::px(14.0), &runs))
    }

    /// Mirrors the original crash: mixed-direction text reaching the shaper
    /// through `shape_text`, which only splits lines on `\n`.
    #[test]
    fn shape_text_with_mixed_direction_paragraphs() -> Result<()> {
        let platform_text_system = Arc::new(text_system()?);
        let text_system = Arc::new(gpui::TextSystem::new(platform_text_system));
        let window_text_system = gpui::WindowTextSystem::new(text_system);

        let text: SharedString = "first line\n\u{05d0}\u{001c}A".into();
        let runs = [gpui::TextRun {
            len: text.len(),
            font: gpui::font("IBM Plex Sans"),
            ..Default::default()
        }];

        let lines = window_text_system.shape_text(text, gpui::px(14.0), &runs, None, None)?;

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].len(), "\u{05d0}\u{001c}A".len());
        assert!(lines[1].width() > Pixels::ZERO);
        Ok(())
    }

    #[test]
    fn layout_line_with_mixed_direction_paragraphs() -> Result<()> {
        let text_system = text_system()?;

        for separator in SEPARATORS {
            for text in [
                format!("\u{05d0}{separator}A"),
                format!("A{separator}\u{05d0}"),
            ] {
                let layout = layout_text(&text_system, &text)?;

                assert_eq!(layout.len, text.len(), "{text:?}");
                assert!(layout.width > Pixels::ZERO, "{text:?}");
                assert!(
                    layout.runs.iter().any(|run| !run.glyphs.is_empty()),
                    "{text:?}"
                );
            }
        }

        Ok(())
    }

    #[test]
    fn layout_line_with_separators_at_line_edges() -> Result<()> {
        let text_system = text_system()?;

        for text in [
            "\u{001c}",
            "\u{001c}\u{001c}",
            "\u{001c}\u{05d0}",
            "\u{05d0}\u{001c}",
            "\u{05d0}\u{001c}\u{001c}A",
            "\u{001c}\u{05d0}\u{001c}A\u{001c}",
        ] {
            let layout = layout_text(&text_system, text)?;
            assert_eq!(layout.len, text.len(), "{text:?}");
        }

        Ok(())
    }

    /// Glyph indices must stay absolute and positions ordered across segment
    /// boundaries, otherwise cursor placement and hit testing desync. Uses
    /// single-direction text so visual order matches logical order.
    #[test]
    fn layout_line_keeps_indices_and_positions_ordered_across_paragraphs() -> Result<()> {
        let text_system = text_system()?;
        let text = "ab\u{001c}cd\u{2029}ef";
        let layout = layout_text(&text_system, text)?;

        let glyphs: Vec<_> = layout.runs.iter().flat_map(|run| &run.glyphs).collect();
        assert!(!glyphs.is_empty());

        for glyph in &glyphs {
            assert!(glyph.index < text.len(), "{:?}", glyph.index);
            assert!(text.is_char_boundary(glyph.index), "{:?}", glyph.index);
        }
        for pair in glyphs.windows(2) {
            assert!(pair[0].index < pair[1].index);
            assert!(pair[0].position.x <= pair[1].position.x);
        }

        // Every segment contributes width, so the whole line is wider than its
        // leading paragraph alone.
        assert!(layout.width > layout_text(&text_system, "ab")?.width);
        Ok(())
    }

    /// A font run boundary that does not line up with a paragraph boundary must
    /// still be clipped to the right segments.
    #[test]
    fn layout_line_with_font_run_straddling_a_separator() -> Result<()> {
        let text_system = text_system()?;
        let font_id = text_system.font_id(&gpui::font("IBM Plex Sans"))?;
        let text = "ab\u{001c}\u{05d0}\u{05d1}";

        // The run boundary falls inside the trailing RTL paragraph.
        let runs = [
            FontRun {
                len: "ab\u{001c}\u{05d0}".len(),
                font_id,
            },
            FontRun {
                len: "\u{05d1}".len(),
                font_id,
            },
        ];
        let layout = text_system.layout_line(text, gpui::px(14.0), &runs);

        assert_eq!(layout.len, text.len());
        assert!(layout.width > Pixels::ZERO);
        Ok(())
    }

    /// Lines with no separator take the fast path and must be shaped exactly as
    /// they were before paragraph splitting existed.
    #[test]
    fn layout_line_without_separators_takes_fast_path() -> Result<()> {
        let text_system = text_system()?;

        for text in [
            "hello world",
            "\u{05d0}\u{05d1}\u{05d2}",
            "mixed \u{05d0}\u{05d1}",
        ] {
            assert!(!contains_paragraph_separator(text), "{text:?}");
            let layout = layout_text(&text_system, text)?;
            assert_eq!(layout.len, text.len(), "{text:?}");
            assert!(layout.width > Pixels::ZERO, "{text:?}");
        }

        Ok(())
    }

    #[test]
    fn reused_layout_output_matches_fresh_output_and_releases_large_buffers() -> Result<()> {
        let text_system = text_system()?;
        let texts = [
            "office a\u{301} text".to_owned(),
            "long paragraph ".repeat(120),
            "mixed אבג سلام \u{001c} next paragraph".to_owned(),
            "x".repeat(MAX_LAYOUT_SCRATCH_GLYPHS * 3),
            "short again".to_owned(),
        ];
        for text in texts {
            let reused = layout_text(&text_system, &text)?;
            {
                let mut state = text_system.0.write();
                let retained = state
                    .layout_lines
                    .iter()
                    .map(|line| line.glyphs.capacity())
                    .sum::<usize>();
                assert!(retained <= MAX_LAYOUT_SCRATCH_GLYPHS);
                if text.len() > MAX_LAYOUT_SCRATCH_GLYPHS * 2 {
                    assert_eq!(
                        retained, 0,
                        "large outputs must not raise the retained high-water mark"
                    );
                }
                if let Some(line) = &state.shape_line {
                    assert!(shape_line_buffer_bytes(line) <= MAX_SHAPE_SCRATCH_BYTES);
                }
                state.layout_lines = Vec::new();
                state.shape_line = None;
            }
            let fresh = layout_text(&text_system, &text)?;
            assert_eq!(
                (reused.width, reused.ascent, reused.descent, reused.len),
                (fresh.width, fresh.ascent, fresh.descent, fresh.len)
            );
            assert_eq!(reused.runs.len(), fresh.runs.len());
            for (reused, fresh) in reused.runs.iter().zip(&fresh.runs) {
                assert_eq!(reused.font_id, fresh.font_id);
                assert_eq!(reused.glyphs.len(), fresh.glyphs.len());
                for (reused, fresh) in reused.glyphs.iter().zip(&fresh.glyphs) {
                    assert_eq!(
                        (reused.id, reused.position, reused.index, reused.is_emoji),
                        (fresh.id, fresh.position, fresh.index, fresh.is_emoji)
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn paragraph_separator_detection() {
        for separator in SEPARATORS {
            assert!(is_paragraph_separator(*separator), "{separator:?}");
            assert!(contains_paragraph_separator(&format!("a{separator}b")));
        }

        for text in [
            "",
            "plain ascii",
            "\u{05d0}",
            "tab\there",
            "emoji \u{1f600}",
        ] {
            assert!(!contains_paragraph_separator(text), "{text:?}");
        }
    }

    #[test]
    fn font_runs_are_clipped_to_segment() {
        let runs = [
            FontRun {
                len: 3,
                font_id: fid(1),
            },
            FontRun {
                len: 4,
                font_id: fid(2),
            },
        ];

        assert_eq!(clip_font_runs(&runs, 0..7).as_slice(), &runs);
        assert_eq!(
            clip_font_runs(&runs, 2..5).as_slice(),
            &[
                FontRun {
                    len: 1,
                    font_id: fid(1)
                },
                FontRun {
                    len: 2,
                    font_id: fid(2)
                },
            ]
        );
        assert_eq!(
            clip_font_runs(&runs, 3..7).as_slice(),
            &[FontRun {
                len: 4,
                font_id: fid(2)
            }]
        );
        assert!(clip_font_runs(&runs, 5..5).is_empty());
    }

    #[test]
    fn primary_wins_over_current_fallback_when_primary_covers() {
        let primary = fid(0);
        let fb = chain(&[1, 2]);
        let covers = |id: FontId, _: char| id == fid(0) || id == fid(1);
        assert_eq!(
            pick_covering_slot('a', Some(0), primary, &fb, &covers),
            None
        );
    }

    #[test]
    fn primary_preferred_over_fallback_when_both_cover() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |_: FontId, _: char| true;
        assert_eq!(pick_covering_slot('a', None, primary, &fb, &covers), None);
    }

    #[test]
    fn falls_through_chain_in_order() {
        let primary = fid(0);
        let fb = chain(&[1, 2, 3]);
        // only fallback 2 at index 1 covers.
        let covers = |id: FontId, _: char| id == fid(2);
        assert_eq!(
            pick_covering_slot('字', None, primary, &fb, &covers),
            Some(1)
        );
    }

    #[test]
    fn no_coverage_returns_primary() {
        let primary = fid(0);
        let fb = chain(&[1, 2]);
        let covers = |_: FontId, _: char| false;
        // nothing covers. return `None` so the `cosmic-text` built in script
        // fallback can take over during shaping.
        assert_eq!(
            pick_covering_slot('\u{1F600}', Some(1), primary, &fb, &covers),
            None
        );
    }

    #[test]
    fn empty_chain_always_returns_primary() {
        let primary = fid(0);
        let fb: SmallVec<[(FontId, SharedString); 4]> = SmallVec::new();
        let covers = |_: FontId, _: char| false;
        assert_eq!(pick_covering_slot('a', None, primary, &fb, &covers), None);
    }

    #[test]
    fn slot_font_id_resolution() {
        let primary = fid(7);
        let fb = chain(&[10, 20]);
        assert_eq!(slot_font_id(None, primary, &fb), fid(7));
        assert_eq!(slot_font_id(Some(0), primary, &fb), fid(10));
        assert_eq!(slot_font_id(Some(1), primary, &fb), fid(20));
    }

    #[test]
    fn run_spans_with_no_chain_emit_one_primary_span() {
        let primary = fid(0);
        let fb: SmallVec<[(FontId, SharedString); 4]> = SmallVec::new();
        let covers = |_: FontId, _: char| false;
        let text = "hello";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), None, primary)]);
    }

    #[test]
    fn run_spans_use_byte_offsets_for_multibyte_chars() {
        let primary = fid(0);
        let fb = chain(&[1]);
        // primary covers ascii. fallback covers cjk.
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                !ch.is_ascii()
            }
        };
        let text = "a字b";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        // '字' is 3 bytes so split is at 1 then 4.
        assert_eq!(
            spans.as_slice(),
            &[
                span(0, 1, None, primary),
                span(1, 4, Some(0), fid(1)),
                span(4, 5, None, primary),
            ]
        );
    }

    #[test]
    fn run_spans_respect_run_offset() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                !ch.is_ascii()
            }
        };
        // outer text has a prefix that is not part of this run.
        let text = "xx字y";
        let run_offset = 2;
        let run_len = text.len() - run_offset;
        let spans = compute_run_spans(text, run_offset, run_len, primary, &fb, &covers);
        assert_eq!(
            spans.as_slice(),
            &[span(2, 5, Some(0), fid(1)), span(5, 6, None, primary)]
        );
    }

    #[test]
    fn run_spans_keep_combining_marks_with_base_in_fallback() {
        let primary = fid(0);
        let fb = chain(&[1]);
        // primary covers ascii only. fallback covers the base char.
        // combining mark must stay in the fallback span even when fallback
        // does not advertise coverage of it.
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                ch == '\u{0905}'
            }
        };
        // \u{0905} devanagari short a + \u{0902} candrabindu mark.
        let text = "\u{0905}\u{0902}";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), Some(0), fid(1))]);
    }

    #[test]
    fn run_spans_keep_zwj_inside_emoji_cluster() {
        let primary = fid(0);
        let fb = chain(&[1]);
        // only fallback covers the emoji codepoints. zwj must not split.
        let covers = |id: FontId, ch: char| id == fid(1) && ch != '\u{200D}';
        // family zwj sequence woman zwj girl.
        let text = "\u{1F469}\u{200D}\u{1F467}";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), Some(0), fid(1))]);
    }

    #[test]
    fn run_spans_collapse_adjacent_same_slot() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |id: FontId, ch: char| {
            if id == primary {
                ch.is_ascii()
            } else {
                !ch.is_ascii()
            }
        };
        let text = "字字字";
        let spans = compute_run_spans(text, 0, text.len(), primary, &fb, &covers);
        assert_eq!(spans.as_slice(), &[span(0, text.len(), Some(0), fid(1))]);
    }

    #[test]
    fn run_spans_empty_run_returns_no_spans() {
        let primary = fid(0);
        let fb = chain(&[1]);
        let covers = |_: FontId, _: char| true;
        let spans = compute_run_spans("anything", 3, 0, primary, &fb, &covers);
        assert!(spans.is_empty());
    }
}


fn is_color_font(font: &CosmicTextFont) -> bool {
    let font = font.as_swash();
    font.color_palettes().next().is_some() || font.color_strikes().next().is_some()
}

fn palette_index(font: &CosmicTextFont, base: FontPaletteBase) -> u16 {
    use swash::Usability;
    let font = font.as_swash();
    match base {
        FontPaletteBase::Normal => 0,
        FontPaletteBase::Index(index) => font.color_palettes().nth(index as usize).map_or(0, |palette| palette.index()),
        FontPaletteBase::Light | FontPaletteBase::Dark => font.color_palettes()
            .find(|palette| matches!((base, palette.usability()),
                (_, Some(Usability::Both)) |
                (FontPaletteBase::Light, Some(Usability::Light)) |
                (FontPaletteBase::Dark, Some(Usability::Dark))))
            .map_or(0, |palette| palette.index()),
    }
}
