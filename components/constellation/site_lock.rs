/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Site isolation in front of the resource and storage threads.
//!
//! A content process runs the pages of one site. Without anything in between it would hold
//! the very same senders as this process to the network and storage threads, and those
//! believe whatever URL or origin a message names: a compromised process could read the
//! cookies, the storage or the credentialed responses of every other site. Each content
//! process gets instead senders of its own, whose messages pass through here first, and
//! only those about its own site go on.

use std::fmt;
use std::sync::Arc;

use log::{debug, warn};
use net_traits::fetch::headers::{is_forbidden_method, is_forbidden_request_header};
use net_traits::filemanager_thread::FileManagerThreadMsg;
use net_traits::pub_domains::reg_suffix;
use net_traits::request::{Origin, RequestBuilder, RequestMode};
use net_traits::{CookieSource, CoreResourceMsg, ResourceThreads};
use parking_lot::Mutex;
use rustc_hash::FxHashSet;
use servo_base::generic_channel::{self, GenericSender};
use servo_base::id::{HistoryStateId, PipelineNamespaceId, PipelineNamespaceRequest};
use servo_url::{Host, ImmutableOrigin, ServoUrl};
use storage_traits::StorageThreads;
use storage_traits::cache_storage::CacheStorageThreadMessage;
use storage_traits::client_storage::{
    ClientStorageThreadHandle, ClientStorageThreadMessage, StorageProxyMap,
};
use storage_traits::indexeddb::{IndexedDBThreadMsg, SyncOperation};
use storage_traits::webstorage_thread::WebStorageThreadMsg;

/// The site a content process is locked to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SiteLock {
    /// The registered domain (eTLD+1) of the pages, or their address when they have no
    /// domain name.
    Site(Host),
    /// Pages with an opaque origin only, such as `data:` documents. They belong to no site,
    /// so nothing kept for a site is theirs.
    Opaque,
    /// A frame sandboxed into an opaque origin, whose document comes from the given site.
    /// Like [`SiteLock::Opaque`] it acts for no site; it only loads the documents of one.
    Sandboxed(Host),
}

impl fmt::Display for SiteLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SiteLock::Site(site) => write!(f, "{site}"),
            SiteLock::Opaque => f.write_str("opaque origins"),
            SiteLock::Sandboxed(site) => write!(f, "sandboxed documents of {site}"),
        }
    }
}

impl SiteLock {
    /// The lock of an event loop made for pages of `registered_domain_name`, which are
    /// `sandboxed` into an opaque origin or not.
    pub(crate) fn new(registered_domain_name: Option<Host>, sandboxed: bool) -> Self {
        match registered_domain_name {
            None => SiteLock::Opaque,
            Some(site) if sandboxed => SiteLock::Sandboxed(site),
            Some(site) => SiteLock::Site(site),
        }
    }

    /// The lock of a process that runs script of `origin` only, such as a service worker.
    pub(crate) fn of_origin(origin: &ImmutableOrigin) -> Self {
        match origin {
            ImmutableOrigin::Tuple(_, Host::Domain(domain), _) => {
                SiteLock::Site(Host::Domain(reg_suffix(domain).to_owned()))
            },
            ImmutableOrigin::Tuple(_, address, _) => SiteLock::Site(address.clone()),
            ImmutableOrigin::Opaque(_) => SiteLock::Opaque,
        }
    }

    /// Whether a page of this site may act as `origin`. Opaque origins are each their own,
    /// and nothing is stored under one that another page could reach, so any process may
    /// use them.
    pub(crate) fn allows_origin(&self, origin: &ImmutableOrigin) -> bool {
        match origin {
            ImmutableOrigin::Opaque(_) => true,
            ImmutableOrigin::Tuple(..) => SiteLock::of_origin(origin) == *self,
        }
    }

    /// Whether a document at `url` may run here: a page of this site, or for a sandboxed
    /// frame, a page of the site it was sandboxed from.
    fn allows_document(&self, url: &ServoUrl) -> bool {
        match self {
            SiteLock::Sandboxed(site) => {
                SiteLock::of_origin(&url.origin()) == SiteLock::Site(site.clone())
            },
            SiteLock::Site(_) | SiteLock::Opaque => self.allows_url(url),
        }
    }

    pub(crate) fn allows_url(&self, url: &ServoUrl) -> bool {
        self.allows_origin(&url.origin())
    }

    /// A fetch is allowed when it is made on behalf of this site. A navigation is the one
    /// kind made on behalf of someone else, whose document then lands here: there it is the
    /// document that has to be of this site. The URLs it was redirected through before do
    /// not matter, as a redirect to another site moves the navigation to another process.
    fn allows_fetch(&self, request: &RequestBuilder) -> bool {
        if request.mode == RequestMode::Navigate {
            let current_url = request.url_list.last().unwrap_or(&request.url);
            return self.allows_document(current_url);
        }
        // The referrer is not checked: a module script or a stylesheet of another site is the
        // referrer of what it imports, so any URL the process loaded can be one. What a fetch
        // may do is decided by the origin it is made for, as in Chromium.
        match &request.origin {
            Origin::Origin(origin) => self.allows_origin(origin),
            Origin::Client => request
                .client
                .as_ref()
                .is_some_and(|client| match &client.origin {
                    Origin::Origin(origin) => self.allows_origin(origin),
                    Origin::Client => false,
                }),
        }
    }

    fn allows_core_resource_message(&self, message: &CoreResourceMsg) -> bool {
        match message {
            CoreResourceMsg::Fetch(request, _) | CoreResourceMsg::FetchRedirect(request, ..)
                if asks_for_what_only_the_network_sets(request) =>
            {
                false
            },
            CoreResourceMsg::Fetch(request, _) => self.allows_fetch(request),
            // A redirected navigation fetches nothing but the location of the redirect.
            CoreResourceMsg::FetchRedirect(request, response, _)
                if request.mode == RequestMode::Navigate =>
            {
                matches!(&response.location_url, Some(Ok(location)) if self.allows_document(location))
            },
            CoreResourceMsg::FetchRedirect(request, ..) => self.allows_fetch(request),
            // Request ids are random, and only cancel what was asked for.
            CoreResourceMsg::Cancel(_) => true,
            // Script only ever touches cookies as `document.cookie` or the Cookie Store API
            // do: the cookies of its own URL, without those hidden from script.
            CoreResourceMsg::SetCookieForUrl(url, _, source, _) |
            CoreResourceMsg::SetCookiesForUrl(url, _, source) |
            CoreResourceMsg::SetCookieForUrlAsync(_, url, _, source) |
            CoreResourceMsg::GetCookieStringForUrl(url, _, source) |
            CoreResourceMsg::GetCookiesForUrl(url, _, source) => {
                *source == CookieSource::NonHTTP && self.allows_url(url)
            },
            CoreResourceMsg::GetCookieDataForUrlAsync(_, url, _) |
            CoreResourceMsg::GetAllCookieDataForUrlAsync(_, url, _) |
            CoreResourceMsg::DeleteCookie(url, _) |
            CoreResourceMsg::DeleteCookieAsync(_, url, _) |
            CoreResourceMsg::NewCookieListener(_, _, url) => self.allows_url(url),
            CoreResourceMsg::RemoveCookieListener(_) => true,
            // A history state id is a namespace and a counter: whose it is depends on the
            // namespaces the process was given, see [`allows_resource_message`].
            CoreResourceMsg::GetHistoryState(..) |
            CoreResourceMsg::SetHistoryState(..) |
            CoreResourceMsg::RemoveHistoryStates(_) => false,
            // The keep-alive and blob token ids are random.
            CoreResourceMsg::TotalSizeOfInFlightKeepAliveRecords(..) |
            CoreResourceMsg::RevokeTokenForFile(_) |
            CoreResourceMsg::RefreshTokenForFile(_) => true,
            CoreResourceMsg::NetworkMediator(_, origin) => self.allows_origin(origin),
            CoreResourceMsg::ToFileManager(message) => self.allows_file_manager_message(message),
            // What the embedder, WebDriver and the browser itself do to the cookies and the
            // cache of every site, or to the network thread.
            CoreResourceMsg::EmbedderGetCookiesForUrl(..) |
            CoreResourceMsg::EmbedderSetCookieForUrl(..) |
            CoreResourceMsg::EmbedderClearCookies(_) |
            CoreResourceMsg::EmbedderClearSessionCookies(_) |
            CoreResourceMsg::DeleteCookiesForSites(..) |
            CoreResourceMsg::DeleteCookies(..) |
            CoreResourceMsg::DeleteSessionCookies(_) |
            CoreResourceMsg::ListCookies(_) |
            CoreResourceMsg::GetCacheEntries(_) |
            CoreResourceMsg::ClearCache(_) |
            CoreResourceMsg::SetCacheBypass(..) |
            CoreResourceMsg::Exit(_) |
            CoreResourceMsg::NewTemporarySession(..) |
            CoreResourceMsg::CollectMemoryReport(_) => false,
        }
    }

    fn allows_file_manager_message(&self, message: &FileManagerThreadMsg) -> bool {
        match message {
            // The file picker answers the user, not the page.
            FileManagerThreadMsg::SelectFiles(..) => true,
            FileManagerThreadMsg::ReadFile(_, _, origin) |
            FileManagerThreadMsg::PromoteMemory(_, _, _, origin) |
            FileManagerThreadMsg::AddSlicedURLEntry(_, _, _, origin) |
            FileManagerThreadMsg::DecRef(_, origin, _) |
            FileManagerThreadMsg::ActivateBlobURL(_, _, origin) |
            FileManagerThreadMsg::RevokeBlobURL(_, origin, _) => self.allows_origin(origin),
            // Blob ids are random: only who made the blob, or was handed its URL, knows one.
            FileManagerThreadMsg::GetTokenForFile(..) |
            FileManagerThreadMsg::RevokeTokenForFile(..) => true,
        }
    }

    fn allows_web_storage_message(&self, message: &WebStorageThreadMsg) -> bool {
        match message {
            WebStorageThreadMsg::Length(_, _, _, origin) |
            WebStorageThreadMsg::Key(_, _, _, origin, _) |
            WebStorageThreadMsg::Keys(_, _, _, origin) |
            WebStorageThreadMsg::GetItem(_, _, _, origin, _) |
            WebStorageThreadMsg::SetItem(_, _, _, origin, _, _) |
            WebStorageThreadMsg::RemoveItem(_, _, _, origin, _) |
            WebStorageThreadMsg::Clear(_, _, _, origin) => self.allows_origin(origin),
            // Copying the session storage of a whole tab into a new one is the browser's
            // doing, as is everything below.
            WebStorageThreadMsg::Clone { .. } |
            WebStorageThreadMsg::ListOrigins(..) |
            WebStorageThreadMsg::ClearDataForSites(..) |
            WebStorageThreadMsg::Exit(_) |
            WebStorageThreadMsg::CollectMemoryReport(_) => false,
        }
    }
}

/// Whether `request` carries a forbidden method or a forbidden request header
/// (<https://fetch.spec.whatwg.org/#forbidden-request-header>), which only the network
/// thread may set: with them a process could send cookies, a `Host`, an `Origin` or a
/// `Sec-Fetch-Site` of its choosing. Script never asks for them, as `Headers` and
/// `XMLHttpRequest` leave them out, so the request is refused rather than cleaned: the
/// process sending it is not running script as it should. The one other source is the
/// headers an embedder gives a load of its own, which then cannot carry such a header.
fn asks_for_what_only_the_network_sets(request: &RequestBuilder) -> bool {
    is_forbidden_method(request.method.as_str().as_bytes()) ||
        request
            .headers
            .iter()
            .any(|(name, value)| is_forbidden_request_header(name.as_str(), value.as_bytes()))
}

/// The storage bottles a content process was given, by id. A bottle id is a plain counter,
/// so the id alone cannot be what lets a process into one.
type OwnedBottles = Arc<Mutex<FxHashSet<i64>>>;

/// The pipeline namespaces a content process was given. Ids made in one, such as history
/// state ids, are the namespace and a counter, so the id alone cannot be what lets a
/// process use one.
pub(crate) type OwnedNamespaces = Arc<Mutex<FxHashSet<PipelineNamespaceId>>>;

/// A sender of namespace requests for a content process, whose namespaces are recorded in
/// `owned_namespaces` before the process learns them. `None` if a channel could not be made.
pub(crate) fn lock_namespace_requests(
    owned_namespaces: OwnedNamespaces,
    destination: GenericSender<PipelineNamespaceRequest>,
) -> Option<GenericSender<PipelineNamespaceRequest>> {
    let (sender, receiver) = generic_channel::channel::<PipelineNamespaceRequest>()?;
    receiver.route_with(move |request| {
        let PipelineNamespaceRequest(reply_sender) = match request {
            Ok(request) => request,
            Err(error) => {
                return debug!("Site lock: namespace channel of a content process closed: {error:?}");
            },
        };
        let Some((sender, receiver)) = generic_channel::channel::<PipelineNamespaceId>() else {
            return warn!("Site lock: no channel for a namespace reply");
        };
        let owned_namespaces = owned_namespaces.clone();
        receiver.route_with(move |namespace| {
            let Ok(namespace) = namespace else {
                return;
            };
            owned_namespaces.lock().insert(namespace);
            reply_sender.send_or_warn(namespace);
        });
        destination.send_or_warn(PipelineNamespaceRequest(sender));
    });
    Some(sender)
}

fn owns_history_state(owned_namespaces: &OwnedNamespaces, id: &HistoryStateId) -> bool {
    owned_namespaces.lock().contains(&id.namespace_id)
}

/// Whether a content process locked to `lock` and given `owned_namespaces` may send
/// `message` to the resource thread. History states are kept by id alone, so a process
/// reaches only those it made, in its own namespaces: the constellation hands a state id
/// back only to the pipeline that made it, and a pipeline loaded anew starts without one.
fn allows_resource_message(
    lock: &SiteLock,
    owned_namespaces: &OwnedNamespaces,
    message: &CoreResourceMsg,
) -> bool {
    match message {
        CoreResourceMsg::GetHistoryState(id, _) | CoreResourceMsg::SetHistoryState(id, _) => {
            owns_history_state(owned_namespaces, id)
        },
        CoreResourceMsg::RemoveHistoryStates(ids) => ids
            .iter()
            .all(|id| owns_history_state(owned_namespaces, id)),
        _ => lock.allows_core_resource_message(message),
    }
}

/// Senders to the resource and storage threads for a content process locked to `lock` and
/// given `owned_namespaces`. They are `None` if a channel could not be made.
pub(crate) fn lock_threads(
    lock: SiteLock,
    owned_namespaces: OwnedNamespaces,
    resource_threads: &ResourceThreads,
    storage_threads: &StorageThreads,
) -> Option<(ResourceThreads, StorageThreads)> {
    let lock = Arc::new(lock);
    let owned_bottles = OwnedBottles::default();
    let (client_storage, indexed_db, web_storage, cache_storage) = storage_threads.senders();

    let client_storage = lock_client_storage(lock.clone(), owned_bottles.clone(), client_storage)?;
    let indexed_db = {
        let lock = lock.clone();
        let owned_bottles = owned_bottles.clone();
        filter(indexed_db, "IndexedDB", move |message| {
            allows_indexed_db_message(&lock, &owned_bottles, message)
        })?
    };
    let web_storage = {
        let lock = lock.clone();
        filter(web_storage, "web storage", move |message| {
            lock.allows_web_storage_message(message)
        })?
    };
    let cache_storage = {
        let lock = lock.clone();
        filter(cache_storage, "cache storage", move |message| {
            allows_cache_storage_message(&lock, &owned_bottles, message)
        })?
    };
    Some((
        lock_resource_threads(lock, owned_namespaces, resource_threads)?,
        StorageThreads::new(client_storage, indexed_db, web_storage, cache_storage),
    ))
}

/// Senders to the resource threads for a content process locked to `lock` and given
/// `owned_namespaces`, or `None` if a channel could not be made.
pub(crate) fn lock_resource_threads(
    lock: Arc<SiteLock>,
    owned_namespaces: OwnedNamespaces,
    resource_threads: &ResourceThreads,
) -> Option<ResourceThreads> {
    let core = filter(
        resource_threads.core_thread.clone(),
        "resource",
        move |message| allows_resource_message(&lock, &owned_namespaces, message),
    )?;
    Some(ResourceThreads::new(core))
}

/// A sender whose messages reach `destination` only when `allows` says so.
fn filter<T, F>(destination: GenericSender<T>, name: &'static str, allows: F) -> Option<GenericSender<T>>
where
    T: serde::Serialize + for<'de> serde::Deserialize<'de> + fmt::Debug + Send + 'static,
    F: Fn(&T) -> bool + Send + 'static,
{
    let (sender, receiver) = generic_channel::channel()?;
    receiver.route_with(move |message| match message {
        Ok(message) if allows(&message) => destination.send_or_warn(message),
        Ok(message) => warn!("Site lock: refused {name} message {message:?}"),
        Err(error) => debug!("Site lock: {name} channel of a content process closed: {error:?}"),
    });
    Some(sender)
}

/// The client storage thread hands out storage bottles. The reply to a process carries the
/// id of its bottle, recorded here as its own, and a handle to the thread, swapped for one
/// that passes through here as well.
fn lock_client_storage(
    lock: Arc<SiteLock>,
    owned_bottles: OwnedBottles,
    destination: GenericSender<ClientStorageThreadMessage>,
) -> Option<GenericSender<ClientStorageThreadMessage>> {
    let (sender, receiver) = generic_channel::channel()?;
    let locked_sender = sender.clone();
    receiver.route_with(move |message| {
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                return debug!("Site lock: client storage channel of a content process closed: {error:?}");
            },
        };
        let message = match message {
            ClientStorageThreadMessage::ObtainBottleMap {
                storage_type,
                webview,
                storage_identifier,
                origin,
                sender: reply_sender,
            } if lock.allows_origin(&origin) => {
                let Some(reply) = record_bottle(
                    owned_bottles.clone(),
                    locked_sender.clone(),
                    reply_sender,
                ) else {
                    return warn!("Site lock: no channel for a storage bottle reply");
                };
                ClientStorageThreadMessage::ObtainBottleMap {
                    storage_type,
                    webview,
                    storage_identifier,
                    origin,
                    sender: reply,
                }
            },
            ClientStorageThreadMessage::CreateDatabase { bottle_id, .. } |
            ClientStorageThreadMessage::DeleteDatabase { bottle_id, .. }
                if owned_bottles.lock().contains(&bottle_id) =>
            {
                message
            },
            ClientStorageThreadMessage::Persisted { ref origin, .. } |
            ClientStorageThreadMessage::Persist { ref origin, .. } |
            ClientStorageThreadMessage::Estimate { ref origin, .. }
                if lock.allows_origin(origin) =>
            {
                message
            },
            refused => return warn!("Site lock: refused client storage message {refused:?}"),
        };
        destination.send_or_warn(message);
    });
    Some(sender)
}

/// A reply channel that records the bottle in a reply as owned before passing it on, with
/// its handle swapped for `locked_sender`.
fn record_bottle(
    owned_bottles: OwnedBottles,
    locked_sender: GenericSender<ClientStorageThreadMessage>,
    reply_sender: GenericSender<Result<StorageProxyMap, String>>,
) -> Option<GenericSender<Result<StorageProxyMap, String>>> {
    let (sender, receiver) = generic_channel::channel::<Result<StorageProxyMap, String>>()?;
    let mut locked_sender = Some(locked_sender);
    receiver.route_with(move |reply| {
        let Ok(reply) = reply else {
            return;
        };
        let reply = reply.map(|bottle| {
            owned_bottles.lock().insert(bottle.bottle_id);
            StorageProxyMap {
                bottle_id: bottle.bottle_id,
                handle: locked_sender
                    .take()
                    .map_or(bottle.handle, ClientStorageThreadHandle::new),
            }
        });
        reply_sender.send_or_warn(reply);
    });
    Some(sender)
}

fn owns(owned_bottles: &OwnedBottles, proxy: &StorageProxyMap) -> bool {
    owned_bottles.lock().contains(&proxy.bottle_id)
}

fn allows_indexed_db_message(
    lock: &SiteLock,
    owned_bottles: &OwnedBottles,
    message: &IndexedDBThreadMsg,
) -> bool {
    match message {
        IndexedDBThreadMsg::Sync(operation) => match operation {
            SyncOperation::OpenDatabase(_, origin, _, _, _, proxy) |
            SyncOperation::DeleteDatabase(_, origin, _, proxy, _) |
            SyncOperation::AbortPendingUpgrades {
                origin,
                proxy_map: proxy,
                ..
            } => lock.allows_origin(origin) && owns(owned_bottles, proxy),
            SyncOperation::GetDatabases(_, origin) |
            SyncOperation::UpgradeVersion(_, origin, ..) |
            SyncOperation::GetObjectStore(_, origin, ..) |
            SyncOperation::Commit(_, origin, ..) |
            SyncOperation::Abort(_, origin, ..) |
            SyncOperation::UpgradeTransactionFinished { origin, .. } |
            SyncOperation::RequestHandled { origin, .. } |
            SyncOperation::CreateTransaction { origin, .. } |
            SyncOperation::TxnMaybeCommit { origin, .. } |
            SyncOperation::TransactionFinished { origin, .. } |
            SyncOperation::CloseDatabase(origin, ..) |
            SyncOperation::Version(_, origin, _) |
            SyncOperation::NotifyEndOfVersionChange { origin, .. } => lock.allows_origin(origin),
            SyncOperation::Exit(_) => false,
        },
        IndexedDBThreadMsg::Async(origin, ..) |
        IndexedDBThreadMsg::AsyncSchemaOperation { origin, .. } |
        IndexedDBThreadMsg::EngineTxnBatchComplete { origin, .. } => lock.allows_origin(origin),
        IndexedDBThreadMsg::CollectMemoryReport(_) => false,
    }
}

fn allows_cache_storage_message(
    lock: &SiteLock,
    owned_bottles: &OwnedBottles,
    message: &CacheStorageThreadMessage,
) -> bool {
    match message {
        CacheStorageThreadMessage::HasCache { origin, proxy, .. } |
        CacheStorageThreadMessage::OpenCache { origin, proxy, .. } |
        CacheStorageThreadMessage::DeleteCache { origin, proxy, .. } => {
            lock.allows_origin(origin) && owns(owned_bottles, proxy)
        },
        CacheStorageThreadMessage::Keys { origin, .. } => lock.allows_origin(origin),
        CacheStorageThreadMessage::Exit(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use http::{HeaderName, HeaderValue, Method};
    use net_traits::FetchChannels;
    use net_traits::blob_url_store::UrlWithBlobClaim;
    use net_traits::request::Referrer;
    use net_traits::response::ResponseInit;
    use servo_base::generic_channel::GenericCallback;
    use servo_base::id::Index;
    // Registers the reader of the public suffix list the site checks need.
    use servo_default_resources as _;

    use super::*;

    fn url(url: &str) -> ServoUrl {
        ServoUrl::parse(url).expect("test URL parses")
    }

    #[test]
    fn a_site_allows_its_subdomains_and_nothing_else() {
        let lock = SiteLock::new(Some(Host::Domain("example.com".to_owned())), false);
        assert!(lock.allows_url(&url("https://example.com/")));
        assert!(lock.allows_url(&url("https://www.example.com/")));
        assert!(lock.allows_url(&url("http://a.b.example.com:8080/")));
        assert!(!lock.allows_url(&url("https://example.org/")));
        assert!(!lock.allows_url(&url("https://notexample.com/")));
        assert!(!lock.allows_url(&url("https://127.0.0.1/")));
    }

    #[test]
    fn an_address_is_a_site_of_its_own() {
        let lock = SiteLock::new(registered_domain(&url("http://127.0.0.1:8000/")), false);
        assert!(lock.allows_url(&url("http://127.0.0.1:9000/")));
        assert!(!lock.allows_url(&url("http://127.0.0.2/")));
        assert!(!lock.allows_url(&url("http://localhost/")));
    }

    #[test]
    fn opaque_pages_reach_no_site() {
        let lock = SiteLock::new(None, false);
        assert!(lock.allows_origin(&ImmutableOrigin::new_opaque()));
        assert!(!lock.allows_url(&url("https://example.com/")));
    }

    #[test]
    fn script_never_reads_http_only_cookies() {
        let lock = SiteLock::new(Some(Host::Domain("example.com".to_owned())), false);
        let (sender, _receiver) = generic_channel::channel().expect("channel");
        let message = CoreResourceMsg::GetCookiesForUrl(
            url("https://example.com/"),
            sender,
            CookieSource::HTTP,
        );
        assert!(!lock.allows_core_resource_message(&message));
    }

    #[test]
    fn script_reads_the_cookies_of_its_own_site_only() {
        let lock = SiteLock::new(Some(Host::Domain("example.com".to_owned())), false);
        let (sender, _receiver) = generic_channel::channel().expect("channel");
        let own = CoreResourceMsg::GetCookiesForUrl(
            url("https://www.example.com/"),
            sender.clone(),
            CookieSource::NonHTTP,
        );
        let other = CoreResourceMsg::GetCookiesForUrl(
            url("https://bank.example/"),
            sender,
            CookieSource::NonHTTP,
        );
        assert!(lock.allows_core_resource_message(&own));
        assert!(!lock.allows_core_resource_message(&other));
    }

    #[test]
    fn a_navigation_brings_in_documents_of_this_site_only() {
        let lock = SiteLock::new(Some(Host::Domain("example.com".to_owned())), false);
        let builder = |target: &str| {
            let mut request = RequestBuilder::new(
                None,
                UrlWithBlobClaim::from_url_without_having_claimed_blob(url(target)),
                Referrer::NoReferrer,
            );
            request.mode = RequestMode::Navigate;
            request.origin = Origin::Origin(url("https://other.example/").origin());
            request
        };
        assert!(lock.allows_fetch(&builder("https://example.com/next")));
        assert!(!lock.allows_fetch(&builder("https://bank.example/account")));
    }

    #[test]
    fn a_fetch_speaks_for_this_site_only() {
        let lock = SiteLock::new(Some(Host::Domain("example.com".to_owned())), false);
        let mut request = RequestBuilder::new(
            None,
            UrlWithBlobClaim::from_url_without_having_claimed_blob(url(
                "https://api.other.example/data",
            )),
            Referrer::NoReferrer,
        );
        request.origin = Origin::Origin(url("https://example.com/").origin());
        assert!(lock.allows_fetch(&request));
        request.origin = Origin::Origin(url("https://bank.example/").origin());
        assert!(!lock.allows_fetch(&request));
    }

    fn navigation(urls: &[&str]) -> RequestBuilder {
        let mut request = RequestBuilder::new(
            None,
            UrlWithBlobClaim::from_url_without_having_claimed_blob(url(urls[0])),
            Referrer::NoReferrer,
        );
        request.mode = RequestMode::Navigate;
        request.url_list = urls.iter().map(|target| url(target)).collect();
        request
    }

    fn redirect_to(location: &str) -> ResponseInit {
        ResponseInit {
            url: url("https://other.example/"),
            headers: Default::default(),
            status_code: 302,
            referrer: None,
            location_url: Some(Ok(url(location))),
        }
    }

    #[test]
    fn a_redirected_navigation_lands_by_its_location() {
        let lock = SiteLock::new(Some(Host::Domain("example.com".to_owned())), false);
        // Picked up here after another site redirected to this one.
        assert!(lock.allows_fetch(&navigation(&["https://other.example/", "https://example.com/"])));
        assert!(!lock.allows_fetch(&navigation(&["https://example.com/", "https://bank.example/"])));

        let callback = || GenericCallback::new(|_| {}).expect("callback");
        let own = CoreResourceMsg::FetchRedirect(
            navigation(&["https://other.example/"]),
            redirect_to("https://www.example.com/"),
            callback(),
        );
        let other = CoreResourceMsg::FetchRedirect(
            navigation(&["https://example.com/"]),
            redirect_to("https://bank.example/"),
            callback(),
        );
        assert!(lock.allows_core_resource_message(&own));
        assert!(!lock.allows_core_resource_message(&other));
    }

    #[test]
    fn a_sandboxed_frame_loads_its_site_but_acts_for_none() {
        let lock = SiteLock::new(Some(Host::Domain("example.com".to_owned())), true);
        assert!(lock.allows_fetch(&navigation(&["https://www.example.com/frame"])));
        assert!(!lock.allows_fetch(&navigation(&["https://bank.example/"])));
        assert!(lock.allows_origin(&ImmutableOrigin::new_opaque()));
        assert!(!lock.allows_url(&url("https://example.com/")));

        let mut request = RequestBuilder::new(
            None,
            UrlWithBlobClaim::from_url_without_having_claimed_blob(url("https://cdn.example/a.js")),
            Referrer::ReferrerUrl(url("https://other.example/module.js")),
        );
        request.origin = Origin::Origin(ImmutableOrigin::new_opaque());
        assert!(lock.allows_fetch(&request));
        request.origin = Origin::Origin(url("https://example.com/").origin());
        assert!(!lock.allows_fetch(&request));
    }

    #[test]
    fn a_fetch_never_sets_what_only_the_network_sets() {
        let lock = SiteLock::new(Some(Host::Domain("example.com".to_owned())), false);
        let request = || {
            let mut request = RequestBuilder::new(
                None,
                UrlWithBlobClaim::from_url_without_having_claimed_blob(url(
                    "https://example.com/data",
                )),
                Referrer::NoReferrer,
            );
            request.origin = Origin::Origin(url("https://example.com/").origin());
            request
        };
        let fetch = |request| {
            let callback = GenericCallback::new(|_| {}).expect("callback");
            CoreResourceMsg::Fetch(request, FetchChannels::ResponseMsg(callback))
        };
        assert!(lock.allows_core_resource_message(&fetch(request())));

        let mut allowed = request();
        allowed
            .headers
            .insert("x-requested-with", HeaderValue::from_static("fetch"));
        assert!(lock.allows_core_resource_message(&fetch(allowed)));

        for (name, value) in [
            ("cookie", "session=stolen"),
            ("Sec-Fetch-Site", "same-origin"),
            ("origin", "https://bank.example"),
            ("proxy-authorization", "Basic abc"),
            ("x-http-method-override", "TRACE"),
        ] {
            let mut forbidden = request();
            forbidden.headers.insert(
                HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                HeaderValue::from_static(value),
            );
            assert!(!lock.allows_core_resource_message(&fetch(forbidden)), "{name}");
        }

        let mut traced = request();
        traced.method = Method::TRACE;
        assert!(!lock.allows_core_resource_message(&fetch(traced)));

        let callback = GenericCallback::new(|_| {}).expect("callback");
        let mut redirected = navigation(&["https://other.example/"]);
        redirected
            .headers
            .insert("cookie", HeaderValue::from_static("session=stolen"));
        let message = CoreResourceMsg::FetchRedirect(
            redirected,
            redirect_to("https://www.example.com/"),
            callback,
        );
        assert!(!lock.allows_core_resource_message(&message));
    }

    #[test]
    fn a_process_reaches_only_the_history_states_it_made() {
        let lock = SiteLock::new(Some(Host::Domain("example.com".to_owned())), false);
        let owned_namespaces = OwnedNamespaces::default();
        owned_namespaces.lock().insert(PipelineNamespaceId(7));
        let state = |namespace, index| HistoryStateId {
            namespace_id: PipelineNamespaceId(namespace),
            index: Index::new(index).expect("non-zero index"),
        };
        let (own, other) = (state(7, 3), state(8, 3));
        let allows = |message| allows_resource_message(&lock, &owned_namespaces, &message);
        let get = |id| {
            let (sender, _) = generic_channel::channel().expect("channel");
            CoreResourceMsg::GetHistoryState(id, sender)
        };

        assert!(allows(get(own)));
        assert!(!allows(get(other)));
        assert!(allows(CoreResourceMsg::SetHistoryState(own, vec![1])));
        assert!(!allows(CoreResourceMsg::SetHistoryState(other, vec![1])));
        assert!(allows(CoreResourceMsg::RemoveHistoryStates(vec![own])));
        assert!(!allows(CoreResourceMsg::RemoveHistoryStates(vec![own, other])));
        // Without its namespaces the site alone lets no history state through.
        assert!(!lock.allows_core_resource_message(&get(own)));
    }

    #[test]
    fn a_namespace_is_owned_before_the_process_learns_it() {
        let owned_namespaces = OwnedNamespaces::default();
        let (constellation_sender, constellation_receiver) =
            generic_channel::channel::<PipelineNamespaceRequest>().expect("channel");
        let process_sender =
            lock_namespace_requests(owned_namespaces.clone(), constellation_sender)
                .expect("locked channel");

        let (reply_sender, reply_receiver) = generic_channel::channel().expect("channel");
        process_sender
            .send(PipelineNamespaceRequest(reply_sender))
            .expect("request sent");
        let PipelineNamespaceRequest(constellation_reply) =
            constellation_receiver.recv().expect("request forwarded");
        constellation_reply
            .send(PipelineNamespaceId(9))
            .expect("reply sent");

        assert_eq!(reply_receiver.recv().expect("reply"), PipelineNamespaceId(9));
        assert!(owned_namespaces.lock().contains(&PipelineNamespaceId(9)));
    }

    fn registered_domain(url: &ServoUrl) -> Option<Host> {
        net_traits::pub_domains::registered_domain_name(url)
    }
}
