/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::hash::{DefaultHasher, Hash, Hasher};

use atomic_refcell::AtomicRefCell;
use base64::Engine as _;
use cssparser::Parser;
use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name, ns};
use js::context::{JSContext, NoGC};
use js::rust::HandleObject;
use layout_api::SVGElementData;
use servo_url::ServoUrl;
use style::attr::AttrValue;
use style::parser::ParserContext;
use style::properties::ComputedValues;
use style::stylesheets::Origin;
use style::values::computed::SVGPaint;
use style::values::generics::svg::SVGPaintKind;
use style::values::specified::LengthPercentage;
use style_traits::{ParsingMode, ToCss};
use uuid::Uuid;
use xml5ever::serialize::TraversalScope;

use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, LayoutDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::document::Document;
use crate::dom::element::attributes::storage::AttrRef;
use crate::dom::element::{AttributeMutation, Element};
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::node::{
    ChildrenMutation, CloneChildrenFlag, Node, NodeDamage, NodeTraits, UnbindContext,
};
use crate::dom::svg::svggraphicselement::SVGGraphicsElement;

#[dom_struct]
pub(crate) struct SVGSVGElement {
    svggraphicselement: SVGGraphicsElement,
    #[no_trace]
    uuid: Uuid,
    // The XML source of subtree rooted at this SVG element, serialized into
    // a base64 encoded `data:` url. This is cached to avoid recomputation
    // on each layout and must be invalidated when the subtree changes.
    #[no_trace]
    cached_serialized_data_url: AtomicRefCell<Option<Result<ServoUrl, ()>>>,
    // A hash of the computed paint baked into `cached_serialized_data_url`, so
    // that a restyle which leaves it unchanged does not serialize again.
    cached_paint_key: Cell<u64>,
}

impl SVGSVGElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGSVGElement {
        SVGSVGElement {
            svggraphicselement: SVGGraphicsElement::new_inherited(local_name, prefix, document),
            uuid: Uuid::new_v4(),
            cached_serialized_data_url: Default::default(),
            cached_paint_key: Cell::new(0),
        }
    }

    #[cfg_attr(crown, allow(crown::unrooted_must_root))]
    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGSVGElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGSVGElement::new_inherited(local_name, prefix, document)),
            document,
            proto,
        )
    }

    /// Serialize the subtree again if its computed paint changed since it was last
    /// serialized, as when a page switches to a dark theme after load. Layout asks
    /// for this whenever the `<svg>` or one of its descendants is restyled.
    pub(crate) fn serialize_and_cache_subtree_if_needed(&self, cx: &mut JSContext) {
        let is_cached = self.cached_serialized_data_url.borrow().is_some();
        if is_cached && self.cached_paint_key.get() == paint_key(&self.computed_paint()) {
            return;
        }
        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
        self.serialize_and_cache_subtree(cx);
    }

    /// The computed paint of each element of the subtree, in tree order.
    ///
    /// The serialized markup carries no CSS context: resvg sees neither the
    /// page's style sheets nor the inherited `color`, so `currentColor` comes
    /// out black and `fill` set by a rule never applies. Icon sets paint this
    /// way all the time, and black icons vanish on dark pages. The computed
    /// values already account for every rule, inline style and presentation
    /// attribute, so writing them on the serialized copy makes resvg paint
    /// what the page styled.
    fn computed_paint(&self) -> Vec<Option<String>> {
        self.upcast::<Node>()
            .traverse_preorder(ShadowIncluding::No)
            .filter_map(DomRoot::downcast::<Element>)
            .map(|element| {
                let is_root = &*element == self.upcast::<Element>();
                element
                    .style_without_reflow()
                    .map(|style| paint_declarations(&style, is_root))
            })
            .collect()
    }

    pub(crate) fn serialize_and_cache_subtree(&self, cx: &mut js::context::JSContext) {
        let document_fragment = self.owner_document().CreateDocumentFragment(cx);
        let cloned_node = Node::clone(
            cx,
            self.upcast(),
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );
        if document_fragment
            .upcast::<Node>()
            .AppendChild(cx, &cloned_node)
            .is_err()
        {
            error!("Unable to clone SVG tree");
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return;
        }

        // This runs while `Window::reflow` handles the results of a reflow, so
        // the styles are current. Asking for a new reflow here re-enters
        // `Window::reflow`, which finds this element pending again and recurses
        // until the stack overflows.
        //
        // The clone has the shape of the original, so its elements pair up in
        // tree order. This has to happen before `<use>` targets are appended.
        let paint = self.computed_paint();
        let cloned_elements: Vec<_> = cloned_node
            .traverse_preorder(ShadowIncluding::No)
            .filter_map(DomRoot::downcast::<Element>)
            .collect();
        for (element, declarations) in cloned_elements.iter().zip(&paint) {
            let Some(declarations) = declarations else {
                continue;
            };
            // resvg drops a `style` that starts with an empty declaration.
            let mut style = String::from(element.get_string_attribute(&local_name!("style")));
            if !style.trim().is_empty() {
                style.push(';');
            }
            style.push_str(declarations);
            element.set_string_attribute(cx, &local_name!("style"), style.into());
        }
        self.cached_paint_key.set(paint_key(&paint));

        self.process_use_elements(cx, &cloned_node);

        let Ok(xml_source) = cloned_node.xml_serialize(TraversalScope::IncludeNode) else {
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return;
        };

        let xml_source: String = xml_source.into();
        let base64_encoded_source = base64::engine::general_purpose::STANDARD.encode(xml_source);
        let data_url = format!("data:image/svg+xml;base64,{base64_encoded_source}");
        match ServoUrl::parse(&data_url) {
            Ok(url) => *self.cached_serialized_data_url.borrow_mut() = Some(Ok(url)),
            Err(error) => error!("Unable to parse serialized SVG data url: {error}"),
        };
    }

    fn process_use_elements(&self, cx: &mut JSContext, root_node: &Node) {
        for node in root_node.traverse_preorder(ShadowIncluding::No) {
            if let Some(element) = node.downcast::<Element>() &&
                element.local_name() == &local_name!("use")
            {
                self.process_single_use_element(cx, element, root_node)
            }
        }
    }

    fn process_single_use_element(
        &self,
        cx: &mut JSContext,
        use_element: &Element,
        root_node: &Node,
    ) {
        let href = use_element.get_string_attribute(&local_name!("href"));
        let Some(id_string) = href.str().strip_prefix("#").map(DOMString::from) else {
            return;
        };

        let document = self.upcast::<Node>().owner_doc();
        let Some(referenced_element) = document.GetElementById(cx, id_string) else {
            return;
        };
        let referenced_node = referenced_element.upcast::<Node>();

        // Don't use this node if it doesn't have an `<svg>` ancestor.
        if !referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| ancestor.is::<SVGSVGElement>())
        {
            return;
        };

        // Don't use this node if it already exists within the same `<svg>` element.
        if referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| *ancestor == self.upcast())
        {
            return;
        };

        let cloned_node = Node::clone(
            cx,
            referenced_node,
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );
        let _ = root_node.AppendChild(cx, &cloned_node);
    }

    pub(crate) fn invalidate_cached_serialized_subtree_and_rasterization_result(
        &self,
        no_gc: &NoGC,
    ) {
        let owner_window = self.owner_window();
        owner_window
            .image_cache()
            .evict_rasterized_image(&self.uuid);
        if let Some(Ok(url)) = &*self.cached_serialized_data_url.borrow() {
            owner_window.layout_mut().remove_cached_image(url);
            owner_window.image_cache().evict_completed_image(
                url,
                owner_window.origin().immutable(),
                &None,
            );
        }

        *self.cached_serialized_data_url.borrow_mut() = None;
        self.upcast::<Node>().dirty(no_gc, NodeDamage::Other);
    }
}

/// The declarations that make resvg paint an element as the page computed it.
/// The root's `opacity` is left out because the page already applies it when
/// it paints the rasterized image.
fn paint_declarations(style: &ComputedValues, is_root: bool) -> String {
    let color = &style.get_inherited_text().color;
    let svg = style.get_inherited_svg();
    let mut declarations = format!("color:{}", color.to_css_string());
    for (name, paint) in [("fill", &svg.fill), ("stroke", &svg.stroke)] {
        if let Some(paint) = paint_value(paint, color) {
            declarations.push_str(&format!(";{name}:{paint}"));
        }
    }
    declarations.push_str(&format!(
        ";fill-opacity:{};stroke-opacity:{};stroke-width:{}",
        svg.fill_opacity.to_css_string(),
        svg.stroke_opacity.to_css_string(),
        svg.stroke_width.to_css_string(),
    ));
    if !is_root {
        declarations.push_str(&format!(";opacity:{}", style.get_effects().opacity));
    }
    declarations
}

/// A paint resvg can parse. Colours are resolved, because resvg only knows
/// `currentColor` with that exact casing. Paint servers are left to the markup:
/// their computed `url()` is absolute and would not resolve inside a `data:` URL.
fn paint_value(paint: &SVGPaint, color: &style::color::AbsoluteColor) -> Option<String> {
    match &paint.kind {
        SVGPaintKind::None => Some("none".to_owned()),
        SVGPaintKind::Color(paint_color) => {
            Some(paint_color.resolve_to_absolute(color).to_css_string())
        },
        SVGPaintKind::PaintServer(_) |
        SVGPaintKind::ContextFill |
        SVGPaintKind::ContextStroke => None,
    }
}

fn paint_key(paint: &[Option<String>]) -> u64 {
    let mut hasher = DefaultHasher::new();
    paint.hash(&mut hasher);
    hasher.finish()
}

impl<'dom> LayoutDom<'dom, SVGSVGElement> {
    pub(crate) fn data(self) -> SVGElementData<'dom> {
        let svg_id = self.unsafe_get().uuid;
        let element = self.upcast::<Element>();
        let width = element.get_attr_for_layout(&ns!(), &local_name!("width"));
        let height = element.get_attr_for_layout(&ns!(), &local_name!("height"));
        let view_box = element.get_attr_for_layout(&ns!(), &local_name!("viewBox"));
        SVGElementData {
            source: self
                .unsafe_get()
                .cached_serialized_data_url
                .borrow()
                .clone(),
            width,
            height,
            view_box,
            svg_id,
        }
    }
}

impl VirtualMethods for SVGSVGElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGGraphicsElement>() as &dyn VirtualMethods)
    }

    fn attribute_mutated(
        &self,
        cx: &mut js::context::JSContext,
        attr: AttrRef<'_>,
        mutation: AttributeMutation,
    ) {
        self.super_type()
            .unwrap()
            .attribute_mutated(cx, attr, mutation);

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }

    fn attribute_affects_presentational_hints(&self, attr: AttrRef<'_>) -> bool {
        match attr.local_name() {
            &local_name!("width") | &local_name!("height") => true,
            _ => self
                .super_type()
                .unwrap()
                .attribute_affects_presentational_hints(attr),
        }
    }

    fn parse_plain_attribute(&self, name: &LocalName, value: DOMString) -> AttrValue {
        match *name {
            local_name!("width") | local_name!("height") => {
                let value = &value.str();
                let parser = &mut Parser::new(value);
                let doc = self.owner_document();
                let url = doc.url().into_url().into();
                let context = ParserContext::new(
                    Origin::Author,
                    &url,
                    None,
                    ParsingMode::ALLOW_UNITLESS_LENGTH,
                    doc.quirks_mode(),
                    /* namespaces = */ Default::default(),
                    None,
                    None,
                    /* attr_taint = */ Default::default(),
                );
                let val = LengthPercentage::parse_quirky(
                    &context,
                    parser,
                    style::values::specified::AllowQuirks::Always,
                );
                AttrValue::LengthPercentage(value.to_string(), val.ok())
            },
            _ => self
                .super_type()
                .unwrap()
                .parse_plain_attribute(name, value),
        }
    }

    fn children_changed(&self, cx: &mut JSContext, mutation: &ChildrenMutation) {
        if let Some(super_type) = self.super_type() {
            super_type.children_changed(cx, mutation);
        }

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }

    fn unbind_from_tree(&self, cx: &mut js::context::JSContext, context: &UnbindContext<'_>) {
        if let Some(s) = self.super_type() {
            s.unbind_from_tree(cx, context);
        }

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }
}
