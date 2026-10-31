// SPDX-License-Identifier: GPL-3.0-or-later
//! Commands emitted by the layout engine and applied by the daemon.

use serde::{Deserialize, Serialize};

use crate::{Rect, StackInfo, WindowId};

/// A single side effect the daemon must perform.
///
/// The layout engine never touches the platform.  After every event it diffs the desired
/// state against what it last emitted and returns only the commands that changed.  Command
/// order is deterministic: workspace activation, hides, shows, frames, focus, raise, tab bar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    /// Set a window's frame.  For tiled windows this is the exact tiling slot; for floating
    /// windows it is the placement chosen by the layout engine.
    SetFrame { wid: WindowId, rect: Rect },
    /// Raise a window above its siblings (used for focus and new windows).
    Raise { wid: WindowId },
    /// Park a window: inactive stack members and windows on inactive workspaces are hidden by
    /// moving them off-screen (the daemon uses the corner-park technique).
    Hide { wid: WindowId },
    /// Unpark a window and make it visible again.
    Show { wid: WindowId },
    /// Give keyboard focus without raising (macOS focus-without-raise).
    Focus { wid: WindowId },
    /// Clear keyboard focus (e.g. after switching to an empty workspace).
    Unfocus,
    /// Ask the platform to close the window.  The window is not removed from the layout until
    /// the platform reports `WindowRemoved`.
    Close { wid: WindowId },
    /// The active workspace changed; the daemon updates the display assignment / panel.
    ActivateWorkspace { index: u32 },
    /// The focused stack changed (composition, active tab, frame) or the tab bar should be
    /// hidden.  `None` means "no stack is focused".
    StackChanged { stack: Option<StackInfo> },
}

#[cfg(test)]
mod tests {
    use super::Command;
    use crate::{Rect, StackInfo, TabInfo};

    #[test]
    fn set_frame_is_tagged() {
        let command = Command::SetFrame {
            wid: 7,
            rect: Rect::new(1.0, 2.0, 3.0, 4.0),
        };
        let value = serde_json::to_value(&command).unwrap();
        assert_eq!(value["command"], "set_frame");
        assert_eq!(value["wid"], 7);
        assert_eq!(value["rect"]["width"], 3.0);
        assert_eq!(serde_json::from_value::<Command>(value).unwrap(), command);
    }

    #[test]
    fn unit_commands_round_trip() {
        let command = Command::Unfocus;
        let json = serde_json::to_string(&command).unwrap();
        assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), command);
    }

    #[test]
    fn stack_changed_round_trip() {
        let command = Command::StackChanged {
            stack: Some(StackInfo {
                workspace: 1,
                tabs: vec![TabInfo {
                    wid: 1,
                    title: "one".into(),
                }],
                active: 0,
                frame: Rect::new(0.0, 0.0, 100.0, 100.0),
            }),
        };
        let json = serde_json::to_string(&command).unwrap();
        assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), command);
        let none = Command::StackChanged { stack: None };
        let json = serde_json::to_string(&none).unwrap();
        assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), none);
    }
}
