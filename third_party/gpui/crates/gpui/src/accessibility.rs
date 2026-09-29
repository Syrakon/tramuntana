//! Opt-in accessibility metadata for assistive technology.
//!
//! GPUI's element tree is presentational: a `div` with a click handler carries
//! nothing that says "button", and the text inside it is a glyph run, not a
//! label. Screen readers need both, so elements declare them explicitly with
//! [`InteractiveElement::a11y`](crate::InteractiveElement::a11y).
//!
//! Every element that declares a role emits an [`AccessNode`] during paint,
//! collected into the frame in paint order. After the frame is presented the
//! window hands that list to [`PlatformWindow::update_accessibility`], which
//! converts it to whatever the platform's accessibility API wants — an AT-SPI
//! tree on Linux, nothing at all on a platform without a backend yet.
//!
//! The tree is therefore as complete as the annotations are: an element that
//! declares no role does not exist as far as assistive technology is
//! concerned. That is the trade for not guessing semantics from layout.
//!
//! [`PlatformWindow::update_accessibility`]: crate::PlatformWindow::update_accessibility

use std::hash::{Hash, Hasher};

use crate::{Bounds, ElementId, FocusId, Pixels, SharedString};

/// What a control *is*, as far as assistive technology is concerned.
///
/// A deliberately small set: every variant has to map cleanly onto each
/// platform backend, and browser chrome needs few roles. Anything without a
/// better fit is a [`AccessRole::Group`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AccessRole {
    /// A control that performs an action when pressed.
    Button,
    /// A control with a checked / unchecked state.
    CheckBox,
    /// A container with no behaviour of its own, grouping the elements inside.
    Group,
    /// Text that names or describes something else.
    Label,
    /// A control that navigates somewhere.
    Link,
    /// A container whose children are items of one kind.
    List,
    /// One item inside a [`AccessRole::List`].
    ListItem,
    /// A menu of commands.
    Menu,
    /// One command inside a [`AccessRole::Menu`].
    MenuItem,
    /// One tab in a [`AccessRole::TabList`].
    Tab,
    /// A strip of tabs selecting what the rest of the window shows.
    TabList,
    /// A single-line editable text field.
    TextInput,
    /// A bar holding controls.
    Toolbar,
}

/// The accessibility description an element declares, before paint gives it
/// bounds. Built by [`InteractiveElement::a11y`](crate::InteractiveElement::a11y)
/// and its companions.
#[derive(Clone, Debug)]
pub struct AccessSpec {
    /// What the element is.
    pub role: AccessRole,
    /// How it is named when read aloud. Never empty in practice — an unnamed
    /// control is one a screen-reader user cannot tell apart from its
    /// neighbours.
    pub label: SharedString,
    /// Current contents, for roles that hold a value (the text in a text
    /// field, the title of the selected tab).
    pub value: Option<SharedString>,
    /// Whether this element is the selected one among its siblings. `None` for
    /// roles where selection means nothing.
    pub selected: Option<bool>,
    /// Whether the control is present but cannot be used right now.
    pub disabled: bool,
}

impl AccessSpec {
    /// Describe an element by role and name.
    pub fn new(role: AccessRole, label: impl Into<SharedString>) -> Self {
        Self {
            role,
            label: label.into(),
            value: None,
            selected: None,
            disabled: false,
        }
    }
}

/// Identity of an [`AccessNode`], stable across frames.
///
/// Assistive technology tracks a control by its id: an id that changes every
/// frame reads as the control being destroyed and replaced, which makes a
/// screen reader announce the whole window again. The id is therefore derived
/// from what identifies the element to the app — its
/// [`ElementId`](crate::ElementId) when it has one, its role and label
/// otherwise — never from its position in the frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct AccessNodeId(pub u64);

impl AccessNodeId {
    /// Derive an id. `disambiguator` is 0 for the first node with a given
    /// identity in a frame and counts up for any later collision, so two
    /// same-named buttons stay two distinct controls.
    pub(crate) fn derive(
        element_id: Option<&ElementId>,
        role: AccessRole,
        label: &str,
        disambiguator: u32,
    ) -> Self {
        let mut hasher = seahash::SeaHasher::new();
        match element_id {
            Some(element_id) => element_id.hash(&mut hasher),
            None => label.hash(&mut hasher),
        }
        role.hash(&mut hasher);
        disambiguator.hash(&mut hasher);
        // 0 is reserved for the window's root node.
        Self(hasher.finish() | 1)
    }

    /// The id of the window's root node, the parent of every emitted node.
    pub const ROOT: Self = Self(0);
}

/// A painted element as assistive technology sees it: what it is, what it is
/// called, where it is on screen, and whether it can take keyboard focus.
#[derive(Clone, Debug)]
pub struct AccessNode {
    /// Stable identity across frames.
    pub id: AccessNodeId,
    /// What the element is.
    pub role: AccessRole,
    /// How it is named when read aloud.
    pub label: SharedString,
    /// Current contents, for roles that hold a value.
    pub value: Option<SharedString>,
    /// Whether this element is the selected one among its siblings.
    pub selected: Option<bool>,
    /// Whether the control is present but cannot be used right now.
    pub disabled: bool,
    /// Where the element was painted, in window coordinates (logical pixels).
    pub bounds: Bounds<Pixels>,
    /// The focus handle the element tracks, when it has one. The window
    /// reports the focused node by matching this against its current focus.
    pub focus_id: Option<FocusId>,
}

/// The accessibility state of one painted frame, handed to the platform window
/// after the frame is presented.
#[derive(Clone, Copy)]
pub struct AccessUpdate<'a> {
    /// Every annotated element of the frame, in paint order.
    pub nodes: &'a [AccessNode],
    /// The focused element's focus id, if anything in the window has focus.
    pub focus: Option<FocusId>,
    /// Whether the window itself is the active one.
    pub window_active: bool,
}

impl AccessUpdate<'_> {
    /// The id of the focused node, resolved against the frame's nodes. `None`
    /// when nothing is focused or the focused element declared no role, in
    /// which case the platform backend should focus the window root instead.
    pub fn focused_node(&self) -> Option<AccessNodeId> {
        let focus = self.focus?;
        self.nodes
            .iter()
            .find(|node| node.focus_id == Some(focus))
            .map(|node| node.id)
    }
}

/// What assistive technology asked the application to do to a control.
///
/// Platform backends translate their own action vocabulary into this one and
/// hand it back on the main thread; the window then applies it to the node's
/// element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessAction {
    /// Activate the control, as a click would.
    Click,
    /// Give the control keyboard focus.
    Focus,
}

/// An [`AccessAction`] aimed at one node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccessActionRequest {
    /// The node the action targets.
    pub node: AccessNodeId,
    /// What to do to it.
    pub action: AccessAction,
}
