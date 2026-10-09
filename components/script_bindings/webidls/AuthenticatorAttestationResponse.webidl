/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://www.w3.org/TR/webauthn-2/#iface-authenticatorattestationresponse
 */

// https://www.w3.org/TR/webauthn-2/#iface-authenticatorattestationresponse
[Pref="dom_credential_management_enabled", Exposed=Window, SecureContext]
interface AuthenticatorAttestationResponse : AuthenticatorResponse {
  [SameObject, Throws] readonly attribute ArrayBuffer attestationObject;
  sequence<DOMString> getTransports();
  [Throws] ArrayBuffer getAuthenticatorData();
  [Throws] ArrayBuffer? getPublicKey();
  long getPublicKeyAlgorithm();
};
