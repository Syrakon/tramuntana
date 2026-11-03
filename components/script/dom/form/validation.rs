/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use embedder_traits::{MissingValueKind, ValidationProblem};
use js::context::{JSContext, NoGC};
use html5ever::local_name;

use crate::dom::bindings::codegen::Bindings::HTMLElementBinding::HTMLElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLInputElementBinding::HTMLInputElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLTextAreaElementBinding::HTMLTextAreaElementMethods;
use crate::dom::bindings::codegen::Bindings::HTMLOrSVGElementBinding::FocusOptions;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::element::Element;
use crate::dom::eventtarget::EventTarget;
use crate::dom::html::htmldatalistelement::HTMLDataListElement;
use crate::dom::html::form_controls::input_type::InputType;
use crate::dom::html::htmlelement::HTMLElement;
use crate::dom::html::form_controls::htmlinputelement::HTMLInputElement;
use crate::dom::html::form_controls::htmlselectelement::HTMLSelectElement;
use crate::dom::html::form_controls::htmltextareaelement::HTMLTextAreaElement;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::validitystate::{ValidationFlags, ValidityState};

/// Trait for elements with constraint validation support
pub(crate) trait Validatable {
    fn as_element(&self) -> &Element;

    /// <https://html.spec.whatwg.org/multipage/#dom-cva-validity>
    fn validity_state(&self, cx: &mut JSContext) -> DomRoot<ValidityState>;

    /// <https://html.spec.whatwg.org/multipage/#candidate-for-constraint-validation>
    fn is_instance_validatable(&self, no_gc: &NoGC) -> bool;

    // Check if element satisfies its constraints, excluding custom errors
    fn perform_validation(
        &self,
        _cx: &mut JSContext,
        _validate_flags: ValidationFlags,
    ) -> ValidationFlags {
        ValidationFlags::empty()
    }

    /// <https://html.spec.whatwg.org/multipage/#concept-fv-valid>
    fn satisfies_constraints(&self, cx: &mut JSContext) -> bool {
        self.validity_state(cx).invalid_flags().is_empty()
    }

    /// <https://html.spec.whatwg.org/multipage/#check-validity-steps>
    fn check_validity(&self, cx: &mut JSContext) -> bool {
        if self.is_instance_validatable(cx.no_gc()) && !self.satisfies_constraints(cx) {
            self.as_element()
                .upcast::<EventTarget>()
                .fire_cancelable_event(cx, atom!("invalid"));
            false
        } else {
            true
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#report-validity-steps>
    fn report_validity(&self, cx: &mut JSContext) -> bool {
        // Step 1.
        if !self.is_instance_validatable(cx.no_gc()) {
            return true;
        }

        if self.satisfies_constraints(cx) {
            return true;
        }

        // Step 1.1: Let `report` be the result of firing an event named invalid at element,
        // with the cancelable attribute initialized to true.
        let report = self
            .as_element()
            .upcast::<EventTarget>()
            .fire_cancelable_event(cx, atom!("invalid"));

        // Step 1.2. If `report` is true, for the element,
        // report the problem, run focusing steps, scroll into view.
        if report {
            if let Some(html_elem) = self.as_element().downcast::<HTMLElement>() {
                // Run focusing steps and scroll into view.
                html_elem.Focus(cx, &FocusOptions::default());
            }
            report_validation_problem(cx, self.as_element());
        }

        // Step 1.3.
        false
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-cva-validationmessage>
    fn validation_message(&self, cx: &mut JSContext) -> DOMString {
        if self.is_instance_validatable(cx.no_gc()) {
            let flags = self.validity_state(cx).invalid_flags();
            validation_message_for_flags(&self.validity_state(cx), flags)
        } else {
            DOMString::new()
        }
    }
}

/// Show the user, next to `element`, why it fails its constraints. Called once the element
/// has been focused and scrolled into view, so the message points at where it now is.
pub(crate) fn report_validation_problem(cx: &mut JSContext, element: &Element) {
    let Some(validatable) = element.as_maybe_validatable() else {
        return;
    };
    let state = validatable.validity_state(cx);
    let Some(problem) = validation_problem(element, &state, state.invalid_flags()) else {
        return;
    };
    element
        .owner_document()
        .embedder_controls()
        .show_validation_message(element, problem);
}

/// The [`ValidationProblem`] the user is told about for `flags`: the first of them in the
/// order of [`ValidationProblem`], with what the element's attributes say about it.
fn validation_problem(
    element: &Element,
    state: &ValidityState,
    flags: ValidationFlags,
) -> Option<ValidationProblem> {
    let input = element.downcast::<HTMLInputElement>();
    let textarea = element.downcast::<HTMLTextAreaElement>();
    let attribute = |name| {
        let value = element.get_string_attribute(&name).to_string();
        (!value.is_empty()).then_some(value)
    };
    // The length of the value and the limit an attribute sets on it, as the page counts.
    let length_and_limit = |limit_of: fn(&HTMLInputElement) -> i32,
                            area_limit_of: fn(&HTMLTextAreaElement) -> i32| {
        let (length, limit) = if let Some(input) = input {
            (
                input.Value().to_string().encode_utf16().count(),
                limit_of(input),
            )
        } else if let Some(textarea) = textarea {
            (textarea.TextLength() as usize, area_limit_of(textarea))
        } else {
            return None;
        };
        Some((
            u32::try_from(length).unwrap_or(u32::MAX),
            u32::try_from(limit).ok()?,
        ))
    };

    if flags.contains(ValidationFlags::CUSTOM_ERROR) {
        return Some(ValidationProblem::Custom(
            state.custom_error_message().to_string(),
        ));
    }
    if flags.contains(ValidationFlags::VALUE_MISSING) {
        let kind = match input.map(|input| input.input_type()).as_deref() {
            Some(InputType::Checkbox(..)) => MissingValueKind::Checkbox,
            Some(InputType::Radio(..)) => MissingValueKind::Radio,
            Some(InputType::File(..)) => MissingValueKind::File,
            _ if element.is::<HTMLSelectElement>() => MissingValueKind::Select,
            _ => MissingValueKind::Text,
        };
        return Some(ValidationProblem::ValueMissing(kind));
    }
    if flags.contains(ValidationFlags::TYPE_MISMATCH) {
        return Some(match input.map(|input| input.input_type()).as_deref() {
            Some(InputType::Email(..)) => ValidationProblem::NotAnEmail,
            Some(InputType::Url(..)) => ValidationProblem::NotAUrl,
            _ => ValidationProblem::BadInput,
        });
    }
    if flags.contains(ValidationFlags::BAD_INPUT) {
        return Some(ValidationProblem::BadInput);
    }
    if flags.contains(ValidationFlags::PATTERN_MISMATCH) {
        return Some(ValidationProblem::PatternMismatch {
            title: attribute(local_name!("title")),
        });
    }
    if flags.contains(ValidationFlags::TOO_LONG) {
        if let Some((length, max)) = length_and_limit(
            |input| input.MaxLength(),
            |textarea| textarea.MaxLength(),
        ) {
            return Some(ValidationProblem::TooLong { max, length });
        }
    }
    if flags.contains(ValidationFlags::TOO_SHORT) {
        if let Some((length, min)) = length_and_limit(
            |input| input.MinLength(),
            |textarea| textarea.MinLength(),
        ) {
            return Some(ValidationProblem::TooShort { min, length });
        }
    }
    if flags.contains(ValidationFlags::RANGE_UNDERFLOW) {
        if let Some(min) = attribute(local_name!("min")) {
            return Some(ValidationProblem::RangeUnderflow { min });
        }
    }
    if flags.contains(ValidationFlags::RANGE_OVERFLOW) {
        if let Some(max) = attribute(local_name!("max")) {
            return Some(ValidationProblem::RangeOverflow { max });
        }
    }
    if flags.contains(ValidationFlags::STEP_MISMATCH) {
        return Some(ValidationProblem::StepMismatch);
    }
    // A limit whose attribute could not be read back still leaves the value wrong.
    (!flags.is_empty()).then_some(ValidationProblem::BadInput)
}

/// <https://html.spec.whatwg.org/multipage/#the-datalist-element%3Abarred-from-constraint-validation>
pub(crate) fn is_barred_by_datalist_ancestor(no_gc: &NoGC, elem: &Node) -> bool {
    elem.upcast::<Node>()
        .ancestors_unrooted(no_gc)
        .any(|node| node.is::<HTMLDataListElement>())
}

// Get message for given validation flags or custom error message
fn validation_message_for_flags(state: &ValidityState, failed_flags: ValidationFlags) -> DOMString {
    if failed_flags.contains(ValidationFlags::CUSTOM_ERROR) {
        state.custom_error_message().clone()
    } else {
        DOMString::from(failed_flags.to_string())
    }
}
