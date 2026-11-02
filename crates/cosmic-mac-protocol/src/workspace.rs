// SPDX-License-Identifier: GPL-3.0-or-later
//! Workspace descriptions and stack (tabbed group) views.

use serde::{Deserialize, Serialize};

use crate::{Direction, Rect, TabInfo};

/// How the children of a group are arranged.
///
/// `Horizontal` means children are placed left-to-right (the split line is vertical).
/// See the crate-level note about the difference from upstream `cosmic-comp` naming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    Horizontal,
    Vertical,
}

impl Orientation {
    pub fn flipped(self) -> Self {
        match self {
            Orientation::Horizontal => Orientation::Vertical,
            Orientation::Vertical => Orientation::Horizontal,
        }
    }

    pub fn is_horizontal(self) -> bool {
        matches!(self, Orientation::Horizontal)
    }

    /// True when a group with this orientation can split along `direction`.
    pub fn handles(self, direction: Direction) -> bool {
        match direction {
            Direction::Left | Direction::Right => self.is_horizontal(),
            Direction::Up | Direction::Down => !self.is_horizontal(),
        }
    }

    /// Pick a split orientation for a window with the given size.
    ///
    /// Wide windows are split into columns, tall windows into rows; this keeps new tiles
    /// roughly square, matching cosmic-comp's aspect-ratio heuristic.
    pub fn from_aspect(width: f64, height: f64) -> Self {
        if width >= height {
            Orientation::Horizontal
        } else {
            Orientation::Vertical
        }
    }
}

/// One workspace in the pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    /// 1-based index, matching COSMIC's `Super+1` … `Super+9` bindings.
    pub index: u32,
    /// Display name.  Currently the index as a string, matching COSMIC's default naming.
    pub name: String,
    /// True for the workspace currently shown on the (single) display.
    pub active: bool,
    pub window_count: usize,
}

impl WorkspaceInfo {
    pub fn new(index: u32, name: impl Into<String>, active: bool, window_count: usize) -> Self {
        WorkspaceInfo {
            index,
            name: name.into(),
            active,
            window_count,
        }
    }
}

/// The workspaces belonging to one output.
///
/// cosmicMac milestone 1 models a single workspace pool and lets the daemon own display
/// assignment, so there is exactly one group with `output == None`.  The wrapper exists so a
/// future per-display model can be added without breaking the IPC shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceGroup {
    #[serde(default)]
    pub output: Option<String>,
    pub workspaces: Vec<WorkspaceInfo>,
}

impl WorkspaceGroup {
    pub fn single(workspaces: Vec<WorkspaceInfo>) -> Self {
        WorkspaceGroup {
            output: None,
            workspaces,
        }
    }
}

/// Height in points of the tab-bar strip above a stack's windows; layout reserves it.
pub const STACK_BAR_HEIGHT: f64 = 26.0;

/// A stack (COSMIC "tabbed" group) as seen by the tab-bar overlay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StackInfo {
    /// 1-based workspace index containing the stack.
    pub workspace: u32,
    /// Tabs in display order.
    pub tabs: Vec<TabInfo>,
    /// Index of the active tab in `tabs`.
    pub active: usize,
    /// Frame of the stack's visible window; the tab bar is drawn directly above it.
    pub frame: Rect,
    /// The stack holds the focused window.
    #[serde(default)]
    pub focused: bool,
}

impl StackInfo {
    pub fn active_tab(&self) -> Option<&TabInfo> {
        self.tabs.get(self.active)
    }
}

#[cfg(test)]
mod tests {
    use super::{Orientation, StackInfo, WorkspaceGroup, WorkspaceInfo};
    use crate::{Rect, TabInfo};

    #[test]
    fn orientation_flip_and_handles() {
        assert_eq!(Orientation::Horizontal.flipped(), Orientation::Vertical);
        assert!(Orientation::Horizontal.handles(crate::Direction::Left));
        assert!(Orientation::Horizontal.handles(crate::Direction::Right));
        assert!(!Orientation::Horizontal.handles(crate::Direction::Up));
        assert!(Orientation::Vertical.handles(crate::Direction::Down));
    }

    #[test]
    fn orientation_from_aspect() {
        assert_eq!(
            Orientation::from_aspect(1000.0, 500.0),
            Orientation::Horizontal
        );
        assert_eq!(
            Orientation::from_aspect(500.0, 1000.0),
            Orientation::Vertical
        );
        assert_eq!(
            Orientation::from_aspect(500.0, 500.0),
            Orientation::Horizontal
        );
    }

    #[test]
    fn workspace_info_round_trip() {
        let info = WorkspaceInfo::new(2, "2", true, 3);
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(serde_json::from_str::<WorkspaceInfo>(&json).unwrap(), info);
    }

    #[test]
    fn workspace_group_round_trip() {
        let group = WorkspaceGroup::single(vec![WorkspaceInfo::new(1, "1", true, 0)]);
        let json = serde_json::to_string(&group).unwrap();
        assert_eq!(
            serde_json::from_str::<WorkspaceGroup>(&json).unwrap(),
            group
        );
    }

    #[test]
    fn stack_active_tab() {
        let stack = StackInfo {
            workspace: 1,
            tabs: vec![
                TabInfo {
                    wid: 1,
                    title: "a".into(),
                },
                TabInfo {
                    wid: 2,
                    title: "b".into(),
                },
            ],
            active: 1,
            frame: Rect::new(0.0, 0.0, 100.0, 100.0),
            focused: true,
        };
        assert_eq!(stack.active_tab().unwrap().wid, 2);
    }

    #[test]
    fn stack_info_without_focused_reads_as_unfocused() {
        let json = r#"{"workspace":1,"tabs":[],"active":0,"frame":{"x":0.0,"y":0.0,"width":1.0,"height":1.0}}"#;
        let stack: StackInfo = serde_json::from_str(json).unwrap();
        assert!(!stack.focused);
    }
}
