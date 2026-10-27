/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! CSS counters: <https://drafts.csswg.org/css-lists-3/#auto-numbering>
//!
//! Box tree construction runs in parallel, but a counter's value depends on every
//! element before it in tree order. So the values are worked out by one sequential
//! walk of the DOM after styling and before box tree construction. Only the values
//! that something reads are kept: the `list-item` counter of every `::marker`, and
//! the counters named by `counter()` and `counters()` in `::before` and `::after`.

use std::sync::LazyLock;

use layout_api::{LayoutElement, LayoutNode, PseudoElementChain};
use rustc_hash::{FxHashMap, FxHashSet};
use script::layout_dom::ServoLayoutNode;
use servo_arc::Arc as ServoArc;
use style::dom::OpaqueNode;
use style::properties::ComputedValues;
use style::selector_parser::PseudoElement;
use style::values::generics::counters::{Content, ContentItem};
use stylo_atoms::Atom;
use web_atoms::{local_name, ns};

type CounterKey = (OpaqueNode, PseudoElementChain);

/// The name of the counter that list items increment and markers show.
pub(crate) static LIST_ITEM: LazyLock<Atom> = LazyLock::new(|| Atom::from("list-item"));

/// The values of every counter read by a pseudo-element, outermost first.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct CounterValues {
    values: FxHashMap<CounterKey, Vec<(Atom, Vec<i32>)>>,
}

impl CounterValues {
    /// Compute the counter values for the DOM below `root`. Also returns the nodes
    /// whose boxes were built with values that differ from `previous`, together with
    /// their ancestors, as box tree construction reuses the boxes of some subtrees.
    /// Nodes that are not in `previous` are new, and get boxes built with these values.
    pub(crate) fn compute<'dom>(
        root: ServoLayoutNode<'dom>,
        previous: &CounterValues,
    ) -> (Self, Vec<ServoLayoutNode<'dom>>) {
        let mut walker = CounterWalker {
            previous,
            stack: Vec::new(),
            values: FxHashMap::default(),
            ancestors: Vec::new(),
            stale: Vec::new(),
            stale_seen: FxHashSet::default(),
        };
        walker.walk_element(root, 0);
        (
            Self {
                values: walker.values,
            },
            walker.stale,
        )
    }

    /// The values of counter `name` as seen by `node`, outermost first. A counter that
    /// was not computed has the single value 0, which is also what a counter that did
    /// not exist yet would be instantiated with.
    pub(crate) fn get(&self, node: &ServoLayoutNode<'_>, name: &Atom) -> &[i32] {
        self.values
            .get(&(node.opaque(), node.pseudo_element_chain()))
            .and_then(|counters| counters.iter().find(|(counter, _)| counter == name))
            .map_or(&[0], |(_, values)| values)
    }

}

struct Counter {
    name: Atom,
    value: i32,
    is_reversed: bool,
}

struct CounterWalker<'a, 'dom> {
    previous: &'a CounterValues,
    /// The counters in scope, outermost first. The counters created by an element stay
    /// in scope for its following siblings, so they are dropped when its parent ends.
    stack: Vec<Counter>,
    values: FxHashMap<CounterKey, Vec<(Atom, Vec<i32>)>>,
    /// The element being walked and its ancestors, outermost first.
    ancestors: Vec<ServoLayoutNode<'dom>>,
    /// The nodes whose boxes show values that changed, and their ancestors.
    stale: Vec<ServoLayoutNode<'dom>>,
    stale_seen: FxHashSet<OpaqueNode>,
}

impl<'dom> CounterWalker<'_, 'dom> {
    /// `siblings_start` is the stack length when the parent of `node` started its
    /// children: the counters above it were created by `node`'s previous siblings.
    fn walk_element(&mut self, node: ServoLayoutNode<'dom>, siblings_start: usize) {
        let Some(element) = node.as_element() else {
            return;
        };
        let Some(style_data) = element.style_data() else {
            return;
        };
        let (style, before, after) = {
            let data = style_data.element_data.borrow();
            if !data.has_styles() || data.styles.is_display_none() {
                return;
            }
            (
                data.styles.primary().clone(),
                data.styles.pseudos.get(&PseudoElement::Before).cloned(),
                data.styles.pseudos.get(&PseudoElement::After).cloned(),
            )
        };

        self.ancestors.push(node);
        let is_list_item = style.get_box().display.is_list_item();
        let html_list = HtmlList::for_node(node);
        self.apply_counter_properties(&style, is_list_item, html_list, node, siblings_start);

        let children_start = self.stack.len();
        if is_list_item {
            let marker = PseudoElementChain::unnested(PseudoElement::Marker);
            self.record(marker, LIST_ITEM.clone());
        }
        if let Some(before) = before {
            self.walk_pseudo_element(node, PseudoElement::Before, &before, children_start);
        }
        for child in node.flat_tree_children() {
            self.walk_element(child, children_start);
        }
        if let Some(after) = after {
            self.walk_pseudo_element(node, PseudoElement::After, &after, children_start);
        }
        self.stack.truncate(children_start);
        self.ancestors.pop();
    }

    fn walk_pseudo_element(
        &mut self,
        node: ServoLayoutNode<'_>,
        pseudo_element: PseudoElement,
        style: &ServoArc<ComputedValues>,
        siblings_start: usize,
    ) {
        let Content::Items(items) = &style.get_counters().content else {
            return;
        };
        if style.get_box().display.is_none() {
            return;
        }
        self.apply_counter_properties(style, false, HtmlList::None, node, siblings_start);

        let chain = PseudoElementChain::unnested(pseudo_element);
        for item in items.items.iter() {
            if let ContentItem::Counter(name, _) | ContentItem::Counters(name, _, _) = item {
                self.record(chain, name.0.clone());
            }
        }
    }

    /// <https://drafts.csswg.org/css-lists-3/#counters-without-boxes>: resets first,
    /// then increments, then sets.
    fn apply_counter_properties(
        &mut self,
        style: &ComputedValues,
        is_list_item: bool,
        html_list: HtmlList,
        node: ServoLayoutNode<'_>,
        siblings_start: usize,
    ) {
        let counters = style.get_counters();
        let list_item = LIST_ITEM.clone();

        for reset in counters.counter_reset.iter() {
            let value = if reset.is_reversed && reset.value == i32::MIN {
                // `reversed(name)` without a value starts at the number of list items.
                count_list_items(node) + 1
            } else {
                reset.value
            };
            self.instantiate(reset.name.0.clone(), value, reset.is_reversed, siblings_start);
        }
        // https://html.spec.whatwg.org/multipage/#lists: `ol`, `ul` and `menu` reset
        // `list-item`, taking `start` and `reversed` into account.
        if html_list != HtmlList::None &&
            !counters.counter_reset.iter().any(|reset| reset.name.0 == list_item)
        {
            let (value, is_reversed) = html_list.list_item_reset(node);
            self.instantiate(list_item.clone(), value, is_reversed, siblings_start);
        }

        for increment in counters.counter_increment.iter() {
            self.increment(increment.name.0.clone(), increment.value, siblings_start);
        }
        // https://drafts.csswg.org/css-lists-3/#list-item-counter
        if is_list_item &&
            !counters
                .counter_increment
                .iter()
                .any(|increment| increment.name.0 == list_item)
        {
            let by = match self.innermost(&list_item) {
                Some(counter) if self.stack[counter].is_reversed => -1,
                _ => 1,
            };
            self.increment(list_item.clone(), by, siblings_start);
        }

        for set in counters.counter_set.iter() {
            self.set(set.name.0.clone(), set.value, siblings_start);
        }
        // https://html.spec.whatwg.org/multipage/#ordinal-value
        if is_list_item &&
            !counters.counter_set.iter().any(|set| set.name.0 == list_item) &&
            let Some(value) = node
                .as_html_element()
                .filter(|element| element.local_name() == &local_name!("li"))
                .and_then(|element| {
                    element
                        .attribute_as_str(&ns!(), &local_name!("value"))
                        .and_then(parse_html_integer)
                })
        {
            self.set(list_item, value, siblings_start);
        }
    }

    fn innermost(&self, name: &Atom) -> Option<usize> {
        self.stack.iter().rposition(|counter| counter.name == *name)
    }

    /// <https://drafts.csswg.org/css-lists-3/#instantiate-counter>
    fn instantiate(&mut self, name: Atom, value: i32, is_reversed: bool, siblings_start: usize) {
        if let Some(index) = self.stack[siblings_start..]
            .iter()
            .rposition(|counter| counter.name == name)
        {
            self.stack.remove(siblings_start + index);
        }
        self.stack.push(Counter {
            name,
            value,
            is_reversed,
        });
    }

    fn innermost_or_instantiate(&mut self, name: Atom, siblings_start: usize) -> usize {
        match self.innermost(&name) {
            Some(index) => index,
            None => {
                self.instantiate(name, 0, false, siblings_start);
                self.stack.len() - 1
            },
        }
    }

    fn increment(&mut self, name: Atom, by: i32, siblings_start: usize) {
        let index = self.innermost_or_instantiate(name, siblings_start);
        let counter = &mut self.stack[index];
        counter.value = counter.value.saturating_add(by);
    }

    fn set(&mut self, name: Atom, value: i32, siblings_start: usize) {
        let index = self.innermost_or_instantiate(name, siblings_start);
        self.stack[index].value = value;
    }

    /// Keep the values of counter `name` for the given pseudo-element of the element
    /// being walked.
    fn record(&mut self, pseudo_element: PseudoElementChain, name: Atom) {
        let Some(node) = self.ancestors.last() else {
            return;
        };
        let key = (node.opaque(), pseudo_element);
        let values: Vec<i32> = self
            .stack
            .iter()
            .filter(|counter| counter.name == name)
            .map(|counter| counter.value)
            .collect();
        let changed = self.previous.values.get(&key).is_some_and(|previous| {
            !previous
                .iter()
                .any(|(counter, previous_values)| *counter == name && *previous_values == values)
        });
        let counters = self.values.entry(key).or_default();
        if !counters.iter().any(|(counter, _)| *counter == name) {
            counters.push((name, values));
        }
        if changed {
            self.mark_ancestors_stale();
        }
    }

    fn mark_ancestors_stale(&mut self) {
        for node in self.ancestors.iter().rev() {
            if !self.stale_seen.insert(node.opaque()) {
                // Its ancestors were marked along with it.
                break;
            }
            self.stale.push(*node);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum HtmlList {
    None,
    Ordered,
    Unordered,
}

impl HtmlList {
    fn for_node(node: ServoLayoutNode<'_>) -> Self {
        let Some(element) = node.as_html_element() else {
            return Self::None;
        };
        let name = element.local_name();
        if *name == local_name!("ol") {
            Self::Ordered
        } else if *name == local_name!("ul") || *name == local_name!("menu") {
            Self::Unordered
        } else {
            Self::None
        }
    }

    /// The initial value of the `list-item` counter, and whether it counts down.
    fn list_item_reset(self, node: ServoLayoutNode<'_>) -> (i32, bool) {
        let Some(element) = node.as_html_element() else {
            return (0, false);
        };
        if self != Self::Ordered {
            return (0, false);
        }
        let start = element
            .attribute_as_str(&ns!(), &local_name!("start"))
            .and_then(parse_html_integer);
        if element
            .attribute(&ns!(), &local_name!("reversed"))
            .is_some()
        {
            let start = start.unwrap_or_else(|| count_list_items(node));
            (start.saturating_add(1), true)
        } else {
            (start.unwrap_or(1).saturating_sub(1), false)
        }
    }
}

/// The number of list items that increment the `list-item` counter of `list`: its
/// rendered descendants with `display: list-item`, not counting those of nested lists.
fn count_list_items(list: ServoLayoutNode<'_>) -> i32 {
    let mut count = 0;
    for child in list.flat_tree_children() {
        let Some(style_data) = child.as_element().and_then(|element| element.style_data())
        else {
            continue;
        };
        let display = {
            let data = style_data.element_data.borrow();
            if !data.has_styles() {
                continue;
            }
            data.styles.primary().get_box().display
        };
        if display.is_none() {
            continue;
        }
        if display.is_list_item() {
            count += 1;
        }
        if HtmlList::for_node(child) == HtmlList::None {
            count += count_list_items(child);
        }
    }
    count
}

/// <https://html.spec.whatwg.org/multipage/#rules-for-parsing-integers>
fn parse_html_integer(input: &str) -> Option<i32> {
    let input = input.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let (negative, digits) = match input.as_bytes().first()? {
        b'-' => (true, &input[1..]),
        b'+' => (false, &input[1..]),
        _ => (false, input),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let value: i64 = digits[..end].parse().ok()?;
    let value = if negative { -value } else { value };
    i32::try_from(value).ok()
}
