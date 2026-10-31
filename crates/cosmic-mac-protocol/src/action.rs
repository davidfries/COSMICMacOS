// SPDX-License-Identifier: GPL-3.0-or-later
//! Directions and window-management actions.
//!
//! Actions mirror the window-management subset of COSMIC's keybinding actions (see
//! cosmic-comp's `data/keybindings.ron`).  They are adjacently tagged so the CLI can send
//! `{"action":"focus","args":{"direction":"left"}}` and the daemon can map its
//! `keybindings.ron` adapter onto them.

use serde::{Deserialize, Serialize};

use crate::WindowId;

/// A cardinal direction, used by focus/move/resize actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub fn opposite(self) -> Self {
        match self {
            Direction::Left => Direction::Right,
            Direction::Right => Direction::Left,
            Direction::Up => Direction::Down,
            Direction::Down => Direction::Up,
        }
    }

    /// The axis this direction travels along.
    pub fn axis(self) -> Axis {
        match self {
            Direction::Left | Direction::Right => Axis::Horizontal,
            Direction::Up | Direction::Down => Axis::Vertical,
        }
    }

    /// True for `Right`/`Down`: the direction of increasing coordinates.
    pub fn is_forward(self) -> bool {
        matches!(self, Direction::Right | Direction::Down)
    }
}

/// One of the two screen axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// A user-requested window-management action.
///
/// Workspace indices are **1-based**, matching COSMIC's `Super+1` … `Super+9` bindings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", content = "args", rename_all = "snake_case")]
pub enum Action {
    /// Move keyboard focus to the nearest window in `direction` (wraps at the screen edge).
    Focus {
        direction: Direction,
    },
    /// Focus a specific window.  Switches to its workspace if necessary.
    FocusWindow {
        wid: WindowId,
    },
    /// Cycle focus forward through the workspace's focus history.
    FocusNext,
    /// Cycle focus backward through the workspace's focus history.
    FocusPrevious,
    /// Move the focused window one slot in `direction`, swapping with its neighbour.
    /// Moving into a stack adds the window as a new stack member.
    Move {
        direction: Direction,
    },
    /// Swap the focused window with the previously focused window in this workspace.
    SwapWindow,
    /// Flip the orientation of the focused window's parent group.
    ToggleOrientation,
    /// Grow the focused window towards `direction` (or shrink it when it is already at the
    /// edge of its group).
    Resize {
        direction: Direction,
    },
    /// Toggle resize mode.  While active the daemon routes arrow keys to [`Action::Resize`].
    ResizeMode,
    /// Toggle maximize for the focused window.
    Maximize,
    /// Toggle tiling for the whole active workspace.
    ToggleTiling,
    /// Move the focused window between the tiling and floating layers.
    ToggleWindowFloating,
    /// Add the focused window to the stack of its neighbour; when already stacked, unstack it.
    ToggleStacking,
    /// Remove the focused window from its stack into its own frame.
    Unstack,
    /// Switch to workspace `index` (1-based), creating intermediate workspaces if needed.
    Workspace {
        index: u32,
    },
    /// Move the focused window to workspace `index` (1-based), staying on the current one.
    MoveToWorkspace {
        index: u32,
    },
    NextWorkspace,
    PreviousWorkspace,
    /// Switch back to the workspace that was active before the current one.
    LastWorkspace,
    /// Move the focused window to the next workspace (wrapping around).
    MoveToNextWorkspace,
    /// Move the focused window to the previous workspace (wrapping around).
    MoveToPreviousWorkspace,
    /// Move the focused window to the previously active workspace.
    MoveToLastWorkspace,
    /// Ask the platform to close the focused window.
    Close,
}

#[cfg(test)]
mod tests {
    use super::{Action, Axis, Direction};

    #[test]
    fn direction_helpers() {
        assert_eq!(Direction::Left.opposite(), Direction::Right);
        assert_eq!(Direction::Up.opposite(), Direction::Down);
        assert_eq!(Direction::Left.axis(), Axis::Horizontal);
        assert_eq!(Direction::Down.axis(), Axis::Vertical);
        assert!(Direction::Right.is_forward());
        assert!(!Direction::Left.is_forward());
        assert!(Direction::Down.is_forward());
        assert!(!Direction::Up.is_forward());
    }

    #[test]
    fn action_serde_is_internally_tagged() {
        let value = serde_json::to_value(Action::Focus {
            direction: Direction::Left,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"action": "focus", "args": {"direction": "left"}})
        );
        let back: Action = serde_json::from_value(value).unwrap();
        assert_eq!(
            back,
            Action::Focus {
                direction: Direction::Left
            }
        );
    }

    #[test]
    fn workspace_action_round_trip() {
        let action = Action::MoveToWorkspace { index: 3 };
        let json = serde_json::to_string(&action).unwrap();
        assert_eq!(json, r#"{"action":"move_to_workspace","args":{"index":3}}"#);
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
    }

    #[test]
    fn unit_actions_round_trip() {
        for action in [
            Action::FocusNext,
            Action::SwapWindow,
            Action::ToggleOrientation,
            Action::ResizeMode,
            Action::Maximize,
            Action::ToggleTiling,
            Action::ToggleWindowFloating,
            Action::ToggleStacking,
            Action::Unstack,
            Action::NextWorkspace,
            Action::PreviousWorkspace,
            Action::LastWorkspace,
            Action::MoveToNextWorkspace,
            Action::MoveToPreviousWorkspace,
            Action::MoveToLastWorkspace,
            Action::Close,
        ] {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), action);
        }
    }

    #[test]
    fn ron_round_trip() {
        let action = Action::Focus {
            direction: Direction::Up,
        };
        let text = ron::to_string(&action).unwrap();
        let back: Action = ron::from_str(&text).unwrap();
        assert_eq!(back, action);
    }
}
