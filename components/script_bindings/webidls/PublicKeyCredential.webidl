/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://www.w3.org/TR/webauthn-2/#iface-pkcredential
 */

// https://www.w3.org/TR/webauthn-2/#iface-pkcredential
[Pref="dom_credential_management_enabled", Exposed=Window, SecureContext]
interface PublicKeyCredential : Credential {
  [SameObject, Throws] readonly attribute ArrayBuffer rawId;
  [SameObject] readonly attribute AuthenticatorResponse response;
  readonly attribute DOMString? authenticatorAttachment;
  AuthenticationExtensionsClientOutputs getClientExtensionResults();
  static Promise<boolean> isUserVerifyingPlatformAuthenticatorAvailable();
};

// https://www.w3.org/TR/webauthn-2/#dictionary-makecredentialoptions
// Members the authenticator has no use for are left out: a dictionary
// ignores what it does not name.
dictionary PublicKeyCredentialCreationOptions {
  required PublicKeyCredentialRpEntity rp;
  required PublicKeyCredentialUserEntity user;
  required BufferSource challenge;
  required sequence<PublicKeyCredentialParameters> pubKeyCredParams;
  sequence<PublicKeyCredentialDescriptor> excludeCredentials;
};

// https://www.w3.org/TR/webauthn-2/#dictionary-rp-credential-params
dictionary PublicKeyCredentialRpEntity {
  required DOMString name;
  DOMString id;
};

// https://www.w3.org/TR/webauthn-2/#dictionary-user-credential-params
dictionary PublicKeyCredentialUserEntity {
  required DOMString name;
  required BufferSource id;
  required DOMString displayName;
};

// https://www.w3.org/TR/webauthn-2/#dictionary-credential-params
dictionary PublicKeyCredentialParameters {
  required DOMString type;
  required long alg;
};

// https://www.w3.org/TR/webauthn-2/#dictionary-credential-descriptor
dictionary PublicKeyCredentialDescriptor {
  required DOMString type;
  required BufferSource id;
};

// https://www.w3.org/TR/webauthn-2/#dictionary-assertion-options
dictionary PublicKeyCredentialRequestOptions {
  required BufferSource challenge;
  DOMString rpId;
  sequence<PublicKeyCredentialDescriptor> allowCredentials;
};

// https://www.w3.org/TR/webauthn-2/#iface-authentication-extensions-client-outputs
// No extension is implemented.
dictionary AuthenticationExtensionsClientOutputs {
};
