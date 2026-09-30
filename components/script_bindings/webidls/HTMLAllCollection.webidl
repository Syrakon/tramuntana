/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://html.spec.whatwg.org/multipage/#htmlallcollection
[Exposed=Window, LegacyUnenumerableNamedProperties]
interface HTMLAllCollection {
  readonly attribute unsigned long length;
  getter Element (unsigned long index);
  getter (HTMLCollection or Element)? namedItem(DOMString name);
  (HTMLCollection or Element)? item(optional DOMString nameOrIndex);

  // Note: HTMLAllCollection objects have a custom [[Call]] internal method and an [[IsHTMLDDA]]
  // internal slot. [[IsHTMLDDA]] is provided by the JSCLASS_EMULATES_UNDEFINED flag on the proxy
  // class (see CGWrapMethod in codegen.py); the legacy caller is not implemented yet.
};
