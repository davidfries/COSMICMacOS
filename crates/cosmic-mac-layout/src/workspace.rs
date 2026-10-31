// SPDX-License-Identifier: GPL-3.0-or-later
//! One workspace: tiling tree, floating layer, focus history and workspace-wide flags.

use cosmic_mac_protocol::{Rect, WindowId};

use crate::tree::Tree;

/// A floating window and the frame it currently occupies.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FloatingWindow {
    pub wid: WindowId,
    pub frame: Rect,
}

/// Maximize state.  `restore` is set for floating windows so unmaximizing returns them to
/// their floating frame; tiled windows are recomputed from the tree instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Maximized {
    pub wid: WindowId,
    pub restore: Option<Rect>,
}

#[derive(Debug)]
pub(crate) struct Workspace {
    pub tree: Tree,
    pub floating: Vec<FloatingWindow>,
    /// Currently focused window (tiling or floating).
    pub focus: Option<WindowId>,
    /// Most-recently-used order, current focus first.
    pub focus_stack: Vec<WindowId>,
    /// Cursor used by `FocusNext`/`FocusPrevious` so repeated cycling visits every window.
    pub cycle: usize,
    /// Workspace-wide tiling flag (`ToggleTiling`).
    pub tiling_enabled: bool,
    pub maximized: Option<Maximized>,
    /// Counter used to cascade floating windows.
    pub cascade: u32,
}

impl Workspace {
    pub fn new(tiling_enabled: bool) -> Self {
        Workspace {
            tree: Tree::new(),
            floating: Vec::new(),
            focus: None,
            focus_stack: Vec::new(),
            cycle: 0,
            tiling_enabled,
            maximized: None,
            cascade: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty() && self.floating.is_empty()
    }

    pub fn window_count(&self) -> usize {
        self.tree.window_count() + self.floating.len()
    }

    pub fn contains(&self, wid: WindowId) -> bool {
        self.floating.iter().any(|floating| floating.wid == wid)
            || self.tree.node_for_window(wid).is_some()
    }

    pub fn floating(&self, wid: WindowId) -> Option<&FloatingWindow> {
        self.floating.iter().find(|floating| floating.wid == wid)
    }

    pub fn floating_mut(&mut self, wid: WindowId) -> Option<&mut FloatingWindow> {
        self.floating
            .iter_mut()
            .find(|floating| floating.wid == wid)
    }

    /// First visible window, tiling windows before floating ones.
    pub fn first_window(&self) -> Option<WindowId> {
        self.tree
            .visible_windows()
            .first()
            .copied()
            .or_else(|| self.floating.first().map(|floating| floating.wid))
    }

    /// Focus `wid`, making it the active stack member when it lives in a stack.
    pub fn set_focus(&mut self, wid: WindowId) {
        self.focus = Some(wid);
        self.focus_stack.retain(|entry| *entry != wid);
        self.focus_stack.insert(0, wid);
        self.cycle = 0;
        if let Some((stack_id, index)) = self.tree.stack_containing(wid) {
            self.tree.set_stack_active(stack_id, index);
        }
    }

    /// Cycle through the focus history.  Unlike [`Workspace::set_focus`] this does not reorder
    /// the MRU list, so repeated presses visit every window once before repeating.
    pub fn cycle_focus(&mut self, forward: bool) -> Option<WindowId> {
        let len = self.focus_stack.len();
        if len < 2 {
            return None;
        }
        self.cycle = if forward {
            (self.cycle + 1) % len
        } else {
            (self.cycle + len - 1) % len
        };
        let wid = self.focus_stack[self.cycle];
        self.focus = Some(wid);
        if let Some((stack_id, index)) = self.tree.stack_containing(wid) {
            self.tree.set_stack_active(stack_id, index);
        }
        Some(wid)
    }

    /// Remove `removed` from the focus history and pick a replacement when it was focused.
    pub fn cleanup_focus(&mut self, removed: WindowId) {
        self.focus_stack.retain(|wid| *wid != removed);
        if self.focus == Some(removed) {
            self.focus = self
                .focus_stack
                .iter()
                .copied()
                .find(|wid| self.contains(*wid))
                .or_else(|| self.first_window());
        }
        if self.focus_stack.is_empty() {
            self.cycle = 0;
        } else {
            self.cycle = self.cycle.min(self.focus_stack.len() - 1);
        }
    }

    /// Make sure `focus` points at a live window and that its stack tab is active.
    pub fn normalize_focus(&mut self) {
        if let Some(focus) = self.focus {
            if !self.contains(focus) {
                self.focus = None;
            } else if let Some((stack_id, index)) = self.tree.stack_containing(focus) {
                self.tree.set_stack_active(stack_id, index);
            }
        }
        if self.focus.is_none()
            && let Some(wid) = self.first_window()
        {
            self.set_focus(wid);
        }
    }
}
