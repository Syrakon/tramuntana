/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Masks for `background-clip: text`. WebRender cannot clip to glyphs, so the glyphs of a box
//! are rasterized into an alpha image here and its backgrounds are clipped to that image.
//!
//! <https://drafts.csswg.org/css-backgrounds-4/#valdef-background-clip-text>

use std::collections::hash_map::Entry;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use fonts::FontRef;
use fonts_traits::FontIdentifier;
use paint_api::{CrossProcessPaintApi, SerializableImageData};
use rustc_hash::{FxHashMap, FxHasher};
use servo_base::generic_channel::GenericSharedMemory;
use servo_base::id::WebViewId;
use webrender_api::units::LayoutSize;
use webrender_api::{GlyphInstance, ImageDescriptor, ImageDescriptorFlags, ImageFormat, ImageKey};

/// The largest mask, in device pixels. A bigger box gets a mask of lower resolution.
const MAX_MASK_AREA: f32 = 4096. * 4096.;

/// Glyphs of one font, positioned relative to the top left corner of the mask.
pub(crate) struct TextMaskRun {
    pub font: FontRef,
    pub glyphs: Vec<GlyphInstance>,
}

struct TextMask {
    image_key: ImageKey,
    /// Whether the display list being built uses this mask.
    used: bool,
}

/// The text masks of a `Document`, kept while its display lists use them.
pub(crate) struct TextMasks {
    paint_api: CrossProcessPaintApi,
    webview_id: WebViewId,
    masks: FxHashMap<u64, TextMask>,
    fonts: FxHashMap<FontIdentifier, peniko::FontData>,
    resources: vello_cpu::Resources,
}

impl TextMasks {
    pub(crate) fn new(paint_api: CrossProcessPaintApi, webview_id: WebViewId) -> Self {
        Self {
            paint_api,
            webview_id,
            masks: Default::default(),
            fonts: Default::default(),
            resources: vello_cpu::Resources::new(),
        }
    }

    /// Start building a display list: masks it does not ask for are released afterwards.
    pub(crate) fn begin_display_list(&mut self) {
        for mask in self.masks.values_mut() {
            mask.used = false;
        }
    }

    /// Release the masks that the last display list did not use. This must be called after
    /// that display list was sent, so that the one before it does not lose its masks early.
    pub(crate) fn release_unused(&mut self) {
        let paint_api = &self.paint_api;
        self.masks.retain(|_, mask| {
            if !mask.used {
                paint_api.delete_image(mask.image_key);
            }
            mask.used
        });
    }

    /// The mask of `runs` for an area of `size`, rasterized at `scale` device pixels per CSS
    /// pixel. Returns `None` when there is nothing to paint through the mask.
    pub(crate) fn mask(
        &mut self,
        runs: &[TextMaskRun],
        size: LayoutSize,
        mut scale: f32,
    ) -> Option<ImageKey> {
        if runs.iter().all(|run| run.glyphs.is_empty()) {
            return None;
        }
        let area = size.width * size.height * scale * scale;
        if area > MAX_MASK_AREA {
            scale *= (MAX_MASK_AREA / area).sqrt();
        }
        let width = (size.width * scale).ceil();
        let height = (size.height * scale).ceil();
        if width < 1. || height < 1. || width > u16::MAX as f32 || height > u16::MAX as f32 {
            return None;
        }
        let (width, height) = (width as u16, height as u16);

        let mut hasher = FxHasher::default();
        (width, height, scale.to_bits()).hash(&mut hasher);
        for run in runs {
            run.font.identifier().hash(&mut hasher);
            run.font.descriptor.pt_size.hash(&mut hasher);
            for glyph in &run.glyphs {
                (glyph.index, glyph.point.x.to_bits(), glyph.point.y.to_bits()).hash(&mut hasher);
            }
        }
        let hash = hasher.finish();
        if let Some(mask) = self.masks.get_mut(&hash) {
            mask.used = true;
            return Some(mask.image_key);
        }

        let alpha = self.rasterize(runs, width, height, scale);
        let image_key = self.paint_api.generate_image_key_blocking(self.webview_id)?;
        let descriptor = ImageDescriptor::new(
            width.into(),
            height.into(),
            ImageFormat::R8,
            ImageDescriptorFlags::empty(),
        );
        self.paint_api.add_image(
            image_key,
            descriptor,
            SerializableImageData::Raw(GenericSharedMemory::from_bytes(&alpha)),
            false,
        );
        self.masks.insert(
            hash,
            TextMask {
                image_key,
                used: true,
            },
        );
        Some(image_key)
    }

    /// Paint the glyphs of `runs` and return the coverage of each pixel.
    fn rasterize(&mut self, runs: &[TextMaskRun], width: u16, height: u16, scale: f32) -> Vec<u8> {
        let mut context = vello_cpu::RenderContext::new(width, height);
        context.set_transform(kurbo::Affine::scale(scale.into()));
        context.set_paint(vello_cpu::peniko::color::palette::css::BLACK);
        for run in runs {
            let identifier = run.font.identifier().clone();
            let font_data = match self.fonts.entry(identifier) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let Ok(data_and_index) = run.font.font_data_and_index() else {
                        continue;
                    };
                    entry.insert(peniko::FontData::new(
                        peniko::Blob::new(Arc::new(data_and_index.data.clone())),
                        data_and_index.index,
                    ))
                },
            };
            context
                .glyph_run(&mut self.resources, font_data)
                .font_size(run.font.descriptor.pt_size.to_f32_px())
                .fill_glyphs(run.glyphs.iter().map(|glyph| vello_cpu::Glyph {
                    id: glyph.index,
                    x: glyph.point.x,
                    y: glyph.point.y,
                }));
        }
        context.flush();
        let mut pixmap = vello_cpu::Pixmap::new(width, height);
        context.render(&mut pixmap, &mut self.resources);
        pixmap.data().iter().map(|pixel| pixel.a).collect()
    }
}

impl Drop for TextMasks {
    fn drop(&mut self) {
        for mask in self.masks.values() {
            self.paint_api.delete_image(mask.image_key);
        }
    }
}
