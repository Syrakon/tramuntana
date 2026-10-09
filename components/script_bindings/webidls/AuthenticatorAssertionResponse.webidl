/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://www.w3.org/TR/webauthn-2/#iface-authenticatorassertionresponse
 */

// https://www.w3.org/TR/webauthn-2/#iface-authenticatorassertionresponse
[Pref="dom_credential_management_enabled", Exposed=Window, SecureContext]
interface AuthenticatorAssertionResponse : AuthenticatorResponse {
  [SameObject, Throws] readonly attribute ArrayBuffer authenticatorData;
  [SameObject, Throws] readonly attribute ArrayBuffer signature;
  [SameObject, Throws] readonly attribute ArrayBuffer? userHandle;
};
