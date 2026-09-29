/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix};
use js::rust::HandleObject;

use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::document::Document;
use crate::dom::node::Node;
use crate::dom::svg::svggraphicselement::SVGGraphicsElement;
use crate::dom::virtualmethods::VirtualMethods;

/// <https://svgwg.org/svg2-draft/linking.html#InterfaceSVGAElement>
///
/// The link itself is not followed yet: this exists so that the interface is on
/// the global and an `<a>` inside an `<svg>` reflects to it, which scripts test
/// for when they decide how to treat a click target.
#[dom_struct]
pub(crate) struct SVGAElement {
    svggraphicselement: SVGGraphicsElement,
}

impl SVGAElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGAElement {
        SVGAElement {
            svggraphicselement: SVGGraphicsElement::new_inherited(local_name, prefix, document),
        }
    }

    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGAElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGAElement::new_inherited(local_name, prefix, document)),
            document,
            proto,
        )
    }
}

impl VirtualMethods for SVGAElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGGraphicsElement>() as &dyn VirtualMethods)
    }
}
