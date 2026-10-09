/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::typedarray::{ArrayBufferU8, HeapArrayBuffer};
use script_bindings::error::Fallible;
use script_bindings::reflector::Reflector;
use script_bindings::trace::RootedTraceableBox;

use crate::dom::bindings::buffer_source::HeapBufferSource;
use crate::dom::bindings::codegen::Bindings::AuthenticatorResponseBinding::AuthenticatorResponseMethods;
use crate::dom::bindings::codegen::DomTypeHolder::DomTypeHolder;
use crate::dom::credentialmanagement::publickeycredential::same_buffer;

/// <https://www.w3.org/TR/webauthn-2/#iface-authenticatorresponse>
#[dom_struct]
pub(crate) struct AuthenticatorResponse {
    reflector_: Reflector,
    client_data_json: Vec<u8>,
    #[ignore_malloc_size_of = "defined in mozjs"]
    client_data_json_buffer: HeapBufferSource<ArrayBufferU8>,
}

impl AuthenticatorResponse {
    pub(crate) fn new_inherited(client_data_json: Vec<u8>) -> AuthenticatorResponse {
        AuthenticatorResponse {
            reflector_: Reflector::new(),
            client_data_json,
            client_data_json_buffer: HeapBufferSource::default(),
        }
    }
}

impl AuthenticatorResponseMethods<DomTypeHolder> for AuthenticatorResponse {
    /// <https://www.w3.org/TR/webauthn-2/#dom-authenticatorresponse-clientdatajson>
    fn GetClientDataJSON(
        &self,
        cx: &mut JSContext,
    ) -> Fallible<RootedTraceableBox<HeapArrayBuffer>> {
        same_buffer(cx, &self.client_data_json_buffer, &self.client_data_json)
    }
}
