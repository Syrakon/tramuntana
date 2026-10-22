/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The Storage Access API: <https://privacycg.github.io/storage-access/>
//!
//! A document embedded by another site gets cookies kept for that site alone (see
//! `net_traits::cookie_partition`). With `requestStorageAccess()`, and the user's leave, it
//! gets those of its own site instead. As in Firefox, the user's answer holds for every
//! document of that site under the same top-level site, and the network process keeps it:
//! a document asks there whether it has access.

use embedder_traits::{AllowOrDeny, EmbedderMsg, StorageAccessSites};
use js::context::JSContext;
use js::realm::CurrentRealm;
use log::warn;
use net_traits::CoreResourceMsg;
use net_traits::cookie_partition::{CookieContext, top_level_site};
use servo_base::generic_channel::{self, GenericSend};

use crate::dom::bindings::error::Error;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::document::Document;
use crate::dom::promise::{Promise, RootedPromise};
use crate::routed_promise::{RoutedPromiseListener, callback_promise};

impl Document {
    /// <https://privacycg.github.io/storage-access/#dom-document-hasstorageaccess>
    pub(crate) fn has_storage_access(&self, cx: &mut CurrentRealm) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);

        // If doc is not fully active, reject with an "InvalidStateError".
        if !self.is_fully_active() {
            promise.reject_error(cx, Error::InvalidState(None));
            return promise;
        }

        // If doc's origin is opaque, resolve with false.
        if !self.origin().is_tuple() {
            promise.resolve_native(cx, &false);
            return promise;
        }

        // A top-level or same-site document has its first-party cookies, and
        // another one has them if the user let its site use them here.
        let has_access = self.uses_first_party_cookies();
        promise.resolve_native(cx, &has_access);
        promise
    }

    /// <https://privacycg.github.io/storage-access/#dom-document-requeststorageaccess>
    pub(crate) fn request_storage_access(&self, cx: &mut CurrentRealm) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);

        // If doc is not fully active, reject with an "InvalidStateError".
        if !self.is_fully_active() {
            promise.reject_error(cx, Error::InvalidState(None));
            return promise;
        }

        // If doc's origin is opaque, reject with a "NotAllowedError".
        if !self.origin().is_tuple() {
            promise.reject_error(cx, not_allowed("the document's origin is opaque"));
            return promise;
        }

        // A top-level or same-site document, or one whose site the
        // user let use its cookies here already, has nothing to ask for.
        if self.uses_first_party_cookies() {
            promise.resolve_native(cx, &());
            return promise;
        }

        // If doc's active sandboxing flag set has its sandboxed storage access by
        // user activation flag set, reject with a "NotAllowedError".
        // The `allow-storage-access-by-user-activation` token that clears that flag is not
        // parsed, so every sandboxed document is refused.
        if !self.active_sandboxing_flag_set().is_empty() {
            promise.reject_error(cx, not_allowed("the document is sandboxed"));
            return promise;
        }

        // The user is only asked about a document they have just interacted with.
        let window = self.window();
        if !window.has_transient_activation() {
            promise.reject_error(cx, not_allowed("there is no user activation"));
            return promise;
        }

        let global = self.global();
        let (Some(webview_id), Some(context)) = (global.webview_id(), global.cookie_context())
        else {
            promise.reject_error(cx, not_allowed("the document has no cookie context"));
            return promise;
        };
        let (Some(embedded_site), Some(top_level_site)) = (
            top_level_site(&context.origin),
            top_level_site(&context.top_level_origin),
        ) else {
            promise.reject_error(cx, not_allowed("the top-level origin is opaque"));
            return promise;
        };

        // Ask the user, without blocking the page while they decide.
        let task_manager = global.task_manager();
        let task_source = task_manager.dom_manipulation_task_source();
        let callback = callback_promise(&promise, self, task_source);
        global.send_to_embedder(EmbedderMsg::RequestStorageAccess(
            webview_id,
            StorageAccessSites {
                embedded_site,
                top_level_site,
            },
            callback,
        ));
        promise
    }

    /// Whether this document's requests and `document.cookie` use the first-party cookies
    /// of its site, as the network process decides.
    fn uses_first_party_cookies(&self) -> bool {
        let global = self.global();
        let Some(context) = global.cookie_context() else {
            // A document with no context is treated as top-level everywhere else.
            return true;
        };
        let Some((sender, receiver)) = generic_channel::channel() else {
            warn!("Could not ask the network process whether a document has storage access");
            return false;
        };
        let message = CoreResourceMsg::HasStorageAccess(self.url(), context, sender);
        if global.resource_threads().send(message).is_err() {
            return false;
        }
        receiver.recv().unwrap_or(false)
    }

    /// The context in which the user answered yes: its site may use its cookies there.
    fn grant_storage_access(&self, context: CookieContext) {
        let global = self.global();
        if global
            .resource_threads()
            .send(CoreResourceMsg::GrantStorageAccess(context))
            .is_err()
        {
            warn!("Could not tell the network process about a storage access grant");
        }
    }
}

impl RoutedPromiseListener<crate::DomTypeHolder, AllowOrDeny> for Document {
    /// <https://privacycg.github.io/storage-access/#dom-document-requeststorageaccess>
    fn handle_response(&self, cx: &mut JSContext, response: AllowOrDeny, promise: &RootedPromise) {
        match (response, self.global().cookie_context()) {
            // If permissionState is "granted", set doc's has storage access to
            // true and resolve.
            (AllowOrDeny::Allow, Some(context)) => {
                self.grant_storage_access(context);
                promise.resolve_native(cx, &());
            },
            // Otherwise, consume user activation and reject.
            (AllowOrDeny::Allow, None) | (AllowOrDeny::Deny, _) => {
                self.window().consume_user_activation();
                promise.reject_error(cx, not_allowed("the user did not allow it"));
            },
        }
    }
}

fn not_allowed(reason: &str) -> Error {
    Error::NotAllowed(Some(format!(
        "Failed to execute 'requestStorageAccess' on 'Document': {reason}."
    )))
}
