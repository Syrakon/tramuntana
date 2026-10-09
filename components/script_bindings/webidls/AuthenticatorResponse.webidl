/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://www.w3.org/TR/webauthn-2/#iface-authenticatorresponse
 */

// https://www.w3.org/TR/webauthn-2/#iface-authenticatorresponse
[Pref="dom_credential_management_enabled", Exposed=Window, SecureContext]
interface AuthenticatorResponse {
  [SameObject, Throws] readonly attribute ArrayBuffer clientDataJSON;
};
