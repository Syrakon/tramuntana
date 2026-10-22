/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

#![deny(missing_docs)]

//! A cache implementing the logic specified in <http://tools.ietf.org/html/rfc7234>
//! and <http://tools.ietf.org/html/rfc7232>.
//!
//! The cache has two tiers. The memory tier holds the responses in use and is
//! bounded by the bytes of their bodies. The disk tier, which only the cache
//! of the public session has, keeps complete responses across restarts; see
//! [`crate::http_disk_cache`] for its files.

use std::collections::HashSet;
use std::io;
use std::ops::Bound;
use std::path::PathBuf;
use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use headers::{
    CacheControl, ContentLength, ContentRange, Expires, HeaderMapExt, LastModified, Pragma, Range,
    Vary,
};
use http::{HeaderMap, Method, StatusCode, header};
use log::{debug, error, warn};
use malloc_size_of::{MallocSizeOf, MallocSizeOfOps};
use malloc_size_of_derive::MallocSizeOf;
use net_traits::http_status::HttpStatus;
use net_traits::request::{CacheMode, Request};
use net_traits::response::{Response, ResponseBody};
use net_traits::{CacheEntryDescriptor, Metadata, ResourceFetchTiming};
use parking_lot::Mutex as ParkingLotMutex;
use quick_cache::sync::{Cache, PlaceholderGuard};
use quick_cache::{DefaultHashBuilder, Lifecycle, OptionsBuilder, Weighter};
use serde::{Deserialize, Serialize};
use servo_arc::Arc;
use servo_base::id::WebViewId;
use servo_config::pref;
use servo_url::ServoUrl;
use tokio::sync::mpsc::{UnboundedSender as TokioSender, unbounded_channel as unbounded};
use tokio::sync::{OwnedRwLockWriteGuard, RwLock as TokioRwLock};

use crate::fetch::methods::{Data, DoneChannel};
use crate::http_disk_cache::DiskCache;

/// The largest response body, in bytes, that the cache keeps.
///
/// A cached response is handed out with its whole body in memory, whichever
/// tier it comes from. One large response, such as a video that a media
/// element fetches (313 MB for a single WebM on a Wikipedia article), would
/// otherwise come back into memory in one piece on every use.
const MAX_CACHED_BODY_SIZE: u64 = 16 * 1024 * 1024;

/// What a cached response is taken to cost in memory besides its body: its
/// headers and URLs. It also keeps the weight of an entry above zero, which
/// the memory tier needs to be able to evict it.
const RESOURCE_OVERHEAD: u64 = 1024;

/// The version of the metadata that the disk tier stores. Metadata of any
/// other version is discarded.
const STORED_ENTRY_VERSION: u32 = 2;

/// The most complete responses that the cache keeps for one url. They differ
/// in the request headers that `Vary` names. A response that varies on
/// `Cookie` would otherwise add one each time a cookie changes.
const MAX_VARIANTS: usize = 4;

/// The key used to differentiate requests in the cache.
#[derive(Clone, Debug, Eq, Hash, MallocSizeOf, PartialEq)]
pub struct CacheKey {
    url: ServoUrl,
}

impl CacheKey {
    /// Create a cache-key from a request.
    pub fn new(request: &Request) -> CacheKey {
        CacheKey {
            url: request.current_url(),
        }
    }

    /// Create a cache-key from a resolved URL.
    pub fn from_url(url: ServoUrl) -> CacheKey {
        CacheKey { url }
    }
}

/// A complete cached resource.
#[derive(Clone, MallocSizeOf)]
pub struct CachedResource {
    #[conditional_malloc_size_of]
    request_headers: Arc<ParkingLotMutex<HeaderMap>>,
    #[conditional_malloc_size_of]
    body: Arc<ParkingLotMutex<ResponseBody>>,
    #[conditional_malloc_size_of]
    aborted: Arc<AtomicBool>,
    #[conditional_malloc_size_of]
    awaiting_body: Arc<ParkingLotMutex<Vec<TokioSender<Data>>>>,
    metadata: CachedMetadata,
    location_url: Option<Result<ServoUrl, String>>,
    status: HttpStatus,
    url_list: Vec<ServoUrl>,
    expires: Duration,
    stale_while_revalidate: Duration,
    #[conditional_malloc_size_of]
    revalidating: StdArc<AtomicBool>,
    last_validated: SystemTime,
    /// The file of the disk tier that holds the body, if any does.
    stored_body: Option<StoredBody>,
    /// Whether the disk tier holds this resource as it is now.
    stored: bool,
}

impl CachedResource {
    pub(crate) fn is_done(&self) -> bool {
        self.body.lock().is_done()
    }
}

/// A body file of the disk tier, as the metadata of its entry describes it.
#[derive(Clone, Deserialize, MallocSizeOf, Serialize)]
struct StoredBody {
    id: u64,
    length: u64,
    /// The SHA-256 of the body. A file of the right length can still hold the
    /// wrong bytes after a crash, since files are not synced when written.
    digest: String,
}

/// Headers as the metadata of the disk tier holds them: a list of names and
/// values. A value is text with one character per byte, so that any value
/// comes back as it was and the usual one reads as itself.
mod stored_headers {
    use http::header::{HeaderName, HeaderValue};
    use http::HeaderMap;
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S: Serializer>(
        headers: &HeaderMap,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let headers: Vec<(&str, String)> = headers
            .iter()
            .map(|(name, value)| {
                let value = value.as_bytes().iter().map(|&byte| char::from(byte));
                (name.as_str(), value.collect())
            })
            .collect();
        headers.serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<HeaderMap, D::Error> {
        let stored = Vec::<(String, String)>::deserialize(deserializer)?;
        let mut headers = HeaderMap::with_capacity(stored.len());
        for (name, value) in stored {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(D::Error::custom)?;
            let value = value
                .chars()
                .map(u8::try_from)
                .collect::<Result<Vec<u8>, _>>()
                .map_err(D::Error::custom)?;
            let value = HeaderValue::from_bytes(&value).map_err(D::Error::custom)?;
            headers.append(name, value);
        }
        Ok(headers)
    }
}

/// A [`CachedResource`] as the metadata of the disk tier holds it.
#[derive(Deserialize, Serialize)]
struct StoredResource {
    /// Only the headers that the `Vary` header of the response names. Nothing
    /// reads the others, and they include the cookies of the request.
    #[serde(with = "stored_headers")]
    request_headers: HeaderMap,
    #[serde(with = "stored_headers")]
    response_headers: HeaderMap,
    final_url: ServoUrl,
    content_type: Option<String>,
    charset: Option<String>,
    metadata_status: HttpStatus,
    location_url: Option<Result<ServoUrl, String>>,
    status: HttpStatus,
    url_list: Vec<ServoUrl>,
    expires: Duration,
    stale_while_revalidate: Duration,
    last_validated: SystemTime,
    /// `None` for a response without a body.
    body: Option<StoredBody>,
}

/// The metadata of one entry of the disk tier.
#[derive(Deserialize, Serialize)]
struct StoredEntry {
    version: u32,
    /// The url of the cache key. Entries are filed under a digest of it.
    url: ServoUrl,
    resources: Vec<StoredResource>,
}

/// Metadata about a loaded resource, such as is obtained from HTTP headers.
#[derive(Clone, MallocSizeOf)]
struct CachedMetadata {
    /// Headers
    #[conditional_malloc_size_of]
    pub headers: Arc<ParkingLotMutex<HeaderMap>>,
    /// Final URL after redirects.
    pub final_url: ServoUrl,
    /// MIME type / subtype.
    pub content_type: Option<String>,
    /// Character set.
    pub charset: Option<String>,
    /// HTTP Status
    pub status: HttpStatus,
}

/// Whether a cached response is fresh or requires validation before or after use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationStatus {
    /// The response is fresh and can be used without any revalidation.
    Valid,
    /// The response is stale.
    Stale {
        /// Whether the stale response can be served immediately, leaving the
        /// caller responsible for revalidating it in the background.
        revalidate_in_background: bool,
    },
}

/// Wrapper around a cached response, including information on re-validation needs
pub(crate) struct CachedResponse {
    /// The response constructed from the cached resource
    pub response: Response,
    /// Whether the stored response is fresh or stale
    pub validation_status: ValidationStatus,
    /// Single-flight guard for the background revalidation.
    pub revalidation_guard: StdArc<AtomicBool>,
}

type CachedResources = std::sync::Arc<TokioRwLock<Vec<CachedResource>>>;

/// The cached resources of one url, as the memory tier holds them.
#[derive(Clone)]
pub struct CacheEntry {
    resources: CachedResources,
    /// The weight of the resources when the entry was last put in the memory
    /// tier. The tier requires that the weight of an entry it holds does not
    /// change, so this is a copy and not computed from `resources`.
    weight: u64,
}

impl CacheEntry {
    fn new(resources: Vec<CachedResource>) -> Self {
        Self {
            weight: weigh(&resources),
            resources: std::sync::Arc::new(TokioRwLock::new(resources)),
        }
    }
}

/// Weighs an entry of the memory tier by the bytes it keeps alive.
#[derive(Clone)]
pub struct BodyWeighter;

impl Weighter<CacheKey, CacheEntry> for BodyWeighter {
    fn weight(&self, _key: &CacheKey, entry: &CacheEntry) -> u64 {
        entry.weight
    }
}

/// The lifecycle hooks of the memory tier.
#[derive(Clone)]
pub struct MemoryCacheLifecycle;

impl Lifecycle<CacheKey, CacheEntry> for MemoryCacheLifecycle {
    type RequestState = ();

    // Cached Resources that are not complete could get evicted which means they cannot fill their body.
    // We allow unfinished resources to stay in the cache.
    fn is_pinned(&self, _key: &CacheKey, entry: &CacheEntry) -> bool {
        entry
            .resources
            .try_read()
            .map(|cached_resources| cached_resources.iter().any(|resource| !resource.is_done()))
            .unwrap_or(true)
    }

    // The disk tier is written when a fetch ends, not when an entry leaves memory.
    fn on_evict(&self, _state: &mut Self::RequestState, _key: CacheKey, _entry: CacheEntry) {}
}

type QuickCache =
    Cache<CacheKey, CacheEntry, BodyWeighter, DefaultHashBuilder, MemoryCacheLifecycle>;
type QuickCachePlaceholderGuard<'a> = PlaceholderGuard<
    'a,
    CacheKey,
    CacheEntry,
    BodyWeighter,
    DefaultHashBuilder,
    MemoryCacheLifecycle,
>;

/// The bytes that `resources` keep alive.
fn weigh(resources: &[CachedResource]) -> u64 {
    resources
        .iter()
        .map(|resource| {
            let body = match *resource.body.lock() {
                ResponseBody::Empty => 0,
                ResponseBody::Receiving(ref body) | ResponseBody::Done(ref body) => body.len(),
            };
            RESOURCE_OVERHEAD + body as u64
        })
        .sum()
}

/// A cache of HTTP responses, in memory and optionally on disk.
/// Elements of the memory tier will be evicted based on the cache heuristic.
/// We weight elements by the bytes of their bodies. We evict currently a whole url.
/// The cache makes extensive use of `Arc::unwrap_or_clone or` and `Arc::into_inner`
/// to modify the cached entries. This is ok because `CachedResource` are cheap to clone
pub struct HttpCache {
    /// cached responses.
    entries: QuickCache,
    /// The disk tier, if this cache has one.
    disk: Option<std::sync::Arc<DiskCache>>,
    /// The webviews whose requests do not use stored responses for now.
    bypassing_webviews: ParkingLotMutex<HashSet<WebViewId>>,
}

impl std::fmt::Debug for HttpCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.entries.iter().map(|(key, _entry)| key))
            .finish()
    }
}

impl MallocSizeOf for HttpCache {
    fn size_of(&self, ops: &mut MallocSizeOfOps) -> usize {
        self.entries
            .iter()
            .map(|(_key, entry)| entry.resources.blocking_read().size_of(ops))
            .sum()
    }
}

impl Default for HttpCache {
    fn default() -> Self {
        Self::new(None)
    }
}

/// Determine if a response is cacheable by default <https://tools.ietf.org/html/rfc7231#section-6.1>
fn is_cacheable_by_default(status_code: StatusCode) -> bool {
    matches!(
        status_code.as_u16(),
        200 | 203 | 204 | 206 | 300 | 301 | 404 | 405 | 410 | 414 | 501
    )
}

/// Determine if a given response is cacheable.
/// Based on <https://tools.ietf.org/html/rfc7234#section-3>
fn response_is_cacheable(metadata: &Metadata) -> bool {
    // TODO: if we determine that this cache should be considered shared:
    // 1. check for absence of private response directive <https://tools.ietf.org/html/rfc7234#section-5.2.2.6>
    // 2. check for absence of the Authorization header field.
    let mut is_cacheable = false;
    let headers = metadata.headers.as_ref().unwrap();
    if headers.contains_key(header::EXPIRES) ||
        headers.contains_key(header::LAST_MODIFIED) ||
        headers.contains_key(header::ETAG)
    {
        is_cacheable = true;
    }
    if let Some(ref directive) = headers.typed_get::<CacheControl>() {
        if directive.no_store() {
            return false;
        }
        if directive.public() ||
            directive.s_max_age().is_some() ||
            directive.max_age().is_some() ||
            directive.no_cache()
        {
            // If cache-control is understood, we can use it and ignore pragma.
            return true;
        }
    }
    if let Some(pragma) = headers.typed_get::<Pragma>() &&
        pragma.is_no_cache()
    {
        return false;
    }
    is_cacheable
}

/// Calculating Age
/// <https://tools.ietf.org/html/rfc7234#section-4.2.3>
fn calculate_response_age(response: &Response) -> Duration {
    // TODO: follow the spec more closely (Date headers, request/response lag, ...)
    response
        .headers
        .get(header::AGE)
        .and_then(|age_header| age_header.to_str().ok())
        .and_then(|age_string| age_string.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_default()
}

/// Determine the expiry date from relevant headers,
/// or uses a heuristic if none are present.
fn get_response_expiry(response: &Response) -> Duration {
    // Calculating Freshness Lifetime <https://tools.ietf.org/html/rfc7234#section-4.2.1>
    let age = calculate_response_age(response);
    let now = SystemTime::now();
    if let Some(directives) = response.headers.typed_get::<CacheControl>() {
        if directives.no_cache() {
            // Requires validation on first use.
            return Duration::ZERO;
        }
        if let Some(max_age) = directives.max_age().or(directives.s_max_age()) {
            return max_age.saturating_sub(age);
        }
    }
    match response.headers.typed_get::<Expires>() {
        Some(expiry) => {
            // `duration_since` fails if `now` is later than `expiry_time` in which case,
            // this whole thing return `Duration::ZERO`.
            let expiry_time: SystemTime = expiry.into();
            return expiry_time.duration_since(now).unwrap_or(Duration::ZERO);
        },
        // Malformed Expires header, shouldn't be used to construct a valid response.
        None if response.headers.contains_key(header::EXPIRES) => return Duration::ZERO,
        _ => {},
    }
    // Calculating Heuristic Freshness
    // <https://tools.ietf.org/html/rfc7234#section-4.2.2>
    if let Some(ref code) = response.status.try_code() {
        // <https://tools.ietf.org/html/rfc7234#section-5.5.4>
        // Since presently we do not generate a Warning header field with a 113 warn-code,
        // 24 hours minus response age is the max for heuristic calculation.
        let max_heuristic = Duration::from_secs(24 * 60 * 60).saturating_sub(age);
        let heuristic_freshness = if let Some(last_modified) =
            // If the response has a Last-Modified header field,
            // caches are encouraged to use a heuristic expiration value
            // that is no more than some fraction of the interval since that time.
            response.headers.typed_get::<LastModified>()
        {
            // `time_since_last_modified` will be `Duration::ZERO` if `last_modified` is
            // after `now`.
            let last_modified: SystemTime = last_modified.into();
            let time_since_last_modified = now.duration_since(last_modified).unwrap_or_default();

            // A typical setting of this fraction might be 10%.
            let raw_heuristic_calc = time_since_last_modified / 10;
            if raw_heuristic_calc < max_heuristic {
                raw_heuristic_calc
            } else {
                max_heuristic
            }
        } else {
            // Compatible with other browsers.
            Duration::ZERO
        };
        if is_cacheable_by_default(*code) {
            // Status codes that are cacheable by default can use heuristics to determine freshness.
            return heuristic_freshness;
        }
        // Other status codes can only use heuristic freshness if the public cache directive is present.
        if let Some(ref directives) = response.headers.typed_get::<CacheControl>() &&
            directives.public()
        {
            return heuristic_freshness;
        }
    }
    // Requires validation upon first use as default.
    Duration::ZERO
}

/// The `headers` crate's `CacheControl` does not understand `stale-while-revalidate` directive,
/// so we need to parse the raw `Cache-Control` header values.
/// <https://datatracker.ietf.org/doc/html/rfc5861#section-3>
fn get_stale_while_revalidate(headers: &HeaderMap) -> Duration {
    for value in headers.get_all(header::CACHE_CONTROL) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for directive in value.split(',') {
            let directive = directive.trim();
            let Some((name, argument)) = directive.split_once('=') else {
                continue;
            };
            if !name.trim().eq_ignore_ascii_case("stale-while-revalidate") {
                continue;
            }
            // The argument is a number of seconds, optionally quoted.
            let argument = argument.trim().trim_matches('"');
            if let Ok(seconds) = argument.parse::<u64>() {
                return Duration::from_secs(seconds);
            }
        }
    }
    Duration::ZERO
}

/// Determine whether the request itself demands revalidation.
/// <https://www.rfc-editor.org/rfc/rfc9111.html#section-5.2.1>
fn request_demands_revalidation(request: &Request) -> bool {
    if matches!(
        request.cache_mode,
        CacheMode::NoCache | CacheMode::Reload | CacheMode::NoStore
    ) {
        return true;
    }
    if let Some(directive) = request.headers.typed_get::<CacheControl>() {
        // The request's `no-store` directive is deliberately *not* treated as
        // demanding revalidation: https://www.rfc-editor.org/rfc/rfc9111.html#section-5.2.1.5
        // > "does not apply to the already stored response".
        if directive.no_cache() {
            return true;
        }

        if directive.max_age() == Some(Duration::ZERO) {
            return true;
        }
    }
    false
}

/// Request Cache-Control Directives
/// <https://tools.ietf.org/html/rfc7234#section-5.2.1>
fn get_expiry_adjustment_from_request_headers(request: &Request, expires: Duration) -> Duration {
    let Some(directive) = request.headers.typed_get::<CacheControl>() else {
        return expires;
    };

    if let Some(max_age) = directive.max_stale() {
        return expires + max_age;
    }

    match directive.max_age() {
        Some(max_age) if expires > max_age => return Duration::ZERO,
        Some(max_age) => return expires - max_age,
        None => {},
    };

    if let Some(min_fresh) = directive.min_fresh() {
        if expires < min_fresh {
            return Duration::ZERO;
        }
        return expires - min_fresh;
    }

    if directive.no_cache() || directive.no_store() {
        return Duration::ZERO;
    }

    expires
}

/// Create a CachedResponse from a request and a CachedResource.
fn create_cached_response(
    request: &Request,
    cached_resource: &CachedResource,
    cached_headers: &HeaderMap,
    done_chan: &mut DoneChannel,
) -> Option<CachedResponse> {
    debug!("creating a cached response for {:?}", request.url());
    if cached_resource.aborted.load(Ordering::Acquire) {
        return None;
    }
    let resource_timing = ResourceFetchTiming::new(request.timing_type());
    let mut response = Response::new(cached_resource.metadata.final_url.clone(), resource_timing);
    response.headers = cached_headers.clone();
    response.body = cached_resource.body.clone();
    if let ResponseBody::Receiving(_) = *cached_resource.body.lock() {
        debug!("existing body is in progress");
        let (done_sender, done_receiver) = unbounded();
        *done_chan = Some((done_sender.clone(), done_receiver));
        cached_resource.awaiting_body.lock().push(done_sender);
    }
    response
        .location_url
        .clone_from(&cached_resource.location_url);
    response.status.clone_from(&cached_resource.status);
    response.url_list.clone_from(&cached_resource.url_list);
    response.referrer = request.referrer.to_url().cloned();
    response.referrer_policy = request.referrer_policy;
    response.aborted = cached_resource.aborted.clone();

    let expires = cached_resource.expires;
    let adjusted_expires = get_expiry_adjustment_from_request_headers(request, expires);
    // A clock that went back says nothing about the age of the response,
    // which is then taken to need validation.
    let time_since_validated = SystemTime::now()
        .duration_since(cached_resource.last_validated)
        .unwrap_or(Duration::MAX);

    // TODO: take must-revalidate into account <https://tools.ietf.org/html/rfc7234#section-5.2.2.1>
    // TODO: if this cache is to be considered shared, take proxy-revalidate into account
    // <https://tools.ietf.org/html/rfc7234#section-5.2.2.7>
    let has_expired = adjusted_expires <= time_since_validated;

    // - fresh: return immediately, no validation.
    // - stale:
    //    - within the stale-while-revalidate window: return immediately + revalidate in the background
    //    - beyond the stale-while-revalidate window: synchronous validation is required.
    let stale_for = time_since_validated.saturating_sub(adjusted_expires);
    let within_stale_while_revalidate_window = stale_for <= cached_resource.stale_while_revalidate;
    let validation_status = if !has_expired {
        ValidationStatus::Valid
    } else {
        ValidationStatus::Stale {
            revalidate_in_background: within_stale_while_revalidate_window &&
                !cached_resource.stale_while_revalidate.is_zero() &&
                !request_demands_revalidation(request),
        }
    };

    let cached_response = CachedResponse {
        response,
        validation_status,
        revalidation_guard: cached_resource.revalidating.clone(),
    };
    Some(cached_response)
}

/// Create a new resource, based on the bytes requested, and an existing resource,
/// with a status-code of 206. `headers` are those of the existing resource; the
/// new one says in its `Content-Range` that `bytes` start at `start` of `total`.
fn create_resource_with_bytes_from_resource(
    bytes: &[u8],
    start: u64,
    total: u64,
    resource: &CachedResource,
    headers: &HeaderMap,
) -> Option<CachedResource> {
    let end = start.checked_add(bytes.len() as u64)?;
    let content_range = ContentRange::bytes(start..end, total).ok()?;
    let mut headers = headers.clone();
    headers.typed_insert(content_range);
    headers.typed_insert(ContentLength(bytes.len() as u64));
    Some(CachedResource {
        request_headers: resource.request_headers.clone(),
        body: Arc::new(ParkingLotMutex::new(ResponseBody::Done(bytes.to_owned()))),
        aborted: Arc::new(AtomicBool::new(false)),
        awaiting_body: Arc::new(ParkingLotMutex::new(vec![])),
        metadata: CachedMetadata {
            headers: Arc::new(ParkingLotMutex::new(headers)),
            ..resource.metadata.clone()
        },
        location_url: resource.location_url.clone(),
        status: StatusCode::PARTIAL_CONTENT.into(),
        url_list: resource.url_list.clone(),
        expires: resource.expires,
        stale_while_revalidate: resource.stale_while_revalidate,
        revalidating: resource.revalidating.clone(),
        last_validated: resource.last_validated,
        stored_body: None,
        stored: false,
    })
}

/// Support for range requests <https://tools.ietf.org/html/rfc7233>.
fn handle_range_request(
    request: &Request,
    candidates: &[&CachedResource],
    range_spec: &Range,
    done_chan: &mut DoneChannel,
) -> Option<CachedResponse> {
    let mut complete_cached_resources = candidates
        .iter()
        .filter(|resource| resource.status == StatusCode::OK);
    let partial_cached_resources = candidates
        .iter()
        .filter(|resource| resource.status == StatusCode::PARTIAL_CONTENT);
    if let Some(complete_resource) = complete_cached_resources.next() {
        // TODO: take the full range spec into account.
        // If we have a complete resource, take the request range from the body.
        // When there isn't a complete resource available, we loop over cached partials,
        // and see if any individual partial response can fulfill the current request for a bytes range.
        // TODO: combine partials that in combination could satisfy the requested range?
        // see <https://tools.ietf.org/html/rfc7233#section-4.3>.
        // TODO: add support for complete and partial resources,
        // whose body is in the ResponseBody::Receiving state.
        let body_len = match *complete_resource.body.lock() {
            ResponseBody::Done(ref body) => body.len(),
            _ => 0,
        };
        let bound = range_spec
            .satisfiable_ranges(body_len.try_into().unwrap())
            .next()
            .unwrap();
        match bound {
            (Bound::Included(beginning), Bound::Included(end)) => {
                if let ResponseBody::Done(ref body) = *complete_resource.body.lock() {
                    if end == u64::MAX {
                        // Prevent overflow on the addition below.
                        return None;
                    }
                    let b = beginning as usize;
                    let e = end as usize + 1;
                    let requested = body.get(b..e);
                    if let Some(bytes) = requested &&
                        let Some(new_resource) = create_resource_with_bytes_from_resource(
                            bytes,
                            beginning,
                            body_len as u64,
                            complete_resource,
                            &complete_resource.metadata.headers.lock(),
                        )
                    {
                        let cached_headers = new_resource.metadata.headers.lock();
                        let cached_response = create_cached_response(
                            request,
                            &new_resource,
                            &cached_headers,
                            done_chan,
                        );
                        if let Some(cached_response) = cached_response {
                            return Some(cached_response);
                        }
                    }
                }
            },
            (Bound::Included(beginning), Bound::Unbounded) => {
                if let ResponseBody::Done(ref body) = *complete_resource.body.lock() {
                    let b = beginning as usize;
                    let requested = body.get(b..);
                    if let Some(bytes) = requested &&
                        let Some(new_resource) = create_resource_with_bytes_from_resource(
                            bytes,
                            beginning,
                            body_len as u64,
                            complete_resource,
                            &complete_resource.metadata.headers.lock(),
                        )
                    {
                        let cached_headers = new_resource.metadata.headers.lock();
                        let cached_response = create_cached_response(
                            request,
                            &new_resource,
                            &cached_headers,
                            done_chan,
                        );
                        if let Some(cached_response) = cached_response {
                            return Some(cached_response);
                        }
                    }
                }
            },
            _ => return None,
        }
    } else {
        for partial_resource in partial_cached_resources {
            let headers = partial_resource.metadata.headers.lock();
            let content_range = headers.typed_get::<ContentRange>();

            let Some(body_len) = content_range.as_ref().and_then(|range| range.bytes_len()) else {
                continue;
            };
            match range_spec.satisfiable_ranges(body_len - 1).next().unwrap() {
                (Bound::Included(beginning), Bound::Included(end)) => {
                    let (res_beginning, res_end) = match content_range {
                        Some(range) => {
                            if let Some(bytes_range) = range.bytes_range() {
                                bytes_range
                            } else {
                                continue;
                            }
                        },
                        _ => continue,
                    };
                    if res_beginning <= beginning && res_end >= end {
                        let resource_body = &*partial_resource.body.lock();
                        let requested = match resource_body {
                            ResponseBody::Done(body) => {
                                let b = beginning as usize - res_beginning as usize;
                                let e = end as usize - res_beginning as usize + 1;
                                body.get(b..e)
                            },
                            _ => continue,
                        };
                        if let Some(bytes) = requested &&
                            let Some(new_resource) = create_resource_with_bytes_from_resource(
                                bytes,
                                beginning,
                                body_len,
                                partial_resource,
                                &headers,
                            )
                        {
                            let cached_headers = new_resource.metadata.headers.lock();
                            let cached_response = create_cached_response(
                                request,
                                &new_resource,
                                &cached_headers,
                                done_chan,
                            );
                            if let Some(cached_response) = cached_response {
                                return Some(cached_response);
                            }
                        }
                    }
                },

                (Bound::Included(beginning), Bound::Unbounded) => {
                    let (res_beginning, res_end, total) = if let Some(range) = content_range {
                        match (range.bytes_range(), range.bytes_len()) {
                            (Some(bytes_range), Some(total)) => {
                                (bytes_range.0, bytes_range.1, total)
                            },
                            _ => continue,
                        }
                    } else {
                        continue;
                    };
                    if total == 0 {
                        // Prevent overflow in the below operations from occuring.
                        continue;
                    };
                    if res_beginning <= beginning && res_end == total - 1 {
                        let resource_body = &*partial_resource.body.lock();
                        let requested = match resource_body {
                            ResponseBody::Done(body) => {
                                let from_byte = beginning as usize - res_beginning as usize;
                                body.get(from_byte..)
                            },
                            _ => continue,
                        };
                        if let Some(bytes) = requested {
                            if bytes.len() as u64 + beginning < total - 1 {
                                // Requested range goes beyond the available range.
                                continue;
                            }
                            let Some(new_resource) = create_resource_with_bytes_from_resource(
                                bytes,
                                beginning,
                                total,
                                partial_resource,
                                &headers,
                            ) else {
                                continue;
                            };
                            let cached_headers = new_resource.metadata.headers.lock();
                            let cached_response = create_cached_response(
                                request,
                                &new_resource,
                                &cached_headers,
                                done_chan,
                            );
                            if let Some(cached_response) = cached_response {
                                return Some(cached_response);
                            }
                        }
                    }
                },

                _ => continue,
            }
        }
    }

    None
}

/// Whether a request with `request_headers` selects `cached_resource`.
/// Calculating Secondary Keys with Vary <https://tools.ietf.org/html/rfc7234#section-4.1>
fn is_selected_by(request_headers: &HeaderMap, cached_resource: &CachedResource) -> bool {
    let cached_headers = cached_resource.metadata.headers.lock();
    let original_request_headers = cached_resource.request_headers.lock();
    let Some(vary_value) = cached_headers.typed_get::<Vary>() else {
        return true;
    };
    if vary_value.is_any() {
        debug!("vary value is any, not caching");
        return false;
    }
    // For every header name found in the Vary header of the stored response.
    for vary_val in vary_value.iter_strs() {
        match request_headers.get(vary_val) {
            Some(header_data) => {
                // If the header is present in the request.
                if let Some(original_header_data) = original_request_headers.get(vary_val) {
                    // Check that the value of the nominated header field,
                    // in the original request, matches the value in the current request.
                    if original_header_data != header_data {
                        debug!("headers don't match, not caching");
                        return false;
                    }
                }
            },
            None => {
                // If a header field is absent from a request,
                // it can only match a stored response if those headers,
                // were also absent in the original request.
                if original_request_headers.get(vary_val).is_some() {
                    debug!("vary header present, not caching");
                    return false;
                }
            },
        }
    }
    true
}

/// Whether `cached_resource` is a response to the whole url that nothing is
/// still receiving or waiting for.
fn is_settled(cached_resource: &CachedResource) -> bool {
    cached_resource.status != StatusCode::PARTIAL_CONTENT &&
        cached_resource.awaiting_body.lock().is_empty() &&
        !matches!(*cached_resource.body.lock(), ResponseBody::Receiving(_))
}

/// Remove the responses that a later one replaces: a settled response goes
/// when a later settled one, not aborted, answers the same requests, and the
/// oldest settled ones go beyond [`MAX_VARIANTS`]. Without this an entry grows
/// by one response each time its url is fetched again, and the disk tier keeps
/// all of them. Returns whether it removed any.
fn prune(resources: &mut Vec<CachedResource>) -> bool {
    let mut kept_variants = 0;
    let mut later_request_headers: Vec<HeaderMap> = Vec::new();
    let mut keep = vec![true; resources.len()];
    // From the newest, which is the one a request is answered with.
    for (index, resource) in resources.iter().enumerate().rev() {
        if !is_settled(resource) {
            continue;
        }
        let superseded = later_request_headers
            .iter()
            .any(|request_headers| is_selected_by(request_headers, resource));
        if superseded || kept_variants == MAX_VARIANTS {
            keep[index] = false;
            continue;
        }
        if !resource.aborted.load(Ordering::Acquire) {
            kept_variants += 1;
            later_request_headers.push(resource.request_headers.lock().clone());
        }
    }
    let removed = keep.contains(&false);
    let mut keep = keep.into_iter();
    resources.retain(|_resource| keep.next().unwrap_or(true));
    removed
}

/// The headers among `request_headers` that the `Vary` header of a response
/// with `response_headers` names.
fn varying_request_headers(request_headers: &HeaderMap, response_headers: &HeaderMap) -> HeaderMap {
    let mut varying = HeaderMap::new();
    let Some(vary_value) = response_headers.typed_get::<Vary>() else {
        return varying;
    };
    for name in vary_value.iter_strs() {
        let Ok(name) = header::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        for value in request_headers.get_all(&name) {
            varying.append(name.clone(), value.clone());
        }
    }
    varying
}

/// Constructing Responses from Caches.
/// <https://tools.ietf.org/html/rfc7234#section-4>
pub(crate) fn construct_response(
    request: &Request,
    done_chan: &mut DoneChannel,
    cache_result: &[CachedResource],
) -> Option<CachedResponse> {
    if pref!(network_http_cache_disabled) {
        return None;
    }

    // TODO: generate warning headers as appropriate <https://tools.ietf.org/html/rfc7234#section-5.5>
    debug!("trying to construct cache response for {:?}", request.url());
    if request.method != Method::GET {
        // Only Get requests are cached, avoid a url based match for others.
        debug!("non-GET method, not caching");
        return None;
    }

    let resources = cache_result
        .iter()
        .filter(|r| !r.aborted.load(Ordering::Relaxed));
    let mut candidates = vec![];
    for cached_resource in resources {
        if is_selected_by(&request.headers, cached_resource) {
            candidates.push(cached_resource);
        }
    }
    // Support for range requests
    if let Some(range_spec) = request.headers.typed_get::<Range>() {
        return handle_range_request(request, candidates.as_slice(), &range_spec, done_chan);
    }
    while let Some(cached_resource) = candidates.pop() {
        // Not a Range request.
        // Do not allow 206 responses to be constructed.
        //
        // See https://tools.ietf.org/html/rfc7234#section-3.1
        //
        // A cache MUST NOT use an incomplete response to answer requests unless the
        // response has been made complete or the request is partial and
        // specifies a range that is wholly within the incomplete response.
        //
        // TODO: Combining partial content to fulfill a non-Range request
        // see https://tools.ietf.org/html/rfc7234#section-3.3
        match cached_resource.status.try_code() {
            Some(ref code) => {
                if *code == StatusCode::PARTIAL_CONTENT {
                    continue;
                }
            },
            None => continue,
        }
        // Returning a response that can be constructed
        // TODO: select the most appropriate one, using a known mechanism from a selecting header field,
        // or using the Date header to return the most recent one.
        let cached_headers = cached_resource.metadata.headers.lock();
        let cached_response =
            create_cached_response(request, cached_resource, &cached_headers, done_chan);
        if let Some(cached_response) = cached_response {
            return Some(cached_response);
        }
    }
    debug!("couldn't find an appropriate response, not caching");
    // The cache wasn't able to construct anything.
    None
}

/// Freshening Stored Responses upon Validation.
/// <https://tools.ietf.org/html/rfc7234#section-4.3.4>
pub fn refresh(
    request: &Request,
    response: Response,
    done_chan: &mut DoneChannel,
    cached_resources: &mut [CachedResource],
) -> Option<Response> {
    assert_eq!(response.status, StatusCode::NOT_MODIFIED);

    let cached_resource = cached_resources.iter_mut().next()?;

    let mut constructed_response = if let Some(range_spec) = request.headers.typed_get::<Range>() {
        handle_range_request(request, &[cached_resource], &range_spec, done_chan)
            .map(|cached_response| cached_response.response)
    } else {
        // done_chan will have been set to Some(..) by http_network_fetch.
        // If the body is not receiving data, set the done_chan back to None.
        // Otherwise, create a new dedicated channel to update the consumer.
        // The response constructed here will replace the 304 one from the network.
        let in_progress_channel = match &*cached_resource.body.lock() {
            ResponseBody::Receiving(..) => Some(unbounded()),
            ResponseBody::Empty | ResponseBody::Done(..) => None,
        };
        match in_progress_channel {
            Some((done_sender, done_receiver)) => {
                *done_chan = Some((done_sender.clone(), done_receiver));
                cached_resource.awaiting_body.lock().push(done_sender);
            },
            None => *done_chan = None,
        }
        // Received a response with 304 status code, in response to a request that matches a cached resource.
        // 1. update the headers of the cached resource.
        // 2. return a response, constructed from the cached resource.
        let resource_timing = ResourceFetchTiming::new(request.timing_type());
        let mut constructed_response =
            Response::new(cached_resource.metadata.final_url.clone(), resource_timing);

        constructed_response.body = cached_resource.body.clone();

        constructed_response
            .status
            .clone_from(&cached_resource.status);
        constructed_response.referrer = request.referrer.to_url().cloned();
        constructed_response.referrer_policy = request.referrer_policy;
        constructed_response
            .status
            .clone_from(&cached_resource.status);
        constructed_response
            .url_list
            .clone_from(&cached_resource.url_list);
        Some(constructed_response)
    };

    // Update cached Resource with response and constructed response.
    if let Some(constructed_response) = constructed_response.as_mut() {
        let mut stored_headers = cached_resource.metadata.headers.lock();
        stored_headers.extend(response.headers);
        // A range answered from the stored resource keeps saying which of its
        // bytes it carries.
        let mut headers = stored_headers.clone();
        for name in [header::CONTENT_RANGE, header::CONTENT_LENGTH] {
            if let Some(value) = constructed_response.headers.get(&name) {
                headers.insert(name, value.clone());
            }
        }
        constructed_response.headers = headers;
        // The freshness lifetime comes from the updated headers and counts
        // from this validation.
        cached_resource.expires = get_response_expiry(constructed_response);
        cached_resource.stale_while_revalidate =
            get_stale_while_revalidate(&constructed_response.headers);
        cached_resource.last_validated = SystemTime::now();
        cached_resource.stored = false;
    }

    constructed_response
}

pub(crate) fn invalidate_cached_resources(cached_resources: &mut [CachedResource]) {
    for cached_resource in cached_resources.iter_mut() {
        cached_resource.expires = Duration::ZERO;
        cached_resource.stored = false;
    }
}

fn resolve_location_url(
    request: &Request,
    response: &Response,
    header_name: header::HeaderName,
) -> Option<ServoUrl> {
    response
        .headers
        .get(header_name)
        .and_then(|value| value.to_str().ok())
        .and_then(|location| request.current_url().join(location).ok())
}

/// Run `task`, which blocks on the file system, off the threads that drive
/// fetches. Returns `None` if the task did not finish.
async fn run_blocking<T, F>(task: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        // Not on a runtime, so there is no fetch that this thread would hold up.
        return Some(task());
    };
    match runtime.spawn_blocking(task).await {
        Ok(result) => Some(result),
        Err(error) => {
            warn!("A task of the disk cache did not finish: {error}");
            None
        },
    }
}

/// Whether the disk tier keeps `resource`. It keeps complete responses only:
/// a partial one is of use to nothing but the media element that fetched it,
/// which fetches with ranges that rarely repeat.
fn is_storable(resource: &CachedResource) -> bool {
    if resource.status == StatusCode::PARTIAL_CONTENT || resource.aborted.load(Ordering::Acquire) {
        return false;
    }
    match *resource.body.lock() {
        ResponseBody::Empty => true,
        ResponseBody::Receiving(_) => false,
        ResponseBody::Done(ref body) => body.len() as u64 <= MAX_CACHED_BODY_SIZE,
    }
}

/// Write the storable resources among `resources`, the entry of `key`, to the
/// disk tier, unless it holds all of them already and `removed` says that
/// none it may hold left the entry.
async fn store_on_disk(
    disk: &std::sync::Arc<DiskCache>,
    key: &CacheKey,
    resources: &mut [CachedResource],
    removed: bool,
) {
    let storable: Vec<usize> = resources
        .iter()
        .enumerate()
        .filter(|(_index, resource)| is_storable(resource))
        .map(|(index, _resource)| index)
        .collect();
    if !removed && storable.iter().all(|&index| resources[index].stored) {
        return;
    }
    if storable.is_empty() {
        // Nothing of the entry is left to keep.
        let name = DiskCache::name_for(key.url.as_str());
        if disk.contains(&name) {
            let task_disk = disk.clone();
            run_blocking(move || task_disk.remove(&name)).await;
        }
        return;
    }

    let mut stored_resources = Vec::with_capacity(storable.len());
    let mut new_bodies = Vec::new();
    let mut kept_bodies = Vec::new();
    for &index in &storable {
        let resource = &resources[index];
        let stored_body = match (&*resource.body.lock(), &resource.stored_body) {
            (ResponseBody::Done(_), Some(stored_body)) => {
                kept_bodies.push(stored_body.id);
                Some(stored_body.clone())
            },
            (ResponseBody::Done(body), None) => {
                let id = disk.new_body_id();
                new_bodies.push((id, body.clone()));
                Some(StoredBody {
                    id,
                    length: body.len() as u64,
                    // Worked out with the write, off this thread.
                    digest: String::new(),
                })
            },
            (ResponseBody::Empty | ResponseBody::Receiving(_), _) => None,
        };
        let response_headers = resource.metadata.headers.lock().clone();
        stored_resources.push(StoredResource {
            request_headers: varying_request_headers(
                &resource.request_headers.lock(),
                &response_headers,
            ),
            response_headers,
            final_url: resource.metadata.final_url.clone(),
            content_type: resource.metadata.content_type.clone(),
            charset: resource.metadata.charset.clone(),
            metadata_status: resource.metadata.status.clone(),
            location_url: resource.location_url.clone(),
            status: resource.status.clone(),
            url_list: resource.url_list.clone(),
            expires: resource.expires,
            stale_while_revalidate: resource.stale_while_revalidate,
            last_validated: resource.last_validated,
            body: stored_body,
        });
    }

    let mut entry = StoredEntry {
        version: STORED_ENTRY_VERSION,
        url: key.url.clone(),
        resources: stored_resources,
    };
    let name = DiskCache::name_for(key.url.as_str());
    let task_disk = disk.clone();
    let written = run_blocking(move || -> io::Result<Vec<Option<StoredBody>>> {
        for (id, body) in &new_bodies {
            let digest = DiskCache::digest(body);
            for stored_body in entry.resources.iter_mut().filter_map(|r| r.body.as_mut()) {
                if stored_body.id == *id {
                    stored_body.digest.clone_from(&digest);
                }
            }
        }
        let metadata = serde_json::to_vec(&entry)?;
        task_disk.store(&name, &metadata, &new_bodies, &kept_bodies)?;
        Ok(entry.resources.into_iter().map(|r| r.body).collect())
    })
    .await;
    match written {
        Some(Ok(stored_bodies)) => {
            for (&index, stored_body) in storable.iter().zip(stored_bodies) {
                resources[index].stored_body = stored_body;
                resources[index].stored = true;
            }
        },
        Some(Err(error)) => warn!("Could not write the cache entry of {}: {error}", key.url),
        None => {},
    }
}

/// Read the entry of `key`, filed as `name`, from the disk tier. An entry that
/// cannot be used whole is removed.
fn read_from_disk(disk: &DiskCache, name: &str, key: &CacheKey) -> Option<Vec<CachedResource>> {
    let metadata = disk.read_metadata(name)?;
    let resources = resources_from_stored(disk, name, key, &metadata);
    match resources {
        Some(_) => disk.mark_used(name),
        None => disk.remove(name),
    }
    resources
}

fn resources_from_stored(
    disk: &DiskCache,
    name: &str,
    key: &CacheKey,
    metadata: &[u8],
) -> Option<Vec<CachedResource>> {
    let entry: StoredEntry = match serde_json::from_slice(metadata) {
        Ok(entry) => entry,
        Err(error) => {
            warn!("Could not parse the cache entry {name}: {error}");
            return None;
        },
    };
    if entry.version != STORED_ENTRY_VERSION || entry.url != key.url || entry.resources.is_empty()
    {
        return None;
    }
    entry
        .resources
        .into_iter()
        .map(|stored| {
            let body = match &stored.body {
                Some(stored_body) => {
                    let body = disk.read_body(name, stored_body.id, stored_body.length)?;
                    if DiskCache::digest(&body) != stored_body.digest {
                        warn!("The cached body {name}.{} is corrupt", stored_body.id);
                        return None;
                    }
                    ResponseBody::Done(body)
                },
                None => ResponseBody::Empty,
            };
            Some(CachedResource {
                request_headers: Arc::new(ParkingLotMutex::new(stored.request_headers)),
                body: Arc::new(ParkingLotMutex::new(body)),
                aborted: Arc::new(AtomicBool::new(false)),
                awaiting_body: Arc::new(ParkingLotMutex::new(vec![])),
                metadata: CachedMetadata {
                    headers: Arc::new(ParkingLotMutex::new(stored.response_headers)),
                    final_url: stored.final_url,
                    content_type: stored.content_type,
                    charset: stored.charset,
                    status: stored.metadata_status,
                },
                location_url: stored.location_url,
                status: stored.status,
                url_list: stored.url_list,
                expires: stored.expires,
                stale_while_revalidate: stored.stale_while_revalidate,
                revalidating: StdArc::new(AtomicBool::new(false)),
                last_validated: stored.last_validated,
                stored_body: stored.body,
                stored: true,
            })
        })
        .collect()
}

impl HttpCache {
    /// Create a cache. `disk` is the directory of its disk tier and the bytes
    /// that tier may take. Without it, or if the directory cannot be used, the
    /// cache keeps responses in memory only.
    pub fn new(disk: Option<(PathBuf, u64)>) -> Self {
        let disk = disk
            .filter(|(_directory, capacity)| *capacity > 0)
            .and_then(|(directory, capacity)| {
                match DiskCache::open(directory.clone(), capacity) {
                    Ok(disk) => Some(std::sync::Arc::new(disk)),
                    Err(error) => {
                        warn!(
                            "Could not open the HTTP cache in {}: {error}",
                            directory.display()
                        );
                        None
                    },
                }
            });

        let items = usize::try_from(pref!(network_http_cache_size)).unwrap_or(usize::MAX);
        let weight = pref!(network_http_cache_memory_size);
        // The capacity is split evenly between shards and an entry heavier
        // than its shard is not kept. One shard lets an entry be as heavy as
        // the whole capacity.
        let options = OptionsBuilder::new()
            .shards(1)
            .estimated_items_capacity(items)
            .weight_capacity(weight)
            .build();
        let entries = match options {
            Ok(options) => Cache::with_options(
                options,
                BodyWeighter,
                DefaultHashBuilder::default(),
                MemoryCacheLifecycle,
            ),
            Err(error) => {
                error!("Invalid options for the HTTP cache: {error:?}");
                Cache::with(
                    items,
                    weight,
                    BodyWeighter,
                    DefaultHashBuilder::default(),
                    MemoryCacheLifecycle,
                )
            },
        };

        Self {
            entries,
            disk,
            bypassing_webviews: Default::default(),
        }
    }

    #[expect(clippy::len_without_is_empty)]
    /// The number of entries in the memory tier. This does not say anything about the disk tier.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Make the requests of `webview_id` stop, or start again, using stored
    /// responses. Their responses are stored either way, so that a reload
    /// which bypasses the cache replaces what the cache holds for the page.
    pub(crate) fn set_bypassing(&self, webview_id: WebViewId, bypassing: bool) {
        let mut bypassing_webviews = self.bypassing_webviews.lock();
        if bypassing {
            bypassing_webviews.insert(webview_id);
        } else {
            bypassing_webviews.remove(&webview_id);
        }
    }

    /// Whether `request` must not be answered with a stored response.
    pub(crate) fn is_bypassing(&self, request: &Request) -> bool {
        request
            .target_webview_id
            .is_some_and(|webview_id| self.bypassing_webviews.lock().contains(&webview_id))
    }

    /// Wake-up consumers of cached resources
    /// whose response body was still receiving data when the resource was constructed,
    /// and whose response has now either been completed or cancelled.
    pub async fn update_awaiting_consumers(&self, request: &Request, response: &Response) {
        let entry_key = CacheKey::new(request);

        let entry = match self.entries.get(&entry_key) {
            None => return,
            Some(entry) => entry,
        };

        let actual_response = response.actual_response();

        {
            // Ensure we only wake-up consumers of relevant resources,
            // ie we don't want to wake-up 200 awaiting consumers with a 206.
            let lock = entry.resources.read().await;
            let relevant_cached_resources = lock.iter().filter(|resource| {
                if actual_response.is_network_error() {
                    return *resource.body.lock() == ResponseBody::Empty;
                }
                resource.status == actual_response.status
            });

            for cached_resource in relevant_cached_resources {
                let mut awaiting_consumers = cached_resource.awaiting_body.lock();
                if awaiting_consumers.is_empty() {
                    continue;
                }
                let to_send = if cached_resource.aborted.load(Ordering::Acquire) {
                    // In the case of an aborted fetch,
                    // wake-up all awaiting consumers.
                    // Each will then start a new network request.
                    // TODO: Wake-up only one consumer, and make it the producer on which others wait.
                    Data::Cancelled
                } else {
                    match *cached_resource.body.lock() {
                        ResponseBody::Done(_) | ResponseBody::Empty => Data::Done,
                        ResponseBody::Receiving(_) => {
                            continue;
                        },
                    }
                };
                for done_sender in awaiting_consumers.drain(..) {
                    let _ = done_sender.send(to_send.clone());
                }
            }
        }

        self.settle(&entry_key, entry).await;
    }

    /// Bring both tiers up to date with `entry`, the entry of `key`, now that
    /// a fetch of it ended and its bodies may be complete: write it to the
    /// disk tier and weigh it again for the memory tier.
    async fn settle(&self, key: &CacheKey, entry: CacheEntry) {
        let mut resources = entry.resources.write().await;
        let removed = prune(&mut resources);
        if let Some(disk) = &self.disk {
            store_on_disk(disk, key, &mut resources, removed).await;
        }
        let weight = weigh(&resources);
        drop(resources);
        if weight == entry.weight {
            return;
        }
        let reweighed = CacheEntry {
            resources: entry.resources,
            weight,
        };
        if self.entries.replace(key.clone(), reweighed, true).is_err() {
            debug!("the cache entry left the memory cache before it was weighed");
        }
    }

    /// Returns descriptors for cache entries currently stored in this cache.
    pub(crate) fn cache_entry_descriptors(&self) -> Vec<CacheEntryDescriptor> {
        let mut urls: HashSet<String> = self
            .entries
            .iter()
            .map(|(key, _)| key.url.to_string())
            .collect();
        if let Some(disk) = &self.disk {
            for name in disk.names() {
                let entry = disk
                    .read_metadata(&name)
                    .and_then(|metadata| serde_json::from_slice::<StoredEntry>(&metadata).ok());
                if let Some(entry) = entry {
                    urls.insert(entry.url.to_string());
                }
            }
        }
        urls.into_iter().map(CacheEntryDescriptor::new).collect()
    }

    /// Clear the contents of this cache.
    pub(crate) fn clear(&self) {
        self.entries.clear();
        if let Some(disk) = &self.disk {
            disk.clear();
        }
    }

    /// Insert a response for `request` into the cache (used by tests that need direct access).
    pub async fn store(&self, request: &Request, response: &Response) {
        let entry_key = CacheKey::new(request);
        let guard = self.get_or_guard(entry_key.clone()).await;
        guard.insert(request, response);
        // What the end of a fetch does for the entry.
        if let Some(entry) = self.entries.get(&entry_key) {
            self.settle(&entry_key, entry).await;
        }
    }

    /// Try to construct a cached response for `request`.
    pub async fn construct_response(
        &self,
        request: &Request,
        done_chan: &mut DoneChannel,
    ) -> Option<Response> {
        let CachedResourcesOrGuard::Value(cached_resources) =
            self.get_or_guard(CacheKey::new(request)).await
        else {
            return None;
        };
        construct_response(request, done_chan, cached_resources.as_slice())
            .map(|cached| cached.response)
    }

    /// Like [`construct_response`](Self::construct_response), but additionally
    /// reports the [`ValidationStatus`] of the constructed response.
    #[cfg(feature = "test-util")]
    pub async fn construct_response_freshness(
        &self,
        request: &Request,
        done_chan: &mut DoneChannel,
    ) -> Option<ValidationStatus> {
        let entry = self.entries.get(&CacheKey::new(request))?;
        let cached_resources = entry.resources.read().await;
        construct_response(request, done_chan, cached_resources.as_slice())
            .map(|cached| cached.validation_status)
    }

    /// Invalidate cache entries referenced by Location/Content-Location headers.
    pub(crate) async fn invalidate_related_urls(
        &self,
        request: &Request,
        response: &Response,
        skip_key: &CacheKey,
    ) {
        for header_name in &[header::LOCATION, header::CONTENT_LOCATION] {
            if let Some(location_url) = resolve_location_url(request, response, header_name.clone())
            {
                let location_key = CacheKey::from_url(location_url);
                if &location_key != skip_key {
                    self.invalidate_entry(&location_key).await;
                }
            }
        }
    }

    async fn invalidate_entry(&self, key: &CacheKey) {
        let CachedResourcesOrGuard::Value(mut guarded_resources) =
            self.get_or_guard(key.clone()).await
        else {
            return;
        };
        invalidate_cached_resources(guarded_resources.as_mut_slice());
        // No fetch of this entry is ending, so nothing else would write it.
        if let Some(disk) = &self.disk {
            store_on_disk(disk, key, &mut guarded_resources, false).await;
        }
    }

    /// If the value exist in the cache, return it. If the value does not exist, return a guard you can use to insert values in the cache.
    /// If the guard is alive, all other accesses to this function will block.
    #[servo_tracing::instrument(skip(self))]
    pub async fn get_or_guard(&self, entry_key: CacheKey) -> CachedResourcesOrGuard<'_> {
        let guard = match self.entries.get_value_or_guard_async(&entry_key).await {
            Ok(entry) => return CachedResourcesOrGuard::Value(entry.resources.write_owned().await),
            Err(guard) => guard,
        };
        // Not in memory. While the guard is alive nothing else reads the entry from disk.
        let Some(resources) = self.load_from_disk(&entry_key).await else {
            return CachedResourcesOrGuard::Guard(guard);
        };
        let entry = CacheEntry::new(resources);
        // The lock is new, so this does not wait.
        let resources = entry.resources.clone().write_owned().await;
        if guard.insert(entry).is_err() {
            error!("Could not insert into cache");
        }
        CachedResourcesOrGuard::Value(resources)
    }

    /// Read the entry of `key` from the disk tier, if it is there.
    async fn load_from_disk(&self, key: &CacheKey) -> Option<Vec<CachedResource>> {
        if pref!(network_http_cache_disabled) {
            return None;
        }
        let disk = self.disk.clone()?;
        let name = DiskCache::name_for(key.url.as_str());
        if !disk.contains(&name) {
            return None;
        }
        let key = key.clone();
        run_blocking(move || read_from_disk(&disk, &name, &key))
            .await
            .flatten()
    }
}

/// Returns an writeable entry into the cache or a guard for insertint an entry
/// The guard will block other queries to the cache entry in both cases.
pub enum CachedResourcesOrGuard<'a> {
    /// The value of the resource in the cache.
    Value(OwnedRwLockWriteGuard<Vec<CachedResource>>),
    /// A guard that blocks requests to the cache entry this guard is for.
    Guard(QuickCachePlaceholderGuard<'a>),
}

impl<'a> CachedResourcesOrGuard<'a> {
    /// Insert into the cache according to http spec
    pub fn insert(self, request: &Request, response: &Response) {
        if pref!(network_http_cache_disabled) {
            return;
        }

        if request.method != Method::GET {
            // Only Get requests are cached.
            return;
        }
        if request.headers.contains_key(header::AUTHORIZATION) {
            // https://tools.ietf.org/html/rfc7234#section-3.1
            // A shared cache MUST NOT use a cached response
            // to a request with an Authorization header field
            //
            // TODO: unless a cache directive that allows such
            // responses to be stored is present in the response.
            return;
        };
        let Ok(metadata): Result<Metadata, _> = response.metadata().map(Into::into) else {
            return;
        };
        if !response_is_cacheable(&metadata) {
            return;
        }
        // The body is still arriving at this point, so the declared length is
        // all there is to go by. A response without one is cached whatever
        // its size turns out to be.
        if response
            .headers
            .typed_get::<ContentLength>()
            .is_some_and(|length| length.0 > MAX_CACHED_BODY_SIZE)
        {
            return;
        }
        let expiry = get_response_expiry(response);
        let stale_while_revalidate = get_stale_while_revalidate(&response.headers);
        let cacheable_metadata = CachedMetadata {
            headers: Arc::new(ParkingLotMutex::new(response.headers.clone())),
            final_url: metadata.final_url,
            content_type: metadata.content_type.map(|v| v.0.to_string()),
            charset: metadata.charset,
            status: metadata.status,
        };
        let entry_resource = CachedResource {
            request_headers: Arc::new(ParkingLotMutex::new(request.headers.clone())),
            body: response.body.clone(),
            aborted: response.aborted.clone(),
            awaiting_body: Arc::new(ParkingLotMutex::new(vec![])),
            metadata: cacheable_metadata,
            location_url: response.location_url.clone(),
            status: response.status.clone(),
            url_list: response.url_list.clone(),
            expires: expiry,
            stale_while_revalidate,
            revalidating: StdArc::new(AtomicBool::new(false)),
            last_validated: SystemTime::now(),
            stored_body: None,
            stored: false,
        };

        match self {
            CachedResourcesOrGuard::Value(mut owned_rw_lock_write_guard) => {
                owned_rw_lock_write_guard.push(entry_resource);
            },
            CachedResourcesOrGuard::Guard(placeholder_guard) => {
                if placeholder_guard
                    .insert(CacheEntry::new(vec![entry_resource]))
                    .is_err()
                {
                    error!("Could not insert into cache");
                }
            },
        }
    }

    /// If the guard is a value, return it as a mut reference. If the guard is a guard, return None
    pub fn try_as_mut(&mut self) -> Option<&mut Vec<CachedResource>> {
        match self {
            CachedResourcesOrGuard::Value(owned_rw_lock_write_guard) => {
                Some(owned_rw_lock_write_guard.as_mut())
            },
            CachedResourcesOrGuard::Guard(_) => None,
        }
    }
}
