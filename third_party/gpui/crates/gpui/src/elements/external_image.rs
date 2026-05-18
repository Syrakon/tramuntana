//! Element that paints an externally-owned GPU texture provided by an
//! embedder (e.g. rumb's Servo bridge). Companion to `gpui::img` for the
//! atlas-cached CPU-side path; this one stays GPU-resident end to end.
//!
//! See [`crate::external_texture`] for the trait the embedder implements
//! and `tramuntana/docs/STAGE_2_GPU_SHARING.md` for the full bridge design.

use std::panic;

use crate::{
    Bounds, Corners, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Style, Window,
    external_texture::ExternalGpuTextureHandle,
};
use gpui_util::ResultExt;

/// Construct an [`ExternalImage`] element backed by an embedder-owned GPU
/// texture handle.
#[track_caller]
pub fn external_image(handle: ExternalGpuTextureHandle) -> ExternalImage {
    ExternalImage {
        handle,
        source_location: panic::Location::caller(),
    }
}

/// Element that emits a single [`crate::scene::ExternalSprite`] for the
/// given handle on every paint. Layout uses the handle's reported size;
/// wrap in a sized container if you want layout to drive the dimensions
/// instead.
pub struct ExternalImage {
    handle: ExternalGpuTextureHandle,
    source_location: &'static panic::Location<'static>,
}

impl IntoElement for ExternalImage {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ExternalImage {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static panic::Location<'static>> {
        Some(self.source_location)
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut crate::App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        // Use the texture's native size as the default layout. A wrapping
        // container with explicit width/height (or flex) can override.
        let size = self.handle.size();
        style.size.width = crate::Length::Definite(crate::DefiniteLength::Absolute(
            crate::AbsoluteLength::Pixels(Pixels(size.width.0 as f32)),
        ));
        style.size.height = crate::Length::Definite(crate::DefiniteLength::Absolute(
            crate::AbsoluteLength::Pixels(Pixels(size.height.0 as f32)),
        ));
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut crate::App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut crate::App,
    ) {
        // Pass through to Window::paint_external_image which emits the
        // ExternalSprite scene primitive. Corner radii default to zero —
        // wrap in a styled container if rounded corners are needed.
        window
            .paint_external_image(bounds, Corners::default(), self.handle.clone())
            .log_err();
    }
}
