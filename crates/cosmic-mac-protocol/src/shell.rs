// SPDX-License-Identifier: GPL-3.0-or-later
//! The shell-state snapshot streamed to [`Request::Subscribe`](crate::Request::Subscribe)
//! clients such as the menu-bar status item.

use serde::{Deserialize, Serialize};

use crate::WorkspaceInfo;

/// What a shell client shows about the window manager.
///
/// The daemon sends one as [`Response::State`](crate::Response::State) right after
/// [`Request::Subscribe`](crate::Request::Subscribe), then another whenever any field changes.
/// Two consecutive states on one subscription always differ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellState {
    /// The workspace pool in order, the same records as
    /// [`Response::Workspaces`](crate::Response::Workspaces).
    pub workspaces: Vec<WorkspaceInfo>,
    /// Whether tiling is enabled on the active workspace
    /// ([`Action::ToggleTiling`](crate::Action::ToggleTiling) flips it).
    pub tiling: bool,
    /// Whether the active workspace's focused window floats; `None` when no window is focused.
    #[serde(default)]
    pub focused_floating: Option<bool>,
    /// Highest index [`Action::Workspace`](crate::Action::Workspace) accepts (COSMIC binds
    /// `Super+1` … `Super+9`).
    pub max_workspaces: u32,
}

impl ShellState {
    /// The active workspace, if the pool reports one.
    pub fn active_workspace(&self) -> Option<&WorkspaceInfo> {
        self.workspaces.iter().find(|workspace| workspace.active)
    }
}

#[cfg(test)]
mod tests {
    use super::ShellState;
    use crate::WorkspaceInfo;

    fn state() -> ShellState {
        ShellState {
            workspaces: vec![
                WorkspaceInfo::new(1, "1", false, 2),
                WorkspaceInfo::new(2, "2", true, 1),
            ],
            tiling: true,
            focused_floating: Some(false),
            max_workspaces: 9,
        }
    }

    #[test]
    fn shell_state_round_trip() {
        let state = state();
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(
            json,
            r#"{"workspaces":[{"index":1,"name":"1","active":false,"window_count":2},{"index":2,"name":"2","active":true,"window_count":1}],"tiling":true,"focused_floating":false,"max_workspaces":9}"#
        );
        assert_eq!(serde_json::from_str::<ShellState>(&json).unwrap(), state);
    }

    #[test]
    fn focused_floating_may_be_null_or_absent() {
        let null = r#"{"workspaces":[],"tiling":false,"focused_floating":null,"max_workspaces":9}"#;
        let absent = r#"{"workspaces":[],"tiling":false,"max_workspaces":9}"#;
        for json in [null, absent] {
            let state: ShellState = serde_json::from_str(json).unwrap();
            assert_eq!(state.focused_floating, None);
        }
    }

    #[test]
    fn active_workspace_finds_the_active_entry() {
        assert_eq!(state().active_workspace().map(|info| info.index), Some(2));
        let none = ShellState {
            workspaces: Vec::new(),
            ..state()
        };
        assert_eq!(none.active_workspace(), None);
    }
}
