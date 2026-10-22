/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// This interface is entirely internal to Servo, and should not be accessible to
// web pages.
[Exposed=DebuggerGlobalScope]
interface DebuggerParameterNamesEvent : Event {
    // The global that the console message was logged in.
    readonly attribute object global;
    readonly attribute object functionObject;
};

partial interface DebuggerGlobalScope {
    // A name is null for a parameter that is a destructuring pattern.
    undefined parameterNamesResult(DebuggerParameterNamesEvent event, sequence<DOMString?> names);
};
