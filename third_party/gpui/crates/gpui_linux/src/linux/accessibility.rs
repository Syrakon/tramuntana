//! AT-SPI accessibility for the Linux windows, via AccessKit.
//!
//! GPUI hands each presented frame to
//! [`PlatformWindow::update_accessibility`](gpui::PlatformWindow::update_accessibility)
//! as a flat list of [`AccessNode`]s. This module turns that list into an
//! AccessKit tree and pushes it to `accesskit_unix`, which speaks AT-SPI to
//! Orca and the rest of the desktop's assistive technology.
//!
//! The tree is one level deep: a root node for the window, and every annotated
//! element as its child. GPUI's list is in paint order, which is reading order
//! for the chrome, so a flat tree reads correctly; nesting would only matter
//! once containers need to be navigable in their own right.
//!
//! Nothing happens until assistive technology actually connects — AccessKit's
//! adapter starts inactive and `update_if_active` is a cheap no-op until then,
//! so the conversion below never runs on a normal desktop.
//!
//! The tree is read-only for now: nodes advertise no actions, so a screen
//! reader can read and follow the chrome but cannot press its buttons. Driving
//! the chrome from assistive technology needs a synthesized event on the main
//! thread, which is a separate piece of work — AccessKit calls action handlers
//! from its own thread, and GPUI is mid-present when the update arrives.

use std::sync::Mutex;

use accesskit::{
    ActivationHandler, DeactivationHandler, Node, NodeId, Rect, Role, TreeId, TreeInfo, TreeUpdate,
};
use accesskit_unix::Adapter;
use gpui::{AccessNode, AccessNodeId, AccessRole, AccessUpdate};

/// The window's own node, parent of every annotated element.
const ROOT: NodeId = NodeId(0);

/// Name the window's accessibility root: its title, falling back to the
/// application id, and to a generic name when a window has neither yet.
pub(crate) fn accessibility_title(title: &str, app_id: Option<&str>) -> String {
    if !title.trim().is_empty() {
        return title.to_string();
    }
    match app_id {
        Some(app_id) if !app_id.trim().is_empty() => app_id.to_string(),
        _ => "Window".to_string(),
    }
}

/// Per-window AccessKit adapter plus the last tree handed to it.
///
/// The adapter asks for a full tree whenever assistive technology connects,
/// which can happen between frames, so the last update is kept to answer that
/// request without waiting for the next paint.
pub(crate) struct AccessibilityBridge {
    adapter: Mutex<Adapter>,
    last_tree: SharedTree,
}

/// The last tree built, shared with the activation handler that AccessKit
/// calls from its own thread.
type SharedTree = std::sync::Arc<Mutex<Option<TreeUpdate>>>;

impl AccessibilityBridge {
    /// Start an adapter for one window. `title` names the window's root node —
    /// it is what a screen reader announces when the window takes focus.
    pub(crate) fn new(title: String) -> Self {
        let last_tree: SharedTree = std::sync::Arc::new(Mutex::new(None));
        let adapter = Adapter::new(
            InitialTree {
                tree: last_tree.clone(),
                title: title.clone(),
            },
            // Actions are not supported yet; the handler has to exist anyway.
            NoActions,
            NoActions,
        );
        Self {
            adapter: Mutex::new(adapter),
            last_tree,
        }
    }

    /// Publish a presented frame. Cheap while no assistive technology is
    /// connected: the tree is only built inside `update_if_active`, which does
    /// not call back until the adapter is active.
    pub(crate) fn update(&self, update: AccessUpdate<'_>, title: &str) {
        let Ok(mut adapter) = self.adapter.lock() else {
            return;
        };
        adapter.update_window_focus_state(update.window_active);

        let last_tree = self.last_tree.clone();
        adapter.update_if_active(|| {
            let tree = build_tree(update, title);
            if let Ok(mut cached) = last_tree.lock() {
                *cached = Some(clone_tree(&tree));
            }
            tree
        });
    }
}

/// Answers AccessKit's request for a full tree when assistive technology
/// connects. Returns the last frame's tree, or a bare window node if no frame
/// has been presented yet.
struct InitialTree {
    tree: SharedTree,
    title: String,
}

impl ActivationHandler for InitialTree {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        let cached = self.tree.lock().ok()?;
        Some(match cached.as_ref() {
            Some(tree) => clone_tree(tree),
            None => {
                let mut root = Node::new(Role::Window);
                root.set_label(self.title.clone());
                TreeUpdate {
                    nodes: vec![(ROOT, root)],
                    tree: Some(TreeInfo::new(ROOT)),
                    tree_id: TreeId::ROOT,
                    focus: ROOT,
                }
            }
        })
    }
}

/// Placeholder for the action and deactivation handlers: the tree is
/// read-only, and it is rebuilt from scratch on the next frame anyway.
struct NoActions;

impl accesskit::ActionHandler for NoActions {
    fn do_action(&mut self, _request: accesskit::ActionRequest) {}
}

impl DeactivationHandler for NoActions {
    fn deactivate_accessibility(&mut self) {}
}

/// Build the AccessKit tree for one presented frame.
fn build_tree(update: AccessUpdate<'_>, title: &str) -> TreeUpdate {
    let mut nodes = Vec::with_capacity(update.nodes.len() + 1);
    let mut children = Vec::with_capacity(update.nodes.len());

    for node in update.nodes {
        let id = node_id(node.id);
        children.push(id);
        nodes.push((id, build_node(node)));
    }

    let mut root = Node::new(Role::Window);
    root.set_label(title.to_string());
    root.set_children(children);
    nodes.push((ROOT, root));

    TreeUpdate {
        nodes,
        tree: Some(TreeInfo::new(ROOT)),
        tree_id: TreeId::ROOT,
        focus: update.focused_node().map_or(ROOT, node_id),
    }
}

fn build_node(node: &AccessNode) -> Node {
    let mut out = Node::new(role(node.role));
    out.set_label(node.label.to_string());
    if let Some(value) = &node.value {
        out.set_value(value.to_string());
    }
    if let Some(selected) = node.selected {
        out.set_selected(selected);
    }
    if node.disabled {
        out.set_disabled();
    }
    // GPUI paints in logical pixels with the origin at the window's top-left,
    // which is exactly the coordinate space AccessKit expects for bounds
    // relative to the window.
    let origin = node.bounds.origin;
    let size = node.bounds.size;
    out.set_bounds(Rect {
        x0: f64::from(origin.x),
        y0: f64::from(origin.y),
        x1: f64::from(origin.x + size.width),
        y1: f64::from(origin.y + size.height),
    });
    out
}

fn node_id(id: AccessNodeId) -> NodeId {
    NodeId(id.0)
}

fn role(role: AccessRole) -> Role {
    match role {
        AccessRole::Button => Role::Button,
        AccessRole::CheckBox => Role::CheckBox,
        AccessRole::Group => Role::Group,
        AccessRole::Label => Role::Label,
        AccessRole::Link => Role::Link,
        AccessRole::List => Role::List,
        AccessRole::ListItem => Role::ListItem,
        AccessRole::Menu => Role::Menu,
        AccessRole::MenuItem => Role::MenuItem,
        AccessRole::Tab => Role::Tab,
        AccessRole::TabList => Role::TabList,
        AccessRole::TextInput => Role::TextInput,
        AccessRole::Toolbar => Role::Toolbar,
    }
}

/// `TreeUpdate` is not `Clone`, and the cached copy has to survive being
/// handed to the adapter, so rebuild it field by field.
fn clone_tree(tree: &TreeUpdate) -> TreeUpdate {
    TreeUpdate {
        nodes: tree
            .nodes
            .iter()
            .map(|(id, node)| (*id, node.clone()))
            .collect(),
        tree: tree.tree.clone(),
        tree_id: tree.tree_id,
        focus: tree.focus,
    }
}
