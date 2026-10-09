/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::typedarray::{ArrayBufferU8, HeapArrayBuffer};
use script_bindings::error::Fallible;
use script_bindings::reflector::reflect_dom_object;
use script_bindings::trace::RootedTraceableBox;

use crate::dom::bindings::buffer_source::HeapBufferSource;
use crate::dom::bindings::codegen::Bindings::AuthenticatorAssertionResponseBinding::AuthenticatorAssertionResponseMethods;
use crate::dom::bindings::codegen::DomTypeHolder::DomTypeHolder;
use crate::dom::bindings::root::DomRoot;
use crate::dom::credentialmanagement::authenticatorresponse::AuthenticatorResponse;
use crate::dom::credentialmanagement::publickeycredential::same_buffer;
use crate::dom::globalscope::GlobalScope;
use crate::script_runtime::CanGc;

/// What the authenticator signed, as the parts an
/// [`AuthenticatorAssertionResponse`] shows to script.
pub(crate) struct Assertion {
    pub(crate) client_data_json: Vec<u8>,
    pub(crate) authenticator_data: Vec<u8>,
    pub(crate) signature: Vec<u8>,
    pub(crate) user_handle: Vec<u8>,
}

/// <https://www.w3.org/TR/webauthn-2/#iface-authenticatorassertionresponse>
#[dom_struct]
pub(crate) struct AuthenticatorAssertionResponse {
    response: AuthenticatorResponse,
    authenticator_data: Vec<u8>,
    #[ignore_malloc_size_of = "defined in mozjs"]
    authenticator_data_buffer: HeapBufferSource<ArrayBufferU8>,
    signature: Vec<u8>,
    #[ignore_malloc_size_of = "defined in mozjs"]
    signature_buffer: HeapBufferSource<ArrayBufferU8>,
    user_handle: Vec<u8>,
    #[ignore_malloc_size_of = "defined in mozjs"]
    user_handle_buffer: HeapBufferSource<ArrayBufferU8>,
}

impl AuthenticatorAssertionResponse {
    fn new_inherited(assertion: Assertion) -> AuthenticatorAssertionResponse {
        AuthenticatorAssertionResponse {
            response: AuthenticatorResponse::new_inherited(assertion.client_data_json),
            authenticator_data: assertion.authenticator_data,
            authenticator_data_buffer: HeapBufferSource::default(),
            signature: assertion.signature,
            signature_buffer: HeapBufferSource::default(),
            user_handle: assertion.user_handle,
            user_handle_buffer: HeapBufferSource::default(),
        }
    }

    pub(crate) fn new(
        global: &GlobalScope,
        assertion: Assertion,
        can_gc: CanGc,
    ) -> DomRoot<AuthenticatorAssertionResponse> {
        reflect_dom_object(
            Box::new(AuthenticatorAssertionResponse::new_inherited(assertion)),
            global,
            can_gc,
        )
    }
}

impl AuthenticatorAssertionResponseMethods<DomTypeHolder> for AuthenticatorAssertionResponse {
    /// <https://www.w3.org/TR/webauthn-2/#dom-authenticatorassertionresponse-authenticatordata>
    fn GetAuthenticatorData(
        &self,
        cx: &mut JSContext,
    ) -> Fallible<RootedTraceableBox<HeapArrayBuffer>> {
        same_buffer(
            cx,
            &self.authenticator_data_buffer,
            &self.authenticator_data,
        )
    }

    /// <https://www.w3.org/TR/webauthn-2/#dom-authenticatorassertionresponse-signature>
    fn GetSignature(&self, cx: &mut JSContext) -> Fallible<RootedTraceableBox<HeapArrayBuffer>> {
        same_buffer(cx, &self.signature_buffer, &self.signature)
    }

    /// <https://www.w3.org/TR/webauthn-2/#dom-authenticatorassertionresponse-userhandle>
    fn GetUserHandle(
        &self,
        cx: &mut JSContext,
    ) -> Fallible<Option<RootedTraceableBox<HeapArrayBuffer>>> {
        // A passkey always has the handle its account was made with.
        same_buffer(cx, &self.user_handle_buffer, &self.user_handle).map(Some)
    }
}
