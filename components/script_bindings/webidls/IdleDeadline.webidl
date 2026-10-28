/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/requestidlecallback/

partial interface Window {
  unsigned long requestIdleCallback(IdleRequestCallback callback,
                                    optional IdleRequestOptions options = {});
  undefined cancelIdleCallback(unsigned long handle);
};

dictionary IdleRequestOptions {
  unsigned long timeout;
};

[Exposed=Window]
interface IdleDeadline {
  DOMHighResTimeStamp timeRemaining();
  readonly attribute boolean didTimeout;
};

callback IdleRequestCallback = undefined (IdleDeadline deadline);
