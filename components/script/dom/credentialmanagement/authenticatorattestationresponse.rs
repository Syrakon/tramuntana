/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::typedarray::{ArrayBufferU8, HeapArrayBuffer};
use script_bindings::error::Fallible;
use script_bindings::reflector::reflect_dom_object;
use script_bindings::str::DOMString;
use script_bindings::trace::RootedTraceableBox;

use crate::dom::bindings::buffer_source::HeapBufferSource;
use crate::dom::bindings::codegen::Bindings::AuthenticatorAttestationResponseBinding::AuthenticatorAttestationResponseMethods;
use crate::dom::bindings::codegen::DomTypeHolder::DomTypeHolder;
use crate::dom::bindings::root::DomRoot;
use crate::dom::credentialmanagement::authenticatorresponse::AuthenticatorResponse;
use crate::dom::credentialmanagement::publickeycredential::{new_buffer, same_buffer};
use crate::dom::globalscope::GlobalScope;
use crate::script_runtime::CanGc;

/// What the authenticator made, as the parts an
/// [`AuthenticatorAttestationResponse`] shows to script.
pub(crate) struct Attestation {
    pub(crate) client_data_json: Vec<u8>,
    pub(crate) attestation_object: Vec<u8>,
    pub(crate) authenticator_data: Vec<u8>,
    /// The public key as SubjectPublicKeyInfo.
    pub(crate) public_key: Vec<u8>,
    /// The COSE algorithm of the public key.
    pub(crate) algorithm: i32,
}

/// <https://www.w3.org/TR/webauthn-2/#iface-authenticatorattestationresponse>
#[dom_struct]
pub(crate) struct AuthenticatorAttestationResponse {
    response: AuthenticatorResponse,
    attestation_object: Vec<u8>,
    #[ignore_malloc_size_of = "defined in mozjs"]
    attestation_object_buffer: HeapBufferSource<ArrayBufferU8>,
    authenticator_data: Vec<u8>,
    public_key: Vec<u8>,
    algorithm: i32,
}

impl AuthenticatorAttestationResponse {
    fn new_inherited(attestation: Attestation) -> AuthenticatorAttestationResponse {
        AuthenticatorAttestationResponse {
            response: AuthenticatorResponse::new_inherited(attestation.client_data_json),
            attestation_object: attestation.attestation_object,
            attestation_object_buffer: HeapBufferSource::default(),
            authenticator_data: attestation.authenticator_data,
            public_key: attestation.public_key,
            algorithm: attestation.algorithm,
        }
    }

    pub(crate) fn new(
        global: &GlobalScope,
        attestation: Attestation,
        can_gc: CanGc,
    ) -> DomRoot<AuthenticatorAttestationResponse> {
        reflect_dom_object(
            Box::new(AuthenticatorAttestationResponse::new_inherited(attestation)),
            global,
            can_gc,
        )
    }
}

impl AuthenticatorAttestationResponseMethods<DomTypeHolder> for AuthenticatorAttestationResponse {
    /// <https://www.w3.org/TR/webauthn-2/#dom-authenticatorattestationresponse-attestationobject>
    fn GetAttestationObject(
        &self,
        cx: &mut JSContext,
    ) -> Fallible<RootedTraceableBox<HeapArrayBuffer>> {
        same_buffer(
            cx,
            &self.attestation_object_buffer,
            &self.attestation_object,
        )
    }

    /// <https://www.w3.org/TR/webauthn-2/#dom-authenticatorattestationresponse-gettransports>
    fn GetTransports(&self) -> Vec<DOMString> {
        // The embedder's authenticator is part of this device.
        vec!["internal".into()]
    }

    /// <https://www.w3.org/TR/webauthn-2/#dom-authenticatorattestationresponse-getauthenticatordata>
    fn GetAuthenticatorData(
        &self,
        cx: &mut JSContext,
    ) -> Fallible<RootedTraceableBox<HeapArrayBuffer>> {
        new_buffer(cx, &self.authenticator_data)
    }

    /// <https://www.w3.org/TR/webauthn-2/#dom-authenticatorattestationresponse-getpublickey>
    fn GetPublicKey(
        &self,
        cx: &mut JSContext,
    ) -> Fallible<Option<RootedTraceableBox<HeapArrayBuffer>>> {
        new_buffer(cx, &self.public_key).map(Some)
    }

    /// <https://www.w3.org/TR/webauthn-2/#dom-authenticatorattestationresponse-getpublickeyalgorithm>
    fn GetPublicKeyAlgorithm(&self) -> i32 {
        self.algorithm
    }
}
