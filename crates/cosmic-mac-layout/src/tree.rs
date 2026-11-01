// SPDX-License-Identifier: GPL-3.0-or-later
//! The tiling tree.
//!
//! Nodes live in a small arena so that parent pointers stay valid while the tree is mutated.
//! A node is either:
//!
//! * a [`Group`] with a split orientation and one weight per child,
//! * a [`NodeKind::Window`] leaf, or
//! * a [`NodeKind::Stack`] leaf holding an ordered list of windows and an active index.
//!
//! Sizes are relative weights rather than pixels.  Every group normalises its weights against
//! the rectangle it is given, which makes gaps, workspace switches and work-area changes
//! trivial: the same weights produce frames that exactly fill the available extent.

use cosmic_mac_protocol::{Direction, Orientation, Rect, WindowId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NodeId(usize);

#[derive(Debug, Clone)]
pub(crate) struct Group {
    pub orientation: Orientation,
    pub children: Vec<NodeId>,
    pub sizes: Vec<f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct Stack {
    pub windows: Vec<WindowId>,
    pub active: usize,
}

impl Stack {
    pub fn new(windows: Vec<WindowId>, active: usize) -> Self {
        Stack { windows, active }
    }

    pub fn active_window(&self) -> Option<WindowId> {
        self.windows.get(self.active).copied()
    }

    pub fn position(&self, wid: WindowId) -> Option<usize> {
        self.windows.iter().position(|w| *w == wid)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum NodeKind {
    Group(Group),
    Window(WindowId),
    Stack(Stack),
}

#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub parent: Option<NodeId>,
    pub kind: NodeKind,
}

#[derive(Debug, Default)]
pub(crate) struct Tree {
    nodes: Vec<Option<Node>>,
    free: Vec<usize>,
    pub root: Option<NodeId>,
}

/// Result of moving a window in a direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoveOutcome {
    /// The node swapped places with a sibling.
    Swapped,
    /// The window joined the stack it was moved towards.
    JoinedStack,
    /// The node left its group and became a sibling of that group in an ancestor.
    LeftGroup,
    /// Nothing was in the requested direction.
    NoOp,
}

impl Tree {
    pub fn new() -> Self {
        Tree::default()
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    fn alloc(&mut self, kind: NodeKind) -> NodeId {
        let node = Node { parent: None, kind };
        if let Some(index) = self.free.pop() {
            self.nodes[index] = Some(node);
            NodeId(index)
        } else {
            self.nodes.push(Some(node));
            NodeId(self.nodes.len() - 1)
        }
    }

    fn take(&mut self, id: NodeId) -> Node {
        let node = self.nodes[id.0].take().expect("node id used twice");
        self.free.push(id.0);
        node
    }

    pub fn get(&self, id: NodeId) -> &Node {
        self.nodes[id.0].as_ref().expect("dangling node id")
    }

    fn get_mut(&mut self, id: NodeId) -> &mut Node {
        self.nodes[id.0].as_mut().expect("dangling node id")
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.get(id).parent
    }

    pub fn kind(&self, id: NodeId) -> &NodeKind {
        &self.get(id).kind
    }

    pub fn group(&self, id: NodeId) -> Option<&Group> {
        match self.kind(id) {
            NodeKind::Group(group) => Some(group),
            _ => None,
        }
    }

    fn group_mut(&mut self, id: NodeId) -> Option<&mut Group> {
        match &mut self.get_mut(id).kind {
            NodeKind::Group(group) => Some(group),
            _ => None,
        }
    }

    pub fn stack(&self, id: NodeId) -> Option<&Stack> {
        match self.kind(id) {
            NodeKind::Stack(stack) => Some(stack),
            _ => None,
        }
    }

    fn stack_mut(&mut self, id: NodeId) -> Option<&mut Stack> {
        match &mut self.get_mut(id).kind {
            NodeKind::Stack(stack) => Some(stack),
            _ => None,
        }
    }

    pub fn is_window(&self, id: NodeId) -> bool {
        matches!(self.kind(id), NodeKind::Window(_))
    }

    pub fn is_stack(&self, id: NodeId) -> bool {
        matches!(self.kind(id), NodeKind::Stack(_))
    }

    /// Index of `id` in its parent group.
    fn index_in_parent(&self, id: NodeId) -> Option<usize> {
        let parent = self.parent(id)?;
        let group = self.group(parent)?;
        group.children.iter().position(|child| *child == id)
    }

    /// Find the node that directly holds `wid` (either a window leaf or a stack containing it).
    pub fn node_for_window(&self, wid: WindowId) -> Option<NodeId> {
        self.nodes.iter().enumerate().find_map(|(index, node)| {
            let node = node.as_ref()?;
            match &node.kind {
                NodeKind::Window(w) if *w == wid => Some(NodeId(index)),
                NodeKind::Stack(stack) if stack.windows.contains(&wid) => Some(NodeId(index)),
                _ => None,
            }
        })
    }

    pub fn window_node(&self, wid: WindowId) -> Option<NodeId> {
        self.nodes.iter().enumerate().find_map(|(index, node)| {
            matches!(node.as_ref().map(|node| &node.kind), Some(NodeKind::Window(w)) if *w == wid)
                .then_some(NodeId(index))
        })
    }

    /// The stack containing `wid`, if any, plus the member index.
    pub fn stack_containing(&self, wid: WindowId) -> Option<(NodeId, usize)> {
        self.nodes.iter().enumerate().find_map(|(index, node)| {
            if let Some(Node {
                kind: NodeKind::Stack(stack),
                ..
            }) = node
                && let Some(position) = stack.position(wid)
            {
                return Some((NodeId(index), position));
            }
            None
        })
    }

    /// All windows below `id` in display order.
    pub fn windows_in(&self, id: NodeId) -> Vec<WindowId> {
        let mut out = Vec::new();
        self.collect_windows(id, &mut out);
        out
    }

    fn collect_windows(&self, id: NodeId, out: &mut Vec<WindowId>) {
        match self.kind(id) {
            NodeKind::Group(group) => {
                for child in &group.children {
                    self.collect_windows(*child, out);
                }
            }
            NodeKind::Window(wid) => out.push(*wid),
            NodeKind::Stack(stack) => out.extend(stack.windows.iter().copied()),
        }
    }

    pub fn windows_in_order(&self) -> Vec<WindowId> {
        match self.root {
            Some(root) => self.windows_in(root),
            None => Vec::new(),
        }
    }

    /// All window ids that occupy a visible frame (inactive stack members are excluded).
    pub fn visible_windows(&self) -> Vec<WindowId> {
        let mut out = Vec::new();
        if let Some(root) = self.root {
            self.collect_visible(root, &mut out);
        }
        out
    }

    fn collect_visible(&self, id: NodeId, out: &mut Vec<WindowId>) {
        match self.kind(id) {
            NodeKind::Group(group) => {
                for child in &group.children {
                    self.collect_visible(*child, out);
                }
            }
            NodeKind::Window(wid) => out.push(*wid),
            NodeKind::Stack(stack) => {
                if let Some(wid) = stack.active_window() {
                    out.push(wid);
                }
            }
        }
    }

    /// Deal `order` onto the window leaves in tree order, keeping the tree's shape.
    ///
    /// Only applies when every leaf is a plain window and `order` holds exactly as many windows
    /// as the tree; otherwise nothing changes and false is returned.
    pub fn relabel_windows(&mut self, order: &[WindowId]) -> bool {
        let Some(root) = self.root else {
            return order.is_empty();
        };
        let mut leaves = Vec::new();
        self.collect_leaves(root, &mut leaves);
        if leaves.len() != order.len() || leaves.iter().any(|leaf| !self.is_window(*leaf)) {
            return false;
        }
        for (leaf, wid) in leaves.into_iter().zip(order) {
            self.replace_kind(leaf, NodeKind::Window(*wid));
        }
        true
    }

    /// The first (or last) leaf (window or stack) under `id` in tree order; `id` itself when
    /// it is a leaf.
    pub fn edge_leaf(&self, id: NodeId, first: bool) -> NodeId {
        let mut current = id;
        while let NodeKind::Group(group) = self.kind(current) {
            let next = if first {
                group.children.first()
            } else {
                group.children.last()
            };
            match next {
                Some(child) => current = *child,
                None => break,
            }
        }
        current
    }

    fn collect_leaves(&self, id: NodeId, out: &mut Vec<NodeId>) {
        match self.kind(id) {
            NodeKind::Group(group) => {
                for child in &group.children {
                    self.collect_leaves(*child, out);
                }
            }
            NodeKind::Window(_) | NodeKind::Stack(_) => out.push(id),
        }
    }

    /// Insert a new window next to `reference`.
    ///
    /// When the reference's parent already has the requested orientation the new window becomes
    /// a sibling and takes half of the reference's space.  Otherwise a new group is created in
    /// the reference's slot, again halving the reference.  `reference == None` makes the window
    /// the root (only valid for an empty tree).
    pub fn insert_window(
        &mut self,
        wid: WindowId,
        reference: Option<NodeId>,
        orientation: Orientation,
    ) -> NodeId {
        let new_id = self.alloc(NodeKind::Window(wid));
        let Some(reference) = reference else {
            debug_assert!(self.root.is_none());
            self.root = Some(new_id);
            return new_id;
        };

        let parent = self.parent(reference);
        let parent_index = self.index_in_parent(reference);
        let matching_parent = parent.filter(|parent| {
            self.group(*parent)
                .is_some_and(|group| group.orientation == orientation)
        });

        if let Some(parent) = matching_parent {
            let index = parent_index.expect("reference in parent");
            let group = self.group_mut(parent).expect("checked group");
            let weight = group.sizes[index] / 2.0;
            group.sizes[index] = weight;
            group.children.insert(index + 1, new_id);
            group.sizes.insert(index + 1, weight);
            self.get_mut(new_id).parent = Some(parent);
        } else {
            let group_id = self.alloc(NodeKind::Group(Group {
                orientation,
                children: vec![reference, new_id],
                sizes: vec![1.0, 1.0],
            }));
            self.get_mut(reference).parent = Some(group_id);
            self.get_mut(new_id).parent = Some(group_id);
            match (parent, parent_index) {
                (Some(parent), Some(index)) => {
                    self.group_mut(parent).expect("parent group").children[index] = group_id;
                    self.get_mut(group_id).parent = Some(parent);
                }
                (None, _) => {
                    self.root = Some(group_id);
                }
                _ => unreachable!("reference node must be in its parent"),
            }
        }
        new_id
    }

    /// Remove `id` from its parent (the node itself is freed) and normalise the tree.
    pub fn detach(&mut self, id: NodeId) {
        let parent = self.parent(id);
        if let Some(parent) = parent {
            if let Some(index) = self.index_in_parent(id) {
                let group = self.group_mut(parent).expect("parent group");
                group.children.remove(index);
                group.sizes.remove(index);
            }
        } else if self.root == Some(id) {
            self.root = None;
        }
        let node = self.take(id);
        // Free the subtree if the caller detached an interior node.
        self.free_kind(node.kind);
        self.normalize();
    }

    fn free_kind(&mut self, kind: NodeKind) {
        if let NodeKind::Group(group) = kind {
            for child in group.children {
                let node = self.take(child);
                self.free_kind(node.kind);
            }
        }
    }

    /// Replace a leaf's kind, keeping its position and weight.
    pub fn replace_kind(&mut self, id: NodeId, kind: NodeKind) {
        self.get_mut(id).kind = kind;
    }

    /// Turn a window leaf into a stack containing that window.
    pub fn window_to_stack(&mut self, id: NodeId) -> Option<WindowId> {
        let wid = match self.kind(id) {
            NodeKind::Window(wid) => *wid,
            _ => return None,
        };
        self.replace_kind(id, NodeKind::Stack(Stack::new(vec![wid], 0)));
        Some(wid)
    }

    /// Dissolve a stack of one into a plain window leaf.
    pub fn stack_to_window(&mut self, id: NodeId) -> Option<WindowId> {
        let stack = self.stack(id)?;
        if stack.windows.len() != 1 {
            return None;
        }
        let wid = stack.windows[0];
        self.replace_kind(id, NodeKind::Window(wid));
        Some(wid)
    }

    /// The window a focus target resolves to.  For stacks this is the active member.
    pub fn target_window(&self, id: NodeId) -> Option<WindowId> {
        match self.kind(id) {
            NodeKind::Window(wid) => Some(*wid),
            NodeKind::Stack(stack) => stack.active_window(),
            NodeKind::Group(_) => None,
        }
    }

    /// Edge-most target inside `id`, nearest to the `direction` side.
    fn descend_near(&self, id: NodeId, direction: Direction) -> NodeId {
        match self.kind(id) {
            NodeKind::Group(group) if !group.children.is_empty() => {
                let child = if direction.is_forward() {
                    group.children[0]
                } else {
                    group.children[group.children.len() - 1]
                };
                self.descend_near(child, direction)
            }
            _ => id,
        }
    }

    /// Edge-most target on the opposite side of `id`.
    fn descend_edge(&self, id: NodeId, direction: Direction) -> NodeId {
        match self.kind(id) {
            NodeKind::Group(group) if !group.children.is_empty() => {
                let child = if direction.is_forward() {
                    group.children[group.children.len() - 1]
                } else {
                    group.children[0]
                };
                self.descend_edge(child, direction)
            }
            _ => id,
        }
    }

    /// Tree-based directional focus with wrap-around.
    ///
    /// `from` must be a leaf node (window or stack), not a group.  At each ancestor whose
    /// orientation matches the direction's axis, the adjacent sibling is chosen.  When no
    /// sibling exists at any level, focus wraps to the opposite edge of the tree.
    pub fn focus_neighbor(&self, from: NodeId, direction: Direction) -> Option<NodeId> {
        let mut node = from;
        while let Some(parent) = self.parent(node) {
            let group = self.group(parent)?;
            if group.orientation.handles(direction) {
                let index = group.children.iter().position(|child| *child == node)?;
                let target = if direction.is_forward() {
                    index
                        .checked_add(1)
                        .filter(|next| *next < group.children.len())
                } else {
                    index.checked_sub(1)
                };
                if let Some(target) = target {
                    return Some(self.descend_near(group.children[target], direction));
                }
            }
            node = parent;
        }
        let root = self.root?;
        if root == from {
            return None;
        }
        // Wrapping only makes sense when the root splits along the same axis; otherwise the
        // screen edge is not a meaningful neighbour.
        let root_group = self.group(root)?;
        if !root_group.orientation.handles(direction) {
            return None;
        }
        Some(self.descend_edge(root, direction.opposite()))
    }

    /// Move a node one slot towards `direction`.
    ///
    /// Within its own group (the parent splits along the direction's axis and has a sibling on
    /// that side) a plain window moving towards a stack joins that stack, and everything else
    /// swaps places with the adjacent sibling (weights move with the children, so the moved
    /// window takes the neighbour's exact frame).
    ///
    /// When the node's own group cannot move it (wrong axis, or the node already sits at that
    /// edge of the group), the node leaves its group, as in i3 and cosmic-comp's
    /// `move_current_node`: the nearest ancestor that splits along the axis receives it as a
    /// new child, directly next to the branch holding the node on the `direction` side (even
    /// when that branch sits at the ancestor's edge).  A group left with one child collapses
    /// into it.  Nothing happens when the node is a direct child of an axis-aligned group at
    /// that group's edge with no axis-aligned ancestor above, or when no group along the way
    /// splits along the axis (the root is never wrapped in a new split).
    pub fn move_node(&mut self, node: NodeId, direction: Direction) -> MoveOutcome {
        let mut current = node;
        while let Some(parent) = self.parent(current) {
            let (orientation, index, len) = {
                let group = self.group(parent).expect("parent group");
                let index = group
                    .children
                    .iter()
                    .position(|child| *child == current)
                    .expect("child in parent");
                (group.orientation, index, group.children.len())
            };

            if orientation.handles(direction) {
                if current != node {
                    self.move_out_of_group(node, parent, current, direction);
                    return MoveOutcome::LeftGroup;
                }
                let target = if direction.is_forward() {
                    index.checked_add(1).filter(|next| *next < len)
                } else {
                    index.checked_sub(1)
                };
                if let Some(target) = target {
                    let sibling = self.group(parent).expect("parent group").children[target];

                    if let NodeKind::Window(wid) = self.kind(current)
                        && self.is_stack(sibling)
                    {
                        // Join the stack: remove the window leaf, then append it.
                        let wid = *wid;
                        if let Some(index) = self.index_in_parent(current) {
                            let group = self.group_mut(parent).expect("parent group");
                            group.children.remove(index);
                            group.sizes.remove(index);
                        }
                        self.take(current);
                        let stack = self.stack_mut(sibling).expect("stack sibling");
                        stack.windows.push(wid);
                        stack.active = stack.windows.len() - 1;
                        self.normalize();
                        return MoveOutcome::JoinedStack;
                    }

                    let group = self.group_mut(parent).expect("parent group");
                    group.children.swap(index, target);
                    group.sizes.swap(index, target);
                    return MoveOutcome::Swapped;
                }
            }

            current = parent;
        }
        MoveOutcome::NoOp
    }

    /// Detach `node` from its parent group and insert it into `ancestor` next to `branch` (the
    /// child of `ancestor` that holds `node`), on the `direction` side.
    ///
    /// The node gets the average weight of its new siblings, so it takes an equal share of the
    /// ancestor's extent and the others shrink proportionally.
    fn move_out_of_group(
        &mut self,
        node: NodeId,
        ancestor: NodeId,
        branch: NodeId,
        direction: Direction,
    ) {
        let old_parent = self.parent(node).expect("node has a parent");
        let old_index = self.index_in_parent(node).expect("node in parent");
        let old_group = self.group_mut(old_parent).expect("parent group");
        old_group.children.remove(old_index);
        old_group.sizes.remove(old_index);

        let group = self.group_mut(ancestor).expect("ancestor group");
        let branch_index = group
            .children
            .iter()
            .position(|child| *child == branch)
            .expect("branch in ancestor");
        let weight = group.sizes.iter().sum::<f64>() / group.sizes.len() as f64;
        let index = if direction.is_forward() {
            branch_index + 1
        } else {
            branch_index
        };
        group.children.insert(index, node);
        group.sizes.insert(index, weight);
        self.get_mut(node).parent = Some(ancestor);
        // The old group may be down to one child; lift it.  `node` now sits in a group of at
        // least two children, so its own id survives normalisation.
        self.normalize();
    }

    /// Resize the focused node along its group's axis.
    ///
    /// `frames` supplies the current geometry (used to translate a pixel delta into a weight
    /// delta and to enforce `min_px`).  When a sibling exists in `direction` the focused node
    /// grows by taking space from it; otherwise the focused node shrinks by giving space to the
    /// sibling on the opposite side.  Returns true when a weight changed.
    pub fn resize_node(
        &mut self,
        node: NodeId,
        direction: Direction,
        frames: &std::collections::BTreeMap<NodeId, Rect>,
        delta_px: f64,
        min_px: f64,
    ) -> bool {
        let mut current = node;
        while let Some(parent) = self.parent(current) {
            let (orientation, index, len) = {
                let group = self.group(parent).expect("parent group");
                let index = group
                    .children
                    .iter()
                    .position(|child| *child == current)
                    .expect("child in parent");
                (group.orientation, index, group.children.len())
            };
            if !orientation.handles(direction) {
                current = parent;
                continue;
            }

            let forward_sibling = if direction.is_forward() {
                index.checked_add(1).filter(|next| *next < len)
            } else {
                index.checked_sub(1)
            };
            let (receiver, donor) = if let Some(target) = forward_sibling {
                (index, target)
            } else {
                let opposite = if direction.is_forward() {
                    index.checked_sub(1)
                } else {
                    index.checked_add(1).filter(|next| *next < len)
                };
                match opposite {
                    Some(target) => (target, index),
                    None => {
                        current = parent;
                        continue;
                    }
                }
            };

            let Some(rect) = frames.get(&parent) else {
                current = parent;
                continue;
            };
            let extent = if orientation.is_horizontal() {
                rect.width
            } else {
                rect.height
            };
            if extent <= 0.0 {
                current = parent;
                continue;
            }

            let (total, donor_weight) = {
                let group = self.group(parent).expect("parent group");
                (group.sizes.iter().sum::<f64>(), group.sizes[donor])
            };
            if total <= 0.0 {
                current = parent;
                continue;
            }

            let delta_weight = delta_px / extent * total;
            let min_weight = min_px / extent * total;
            let allowed = (donor_weight - min_weight).max(0.0);
            let delta_weight = delta_weight.min(allowed);
            if delta_weight <= 0.0 {
                current = parent;
                continue;
            }

            let group = self.group_mut(parent).expect("parent group");
            group.sizes[donor] -= delta_weight;
            group.sizes[receiver] += delta_weight;
            return true;
        }
        false
    }

    /// Flip the orientation of the focused node's parent group.
    pub fn toggle_orientation(&mut self, node: NodeId) -> bool {
        let Some(parent) = self.parent(node) else {
            return false;
        };
        let Some(group) = self.group_mut(parent) else {
            return false;
        };
        group.orientation = group.orientation.flipped();
        true
    }

    /// Swap two plain window leaves, keeping every frame slot in place.
    pub fn swap_plain_windows(&mut self, a: NodeId, b: NodeId) -> bool {
        if a == b {
            return false;
        }
        let (NodeKind::Window(wa), NodeKind::Window(wb)) = (self.kind(a), self.kind(b)) else {
            return false;
        };
        let (wa, wb) = (*wa, *wb);
        self.replace_kind(a, NodeKind::Window(wb));
        self.replace_kind(b, NodeKind::Window(wa));
        true
    }

    /// Swap the windows held by two stack slots.
    pub fn swap_stack_slots(&mut self, a: NodeId, ia: usize, b: NodeId, ib: usize) -> bool {
        if a == b && ia == ib {
            return false;
        }
        let Some(wa) = self
            .stack(a)
            .and_then(|stack| stack.windows.get(ia).copied())
        else {
            return false;
        };
        let Some(wb) = self
            .stack(b)
            .and_then(|stack| stack.windows.get(ib).copied())
        else {
            return false;
        };
        if a == b {
            let stack = self.stack_mut(a).expect("stack");
            stack.windows[ia] = wb;
            stack.windows[ib] = wa;
            return true;
        }
        self.stack_mut(a).expect("stack a").windows[ia] = wb;
        self.stack_mut(b).expect("stack b").windows[ib] = wa;
        true
    }

    /// Move the window in stack slot `(id, index)` into a plain window leaf at `target`.
    pub fn stack_slot_to_window(&mut self, stack_id: NodeId, index: usize, target: NodeId) -> bool {
        let Some(wid) = self
            .stack(stack_id)
            .and_then(|stack| stack.windows.get(index).copied())
        else {
            return false;
        };
        let Some(other) = self.window_node_from(target) else {
            return false;
        };
        self.replace_kind(target, NodeKind::Window(wid));
        self.stack_mut(stack_id).expect("stack").windows[index] = other;
        true
    }

    fn window_node_from(&self, id: NodeId) -> Option<WindowId> {
        match self.kind(id) {
            NodeKind::Window(wid) => Some(*wid),
            _ => None,
        }
    }

    /// Append a window to a stack and activate it.
    pub fn push_stack_member(&mut self, stack_id: NodeId, wid: WindowId) -> bool {
        let Some(stack) = self.stack_mut(stack_id) else {
            return false;
        };
        stack.windows.push(wid);
        stack.active = stack.windows.len() - 1;
        true
    }

    /// Remove a member from a stack.  Returns the removed window.
    ///
    /// When one member remains the stack is converted back into a plain window leaf; when no
    /// member remains the (now empty) stack is left for the caller to detach.
    pub fn remove_stack_member(&mut self, stack_id: NodeId, index: usize) -> Option<WindowId> {
        let stack = self.stack_mut(stack_id)?;
        if index >= stack.windows.len() {
            return None;
        }
        let wid = stack.windows.remove(index);
        if stack.active > index {
            stack.active -= 1;
        }
        if stack.active >= stack.windows.len() {
            stack.active = stack.windows.len().saturating_sub(1);
        }
        if stack.windows.len() == 1 {
            self.stack_to_window(stack_id);
        }
        Some(wid)
    }

    /// Set the active member of a stack by index, clamping out-of-range values.
    pub fn set_stack_active(&mut self, stack_id: NodeId, index: usize) -> bool {
        let Some(stack) = self.stack_mut(stack_id) else {
            return false;
        };
        if stack.windows.is_empty() {
            return false;
        }
        let index = index.min(stack.windows.len() - 1);
        stack.active = index;
        true
    }

    /// Replace the window held by a stack slot.
    pub fn set_stack_slot(&mut self, stack_id: NodeId, index: usize, wid: WindowId) -> bool {
        let Some(stack) = self.stack_mut(stack_id) else {
            return false;
        };
        let Some(slot) = stack.windows.get_mut(index) else {
            return false;
        };
        *slot = wid;
        true
    }

    /// All stack nodes, in arena order (which is creation order, not tree order).
    pub fn stack_ids(&self) -> Vec<NodeId> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| {
                matches!(
                    node.as_ref().map(|node| &node.kind),
                    Some(NodeKind::Stack(_))
                )
                .then_some(NodeId(index))
            })
            .collect()
    }

    /// Normalise the tree: drop empty groups and lift single-child groups.
    pub fn normalize(&mut self) {
        let Some(root) = self.root else {
            return;
        };
        match self.normalize_node(root) {
            Some(new_root) => {
                self.root = Some(new_root);
                self.get_mut(new_root).parent = None;
            }
            None => self.root = None,
        }
    }

    fn normalize_node(&mut self, id: NodeId) -> Option<NodeId> {
        let kind = self.kind(id).clone();
        match kind {
            NodeKind::Group(group) => {
                let mut children = Vec::with_capacity(group.children.len());
                let mut sizes = Vec::with_capacity(group.sizes.len());
                for (index, child) in group.children.iter().enumerate() {
                    if let Some(child) = self.normalize_node(*child) {
                        children.push(child);
                        sizes.push(group.sizes[index]);
                    }
                }

                if children.is_empty() {
                    self.take(id);
                    return None;
                }

                if children.len() == 1 {
                    let child = children[0];
                    let child_node = self.take(child);
                    self.get_mut(id).kind = child_node.kind;
                    self.reparent(id);
                    return Some(id);
                }

                let node = self.group_mut(id).expect("group");
                node.children = children;
                node.sizes = sizes;
                Some(id)
            }
            NodeKind::Window(_) | NodeKind::Stack(_) => Some(id),
        }
    }

    fn reparent(&mut self, id: NodeId) {
        let children = match self.kind(id) {
            NodeKind::Group(group) => group.children.clone(),
            _ => Vec::new(),
        };
        for child in children {
            self.get_mut(child).parent = Some(id);
        }
    }

    /// Number of windows in the tree (stack members included).
    pub fn window_count(&self) -> usize {
        self.nodes
            .iter()
            .flatten()
            .map(|node| match &node.kind {
                NodeKind::Window(_) => 1,
                NodeKind::Stack(stack) => stack.windows.len(),
                NodeKind::Group(_) => 0,
            })
            .sum()
    }
}
