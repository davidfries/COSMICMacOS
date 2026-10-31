// SPDX-License-Identifier: GPL-3.0-or-later
//! Pure-Rust window-management layout engine.
//!
//! [`Layout`] is a state machine.  Feed it [`ToplevelInfo`]s and [`Action`]s, and it returns
//! [`Command`]s describing exactly what the platform layer must do.  It has no knowledge of
//! macOS, Accessibility, displays or keybindings, which is what makes the tiling/stack logic
//! testable on any host.
//!
//! # Model
//!
//! * A single workspace pool (the daemon owns display assignment).
//! * Each workspace has a tiling tree, a floating layer, a focus history and a
//!   `tiling_enabled` flag.
//! * Tiling nodes are groups (split containers), windows and stacks (ordered tab groups).
//!   Only the active stack member occupies the stack's frame; the others are hidden.
//! * Every operation mutates the model and then calls [`Layout::sync`], which diffs the
//!   desired visible state against the last emitted state.  Commands are deterministic and
//!   ordered: workspace activation, hides, shows, frames, focus, raise, tab bar.
//!
//! # Example
//!
//! ```
//! use cosmic_mac_layout::Layout;
//! use cosmic_mac_protocol::{Action, Config, Direction, Rect, ToplevelInfo};
//!
//! let mut layout = Layout::with_work_area(Config::default(), Rect::new(0.0, 0.0, 1000.0, 500.0));
//! layout.window_added(ToplevelInfo::new(1, "com.apple.TextEdit", "one"));
//! layout.window_added(ToplevelInfo::new(2, "com.apple.TextEdit", "two"));
//! layout.action(Action::Focus { direction: Direction::Left });
//! assert_eq!(layout.focused(), Some(1));
//! ```

mod geometry;
mod tree;
mod workspace;

use std::collections::{BTreeMap, BTreeSet};

use cosmic_mac_protocol::{
    Action, Command, Config, Direction, Orientation, Rect, StackInfo, TabInfo, ToplevelInfo,
    TreeNode, WindowId, WindowState, WorkspaceInfo,
};

use tree::{NodeId, NodeKind, Tree};
use workspace::{FloatingWindow, Maximized, Workspace};

/// Smallest size a tiled window may be resized to, in logical pixels.
pub const MIN_WINDOW_SIZE: f64 = 100.0;

/// Amount one resize step transfers, in logical pixels.
pub const RESIZE_STEP: f64 = 32.0;

/// Highest workspace index accepted by [`Action::Workspace`] (COSMIC binds `Super+1` … `9`).
pub const MAX_WORKSPACES: u32 = 9;

const CASCADE_STEP: f64 = 32.0;
const CASCADE_WRAP: u32 = 8;
const DEFAULT_FLOATING_FRACTION: f64 = 0.6;
const DEFAULT_FLOATING_MAX_WIDTH: f64 = 960.0;
const DEFAULT_FLOATING_MAX_HEIGHT: f64 = 720.0;
const FOCUS_EPSILON: f64 = 0.5;

/// What the engine last told the platform.  Used to emit minimal, deterministic diffs.
#[derive(Debug, Default)]
struct Emitted {
    frames: BTreeMap<WindowId, Rect>,
    visible: BTreeSet<WindowId>,
    focus: Option<WindowId>,
    workspace: Option<u32>,
    stack: Option<StackInfo>,
}

impl Emitted {
    fn forget(&mut self, wid: WindowId) {
        self.frames.remove(&wid);
        self.visible.remove(&wid);
        if self.focus == Some(wid) {
            self.focus = None;
        }
    }
}

#[derive(Debug, Clone)]
struct WorkspaceFrames {
    /// Windows occupying a frame right now (active stack members and floating windows).
    visible: BTreeMap<WindowId, Rect>,
    /// Every window's frame, including inactive stack members (which share the stack frame).
    all: BTreeMap<WindowId, Rect>,
    /// Every tree node's frame, used for resize math and snapshots.
    nodes: BTreeMap<NodeId, Rect>,
}

/// The layout state machine.
#[derive(Debug)]
pub struct Layout {
    config: Config,
    work_area: Rect,
    workspaces: Vec<Workspace>,
    active: usize,
    last_workspace: Option<usize>,
    resize_mode: bool,
    infos: BTreeMap<WindowId, ToplevelInfo>,
    emitted: Emitted,
}

impl Default for Layout {
    fn default() -> Self {
        Layout::new(Config::default())
    }
}

impl Layout {
    /// Create a layout with an empty work area.  Call [`Layout::set_work_area`] before adding
    /// windows, or use [`Layout::with_work_area`].
    pub fn new(config: Config) -> Self {
        let tiling_enabled = config.autotile;
        Layout {
            config,
            work_area: Rect::ZERO,
            workspaces: vec![Workspace::new(tiling_enabled)],
            active: 0,
            last_workspace: None,
            resize_mode: false,
            infos: BTreeMap::new(),
            emitted: Emitted {
                workspace: Some(1),
                ..Emitted::default()
            },
        }
    }

    /// Create a layout with a known work area.
    pub fn with_work_area(config: Config, work_area: Rect) -> Self {
        let mut layout = Layout::new(config);
        layout.work_area = work_area;
        layout
    }

    // -------------------------------------------------------------------------------------
    // Accessors
    // -------------------------------------------------------------------------------------

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn work_area(&self) -> Rect {
        self.work_area
    }

    /// 1-based index of the active workspace.
    pub fn active_workspace(&self) -> u32 {
        (self.active + 1) as u32
    }

    pub fn resize_mode(&self) -> bool {
        self.resize_mode
    }

    /// Whether tiling is enabled on the active workspace.
    pub fn tiling_enabled(&self) -> bool {
        self.workspaces[self.active].tiling_enabled
    }

    pub fn focused(&self) -> Option<WindowId> {
        self.workspaces[self.active].focus
    }

    pub fn window_info(&self, wid: WindowId) -> Option<&ToplevelInfo> {
        self.infos.get(&wid)
    }

    /// Frames of all windows currently visible on the active workspace.
    pub fn visible_frames(&self) -> BTreeMap<WindowId, Rect> {
        self.workspace_frames(self.active).visible
    }

    pub fn frame_of(&self, wid: WindowId) -> Option<Rect> {
        let index = self.window_workspace(wid)?;
        self.workspace_frames(index).all.get(&wid).copied()
    }

    pub fn is_visible(&self, wid: WindowId) -> bool {
        let Some(index) = self.window_workspace(wid) else {
            return false;
        };
        index == self.active && self.workspace_frames(index).visible.contains_key(&wid)
    }

    pub fn workspaces(&self) -> Vec<WorkspaceInfo> {
        self.workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| {
                let number = (index + 1) as u32;
                WorkspaceInfo::new(
                    number,
                    number.to_string(),
                    index == self.active,
                    workspace.window_count(),
                )
            })
            .collect()
    }

    /// Full view of every managed window, sorted by window id.
    pub fn windows(&self) -> Vec<WindowState> {
        let mut out = Vec::new();
        for (index, workspace) in self.workspaces.iter().enumerate() {
            let frames = self.workspace_frames(index);
            let mut wids = workspace.tree.windows_in_order();
            wids.extend(workspace.floating.iter().map(|floating| floating.wid));
            for wid in wids {
                let info = self.infos.get(&wid).cloned().unwrap_or_default();
                let (in_stack, stack_active) = match workspace.tree.stack_containing(wid) {
                    Some((stack_id, member)) => (
                        true,
                        workspace
                            .tree
                            .stack(stack_id)
                            .is_some_and(|stack| stack.active == member),
                    ),
                    None => (false, false),
                };
                out.push(WindowState {
                    info,
                    workspace: (index + 1) as u32,
                    floating: workspace
                        .floating
                        .iter()
                        .any(|floating| floating.wid == wid),
                    visible: index == self.active && frames.visible.contains_key(&wid),
                    focused: index == self.active && workspace.focus == Some(wid),
                    in_stack,
                    stack_active,
                    frame: frames.all.get(&wid).copied().unwrap_or(Rect::ZERO),
                });
            }
        }
        out.sort_by_key(|state| state.info.wid);
        out
    }

    /// Serializable snapshot of the active workspace's tiling tree.
    pub fn tree_snapshot(&self) -> Option<TreeNode> {
        let workspace = &self.workspaces[self.active];
        let root = workspace.tree.root?;
        let frames = self.workspace_frames(self.active);
        Some(snapshot_node(
            &workspace.tree,
            root,
            &frames.nodes,
            &frames.all,
            &self.infos,
        ))
    }

    /// The stack containing the focused window, if any.
    pub fn active_stack(&self) -> Option<StackInfo> {
        let workspace = &self.workspaces[self.active];
        let frames = self.workspace_frames(self.active);
        focused_stack_info(
            workspace,
            (self.active + 1) as u32,
            &frames.visible,
            &frames.nodes,
            &self.infos,
        )
    }

    /// Every stack in the active workspace.
    pub fn stacks(&self) -> Vec<StackInfo> {
        let workspace = &self.workspaces[self.active];
        let frames = self.workspace_frames(self.active);
        workspace
            .tree
            .stack_ids()
            .into_iter()
            .filter_map(|stack_id| {
                let stack = workspace.tree.stack(stack_id)?;
                let frame = stack
                    .active_window()
                    .and_then(|wid| frames.visible.get(&wid).copied())
                    .or_else(|| frames.nodes.get(&stack_id).copied())?;
                Some(StackInfo {
                    workspace: (self.active + 1) as u32,
                    tabs: tabs_for(stack, &self.infos),
                    active: stack.active,
                    frame,
                })
            })
            .collect()
    }

    // -------------------------------------------------------------------------------------
    // Events
    // -------------------------------------------------------------------------------------

    /// A new window appeared.  It is tiled or floated according to the config and the
    /// workspace's tiling flag, focused, and a full command diff is returned.
    pub fn window_added(&mut self, info: ToplevelInfo) -> Vec<Command> {
        let wid = info.wid;
        let floating =
            self.config.should_float(&info) || !self.workspaces[self.active].tiling_enabled;
        self.infos.insert(wid, info.clone());
        if floating {
            let frame = self.floating_frame(self.active, &info);
            let workspace = &mut self.workspaces[self.active];
            workspace.floating.push(FloatingWindow { wid, frame });
            workspace.cascade += 1;
        } else {
            self.insert_tiled(self.active, wid, None);
        }
        self.workspaces[self.active].set_focus(wid);
        self.sync()
    }

    /// A window disappeared.  The tree is collapsed, focus moves to the next window and empty
    /// trailing workspaces are removed.
    pub fn window_removed(&mut self, wid: WindowId) -> Vec<Command> {
        let Some(index) = self.window_workspace(wid) else {
            self.infos.remove(&wid);
            self.emitted.forget(wid);
            return self.sync();
        };
        self.remove_window_from(index, wid);
        self.infos.remove(&wid);
        self.emitted.forget(wid);
        let workspace = &mut self.workspaces[index];
        if workspace.maximized.is_some_and(|max| max.wid == wid) {
            workspace.maximized = None;
        }
        workspace.cleanup_focus(wid);
        self.trim_workspaces();
        self.sync()
    }

    /// External focus change (Cmd+Tab, Dock click, notification).  Switches to the window's
    /// workspace and activates its stack tab.
    pub fn focus_changed(&mut self, wid: WindowId) -> Vec<Command> {
        let Some(index) = self.window_workspace(wid) else {
            return self.sync();
        };
        if index != self.active {
            self.switch_to(index);
        }
        self.workspaces[index].set_focus(wid);
        self.sync()
    }

    /// A window's metadata changed (for example its title).  Geometry is unaffected.
    pub fn update_window(&mut self, info: ToplevelInfo) -> Vec<Command> {
        if let Some(existing) = self.infos.get_mut(&info.wid) {
            *existing = info;
        }
        self.sync()
    }

    pub fn set_work_area(&mut self, work_area: Rect) -> Vec<Command> {
        self.work_area = work_area;
        for workspace in &mut self.workspaces {
            for floating in &mut workspace.floating {
                floating.frame = floating.frame.clamp_within(work_area);
            }
            if let Some(maximized) = &mut workspace.maximized
                && let Some(restore) = &mut maximized.restore
            {
                *restore = restore.clamp_within(work_area);
            }
        }
        self.sync()
    }

    pub fn update_config(&mut self, config: Config) -> Vec<Command> {
        self.config = config;
        self.sync()
    }

    // -------------------------------------------------------------------------------------
    // Actions
    // -------------------------------------------------------------------------------------

    /// Perform a window-management action and return the resulting commands.
    pub fn action(&mut self, action: Action) -> Vec<Command> {
        let mut immediate = Vec::new();
        match action {
            Action::Focus { direction } => self.act_focus(direction),
            Action::FocusWindow { wid } => self.act_focus_window(wid),
            Action::FocusNext => {
                self.workspaces[self.active].cycle_focus(true);
            }
            Action::FocusPrevious => {
                self.workspaces[self.active].cycle_focus(false);
            }
            Action::Move { direction } => self.act_move(direction),
            Action::SwapWindow => self.act_swap_window(),
            Action::ToggleOrientation => self.act_toggle_orientation(),
            Action::Resize { direction } => self.act_resize(direction),
            Action::ResizeMode => self.resize_mode = !self.resize_mode,
            Action::Maximize => self.act_maximize(),
            Action::ToggleTiling => self.act_toggle_tiling(),
            Action::ToggleWindowFloating => self.act_toggle_window_floating(),
            Action::ToggleStacking => self.act_toggle_stacking(),
            Action::Unstack => self.act_unstack(),
            Action::Workspace { index } => self.act_workspace(index),
            Action::MoveToWorkspace { index } => self.act_move_to_workspace(index),
            Action::NextWorkspace => self.act_next_workspace(),
            Action::PreviousWorkspace => self.act_previous_workspace(),
            Action::LastWorkspace => self.act_last_workspace(),
            Action::MoveToNextWorkspace => self.act_move_to_next_workspace(),
            Action::MoveToPreviousWorkspace => self.act_move_to_previous_workspace(),
            Action::MoveToLastWorkspace => self.act_move_to_last_workspace(),
            Action::Close => {
                if let Some(wid) = self.focused() {
                    immediate.push(Command::Close { wid });
                }
            }
        }
        immediate.extend(self.sync());
        immediate
    }

    // -------------------------------------------------------------------------------------
    // Focus
    // -------------------------------------------------------------------------------------

    fn act_focus(&mut self, direction: Direction) {
        let index = self.active;
        let Some(focus) = self.workspaces[index].focus else {
            if let Some(wid) = self.workspaces[index].first_window() {
                self.workspaces[index].set_focus(wid);
            }
            return;
        };

        if let Some(node) = self.workspaces[index].tree.node_for_window(focus) {
            // Tiled focus is tree-based (with wrap-around at the root).  Perpendicular
            // directions simply have no neighbour, matching cosmic-comp.
            if let Some(target) = self.workspaces[index].tree.focus_neighbor(node, direction)
                && let Some(wid) = self.workspaces[index].tree.target_window(target)
            {
                self.workspaces[index].set_focus(wid);
            }
            return;
        }

        // Floating focus uses a spatial search across both layers.
        if let Some(wid) = self.geometric_focus(Some(focus), direction) {
            self.workspaces[index].set_focus(wid);
        }
    }

    fn act_focus_window(&mut self, wid: WindowId) {
        let Some(index) = self.window_workspace(wid) else {
            return;
        };
        if index != self.active {
            self.switch_to(index);
        }
        self.workspaces[index].set_focus(wid);
    }

    /// Spatial fallback used when focus is on a floating window, or when the tree has no
    /// neighbour in that direction.  Wraps to the opposite screen edge.
    fn geometric_focus(&self, from: Option<WindowId>, direction: Direction) -> Option<WindowId> {
        let frames = self.workspace_frames(self.active).visible;
        let (cx, cy) = from
            .and_then(|wid| frames.get(&wid).copied())
            .map(Rect::center)
            .unwrap_or_else(|| self.work_area.center());
        // Each candidate carries (wid, primary distance, perpendicular distance, y, x).  The
        // position fields only exist to break ties deterministically: on an exact tie the
        // topmost window wins, then the leftmost one, then the lowest window id.  Without
        // that, a tie would be resolved by map iteration order and flip between runs.
        let candidates: Vec<(WindowId, f64, f64, f64, f64)> = frames
            .iter()
            .filter(|(wid, _)| Some(**wid) != from)
            .map(|(wid, rect)| {
                let (x, y) = rect.center();
                (*wid, 0.0, 0.0, y, x)
            })
            .collect();
        if candidates.is_empty() {
            return None;
        }

        let directional = candidates
            .iter()
            .filter_map(|&(wid, _, _, y, x)| {
                let (primary, perpendicular) = match direction {
                    Direction::Left if x < cx - FOCUS_EPSILON => (cx - x, (y - cy).abs()),
                    Direction::Right if x > cx + FOCUS_EPSILON => (x - cx, (y - cy).abs()),
                    Direction::Up if y < cy - FOCUS_EPSILON => (cy - y, (x - cx).abs()),
                    Direction::Down if y > cy + FOCUS_EPSILON => (y - cy, (x - cx).abs()),
                    _ => return None,
                };
                Some((wid, primary, perpendicular, y, x))
            })
            .min_by(compare_focus_candidates);
        if let Some(&(wid, ..)) = directional.as_ref() {
            return Some(wid);
        }

        // Wrap around: pick the window at the far edge on the opposite side.
        let wrapped = candidates
            .iter()
            .map(|&(wid, _, _, y, x)| {
                let (primary, perpendicular) = match direction {
                    // Nothing to the right: wrap to the leftmost window.
                    Direction::Right => (x - cx, (y - cy).abs()),
                    // Nothing to the left: wrap to the rightmost window.
                    Direction::Left => (cx - x, (y - cy).abs()),
                    // Nothing below: wrap to the topmost window.
                    Direction::Down => (y - cy, (x - cx).abs()),
                    // Nothing above: wrap to the bottommost window.
                    Direction::Up => (cy - y, (x - cx).abs()),
                };
                (wid, primary, perpendicular, y, x)
            })
            .min_by(compare_focus_candidates);
        wrapped.map(|(wid, ..)| wid)
    }

    // -------------------------------------------------------------------------------------
    // Move / swap / orientation
    // -------------------------------------------------------------------------------------

    fn act_move(&mut self, direction: Direction) {
        let index = self.active;
        let Some(wid) = self.workspaces[index].focus else {
            return;
        };
        let workspace = &mut self.workspaces[index];

        if let Some((stack_id, member)) = workspace.tree.stack_containing(wid) {
            // A stack member moving towards another stack changes stacks instead of moving the
            // whole stack.
            if let Some(sibling) = adjacent_sibling(&workspace.tree, stack_id, direction)
                && let Some(stack) = workspace.tree.stack(sibling)
            {
                // Normalising after the removal can lift the sibling stack into its parent's
                // node id, so remember a member and re-resolve the node afterwards.
                let anchor = stack
                    .active_window()
                    .or_else(|| stack.windows.first().copied());
                workspace.tree.remove_stack_member(stack_id, member);
                if workspace
                    .tree
                    .stack(stack_id)
                    .is_some_and(|stack| stack.windows.is_empty())
                {
                    workspace.tree.detach(stack_id);
                }
                if let Some(target) = anchor.and_then(|wid| workspace.tree.node_for_window(wid))
                    && workspace.tree.is_stack(target)
                {
                    workspace.tree.push_stack_member(target, wid);
                }
                workspace.set_focus(wid);
                return;
            }
            workspace.tree.move_node(stack_id, direction);
            workspace.set_focus(wid);
        } else if let Some(node) = workspace.tree.window_node(wid) {
            workspace.tree.move_node(node, direction);
            workspace.set_focus(wid);
        }
    }

    fn act_swap_window(&mut self) {
        let index = self.active;
        let workspace = &mut self.workspaces[index];
        let Some(focused) = workspace.focus else {
            return;
        };
        let Some(previous) = workspace.focus_stack.get(1).copied() else {
            return;
        };
        if previous == focused {
            return;
        }
        let (Some(focused_loc), Some(previous_loc)) =
            (locate(workspace, focused), locate(workspace, previous))
        else {
            return;
        };

        match (focused_loc, previous_loc) {
            (Location::Plain(a), Location::Plain(b)) => {
                workspace.tree.swap_plain_windows(a, b);
            }
            (Location::Stack(a, ia), Location::Stack(b, ib)) => {
                workspace.tree.swap_stack_slots(a, ia, b, ib);
            }
            (Location::Plain(leaf), Location::Stack(stack, member)) => {
                workspace.tree.stack_slot_to_window(stack, member, leaf);
            }
            (Location::Stack(stack, member), Location::Plain(leaf)) => {
                workspace.tree.stack_slot_to_window(stack, member, leaf);
            }
            (Location::Plain(leaf), Location::Floating(floating)) => {
                workspace.floating[floating].wid = focused;
                workspace
                    .tree
                    .replace_kind(leaf, NodeKind::Window(previous));
            }
            (Location::Floating(floating), Location::Plain(leaf)) => {
                workspace.floating[floating].wid = previous;
                workspace.tree.replace_kind(leaf, NodeKind::Window(focused));
            }
            (Location::Stack(stack, member), Location::Floating(floating)) => {
                workspace.floating[floating].wid = focused;
                workspace.tree.set_stack_slot(stack, member, previous);
            }
            (Location::Floating(floating), Location::Stack(stack, member)) => {
                workspace.floating[floating].wid = previous;
                workspace.tree.set_stack_slot(stack, member, focused);
            }
            (Location::Floating(a), Location::Floating(b)) => {
                workspace.floating[a].wid = previous;
                workspace.floating[b].wid = focused;
            }
        }

        workspace.set_focus(focused);
    }

    fn act_toggle_orientation(&mut self) {
        let index = self.active;
        let Some(wid) = self.workspaces[index].focus else {
            return;
        };
        if let Some(node) = self.workspaces[index].tree.node_for_window(wid) {
            self.workspaces[index].tree.toggle_orientation(node);
        }
    }

    fn act_resize(&mut self, direction: Direction) {
        let index = self.active;
        let Some(wid) = self.workspaces[index].focus else {
            return;
        };
        let Some(node) = self.workspaces[index].tree.node_for_window(wid) else {
            return;
        };
        let geometry = geometry::compute(
            &self.workspaces[index].tree,
            self.work_area,
            self.config.gaps,
            &self.infos,
        );
        self.workspaces[index].tree.resize_node(
            node,
            direction,
            &geometry.nodes,
            RESIZE_STEP,
            MIN_WINDOW_SIZE,
        );
    }

    // -------------------------------------------------------------------------------------
    // Maximize
    // -------------------------------------------------------------------------------------

    fn act_maximize(&mut self) {
        let workspace = &mut self.workspaces[self.active];
        let Some(wid) = workspace.focus else {
            return;
        };
        if workspace.maximized.is_some_and(|max| max.wid == wid) {
            let restore = workspace.maximized.take().and_then(|max| max.restore);
            if let Some(restore) = restore
                && let Some(floating) = workspace.floating_mut(wid)
            {
                floating.frame = restore;
            }
        } else {
            let restore = workspace.floating(wid).map(|floating| floating.frame);
            workspace.maximized = Some(Maximized { wid, restore });
        }
    }

    // -------------------------------------------------------------------------------------
    // Tiling / floating / stacks
    // -------------------------------------------------------------------------------------

    fn act_toggle_tiling(&mut self) {
        let index = self.active;
        if self.workspaces[index].tiling_enabled {
            let geometry = geometry::compute(
                &self.workspaces[index].tree,
                self.work_area,
                self.config.gaps,
                &self.infos,
            );
            let wids = self.workspaces[index].tree.windows_in_order();
            for wid in wids {
                // Active stack members keep their exact frame; inactive members (and windows
                // hidden by a stack) get a cascade placement so they do not overlap.
                let frame = match geometry.frames.get(&wid).copied() {
                    Some(frame) => frame,
                    None => {
                        let info = self.infos.get(&wid).cloned().unwrap_or_default();
                        let cascade = self.workspaces[index].cascade % CASCADE_WRAP;
                        self.workspaces[index].cascade += 1;
                        floating_frame(self.work_area, &info, cascade)
                    }
                };
                self.workspaces[index]
                    .floating
                    .push(FloatingWindow { wid, frame });
            }
            self.workspaces[index].tree = Tree::new();
            self.workspaces[index].tiling_enabled = false;
            self.workspaces[index].maximized = None;
        } else {
            let floating = std::mem::take(&mut self.workspaces[index].floating);
            self.workspaces[index].tiling_enabled = true;
            self.workspaces[index].maximized = None;
            let saved_focus = self.workspaces[index].focus;
            for floating in floating {
                self.insert_tiled(
                    index,
                    floating.wid,
                    Some((floating.frame.width, floating.frame.height)),
                );
            }
            if let Some(focus) = saved_focus
                && self.workspaces[index].tree.node_for_window(focus).is_some()
            {
                self.workspaces[index].set_focus(focus);
            }
        }
    }

    fn act_toggle_window_floating(&mut self) {
        let index = self.active;
        if !self.workspaces[index].tiling_enabled {
            // Everything is already floating; there is no tiling layer to return to.
            return;
        }
        let Some(wid) = self.workspaces[index].focus else {
            return;
        };

        if let Some(position) = self.workspaces[index]
            .floating
            .iter()
            .position(|floating| floating.wid == wid)
        {
            let frame = self.workspaces[index].floating[position].frame;
            self.workspaces[index].floating.remove(position);
            self.insert_tiled(index, wid, Some((frame.width, frame.height)));
            self.workspaces[index].set_focus(wid);
            if self.workspaces[index]
                .maximized
                .is_some_and(|max| max.wid == wid)
            {
                self.workspaces[index].maximized = None;
            }
            return;
        }

        let workspace = &self.workspaces[index];
        if workspace.tree.node_for_window(wid).is_none() {
            return;
        }
        let geometry = geometry::compute(
            &workspace.tree,
            self.work_area,
            self.config.gaps,
            &self.infos,
        );
        let frame = if let Some((stack_id, _)) = workspace.tree.stack_containing(wid) {
            geometry.stacks.get(&stack_id).copied()
        } else {
            geometry.frames.get(&wid).copied()
        }
        .unwrap_or(self.work_area);

        self.remove_window_from(index, wid);
        let workspace = &mut self.workspaces[index];
        workspace.floating.push(FloatingWindow { wid, frame });
        workspace.cascade += 1;
        workspace.set_focus(wid);
        if workspace.maximized.is_some_and(|max| max.wid == wid) {
            workspace.maximized = None;
        }
    }

    fn act_toggle_stacking(&mut self) {
        let index = self.active;
        let Some(wid) = self.workspaces[index].focus else {
            return;
        };
        if self.workspaces[index].tree.stack_containing(wid).is_some() {
            self.act_unstack();
            return;
        }

        let workspace = &mut self.workspaces[index];
        let Some(node) = workspace.tree.window_node(wid) else {
            return;
        };
        let Some(sibling) = adjacent_sibling_any(&workspace.tree, node) else {
            return;
        };
        // Detaching the focused window can lift the sibling into the parent's node id, so
        // identify the sibling by content and re-resolve it afterwards.
        let neighbour = if workspace.tree.is_window(sibling) {
            workspace.tree.target_window(sibling).map(Neighbour::Window)
        } else if let Some(stack) = workspace.tree.stack(sibling) {
            stack
                .active_window()
                .or_else(|| stack.windows.first().copied())
                .map(Neighbour::Stack)
        } else {
            None
        };
        let Some(neighbour) = neighbour else {
            return;
        };

        workspace.tree.detach(node);
        match neighbour {
            Neighbour::Window(other) => {
                if let Some(target) = workspace.tree.node_for_window(other)
                    && workspace.tree.is_window(target)
                {
                    workspace.tree.window_to_stack(target);
                    workspace.tree.push_stack_member(target, wid);
                }
            }
            Neighbour::Stack(anchor) => {
                if let Some(target) = workspace.tree.node_for_window(anchor)
                    && workspace.tree.is_stack(target)
                {
                    workspace.tree.push_stack_member(target, wid);
                }
            }
        }
        workspace.set_focus(wid);
    }

    fn act_unstack(&mut self) {
        let index = self.active;
        let (stack_id, member, orientation) = {
            let workspace = &self.workspaces[index];
            let Some(wid) = workspace.focus else {
                return;
            };
            let Some((stack_id, member)) = workspace.tree.stack_containing(wid) else {
                return;
            };
            let orientation = workspace
                .tree
                .parent(stack_id)
                .and_then(|parent| workspace.tree.group(parent))
                .map(|group| group.orientation)
                .unwrap_or_else(|| {
                    let frame = self
                        .workspace_frames(index)
                        .nodes
                        .get(&stack_id)
                        .copied()
                        .unwrap_or(self.work_area);
                    Orientation::from_aspect(frame.width, frame.height)
                });
            (stack_id, member, orientation)
        };

        let workspace = &mut self.workspaces[index];
        let Some(wid) = workspace.focus else {
            return;
        };
        let members = workspace
            .tree
            .stack(stack_id)
            .map(|stack| stack.windows.len())
            .unwrap_or(0);
        if members <= 1 {
            workspace.tree.stack_to_window(stack_id);
            return;
        }
        workspace.tree.remove_stack_member(stack_id, member);
        workspace
            .tree
            .insert_window(wid, Some(stack_id), orientation);
        workspace.set_focus(wid);
    }

    // -------------------------------------------------------------------------------------
    // Workspaces
    // -------------------------------------------------------------------------------------

    fn act_workspace(&mut self, index: u32) {
        if index == 0 || index > MAX_WORKSPACES {
            return;
        }
        let target = (index - 1) as usize;
        self.ensure_workspaces(target);
        self.switch_to(target);
    }

    fn act_move_to_workspace(&mut self, index: u32) {
        if index == 0 || index > MAX_WORKSPACES {
            return;
        }
        let target = (index - 1) as usize;
        if target == self.active {
            return;
        }
        self.ensure_workspaces(target);
        self.move_focused_to(target);
    }

    fn act_next_workspace(&mut self) {
        let len = self.workspaces.len();
        if len > 1 {
            self.switch_to((self.active + 1) % len);
        }
    }

    fn act_previous_workspace(&mut self) {
        let len = self.workspaces.len();
        if len > 1 {
            self.switch_to((self.active + len - 1) % len);
        }
    }

    fn act_last_workspace(&mut self) {
        if let Some(last) = self.last_workspace
            && last != self.active
            && last < self.workspaces.len()
        {
            self.switch_to(last);
        }
    }

    fn act_move_to_next_workspace(&mut self) {
        let len = self.workspaces.len();
        if len > 1 {
            self.move_focused_to((self.active + 1) % len);
        }
    }

    fn act_move_to_previous_workspace(&mut self) {
        let len = self.workspaces.len();
        if len > 1 {
            self.move_focused_to((self.active + len - 1) % len);
        }
    }

    fn act_move_to_last_workspace(&mut self) {
        if let Some(last) = self.last_workspace
            && last != self.active
            && last < self.workspaces.len()
        {
            self.move_focused_to(last);
        }
    }

    fn switch_to(&mut self, index: usize) {
        if index == self.active || index >= self.workspaces.len() {
            return;
        }
        self.last_workspace = Some(self.active);
        self.active = index;
        self.workspaces[index].normalize_focus();
    }

    fn ensure_workspaces(&mut self, index: usize) {
        while self.workspaces.len() <= index {
            let tiling_enabled = self.config.autotile;
            self.workspaces.push(Workspace::new(tiling_enabled));
        }
    }

    /// Remove empty trailing workspaces (COSMIC's dynamic workspace behaviour).  If the active
    /// workspace was the trailing empty one, the previous workspace becomes active.
    fn trim_workspaces(&mut self) {
        while self.workspaces.len() > 1
            && self
                .workspaces
                .last()
                .is_some_and(|workspace| workspace.is_empty())
        {
            self.workspaces.pop();
        }
        if self.active >= self.workspaces.len() {
            self.active = self.workspaces.len() - 1;
        }
        if self
            .last_workspace
            .is_some_and(|last| last >= self.workspaces.len() || last == self.active)
        {
            self.last_workspace = None;
        }
    }

    fn move_focused_to(&mut self, target: usize) {
        if target == self.active || target >= self.workspaces.len() {
            return;
        }
        let source = self.active;
        let Some(wid) = self.workspaces[source].focus else {
            return;
        };

        let floating_placement = if let Some(position) = self.workspaces[source]
            .floating
            .iter()
            .position(|floating| floating.wid == wid)
        {
            Some(self.workspaces[source].floating.remove(position).frame)
        } else {
            None
        };
        if floating_placement.is_none() {
            self.remove_window_from(source, wid);
        }

        let target_tiling = self.workspaces[target].tiling_enabled;
        if let Some(frame) = floating_placement {
            let workspace = &mut self.workspaces[target];
            workspace.floating.push(FloatingWindow { wid, frame });
            workspace.cascade += 1;
        } else if target_tiling {
            self.insert_tiled(target, wid, None);
        } else {
            let info = self.infos.get(&wid).cloned().unwrap_or_default();
            let cascade = self.workspaces[target].cascade % CASCADE_WRAP;
            let frame = floating_frame(self.work_area, &info, cascade);
            let workspace = &mut self.workspaces[target];
            workspace.floating.push(FloatingWindow { wid, frame });
            workspace.cascade += 1;
        }

        let workspace = &mut self.workspaces[target];
        if workspace.focus.is_none() {
            workspace.set_focus(wid);
        } else if !workspace.focus_stack.contains(&wid) {
            workspace.focus_stack.push(wid);
        }

        let workspace = &mut self.workspaces[source];
        if workspace.maximized.is_some_and(|max| max.wid == wid) {
            workspace.maximized = None;
        }
        workspace.cleanup_focus(wid);
        if source == self.active {
            workspace.normalize_focus();
        }
        self.trim_workspaces();
    }

    // -------------------------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------------------------

    fn window_workspace(&self, wid: WindowId) -> Option<usize> {
        self.workspaces
            .iter()
            .position(|workspace| workspace.contains(wid))
    }

    fn remove_window_from(&mut self, index: usize, wid: WindowId) {
        let workspace = &mut self.workspaces[index];
        if let Some(position) = workspace
            .floating
            .iter()
            .position(|floating| floating.wid == wid)
        {
            workspace.floating.remove(position);
            return;
        }
        if let Some((stack_id, member)) = workspace.tree.stack_containing(wid) {
            workspace.tree.remove_stack_member(stack_id, member);
            if workspace
                .tree
                .stack(stack_id)
                .is_some_and(|stack| stack.windows.is_empty())
            {
                workspace.tree.detach(stack_id);
            }
            return;
        }
        if let Some(node) = workspace.tree.window_node(wid) {
            workspace.tree.detach(node);
        }
    }

    /// Insert a tiled window next to the current tiling focus, choosing the split orientation
    /// from `aspect` (or the reference frame's aspect).
    fn insert_tiled(&mut self, index: usize, wid: WindowId, aspect: Option<(f64, f64)>) {
        let reference = self.tiling_reference(index);
        let orientation = self.insertion_orientation(index, reference, aspect);
        self.workspaces[index]
            .tree
            .insert_window(wid, reference, orientation);
    }

    fn tiling_reference(&self, index: usize) -> Option<NodeId> {
        let workspace = &self.workspaces[index];
        if let Some(focus) = workspace.focus
            && let Some(node) = workspace.tree.node_for_window(focus)
        {
            return Some(node);
        }
        for wid in &workspace.focus_stack {
            if let Some(node) = workspace.tree.node_for_window(*wid) {
                return Some(node);
            }
        }
        workspace
            .tree
            .visible_windows()
            .first()
            .and_then(|wid| workspace.tree.node_for_window(*wid))
    }

    fn insertion_orientation(
        &self,
        index: usize,
        reference: Option<NodeId>,
        aspect: Option<(f64, f64)>,
    ) -> Orientation {
        let (width, height) = match aspect {
            Some((width, height)) => (width, height),
            None => match reference {
                Some(node) => {
                    let geometry = geometry::compute(
                        &self.workspaces[index].tree,
                        self.work_area,
                        self.config.gaps,
                        &self.infos,
                    );
                    geometry
                        .nodes
                        .get(&node)
                        .map(|rect| (rect.width, rect.height))
                        .unwrap_or((self.work_area.width, self.work_area.height))
                }
                None => (self.work_area.width, self.work_area.height),
            },
        };
        Orientation::from_aspect(width, height)
    }

    fn floating_frame(&self, index: usize, info: &ToplevelInfo) -> Rect {
        let cascade = self.workspaces[index].cascade % CASCADE_WRAP;
        floating_frame(self.work_area, info, cascade)
    }

    fn workspace_frames(&self, index: usize) -> WorkspaceFrames {
        let workspace = &self.workspaces[index];
        let geometry = geometry::compute(
            &workspace.tree,
            self.work_area,
            self.config.gaps,
            &self.infos,
        );
        let mut visible = geometry.frames;
        let mut all = visible.clone();
        for (stack_id, frame) in &geometry.stacks {
            if let Some(stack) = workspace.tree.stack(*stack_id) {
                for wid in &stack.windows {
                    all.insert(*wid, *frame);
                }
            }
        }
        for floating in &workspace.floating {
            visible.insert(floating.wid, floating.frame);
            all.insert(floating.wid, floating.frame);
        }
        if let Some(maximized) = workspace.maximized
            && visible.contains_key(&maximized.wid)
        {
            visible.insert(maximized.wid, self.work_area);
            all.insert(maximized.wid, self.work_area);
        }
        WorkspaceFrames {
            visible,
            all,
            nodes: geometry.nodes,
        }
    }

    /// Diff the desired visible state against the last emitted state.
    fn sync(&mut self) -> Vec<Command> {
        self.workspaces[self.active].normalize_focus();

        let active_index = (self.active + 1) as u32;
        let frames = self.workspace_frames(self.active);
        let focus = self.workspaces[self.active]
            .focus
            .filter(|wid| frames.visible.contains_key(wid));
        let stack = focused_stack_info(
            &self.workspaces[self.active],
            active_index,
            &frames.visible,
            &frames.nodes,
            &self.infos,
        );

        let mut commands = Vec::new();
        if self.emitted.workspace != Some(active_index) {
            commands.push(Command::ActivateWorkspace {
                index: active_index,
            });
            self.emitted.workspace = Some(active_index);
        }

        let hides: Vec<WindowId> = self
            .emitted
            .visible
            .iter()
            .filter(|wid| !frames.visible.contains_key(wid))
            .copied()
            .collect();
        for wid in hides {
            commands.push(Command::Hide { wid });
            self.emitted.visible.remove(&wid);
            self.emitted.frames.remove(&wid);
        }

        let shows: Vec<WindowId> = frames
            .visible
            .keys()
            .filter(|wid| !self.emitted.visible.contains(wid))
            .copied()
            .collect();
        for wid in shows {
            commands.push(Command::Show { wid });
            self.emitted.visible.insert(wid);
        }

        for (wid, rect) in &frames.visible {
            if self.emitted.frames.get(wid) != Some(rect) {
                commands.push(Command::SetFrame {
                    wid: *wid,
                    rect: *rect,
                });
                self.emitted.frames.insert(*wid, *rect);
            }
        }

        if focus != self.emitted.focus {
            match focus {
                Some(wid) => {
                    commands.push(Command::Focus { wid });
                    commands.push(Command::Raise { wid });
                }
                None => {
                    if self.emitted.focus.is_some() {
                        commands.push(Command::Unfocus);
                    }
                }
            }
            self.emitted.focus = focus;
        }

        if stack != self.emitted.stack {
            commands.push(Command::StackChanged {
                stack: stack.clone(),
            });
            self.emitted.stack = stack;
        }

        commands
    }
}

// -----------------------------------------------------------------------------------------
// Free helpers
// -----------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Neighbour {
    Window(WindowId),
    Stack(WindowId),
}

#[derive(Debug, Clone, Copy)]
enum Location {
    Plain(NodeId),
    Stack(NodeId, usize),
    Floating(usize),
}

fn locate(workspace: &Workspace, wid: WindowId) -> Option<Location> {
    if let Some(index) = workspace
        .floating
        .iter()
        .position(|floating| floating.wid == wid)
    {
        return Some(Location::Floating(index));
    }
    if let Some((stack_id, member)) = workspace.tree.stack_containing(wid) {
        return Some(Location::Stack(stack_id, member));
    }
    workspace.tree.window_node(wid).map(Location::Plain)
}

/// Total order for directional-focus candidates: nearest first, then the one perpendicular
/// to the focus direction, then topmost, then leftmost, then the lowest window id.  Used by
/// both the directional pass and the wrap-around pass so ties never depend on iteration order.
fn compare_focus_candidates(
    a: &(WindowId, f64, f64, f64, f64),
    b: &(WindowId, f64, f64, f64, f64),
) -> std::cmp::Ordering {
    a.1.total_cmp(&b.1)
        .then(a.2.total_cmp(&b.2))
        .then(a.3.total_cmp(&b.3))
        .then(a.4.total_cmp(&b.4))
        .then(a.0.cmp(&b.0))
}

fn adjacent_sibling(tree: &Tree, node: NodeId, direction: Direction) -> Option<NodeId> {
    let parent = tree.parent(node)?;
    let group = tree.group(parent)?;
    if !group.orientation.handles(direction) {
        return None;
    }
    let index = group.children.iter().position(|child| *child == node)?;
    let target = if direction.is_forward() {
        index.checked_add(1)
    } else {
        index.checked_sub(1)
    }?;
    group.children.get(target).copied()
}

/// Any sibling of `node`, preferring the next one and falling back to the previous one.
fn adjacent_sibling_any(tree: &Tree, node: NodeId) -> Option<NodeId> {
    let parent = tree.parent(node)?;
    let group = tree.group(parent)?;
    let index = group.children.iter().position(|child| *child == node)?;
    group
        .children
        .get(index + 1)
        .or_else(|| {
            index
                .checked_sub(1)
                .and_then(|prev| group.children.get(prev))
        })
        .copied()
}

fn floating_frame(work_area: Rect, info: &ToplevelInfo, cascade: u32) -> Rect {
    if work_area.is_empty() {
        return Rect::new(work_area.x, work_area.y, 0.0, 0.0);
    }
    let (mut width, mut height) = info
        .initial_frame
        .filter(|frame| frame.width > 0.0 && frame.height > 0.0)
        .map(|frame| (frame.width, frame.height))
        .unwrap_or((
            (work_area.width * DEFAULT_FLOATING_FRACTION).min(DEFAULT_FLOATING_MAX_WIDTH),
            (work_area.height * DEFAULT_FLOATING_FRACTION).min(DEFAULT_FLOATING_MAX_HEIGHT),
        ));
    width = width.min(work_area.width);
    height = height.min(work_area.height);

    let offset = if info.is_dialog {
        0.0
    } else {
        CASCADE_STEP * (cascade % CASCADE_WRAP) as f64
    };
    let x = work_area.x + (work_area.width - width) / 2.0 + offset;
    let y = work_area.y + (work_area.height - height) / 2.0 + offset;
    Rect::new(x, y, width, height).clamp_within(work_area)
}

fn tabs_for(stack: &tree::Stack, infos: &BTreeMap<WindowId, ToplevelInfo>) -> Vec<TabInfo> {
    stack
        .windows
        .iter()
        .map(|wid| TabInfo {
            wid: *wid,
            title: infos
                .get(wid)
                .map(|info| info.title.clone())
                .unwrap_or_default(),
        })
        .collect()
}

fn focused_stack_info(
    workspace: &Workspace,
    workspace_index: u32,
    visible: &BTreeMap<WindowId, Rect>,
    nodes: &BTreeMap<NodeId, Rect>,
    infos: &BTreeMap<WindowId, ToplevelInfo>,
) -> Option<StackInfo> {
    let focus = workspace.focus?;
    let (stack_id, _) = workspace.tree.stack_containing(focus)?;
    let stack = workspace.tree.stack(stack_id)?;
    let frame = stack
        .active_window()
        .and_then(|wid| visible.get(&wid).copied())
        .or_else(|| nodes.get(&stack_id).copied())?;
    Some(StackInfo {
        workspace: workspace_index,
        tabs: tabs_for(stack, infos),
        active: stack.active,
        frame,
    })
}

fn snapshot_node(
    tree: &Tree,
    id: NodeId,
    nodes: &BTreeMap<NodeId, Rect>,
    frames: &BTreeMap<WindowId, Rect>,
    infos: &BTreeMap<WindowId, ToplevelInfo>,
) -> TreeNode {
    let tile = nodes.get(&id).copied().unwrap_or(Rect::ZERO);
    match tree.kind(id) {
        NodeKind::Group(group) => TreeNode::Group {
            orientation: group.orientation,
            frame: tile,
            children: group
                .children
                .iter()
                .map(|child| snapshot_node(tree, *child, nodes, frames, infos))
                .collect(),
        },
        NodeKind::Window(wid) => TreeNode::Window {
            wid: *wid,
            title: infos
                .get(wid)
                .map(|info| info.title.clone())
                .unwrap_or_default(),
            frame: frames.get(wid).copied().unwrap_or(tile),
        },
        NodeKind::Stack(stack) => TreeNode::Stack {
            frame: stack
                .active_window()
                .and_then(|wid| frames.get(&wid).copied())
                .unwrap_or(tile),
            active: stack.active,
            tabs: tabs_for(stack, infos),
        },
    }
}
