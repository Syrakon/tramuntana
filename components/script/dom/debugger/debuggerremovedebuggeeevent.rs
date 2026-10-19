/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::conversions::ToJSValConvertible;
use js::jsapi::{Heap, JSObject, Value};
use js::rust::MutableHandleObject;
use script_bindings::reflector::{DomObject, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::DebuggerRemoveDebuggeeEventBinding::DebuggerRemoveDebuggeeEventMethods;
use crate::dom::bindings::codegen::Bindings::EventBinding::Event_Binding::EventMethods;
use crate::dom::bindings::root::DomRoot;
use crate::dom::event::Event;
use crate::dom::types::GlobalScope;

#[dom_struct]
/// Event for Rust → JS calls in [`crate::dom::debugger::DebuggerGlobalScope`].
pub(crate) struct DebuggerRemoveDebuggeeEvent {
    event: Event,
    #[ignore_malloc_size_of = "Measured by the JS engine"]
    global: Heap<*mut JSObject>,
}

impl DebuggerRemoveDebuggeeEvent {
    pub(crate) fn new(
        cx: &mut JSContext,
        debugger_global: &GlobalScope,
        global: &GlobalScope,
    ) -> DomRoot<Self> {
        let result = Box::new(Self {
            event: Event::new_inherited(),
            global: Heap::default(),
        });
        let result = reflect_dom_object(cx, result, debugger_global);
        result
            .event
            .init_event("removeDebuggee".into(), false, false);

        // Wrapped into the debugger realm, as in `DebuggerAddDebuggeeEvent::new`.
        rooted!(&in(cx) let mut wrapped_global: Value);
        global.reflector().to_jsval(cx, wrapped_global.handle_mut());
        result.global.set(wrapped_global.to_object());

        result
    }
}

impl DebuggerRemoveDebuggeeEventMethods<crate::DomTypeHolder> for DebuggerRemoveDebuggeeEvent {
    // check-tidy: no specs after this line
    fn Global(&self, mut return_value: MutableHandleObject) {
        return_value.set(self.global.get());
    }

    fn IsTrusted(&self) -> bool {
        self.event.IsTrusted()
    }
}
