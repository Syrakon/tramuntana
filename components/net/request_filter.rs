/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! An extension point for embedders that want to block requests before they reach
//! the network, such as content blockers.
//!
//! Unlike [`crate::request_interceptor::RequestInterceptor`], which asks the embedder
//! over a channel and can substitute a whole response, a filter answers synchronously
//! and can only allow or block. That keeps the cost per subresource low enough to run
//! on every request.

use std::sync::Arc;

use content_security_policy::Destination;
use http::Method;
use net_traits::request::{Origin, Request, RequestMode};
use servo_base::id::WebViewId;
use servo_url::{ImmutableOrigin, ServoUrl};

/// What a [`RequestFilter`] decided about a request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilterVerdict {
    /// Let the fetch continue.
    Allow,
    /// Fail the fetch with [`net_traits::NetworkError::Blocked`].
    Block,
}

/// Everything a [`RequestFilter`] is told about a request.
pub struct FilteredRequest<'a> {
    /// The URL about to be fetched, with HSTS upgrades applied. On a redirect this is
    /// the URL of the current hop, so every hop is filtered separately.
    pub url: &'a ServoUrl,
    /// The URL of the document that caused this request, when one is known. Content
    /// blockers compare it with `url` to tell a first-party load from a third-party
    /// one. It is derived from the request's origin, falling back to its referrer, so
    /// it may carry no path.
    pub document_url: Option<ServoUrl>,
    /// What the request is for: a script, an image, a subframe, and so on.
    pub destination: Destination,
    /// The request method.
    pub method: &'a Method,
    /// Whether this request navigates the top-level frame of a [`WebViewId`].
    pub is_for_main_frame: bool,
    /// Whether this request opens a WebSocket. Its URL has already been given an
    /// HTTP scheme and its destination is empty, like that of `fetch()`, so the mode
    /// is the only thing that tells the two apart.
    pub is_websocket: bool,
    /// The `WebView` this request belongs to, when it belongs to one.
    pub webview_id: Option<WebViewId>,
}

impl<'a> FilteredRequest<'a> {
    /// Describe `request` for a filter. `url` is passed separately because the caller
    /// already holds the current URL.
    pub fn new(request: &'a Request, url: &'a ServoUrl) -> Self {
        Self {
            url,
            document_url: document_url_of(request),
            destination: request.destination,
            method: &request.method,
            is_for_main_frame: matches!(request.destination, Destination::Document),
            is_websocket: matches!(request.mode, RequestMode::WebSocket { .. }),
            webview_id: request.target_webview_id,
        }
    }
}

/// The document a request is made from, as a URL. A tuple origin is preferred over the
/// referrer because the referrer policy can strip the referrer away entirely.
fn document_url_of(request: &Request) -> Option<ServoUrl> {
    if let Origin::Origin(origin @ ImmutableOrigin::Tuple(..)) = &request.origin &&
        let Ok(url) = ServoUrl::parse(&origin.ascii_serialization())
    {
        return Some(url);
    }
    request.referrer.to_url().cloned()
}

/// A synchronous veto on outgoing requests, supplied by the embedder through
/// `ServoBuilder::request_filter`.
///
/// Implementations are called on the fetch threads, once per request and once more per
/// redirect hop, so they must not block and must not call back into the embedder.
pub trait RequestFilter: Send + Sync {
    fn filter(&self, request: &FilteredRequest<'_>) -> FilterVerdict;
}

/// The filter a [`crate::fetch::methods::FetchContext`] carries, if any.
pub type SharedRequestFilter = Option<Arc<dyn RequestFilter>>;
