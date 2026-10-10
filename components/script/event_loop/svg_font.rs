/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use app_units::Au;
use fonts::{
    FallbackFontSelectionOptions, FontContext, FontDescriptor, FontFamilyDescriptor,
    FontSearchScope, fallback_font_families,
};
use fonts_traits::FontDataAndIndex;
use net_traits::image_cache::FontResolver;
use resvg::usvg::{Font, FontFamily, FontStretch, FontStyle, fontdb};
use rustc_hash::FxHashMap;
use style::computed_values::font_optical_sizing::T as FontOpticalSizing;
use style::properties::longhands::font_variant_caps::computed_value::T as FontVariantCaps;
use style::values::computed::font::{
    FamilyName, FontFamilyNameSyntax, GenericFontFamily, SingleFontFamily,
};
use style::values::computed::{
    FontStyle as ServoFontStyle, FontSynthesis, FontWeight, FontWidth as ServoFontWidth,
};
use webrender_api::FontVariation;

/// Used to dynamically query fonts used in SVGs and insert them into the fontDB used when rasterizing.
///
/// The caches hold font data, not [`fontdb::ID`]s: usvg clones the database of its
/// options for every tree it parses, so an ID is only meaningful in the database
/// that issued it. An ID remembered from one SVG names nothing, or another font,
/// in the database of the next one, and usvg then drops the text.
#[derive(MallocSizeOf)]
pub struct SvgFontResolver {
    /// Cache for Font to its data
    font_cache: Mutex<FxHashMap<Font, FontDataAndIndex>>,
    fallback_cache: Mutex<FxHashMap<char, Vec<FontDataAndIndex>>>,
    #[conditional_malloc_size_of]
    context: Arc<FontContext>,
}

impl SvgFontResolver {
    pub(crate) fn new(context: Arc<FontContext>) -> Self {
        Self {
            font_cache: Mutex::new(FxHashMap::default()),
            fallback_cache: Mutex::new(FxHashMap::default()),
            context,
        }
    }

    /// Find the data of the first family of `font` that is available.
    fn find_font(&self, font: &Font) -> Option<FontDataAndIndex> {
        let font_descriptor = font_to_fontdescriptor(font);

        // usvg gives text without a `font-family` the family named in its options,
        // "Times New Roman" by default, and a list may name only fonts that are not
        // installed. Neither is a reason to lose the text: end on sans-serif.
        let families = font
            .families()
            .iter()
            .chain(std::iter::once(&FontFamily::SansSerif));
        for family in families {
            let family_descriptor = FontFamilyDescriptor::new(
                fontfamily_to_singlefontfamily(family),
                FontSearchScope::Any,
            );

            let Some(font_template) = self
                .context
                .matching_templates(&font_descriptor, &family_descriptor)
                .into_iter()
                .next()
            else {
                log::debug!(
                    "Cannot find matching font_template from font {font:?}, font-descriptor {font_descriptor:?}, family_descriptor: {family_descriptor:?} for this family {family:?}"
                );
                continue;
            };

            let Some(font_ref) = self.context.font(font_template, &font_descriptor) else {
                continue;
            };

            let Ok(data_and_index) = font_ref.font_data_and_index() else {
                continue;
            };
            return Some(data_and_index.clone());
        }

        None
    }
}

/// The caches are plain maps that every writer leaves consistent, so one that was
/// poisoned by a panic elsewhere is still usable.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The ID of `font` in `database`, loading it there if this database does not have
/// it yet.
fn load_into_database(
    font: &FontDataAndIndex,
    database: &mut Arc<fontdb::Database>,
) -> Option<fontdb::ID> {
    let bytes: &[u8] = font.data.as_ref();
    let loaded = database.faces().find(|face| {
        face.index == font.index &&
            match &face.source {
                fontdb::Source::Binary(source) => {
                    let source: &[u8] = (**source).as_ref();
                    std::ptr::eq(source, bytes)
                },
                _ => false,
            }
    });
    if let Some(face) = loaded {
        return Some(face.id);
    }

    let ids = Arc::make_mut(database)
        .load_font_source(fontdb::Source::Binary(Arc::new(font.data.clone())));
    ids.get(font.index as usize).copied()
}

fn font_to_fontdescriptor(font: &Font) -> FontDescriptor {
    let style = match font.style() {
        FontStyle::Normal => ServoFontStyle::normal(),
        FontStyle::Italic => ServoFontStyle::ITALIC,
        FontStyle::Oblique => ServoFontStyle::OBLIQUE,
    };

    let width = match font.stretch() {
        FontStretch::UltraCondensed => ServoFontWidth::ULTRA_CONDENSED,
        FontStretch::ExtraCondensed => ServoFontWidth::EXTRA_CONDENSED,
        FontStretch::Condensed => ServoFontWidth::CONDENSED,
        FontStretch::SemiCondensed => ServoFontWidth::SEMI_CONDENSED,
        FontStretch::Normal => ServoFontWidth::NORMAL,
        FontStretch::SemiExpanded => ServoFontWidth::SEMI_EXPANDED,
        FontStretch::Expanded => ServoFontWidth::EXPANDED,
        FontStretch::ExtraExpanded => ServoFontWidth::EXTRA_EXPANDED,
        FontStretch::UltraExpanded => ServoFontWidth::ULTRA_EXPANDED,
    };

    let variation_settings = font
        .variations()
        .iter()
        .map(|variation| FontVariation {
            tag: u32::from_be_bytes(variation.tag),
            value: variation.value,
        })
        .collect();

    FontDescriptor {
        weight: FontWeight::from_float(font.weight() as f32),
        width,
        style,
        variant: FontVariantCaps::Normal,
        pt_size: Au::from_px(16),
        variation_settings,
        synthesis_weight: FontSynthesis::Auto,
        optical_sizing: FontOpticalSizing::Auto,
    }
}

fn fallback_descriptor() -> FontDescriptor {
    FontDescriptor {
        weight: FontWeight::normal(),
        width: ServoFontWidth::hundred(),
        style: ServoFontStyle::normal(),
        variant: FontVariantCaps::Normal,
        pt_size: Au::from_px(16),
        variation_settings: vec![],
        synthesis_weight: FontSynthesis::Auto,
        optical_sizing: FontOpticalSizing::Auto,
    }
}

fn fontfamily_to_singlefontfamily(family: &FontFamily) -> SingleFontFamily {
    match family {
        FontFamily::Serif => SingleFontFamily::Generic(GenericFontFamily::Serif),
        FontFamily::SansSerif => SingleFontFamily::Generic(GenericFontFamily::SansSerif),
        FontFamily::Cursive => SingleFontFamily::Generic(GenericFontFamily::Cursive),
        FontFamily::Fantasy => SingleFontFamily::Generic(GenericFontFamily::Fantasy),
        FontFamily::Monospace => SingleFontFamily::Generic(GenericFontFamily::Monospace),
        FontFamily::Named(name) => SingleFontFamily::FamilyName(FamilyName {
            name: name.as_str().into(),
            syntax: FontFamilyNameSyntax::Quoted,
        }),
    }
}

impl FontResolver for SvgFontResolver {
    fn resolve(&self, font: &Font, database: &mut Arc<fontdb::Database>) -> Option<fontdb::ID> {
        let cached = lock(&self.font_cache).get(font).cloned();
        let data_and_index = match cached {
            Some(data_and_index) => data_and_index,
            None => {
                let data_and_index = self.find_font(font)?;
                lock(&self.font_cache).insert(font.clone(), data_and_index.clone());
                data_and_index
            },
        };
        load_into_database(&data_and_index, database)
    }

    fn resolve_fallback(
        &self,
        character: char,
        excluded: &[fontdb::ID],
        database: &mut Arc<fontdb::Database>,
    ) -> Option<fontdb::ID> {
        let cached = lock(&self.fallback_cache)
            .get(&character)
            .cloned()
            .unwrap_or_default();
        for data_and_index in &cached {
            if let Some(id) = load_into_database(data_and_index, database) &&
                !excluded.contains(&id)
            {
                return Some(id);
            }
        }
        let fallback_options = FallbackFontSelectionOptions::new(
            character,
            None,
            icu_locale_core::subtags::Language::UNKNOWN,
        );
        for family in fallback_font_families(fallback_options) {
            let family = FontFamilyDescriptor::new(
                SingleFontFamily::FamilyName(FamilyName {
                    name: family.into(),
                    syntax: FontFamilyNameSyntax::Quoted,
                }),
                FontSearchScope::Any,
            );
            let fallback_descriptor = fallback_descriptor();

            let font_templates = self
                .context
                .matching_templates(&fallback_descriptor, &family);

            for font_template in font_templates {
                let Some(font_ref) = self.context.font(font_template, &fallback_descriptor) else {
                    continue;
                };
                if !font_ref.has_glyph_for(character) {
                    continue;
                }

                let Ok(data_and_index) = font_ref.font_data_and_index() else {
                    continue;
                };

                let Some(id) = load_into_database(data_and_index, database) else {
                    continue;
                };

                if excluded.contains(&id) {
                    continue;
                }

                lock(&self.fallback_cache)
                    .entry(character)
                    .or_default()
                    .push(data_and_index.clone());
                return Some(id);
            }
        }
        None
    }
}
