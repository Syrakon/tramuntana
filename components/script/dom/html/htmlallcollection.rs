/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{local_name, ns};
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::HTMLAllCollectionBinding::HTMLAllCollectionMethods;
use crate::dom::bindings::codegen::Bindings::HTMLCollectionBinding::HTMLCollectionMethods;
use crate::dom::bindings::codegen::UnionTypes::HTMLCollectionOrElement;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::document::Document;
use crate::dom::element::Element;
use crate::dom::html::htmlcollection::{CollectionFilter, HTMLCollection};
use crate::dom::node::Node;
use crate::dom::window::Window;

/// <https://html.spec.whatwg.org/multipage/#htmlallcollection>
///
/// The object behind `document.all`. Its wrapper is a proxy whose class carries
/// `JSCLASS_EMULATES_UNDEFINED`, which gives it the [[IsHTMLDDA]] slot: `typeof` reports
/// "undefined", it is falsy and it compares loosely equal to `undefined` and `null`, but not
/// strictly equal to them.
#[dom_struct]
pub(crate) struct HTMLAllCollection {
    reflector_: Reflector,
    /// Every element in the document, in tree order.
    collection: Dom<HTMLCollection>,
}

impl HTMLAllCollection {
    fn new_inherited(collection: &HTMLCollection) -> HTMLAllCollection {
        HTMLAllCollection {
            reflector_: Reflector::new(),
            collection: Dom::from_ref(collection),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        document: &Document,
    ) -> DomRoot<HTMLAllCollection> {
        let collection =
            HTMLCollection::new_with_filter_fn(cx, window, document.upcast(), |_, _| true);
        reflect_dom_object_with_cx(
            Box::new(HTMLAllCollection::new_inherited(&collection)),
            window,
            cx,
        )
    }

    /// <https://html.spec.whatwg.org/multipage/#concept-get-all-named>
    fn all_named_elements(
        &self,
        cx: &mut JSContext,
        name: DOMString,
    ) -> Option<HTMLCollectionOrElement> {
        // Step 1. If name is the empty string, return null.
        if name.is_empty() {
            return None;
        }
        let name = Atom::from(name);

        // Step 2. Let subCollection be an HTMLCollection object rooted at the same Document as
        // the HTMLAllCollection object, whose filter matches only elements that are either
        // "all"-named elements with a name attribute equal to name, or elements with an ID equal
        // to name.
        let mut matches = self
            .collection
            .elements_iter()
            .filter(|element| matches_all_name(element, &name));
        let first = matches.next()?;

        // Step 3. If there is exactly one element in subCollection, then return that element.
        if matches.next().is_none() {
            return Some(HTMLCollectionOrElement::Element(first));
        }

        // Step 5. Otherwise, return subCollection.
        #[derive(JSTraceable, MallocSizeOf)]
        struct AllNamedFilter {
            #[no_trace]
            name: Atom,
        }
        impl CollectionFilter for AllNamedFilter {
            fn filter(&self, element: &Element, _root: &Node) -> bool {
                matches_all_name(element, &self.name)
            }
        }
        let root = first.upcast::<Node>().owner_doc();
        let window = root.window();
        Some(HTMLCollectionOrElement::HTMLCollection(
            HTMLCollection::create(
                cx,
                window,
                root.upcast(),
                Box::new(AllNamedFilter { name }),
            ),
        ))
    }
}

/// <https://html.spec.whatwg.org/multipage/#all-named-elements>
fn is_all_named(element: &Element) -> bool {
    *element.namespace() == ns!(html) &&
        matches!(
            *element.local_name(),
            local_name!("a") |
                local_name!("button") |
                local_name!("embed") |
                local_name!("form") |
                local_name!("frame") |
                local_name!("frameset") |
                local_name!("iframe") |
                local_name!("img") |
                local_name!("input") |
                local_name!("map") |
                local_name!("meta") |
                local_name!("object") |
                local_name!("select") |
                local_name!("textarea")
        )
}

/// Whether `element` belongs to the "all"-named sub-collection for `name`: its ID is `name`, or
/// it is an "all"-named element whose name attribute is `name`.
fn matches_all_name(element: &Element, name: &Atom) -> bool {
    element.get_id().is_some_and(|id| id == *name) ||
        (is_all_named(element) && element.get_name().is_some_and(|n| n == *name))
}

/// <https://tc39.es/ecma262/#array-index>: the canonical decimal form of an integer in
/// [0, 2^32 - 2].
fn parse_array_index(value: &str) -> Option<u32> {
    let index = value.parse::<u32>().ok()?;
    (index != u32::MAX && index.to_string() == value).then_some(index)
}

impl HTMLAllCollectionMethods<crate::DomTypeHolder> for HTMLAllCollection {
    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-length>
    fn Length(&self) -> u32 {
        self.collection.Length()
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-item>
    fn IndexedGetter(&self, index: u32) -> Option<DomRoot<Element>> {
        self.collection.Item(index)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-nameditem>
    fn NamedItem(&self, cx: &mut JSContext, name: DOMString) -> Option<HTMLCollectionOrElement> {
        self.all_named_elements(cx, name)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-nameditem>
    fn NamedGetter(&self, cx: &mut JSContext, name: DOMString) -> Option<HTMLCollectionOrElement> {
        self.all_named_elements(cx, name)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-htmlallcollection-item>
    fn Item(
        &self,
        cx: &mut JSContext,
        name_or_index: Option<DOMString>,
    ) -> Option<HTMLCollectionOrElement> {
        // Step 1. If nameOrIndex was not provided, return null.
        let name_or_index = name_or_index?;

        // Step 2. Return the result of getting the "all"-indexed or named element(s) from this,
        // given nameOrIndex.
        // <https://html.spec.whatwg.org/multipage/#concept-get-all-indexed-or-named>
        if let Some(index) = parse_array_index(&name_or_index.str()) {
            return self
                .collection
                .Item(index)
                .map(HTMLCollectionOrElement::Element);
        }
        self.all_named_elements(cx, name_or_index)
    }

    /// <https://html.spec.whatwg.org/multipage/#the-htmlallcollection-interface:supported-property-names>
    fn SupportedPropertyNames(&self) -> Vec<DOMString> {
        let mut names: Vec<DOMString> = vec![];
        let mut push = |name: &Atom| {
            if name.is_empty() {
                return;
            }
            let name = DOMString::from(&**name);
            if !names.contains(&name) {
                names.push(name);
            }
        };
        for element in self.collection.elements_iter() {
            if let Some(id) = element.get_id() {
                push(&id);
            }
            if is_all_named(&element) &&
                let Some(name) = element.get_name()
            {
                push(&name);
            }
        }
        names
    }
}
