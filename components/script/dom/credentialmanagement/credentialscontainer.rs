/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use dom_struct::dom_struct;
use embedder_traits::{EmbedderMsg, PasskeyOperation, PasskeyResponse};
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::codegen::GenericBindings::CredentialsContainerBinding::{
    CredentialCreationOptions, CredentialMediationRequirement, CredentialRequestOptions,
};
use script_bindings::codegen::GenericBindings::PublicKeyCredentialBinding::PublicKeyCredentialDescriptor;
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::error::{Error, Fallible};
use script_bindings::reflector::{Reflector, reflect_dom_object};
use script_bindings::trace::RootedTraceableBox;

use crate::dom::bindings::codegen::Bindings::CredentialsContainerBinding::CredentialsContainerMethods;
use crate::dom::bindings::codegen::DomTypeHolder::DomTypeHolder;
use crate::dom::bindings::codegen::UnionTypes::ArrayBufferViewOrArrayBuffer;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::credentialmanagement::authenticatorassertionresponse::{
    Assertion, AuthenticatorAssertionResponse,
};
use crate::dom::credentialmanagement::authenticatorattestationresponse::{
    Attestation, AuthenticatorAttestationResponse,
};
use crate::dom::credentialmanagement::authenticatorresponse::AuthenticatorResponse;
use crate::dom::credentialmanagement::credential::Credential;
use crate::dom::credentialmanagement::publickeycredential::PublicKeyCredential;
use crate::dom::globalscope::GlobalScope;
use crate::dom::promise::{Promise, RootedPromise};
use crate::routed_promise::{RoutedPromiseListener, callback_promise};
use crate::script_runtime::CanGc;

/// The bytes of a `BufferSource`, copied out.
fn bytes_of(source: &ArrayBufferViewOrArrayBuffer) -> Vec<u8> {
    match source {
        ArrayBufferViewOrArrayBuffer::ArrayBufferView(view) => view.to_vec(),
        ArrayBufferViewOrArrayBuffer::ArrayBuffer(buffer) => buffer.to_vec(),
    }
}

/// The ids of the descriptors that name a public key credential.
/// <https://www.w3.org/TR/webauthn-2/#dictionary-credential-descriptor>
fn credential_ids(
    descriptors: Option<&Vec<RootedTraceableBox<PublicKeyCredentialDescriptor>>>,
) -> Vec<Vec<u8>> {
    descriptors
        .into_iter()
        .flatten()
        .filter(|descriptor| &*descriptor.type_.str() == "public-key")
        .map(|descriptor| bytes_of(&descriptor.id))
        .collect()
}

/// Whether `mediation` has the user asked nothing, which no passkey is made
/// or used under: the embedder's authenticator always asks.
fn without_asking(mediation: CredentialMediationRequirement) -> bool {
    matches!(
        mediation,
        CredentialMediationRequirement::Silent | CredentialMediationRequirement::Conditional
    )
}

#[dom_struct]
pub(crate) struct CredentialsContainer {
    reflector_: Reflector,
}

impl CredentialsContainer {
    pub(crate) fn new_inherited() -> CredentialsContainer {
        CredentialsContainer {
            reflector_: Reflector::new(),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<CredentialsContainer> {
        reflect_dom_object(cx, Box::new(CredentialsContainer::new_inherited()), global)
    }

    /// <https://www.w3.org/TR/credential-management-1/#abstract-opdef-request-a-credential>
    fn request_credential(
        &self,
        cx: &mut CurrentRealm,
        options: &CredentialRequestOptions<DomTypeHolder>,
    ) -> Fallible<RootedPromise> {
        // Step 1. Let settings be the current settings object.
        let global = GlobalScope::from_current_realm(cx);
        // Step 2. Assert: settings is a secure context.
        assert!(global.is_secure_context());
        // Step 3. Let document be settings’s relevant global object's associated Document.
        let document = global.as_window().Document();

        let promise = Promise::new_in_realm(cx);
        // Step 4. If document is not fully active, then return a promise rejected with an "InvalidStateError" DOMException.
        if !document.is_fully_active() {
            promise.reject_error(cx, Error::InvalidState(None));
            return Ok(promise);
        }
        // Step 5. If options.signal is aborted, then return a promise rejected with options.signal’s abort reason.
        if options.signal.as_ref().is_some_and(|s| s.aborted()) {
            promise.reject_error(cx, Error::Abort(None));
            return Ok(promise);
        }
        // <https://www.w3.org/TR/webauthn-2/#sctn-discover-from-external-source>
        let Some(public_key) = options.publicKey.as_ref() else {
            return Err(Error::NotSupported(None));
        };
        if without_asking(options.mediation) {
            return Err(Error::NotAllowed(None));
        }
        Ok(self.ask_for_passkey(
            cx,
            &global,
            PasskeyOperation::Get {
                rp_id: public_key.rpId.as_ref().map(|id| id.to_string()),
                challenge: bytes_of(&public_key.challenge),
                allow: credential_ids(public_key.allowCredentials.as_ref()),
            },
        ))
    }

    /// Hand `operation` to the embedder's authenticator and return the promise
    /// its answer settles.
    ///
    /// Only the top-level document of a webview may ask, so that the origin
    /// sent along is the one of the page the user sees, and it is the origin
    /// this document has, which script cannot change.
    fn ask_for_passkey(
        &self,
        cx: &mut CurrentRealm,
        global: &GlobalScope,
        operation: PasskeyOperation,
    ) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        let origin = global.origin().immutable();
        let webview_id = match global.webview_id() {
            Some(id) if global.as_window().is_top_level() && origin.is_tuple() => id,
            _ => {
                promise.reject_error(cx, Error::NotAllowed(None));
                return promise;
            },
        };
        let task_source = global.task_manager().dom_manipulation_task_source();
        let callback = callback_promise(&promise, self, task_source);
        global.send_to_embedder(EmbedderMsg::RequestPasskey(
            webview_id,
            origin.ascii_serialization(),
            operation,
            callback,
        ));
        promise
    }

    /// <https://www.w3.org/TR/credential-management-1/#abstract-opdef-store-a-credential>
    fn store_credential(
        &self,
        cx: &mut CurrentRealm,
        _credential: &Credential,
    ) -> Fallible<RootedPromise> {
        // Step 1. Let settings be the current settings object.
        let global = GlobalScope::from_current_realm(cx);
        // Step 2. Assert: settings is a secure context.
        assert!(global.is_secure_context());

        let promise = Promise::new_in_realm(cx);
        // Step 3. If settings’s relevant global object's associated Document is not fully active, then return a promise rejected with an "InvalidStateError" DOMException.
        if !global.as_window().Document().is_fully_active() {
            promise.reject_error(cx, Error::InvalidState(None));
            return Ok(promise);
        }
        promise.reject_error(cx, Error::NotSupported(None));
        Ok(promise)
    }

    /// <https://www.w3.org/TR/credential-management-1/#abstract-opdef-create-a-credential>
    fn create_credential(
        &self,
        cx: &mut CurrentRealm,
        _options: &CredentialCreationOptions<DomTypeHolder>,
    ) -> Fallible<RootedPromise> {
        // Step 1. Let settings be the current settings object.
        let global = GlobalScope::from_current_realm(cx);
        // Step 2. Assert: settings is a secure context.
        assert!(global.is_secure_context());
        // Step 3. Let global be settings’ global object.
        // Step 4. Let document be the relevant global object’s associated Document.
        let document = global.as_window().Document();

        let promise = Promise::new_in_realm(cx);
        // Step 5. If document is not fully active, then return a promise rejected with an "InvalidStateError" DOMException.
        if !document.is_fully_active() {
            promise.reject_error(cx, Error::InvalidState(None));
            return Ok(promise);
        }
        // <https://www.w3.org/TR/webauthn-2/#sctn-createCredential>
        let Some(public_key) = options.publicKey.as_ref() else {
            return Err(Error::NotSupported(None));
        };
        if without_asking(options.mediation) {
            return Err(Error::NotAllowed(None));
        }
        let algorithms = public_key
            .pubKeyCredParams
            .iter()
            .filter(|parameters| &*parameters.type_.str() == "public-key")
            .map(|parameters| parameters.alg)
            .collect();
        Ok(self.ask_for_passkey(
            cx,
            &global,
            PasskeyOperation::Create {
                rp_id: public_key.rp.id.as_ref().map(|id| id.to_string()),
                rp_name: public_key.rp.name.to_string(),
                challenge: bytes_of(&public_key.challenge),
                user_handle: bytes_of(&public_key.user.id),
                user_name: public_key.user.name.to_string(),
                display_name: public_key.user.displayName.to_string(),
                algorithms,
                exclude: credential_ids(public_key.excludeCredentials.as_ref()),
            },
        ))
    }
}

impl RoutedPromiseListener<DomTypeHolder, PasskeyResponse> for CredentialsContainer {
    /// The embedder's authenticator answered: settle the promise of
    /// [`CredentialsContainer::ask_for_passkey`].
    fn handle_response(
        &self,
        cx: &mut JSContext,
        response: PasskeyResponse,
        promise: &RootedPromise,
    ) {
        let can_gc = CanGc::from_cx(cx);
        let global = self.global();
        match response {
            PasskeyResponse::Created {
                credential_id,
                client_data_json,
                authenticator_data,
                attestation_object,
                public_key,
                algorithm,
            } => {
                let response = AuthenticatorAttestationResponse::new(
                    &global,
                    Attestation {
                        client_data_json,
                        attestation_object,
                        authenticator_data,
                        public_key,
                        algorithm,
                    },
                    can_gc,
                );
                let credential = PublicKeyCredential::new(
                    &global,
                    credential_id,
                    response.upcast::<AuthenticatorResponse>(),
                    can_gc,
                );
                promise.resolve_native(cx, &credential);
            },
            PasskeyResponse::Asserted {
                credential_id,
                client_data_json,
                authenticator_data,
                signature,
                user_handle,
            } => {
                let response = AuthenticatorAssertionResponse::new(
                    &global,
                    Assertion {
                        client_data_json,
                        authenticator_data,
                        signature,
                        user_handle,
                    },
                    can_gc,
                );
                let credential = PublicKeyCredential::new(
                    &global,
                    credential_id,
                    response.upcast::<AuthenticatorResponse>(),
                    can_gc,
                );
                promise.resolve_native(cx, &credential);
            },
            PasskeyResponse::Refused => promise.reject_error(cx, Error::NotAllowed(None)),
            PasskeyResponse::WrongRelyingParty => {
                promise.reject_error(cx, Error::Security(None))
            },
            PasskeyResponse::AlreadyRegistered => {
                promise.reject_error(cx, Error::InvalidState(None))
            },
            PasskeyResponse::Unsupported => {
                promise.reject_error(cx, Error::NotSupported(None))
            },
        }
    }
}

impl CredentialsContainerMethods<DomTypeHolder> for CredentialsContainer {
    /// <https://www.w3.org/TR/credential-management-1/#dom-credentialscontainer-get>
    fn Get(
        &self,
        cx: &mut CurrentRealm,
        options: &CredentialRequestOptions<DomTypeHolder>,
    ) -> Fallible<RootedPromise> {
        self.request_credential(cx, options)
    }

    /// <https://www.w3.org/TR/credential-management-1/#dom-credentialscontainer-store>
    fn Store(&self, cx: &mut CurrentRealm, credential: &Credential) -> Fallible<RootedPromise> {
        self.store_credential(cx, credential)
    }

    /// <https://www.w3.org/TR/credential-management-1/#dom-credentialscontainer-create>
    fn Create(
        &self,
        cx: &mut CurrentRealm,
        options: &CredentialCreationOptions<DomTypeHolder>,
    ) -> Fallible<RootedPromise> {
        self.create_credential(cx, options)
    }

    /// <https://www.w3.org/TR/credential-management-1/#dom-credentialscontainer-preventsilentaccess>
    fn PreventSilentAccess(&self, cx: &mut CurrentRealm) -> Fallible<RootedPromise> {
        // No credential here is ever given without asking the user, so there
        // is no silent access to prevent. Pages call this when signing out.
        let promise = Promise::new_in_realm(cx);
        promise.resolve_native(cx, &());
        Ok(promise)
    }
}
