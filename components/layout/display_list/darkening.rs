/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Darkening of pages that stay light when the user asked for dark ones.
//!
//! When the embedder sets [`embedder_traits::Theme::ForcedDark`] and a page
//! paints a light canvas anyway (it offers no dark scheme of its own), the
//! colours of its display list are rewritten as they are converted for
//! WebRender: a light background turns dark, dark text turns light, and
//! colours that already suit a dark page are kept. Images are not touched.
//!
//! The rule follows Chromium's dark mode: each colour is judged by its role,
//! and the ones that are changed have their lightness inverted in CIE Lab, so
//! that the hue survives. The inversion maps white onto [`DARKEST`], `#090909`,
//! rather than pure black, and black onto white.
//!
//! Images keep their colours, except the ones that read as a dark icon or
//! logo (see [`is_dark_icon`]): drawn for a light page, they would vanish on
//! the darkened one, so they are painted inverted.

use std::cell::Cell;
use std::sync::LazyLock;

use pixels::{PixelFormat, RasterImage};
use style::color::{AbsoluteColor, ColorSpace};

/// What a colour paints, which decides whether it is changed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ColorRole {
    /// Backgrounds, gradients and shadows: changed when they are light.
    Background,
    /// Borders and outlines: changed when they are lighter than mid grey, so
    /// that a pale hairline on a white page stays a pale hairline once the
    /// page is dark.
    Border,
    /// Text, its decorations and the caret: changed when they are dark.
    Foreground,
}

/// Brightness, from 0 to 1, above which a background counts as light.
/// Chromium's value: 205 of 255.
const BACKGROUND_THRESHOLD: f32 = 205. / 255.;

/// Brightness above which a border counts as light, and below which text
/// counts as dark. Chromium's foreground value: 150 of 255.
const FOREGROUND_THRESHOLD: f32 = 150. / 255.;

/// The colour a white background turns into.
const DARKEST: (u8, u8, u8) = (0x09, 0x09, 0x09);

/// The Lab lightness of [`DARKEST`], where the inversion sends `L = 100`.
static DARKEST_LIGHTNESS: LazyLock<f32> = LazyLock::new(|| {
    AbsoluteColor::srgb_legacy(DARKEST.0, DARKEST.1, DARKEST.2, 1.0)
        .to_color_space(ColorSpace::Lab)
        .components
        .0
});

thread_local! {
    /// Whether the display list being built on this thread is darkened. A
    /// display list is built in one go on the layout thread, and the colours
    /// are converted from free functions all over the builder, so the switch
    /// lives here rather than being threaded through each of them.
    static DARKENING: Cell<bool> = const { Cell::new(false) };
}

/// Turns darkening on for the display list being built, and off again when
/// dropped, so that no later display list on this thread inherits it.
pub(crate) struct DarkeningScope;

impl DarkeningScope {
    pub(crate) fn enter(darken: bool) -> Self {
        DARKENING.with(|cell| cell.set(darken));
        DarkeningScope
    }
}

impl Drop for DarkeningScope {
    fn drop(&mut self) {
        DARKENING.with(|cell| cell.set(false));
    }
}

/// Whether the display list being built on this thread is darkened.
pub(crate) fn is_active() -> bool {
    DARKENING.with(Cell::get)
}

/// The colour to paint in place of `color` in the display list being built.
pub(crate) fn darkened(color: AbsoluteColor, role: ColorRole) -> AbsoluteColor {
    if !DARKENING.with(Cell::get) {
        return color;
    }
    darken(color, role)
}

/// Perceived brightness of a colour, from 0 to 1, with the weights Chromium
/// uses to classify colours.
pub(crate) fn brightness(color: AbsoluteColor) -> f32 {
    let srgb = color.to_color_space(ColorSpace::Srgb);
    let (r, g, b) = (
        srgb.components.0.clamp(0.0, 1.0),
        srgb.components.1.clamp(0.0, 1.0),
        srgb.components.2.clamp(0.0, 1.0),
    );
    0.299 * r + 0.587 * g + 0.114 * b
}

/// Whether a canvas of this colour makes a light page.
pub(crate) fn is_light_canvas(color: AbsoluteColor) -> bool {
    brightness(color) > BACKGROUND_THRESHOLD
}

fn darken(color: AbsoluteColor, role: ColorRole) -> AbsoluteColor {
    let brightness = brightness(color);
    let change = match role {
        ColorRole::Background => brightness > BACKGROUND_THRESHOLD,
        ColorRole::Border => brightness > FOREGROUND_THRESHOLD,
        ColorRole::Foreground => brightness < FOREGROUND_THRESHOLD,
    };
    if !change {
        return color;
    }
    let lab = color.to_color_space(ColorSpace::Lab);
    let lightness = lab.components.0.clamp(0.0, 100.0);
    let darkest = *DARKEST_LIGHTNESS;
    AbsoluteColor::new(
        ColorSpace::Lab,
        darkest + (100.0 - lightness) * (100.0 - darkest) / 100.0,
        lab.components.1,
        lab.components.2,
        lab.alpha,
    )
    .to_color_space(ColorSpace::Srgb)
}

/// How many pixels along each side [`is_dark_icon`] looks at, at most.
const ICON_SAMPLES: u32 = 64;

/// Whether `image` reads as a dark icon or logo drawn for a light page: it
/// has transparent parts, and what it draws is dark and nearly without
/// colour. A photo, a colourful logo or an image with a background of its own
/// is none of these, and keeps its colours.
pub(crate) fn is_dark_icon(image: &RasterImage) -> bool {
    let Some(frame) = image.frame(0) else {
        return false;
    };
    let channels = match image.format {
        PixelFormat::RGBA8 => PixelChannels::Rgba,
        PixelFormat::BGRA8 => PixelChannels::Bgra,
        PixelFormat::KA8 => PixelChannels::LumaAlpha,
        // No alpha: nothing shows through, so it is not an icon on the page.
        PixelFormat::K8 | PixelFormat::RGB8 => return false,
    };
    classify_icon(frame.bytes, frame.width, frame.height, channels)
}

/// The layout of the pixels [`classify_icon`] reads. Colour channels are
/// premultiplied by alpha, as Servo keeps decoded images.
#[derive(Clone, Copy)]
enum PixelChannels {
    Rgba,
    Bgra,
    LumaAlpha,
}

fn classify_icon(bytes: &[u8], width: u32, height: u32, channels: PixelChannels) -> bool {
    let stride = match channels {
        PixelChannels::Rgba | PixelChannels::Bgra => 4,
        PixelChannels::LumaAlpha => 2,
    };
    if width == 0 || height == 0 || bytes.len() < (width * height) as usize * stride {
        return false;
    }
    let step_x = width.div_ceil(ICON_SAMPLES).max(1);
    let step_y = height.div_ceil(ICON_SAMPLES).max(1);
    let (mut samples, mut transparent, mut drawn, mut dark_grey) = (0u32, 0u32, 0u32, 0u32);
    for y in (0..height).step_by(step_y as usize) {
        for x in (0..width).step_by(step_x as usize) {
            let at = (y * width + x) as usize * stride;
            let Some(pixel) = bytes.get(at..at + stride) else {
                continue;
            };
            let (r, g, b, a) = match channels {
                PixelChannels::Rgba => (pixel[0], pixel[1], pixel[2], pixel[3]),
                PixelChannels::Bgra => (pixel[2], pixel[1], pixel[0], pixel[3]),
                PixelChannels::LumaAlpha => (pixel[0], pixel[0], pixel[0], pixel[1]),
            };
            samples += 1;
            if a < 26 {
                transparent += 1;
                continue;
            }
            if a < 128 {
                // Antialiased edges say nothing of the colour.
                continue;
            }
            drawn += 1;
            // Undo the premultiplication to judge the colour that is drawn.
            let unmultiply = |c: u8| (c as f32 / a as f32).min(1.0);
            let (r, g, b) = (unmultiply(r), unmultiply(g), unmultiply(b));
            let brightness = 0.299 * r + 0.587 * g + 0.114 * b;
            let chroma = r.max(g).max(b) - r.min(g).min(b);
            if brightness < 0.35 && chroma < 0.2 {
                dark_grey += 1;
            }
        }
    }
    if samples == 0 || drawn * 100 < samples {
        return false;
    }
    // Some of it shows the page through, and nearly all it draws is dark grey.
    transparent * 10 >= samples && dark_grey * 10 >= drawn * 9
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srgb(r: u8, g: u8, b: u8) -> AbsoluteColor {
        AbsoluteColor::srgb_legacy(r, g, b, 1.0)
    }

    /// A white background turns into #090909, not pure black.
    #[test]
    fn white_background_turns_090909() {
        let dark = darken(srgb(255, 255, 255), ColorRole::Background)
            .to_color_space(ColorSpace::Srgb);
        for channel in [dark.components.0, dark.components.1, dark.components.2] {
            assert_eq!((channel * 255.0).round() as u8, 0x09, "{dark:?}");
        }
    }

    /// Premultiplied RGBA pixels: `(r, g, b, a)` straight, stored premultiplied.
    fn image(pixels: &[(u8, u8, u8, u8)]) -> Vec<u8> {
        pixels
            .iter()
            .flat_map(|&(r, g, b, a)| {
                let m = |c: u8| (c as u32 * a as u32 / 255) as u8;
                [m(r), m(g), m(b), a]
            })
            .collect()
    }

    /// A black logo on a transparent ground is a dark icon.
    #[test]
    fn black_logo_on_transparency_is_a_dark_icon() {
        let mut pixels = vec![(0, 0, 0, 0); 50];
        pixels.extend(vec![(20, 20, 20, 255); 50]);
        assert!(classify_icon(&image(&pixels), 10, 10, PixelChannels::Rgba));
    }

    /// A colourful logo, an opaque photo and a white icon keep their colours.
    #[test]
    fn other_images_are_not_dark_icons() {
        let mut colourful = vec![(0, 0, 0, 0); 50];
        colourful.extend(vec![(220, 40, 40, 255); 50]);
        assert!(!classify_icon(&image(&colourful), 10, 10, PixelChannels::Rgba));

        let opaque = vec![(20, 20, 20, 255); 100];
        assert!(!classify_icon(&image(&opaque), 10, 10, PixelChannels::Rgba));

        let mut white = vec![(0, 0, 0, 0); 50];
        white.extend(vec![(250, 250, 250, 255); 50]);
        assert!(!classify_icon(&image(&white), 10, 10, PixelChannels::Rgba));
    }

    /// Black text turns white.
    #[test]
    fn black_text_turns_light() {
        let light = darken(srgb(0, 0, 0), ColorRole::Foreground);
        assert!(brightness(light) > 0.95);
    }

    /// Colours that already suit a dark page are kept: a dark background, a
    /// light text, a dark border.
    #[test]
    fn dark_friendly_colours_are_kept() {
        let navy = srgb(20, 30, 60);
        assert_eq!(darken(navy, ColorRole::Background), navy);
        let white = srgb(255, 255, 255);
        assert_eq!(darken(white, ColorRole::Foreground), white);
        let charcoal = srgb(50, 50, 50);
        assert_eq!(darken(charcoal, ColorRole::Border), charcoal);
    }

    /// A pale hairline stays darker than the text and lighter than the page
    /// once both are darkened: the order of the three is preserved.
    #[test]
    fn contrast_order_survives() {
        let page = brightness(darken(srgb(255, 255, 255), ColorRole::Background));
        let hairline = brightness(darken(srgb(221, 221, 221), ColorRole::Border));
        let text = brightness(darken(srgb(34, 34, 34), ColorRole::Foreground));
        assert!(page < hairline && hairline < text, "{page} {hairline} {text}");
    }

    /// The hue of a changed colour survives: a pale blue stays blue.
    #[test]
    fn hue_survives() {
        let dark = darken(srgb(220, 235, 255), ColorRole::Background)
            .to_color_space(ColorSpace::Srgb);
        assert!(dark.components.2 > dark.components.0);
    }

    /// Alpha is kept, so a translucent overlay stays translucent.
    #[test]
    fn alpha_is_kept() {
        let overlay = AbsoluteColor::srgb_legacy(255, 255, 255, 0.5);
        assert_eq!(darken(overlay, ColorRole::Background).alpha, 0.5);
    }

    /// Outside a darkening scope, colours pass through untouched, and the
    /// scope ends when it is dropped.
    #[test]
    fn scope_switches_darkening() {
        let white = srgb(255, 255, 255);
        assert_eq!(darkened(white, ColorRole::Background), white);
        {
            let _scope = DarkeningScope::enter(true);
            assert_ne!(darkened(white, ColorRole::Background), white);
        }
        assert_eq!(darkened(white, ColorRole::Background), white);
    }
}
