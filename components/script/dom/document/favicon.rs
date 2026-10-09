/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The implicit `/favicon.ico` of a document.
//!
//! A page normally points at its icon with `<link rel=icon>`, which
//! [`HTMLLinkElement`](crate::dom::html::htmllinkelement::HTMLLinkElement)
//! fetches. Many sites declare no such link and rely on the icon being served
//! from `/favicon.ico` at the root of the origin instead. The HTML standard
//! does not describe that fallback, but every browser performs it, so a
//! document with no icon link asks for it here once parsing is done.

use embedder_traits::Image;
use net_traits::image_cache::{
    Image as DecodedImage, ImageCache, ImageCacheResponseCallback, ImageCacheResult,
    ImageLoadListener, ImageOrMetadataAvailable, ImageResponse, PendingImageId,
};
use net_traits::request::{Destination, RequestId};
use net_traits::{FetchMetadata, FetchResponseMsg, NetworkError, ResourceFetchTiming};
use pixels::RasterImage;
use servo_base::generic_channel::GenericSharedMemory;
use servo_url::ServoUrl;
use webrender_api::units::DeviceIntSize;

use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::csp::{GlobalCspReporting, Violation};
use crate::dom::document::Document;
use crate::dom::performance::performanceresourcetiming::InitiatorType;
use crate::dom::types::GlobalScope;
use crate::fetch::{RequestWithGlobalScope, create_a_potential_cors_request};
use crate::network_listener::{FetchResponseListener, ResourceTimingListener, submit_timing};

/// The size a vector favicon is rasterized at. The embedder scales the result
/// down to whatever its chrome uses, so this only sets the ceiling on detail.
const VECTOR_FAVICON_SIZE: DeviceIntSize = DeviceIntSize::new(250, 250);

/// The URL an icon is served from when a document declares none: `/favicon.ico`
/// at the root of the document's own origin. Returns `None` for schemes where
/// the convention means nothing (`about:`, `data:`, `file:`, …).
pub(crate) fn default_favicon_url(document_url: &ServoUrl) -> Option<ServoUrl> {
    if !matches!(document_url.scheme(), "http" | "https") {
        return None;
    }
    // Deliberately resolved against the document URL rather than the document's
    // base URL: `<base href>` moves relative links, not the site's own icon.
    ServoUrl::parse_with_base(Some(document_url), "/favicon.ico").ok()
}

/// Ask the image cache for `url`, fetching it if it is not there already.
pub(crate) fn fetch_default_favicon(document: &Document, url: ServoUrl) {
    let window = document.window();
    let image_cache = window.image_cache();

    // The implicit icon carries no `crossorigin` attribute, so it is fetched
    // without CORS, exactly like a `<link rel=icon>` without one.
    let cache_result =
        image_cache.get_cached_image_status(url.clone(), window.origin().immutable().clone(), None);

    let id = match cache_result {
        ImageCacheResult::Available(ImageOrMetadataAvailable::ImageAvailable { image, .. }) => {
            process_default_favicon_image(document, image);
            return;
        },
        ImageCacheResult::Available(ImageOrMetadataAvailable::MetadataAvailable(_, id)) |
        ImageCacheResult::Pending(id) => {
            let sender = register_image_cache_callback(document, id);
            image_cache.add_listener(ImageLoadListener::new(sender, window.pipeline_id(), id));
            return;
        },
        ImageCacheResult::ReadyForRequest(id) => id,
        ImageCacheResult::FailedToLoadOrDecode => return,
    };

    let global = document.global();
    let request = create_a_potential_cors_request(
        Some(window.webview_id()),
        url.clone(),
        Destination::Image,
        None,
        None,
        global.get_referrer(),
    )
    .with_global_scope(&global);

    let sender = register_image_cache_callback(document, id);
    image_cache.add_listener(ImageLoadListener::new(sender, window.pipeline_id(), id));

    document.fetch_background(
        request,
        DefaultFaviconFetchContext {
            document: Trusted::new(document),
            image_cache,
            id,
            url,
        },
    );
}

/// Hand the decoded image to the document once the image cache has it.
fn register_image_cache_callback(
    document: &Document,
    id: PendingImageId,
) -> ImageCacheResponseCallback {
    let trusted_document = Trusted::new(document);
    document
        .window()
        .register_image_cache_listener(id, move |response, _| {
            let ImageResponse::Loaded(image, _) = response.response else {
                // Metadata and failures tell us nothing about the icon.
                return;
            };

            let trusted_document = trusted_document.clone();
            trusted_document
                .root()
                .global()
                .task_manager()
                .networking_task_source()
                .queue(task!(process_default_favicon_response: move || {
                    let document = trusted_document.root();
                    process_default_favicon_image(&document, image);
                }));
        })
}

/// Rasterize the image if needed and store it, unless the document has since
/// found an icon of its own: a `<link rel=icon>` always outranks the fallback,
/// however late its fetch completes.
fn process_default_favicon_image(document: &Document, image: DecodedImage) {
    match image {
        DecodedImage::Raster(raster_image) => {
            if document.has_favicon() {
                return;
            }
            document.set_favicon(embedder_image_from_raster(&raster_image));
        },
        DecodedImage::Vector(vector_image) => {
            let image_cache = document.window().image_cache();
            if let Some(raster_image) =
                image_cache.rasterize_vector_image(vector_image.id, VECTOR_FAVICON_SIZE, None)
            {
                if document.has_favicon() {
                    return;
                }
                document.set_favicon(embedder_image_from_raster(&raster_image));
            } else {
                // Rasterization is under way. The completed raster image is not
                // handed to the listener, so it is collected from the cache once
                // the listener says it is there.
                let trusted_document = Trusted::new(document);
                let sender = document.window().register_rasterization_complete_listener(
                    vector_image.id,
                    VECTOR_FAVICON_SIZE,
                    move || {
                        let document = trusted_document.root();
                        let rasterized = document.window().image_cache().rasterize_vector_image(
                            vector_image.id,
                            VECTOR_FAVICON_SIZE,
                            None,
                        );
                        if let Some(raster_image) = rasterized &&
                            !document.has_favicon()
                        {
                            document.set_favicon(embedder_image_from_raster(&raster_image));
                        }
                    },
                );
                image_cache.add_rasterization_complete_listener(
                    document.window().pipeline_id(),
                    vector_image.id,
                    VECTOR_FAVICON_SIZE,
                    sender,
                );
            }
        },
    }
}

/// Convert a decoded raster image into the representation the embedder reads.
/// Animated icons are flattened to their first frame.
pub(crate) fn embedder_image_from_raster(raster_image: &RasterImage) -> Image {
    let frame = raster_image.first_frame();
    let format = match raster_image.format {
        pixels::PixelFormat::K8 => embedder_traits::PixelFormat::K8,
        pixels::PixelFormat::KA8 => embedder_traits::PixelFormat::KA8,
        pixels::PixelFormat::RGB8 => embedder_traits::PixelFormat::RGB8,
        pixels::PixelFormat::RGBA8 => embedder_traits::PixelFormat::RGBA8,
        pixels::PixelFormat::BGRA8 => embedder_traits::PixelFormat::BGRA8,
    };

    Image::new(
        frame.width,
        frame.height,
        std::sync::Arc::new(GenericSharedMemory::from_arc_vec(
            raster_image.bytes.clone(),
        )),
        raster_image.frames[0].byte_range.clone(),
        format,
    )
}

/// Feeds the `/favicon.ico` response into the image cache, which decodes it and
/// notifies the listener registered above.
struct DefaultFaviconFetchContext {
    /// The document whose icon this is.
    document: Trusted<Document>,
    image_cache: std::sync::Arc<dyn ImageCache>,
    id: PendingImageId,
    /// The URL being fetched, reported as this load's resource timing entry.
    url: ServoUrl,
}

impl FetchResponseListener for DefaultFaviconFetchContext {
    fn process_request_body(&mut self, _: RequestId) {}

    fn process_response(
        &mut self,
        _: &mut js::context::JSContext,
        request_id: RequestId,
        metadata: Result<FetchMetadata, NetworkError>,
    ) {
        self.image_cache.notify_pending_response(
            self.id,
            FetchResponseMsg::ProcessResponse(request_id, metadata),
        );
    }

    fn process_response_chunk(
        &mut self,
        _: &mut js::context::JSContext,
        request_id: RequestId,
        chunk: Vec<u8>,
    ) {
        self.image_cache.notify_pending_response(
            self.id,
            FetchResponseMsg::ProcessResponseChunk(request_id, chunk.into()),
        );
    }

    fn process_response_eof(
        self,
        cx: &mut js::context::JSContext,
        request_id: RequestId,
        response: Result<(), NetworkError>,
        timing: ResourceFetchTiming,
    ) {
        self.image_cache.notify_pending_response(
            self.id,
            FetchResponseMsg::ProcessResponseEOF(request_id, response.clone(), timing.clone()),
        );
        submit_timing(cx, &self, &response, &timing);
    }

    fn process_csp_violations(&mut self, _request_id: RequestId, violations: Vec<Violation>) {
        let global = &self.resource_timing_global();
        global.report_csp_violations(violations, None, None);
    }
}

impl ResourceTimingListener for DefaultFaviconFetchContext {
    fn resource_timing_information(&self) -> (InitiatorType, ServoUrl) {
        (InitiatorType::Other, self.url.clone())
    }

    fn resource_timing_global(&self) -> DomRoot<GlobalScope> {
        self.document.root().global()
    }
}
