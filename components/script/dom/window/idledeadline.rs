/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::IdleDeadlineBinding::IdleDeadlineMethods;
use crate::dom::bindings::codegen::Bindings::PerformanceBinding::PerformanceMethods;
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::window::Window;

/// <https://w3c.github.io/requestidlecallback/#the-idledeadline-interface>
#[dom_struct]
pub(crate) struct IdleDeadline {
    reflector_: Reflector,
    window: Dom<Window>,
    /// The end of the idle period, on the clock of `performance.now()`.
    deadline: f64,
    did_timeout: bool,
}

impl IdleDeadline {
    fn new_inherited(window: &Window, deadline: f64, did_timeout: bool) -> IdleDeadline {
        IdleDeadline {
            reflector_: Reflector::new(),
            window: Dom::from_ref(window),
            deadline,
            did_timeout,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        deadline: f64,
        did_timeout: bool,
    ) -> DomRoot<IdleDeadline> {
        reflect_dom_object(
            cx,
            Box::new(IdleDeadline::new_inherited(window, deadline, did_timeout)),
            window,
        )
    }
}

impl IdleDeadlineMethods<crate::DomTypeHolder> for IdleDeadline {
    /// <https://w3c.github.io/requestidlecallback/#dom-idledeadline-timeremaining>
    fn TimeRemaining(&self, cx: &mut JSContext) -> Finite<f64> {
        let now = *self.window.Performance(cx).Now();
        Finite::wrap((self.deadline - now).max(0.0))
    }

    /// <https://w3c.github.io/requestidlecallback/#dom-idledeadline-didtimeout>
    fn DidTimeout(&self) -> bool {
        self.did_timeout
    }
}
