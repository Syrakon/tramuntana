/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::ptr;
use std::rc::Rc;

use base64ct::{Base64UrlUnpadded, Encoding};
use dom_struct::dom_struct;
use js::context::JSContext;
use js::jsapi::JSObject;
use js::typedarray::{ArrayBufferU8, HeapArrayBuffer};
use script_bindings::error::{Error, Fallible};
use script_bindings::realms::{AlreadyInRealm, InRealm};
use script_bindings::reflector::reflect_dom_object;
use script_bindings::str::{DOMString, USVString};
use script_bindings::trace::RootedTraceableBox;

use crate::dom::bindings::buffer_source::{HeapBufferSource, create_buffer_source};
use crate::dom::bindings::codegen::Bindings::PublicKeyCredentialBinding::{
    AuthenticationExtensionsClientOutputs, PublicKeyCredentialMethods,
};
use crate::dom::bindings::codegen::DomTypeHolder::DomTypeHolder;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::credentialmanagement::authenticatorresponse::AuthenticatorResponse;
use crate::dom::credentialmanagement::credential::Credential;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::Promise;
use crate::dom::window::Window;
use crate::script_runtime::CanGc;

/// `bytes` as the `ArrayBuffer` kept in `slot`: made on first use, the same
/// object from then on, as `[SameObject]` asks.
pub(super) fn same_buffer(
    cx: &mut JSContext,
    slot: &HeapBufferSource<ArrayBufferU8>,
    bytes: &[u8],
) -> Fallible<RootedTraceableBox<HeapArrayBuffer>> {
    if !slot.is_initialized() {
        slot.set_data(cx.into(), bytes, CanGc::from_cx(cx))
            .map_err(|_| Error::JSFailed)?;
    }
    slot.get_typed_array().map_err(|_| Error::JSFailed)
}

/// `bytes` as a new `ArrayBuffer`.
pub(super) fn new_buffer(
    cx: &mut JSContext,
    bytes: &[u8],
) -> Fallible<RootedTraceableBox<HeapArrayBuffer>> {
    rooted!(&in(cx) let mut object = ptr::null_mut::<JSObject>());
    create_buffer_source::<ArrayBufferU8>(
        cx.into(),
        bytes,
        object.handle_mut(),
        CanGc::from_cx(cx),
    )
    .map_err(|_| Error::JSFailed)
}

/// <https://www.w3.org/TR/webauthn-2/#iface-pkcredential>
#[dom_struct]
pub(crate) struct PublicKeyCredential {
    credential: Credential,
    raw_id: Vec<u8>,
    #[ignore_malloc_size_of = "defined in mozjs"]
    raw_id_buffer: HeapBufferSource<ArrayBufferU8>,
    response: Dom<AuthenticatorResponse>,
}

impl PublicKeyCredential {
    fn new_inherited(raw_id: Vec<u8>, response: &AuthenticatorResponse) -> PublicKeyCredential {
        // <https://www.w3.org/TR/webauthn-2/#dom-publickeycredential-identifier-slot>
        // The id is the base64url encoding of the raw id.
        let id = USVString(Base64UrlUnpadded::encode_string(&raw_id));
        PublicKeyCredential {
            credential: Credential::new_inherited(id, "public-key".into()),
            raw_id,
            raw_id_buffer: HeapBufferSource::default(),
            response: Dom::from_ref(response),
        }
    }

    pub(crate) fn new(
        global: &GlobalScope,
        raw_id: Vec<u8>,
        response: &AuthenticatorResponse,
        can_gc: CanGc,
    ) -> DomRoot<PublicKeyCredential> {
        reflect_dom_object(
            Box::new(PublicKeyCredential::new_inherited(raw_id, response)),
            global,
            can_gc,
        )
    }
}

impl PublicKeyCredentialMethods<DomTypeHolder> for PublicKeyCredential {
    /// <https://www.w3.org/TR/webauthn-2/#dom-publickeycredential-rawid>
    fn GetRawId(&self, cx: &mut JSContext) -> Fallible<RootedTraceableBox<HeapArrayBuffer>> {
        same_buffer(cx, &self.raw_id_buffer, &self.raw_id)
    }

    /// <https://www.w3.org/TR/webauthn-2/#dom-publickeycredential-response>
    fn Response(&self) -> DomRoot<AuthenticatorResponse> {
        self.response.as_rooted()
    }

    /// <https://www.w3.org/TR/webauthn-3/#dom-publickeycredential-authenticatorattachment>
    fn GetAuthenticatorAttachment(&self) -> Option<DOMString> {
        // The embedder's authenticator is part of this device.
        Some("platform".into())
    }

    /// <https://www.w3.org/TR/webauthn-2/#dom-publickeycredential-getclientextensionresults>
    fn GetClientExtensionResults(&self) -> AuthenticationExtensionsClientOutputs {
        AuthenticationExtensionsClientOutputs {}
    }

    /// <https://www.w3.org/TR/webauthn-2/#dom-publickeycredential-isuserverifyingplatformauthenticatoravailable>
    fn IsUserVerifyingPlatformAuthenticatorAvailable(
        cx: &mut JSContext,
        _global: &Window,
    ) -> Rc<Promise> {
        let in_realm_proof = AlreadyInRealm::assert::<DomTypeHolder>();
        let promise =
            Promise::new_in_current_realm(InRealm::Already(&in_realm_proof), CanGc::from_cx(cx));
        // Whether the embedder has an authenticator is not known here: it is
        // asked only when a passkey is. Saying yes lets the page offer one.
        promise.resolve_native(&true, CanGc::from_cx(cx));
        promise
    }
}
