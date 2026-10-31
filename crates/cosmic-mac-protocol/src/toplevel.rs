// SPDX-License-Identifier: GPL-3.0-or-later
//! Toplevel (window) descriptions and toplevel-management state.
//!
//! These types follow the semantics of `cosmic_toplevel_info_v1` and
//! `cosmic_toplevel_management_v1`, adapted to macOS concepts:
//!
//! * `app_id` is the application bundle identifier (`NSRunningApplication.bundleIdentifier`).
//! * `role` / `subrole` are the Accessibility role and subrole (`AXWindow`, `AXDialog`, …).
//! * `initial_frame` is the window's frame when it was first observed.  The protocol
//!   toplevel-info interface does not carry geometry, but the layout engine needs a size to
//!   place floating windows, and macOS hands us the frame for free via AX.

use serde::{Deserialize, Serialize};

use crate::{Rect, Size, WindowId};

fn default_true() -> bool {
    true
}

/// Everything the window manager knows about a toplevel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToplevelInfo {
    /// Stable window id (CoreGraphics / SkyLight window number).
    pub wid: WindowId,
    /// Process id of the owning application.
    pub pid: i32,
    /// Bundle identifier of the owning application ("" when unknown).
    pub app_id: String,
    /// Window title ("" when unknown).
    pub title: String,
    /// Accessibility role, e.g. `AXWindow`.
    #[serde(default)]
    pub role: Option<String>,
    /// Accessibility subrole, e.g. `AXDialog`, `AXStandardWindow`.
    #[serde(default)]
    pub subrole: Option<String>,
    /// Whether the window can be resized.  Non-resizable windows float by default.
    #[serde(default = "default_true")]
    pub resizable: bool,
    /// Whether the window is a dialog.  Dialogs float by default and are centred.
    #[serde(default)]
    pub is_dialog: bool,
    /// Frame observed when the window was registered.  Used for floating placement.
    #[serde(default)]
    pub initial_frame: Option<Rect>,
    /// Minimum size the application accepts, when known.
    ///
    /// Discovered by the platform's AX probe (or reported by the app) and learned by the
    /// daemon when a `SetFrame` is clamped.  `None` means "no known minimum".
    #[serde(default)]
    pub min_size: Option<Size>,
    /// Maximum size the application accepts, when known.
    ///
    /// `None` means "no known maximum"; an axis may still be unbounded when the other is
    /// constrained (see [`Size::UNBOUNDED`]).
    #[serde(default)]
    pub max_size: Option<Size>,
}

impl Default for ToplevelInfo {
    fn default() -> Self {
        ToplevelInfo {
            wid: 0,
            pid: 0,
            app_id: String::new(),
            title: String::new(),
            role: None,
            subrole: None,
            resizable: true,
            is_dialog: false,
            initial_frame: None,
            min_size: None,
            max_size: None,
        }
    }
}

impl ToplevelInfo {
    /// Convenience constructor for tests and IPC clients.
    pub fn new(wid: WindowId, app_id: impl Into<String>, title: impl Into<String>) -> Self {
        ToplevelInfo {
            wid,
            app_id: app_id.into(),
            title: title.into(),
            ..ToplevelInfo::default()
        }
    }

    pub fn with_pid(mut self, pid: i32) -> Self {
        self.pid = pid;
        self
    }

    pub fn with_resizable(mut self, resizable: bool) -> Self {
        self.resizable = resizable;
        self
    }

    pub fn with_dialog(mut self, is_dialog: bool) -> Self {
        self.is_dialog = is_dialog;
        self
    }

    pub fn with_initial_frame(mut self, frame: Rect) -> Self {
        self.initial_frame = Some(frame);
        self
    }

    /// Attach the minimum size the application accepts.
    pub fn with_min_size(mut self, min: Size) -> Self {
        self.min_size = Some(min);
        self
    }

    /// Attach the maximum size the application accepts.
    pub fn with_max_size(mut self, max: Size) -> Self {
        self.max_size = Some(max);
        self
    }
}

/// State flags exposed through toplevel management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToplevelState {
    Activated,
    Maximized,
    Minimized,
    Fullscreen,
    TiledLeft,
    TiledRight,
    TiledTop,
    TiledBottom,
}

/// Operations the daemon can perform on a toplevel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToplevelCapability {
    Activate,
    Close,
    Maximize,
    Minimize,
    Fullscreen,
    Move,
    Resize,
}

/// The toplevel-management view of a single window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToplevelManagement {
    pub wid: WindowId,
    #[serde(default)]
    pub states: Vec<ToplevelState>,
    #[serde(default)]
    pub capabilities: Vec<ToplevelCapability>,
}

impl ToplevelManagement {
    /// The capabilities a normal resizable macOS window supports.
    pub fn standard(wid: WindowId) -> Self {
        ToplevelManagement {
            wid,
            states: Vec::new(),
            capabilities: vec![
                ToplevelCapability::Activate,
                ToplevelCapability::Close,
                ToplevelCapability::Maximize,
                ToplevelCapability::Minimize,
                ToplevelCapability::Fullscreen,
                ToplevelCapability::Move,
                ToplevelCapability::Resize,
            ],
        }
    }

    pub fn has_state(&self, state: ToplevelState) -> bool {
        self.states.contains(&state)
    }

    pub fn has_capability(&self, capability: ToplevelCapability) -> bool {
        self.capabilities.contains(&capability)
    }
}

/// A tab in a stack.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabInfo {
    pub wid: WindowId,
    pub title: String,
}

/// The window-manager's full view of one window, as returned by `cosmic-mac-ctl list-windows`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub info: ToplevelInfo,
    /// 1-based workspace index.
    pub workspace: u32,
    /// True when the window is in the floating layer.
    pub floating: bool,
    /// True when the window occupies a frame on the active workspace.  Inactive stack members
    /// and windows on other workspaces are not visible.
    pub visible: bool,
    pub focused: bool,
    /// True when the window is a member of a stack.
    pub in_stack: bool,
    /// True when this window is the active (displayed) member of its stack.
    pub stack_active: bool,
    /// The frame the window has or would have in the layout.
    pub frame: Rect,
    /// The frame the window really has on screen, read from Accessibility when the snapshot
    /// was taken.  `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_frame: Option<Rect>,
}

#[cfg(test)]
mod tests {
    use super::{TabInfo, ToplevelCapability, ToplevelInfo, ToplevelManagement, ToplevelState};
    use crate::{Rect, Size};

    #[test]
    fn info_defaults_are_sane() {
        let info = ToplevelInfo::new(7, "com.apple.TextEdit", "Untitled");
        assert_eq!(info.wid, 7);
        assert!(info.resizable);
        assert!(!info.is_dialog);
        assert!(info.initial_frame.is_none());
        assert!(info.role.is_none());
        assert!(info.min_size.is_none());
        assert!(info.max_size.is_none());
    }

    #[test]
    fn info_builder_and_serde() {
        let info = ToplevelInfo::new(3, "com.apple.Safari", "Example")
            .with_pid(42)
            .with_resizable(false)
            .with_dialog(true)
            .with_initial_frame(Rect::new(10.0, 20.0, 300.0, 200.0))
            .with_min_size(Size::new(642.0, 100.0))
            .with_max_size(Size::new(4000.0, Size::UNBOUNDED.height));
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(serde_json::from_str::<ToplevelInfo>(&json).unwrap(), info);
        assert_eq!(info.min_size, Some(Size::new(642.0, 100.0)));
        assert!(!info.max_size.unwrap().is_unbounded());
    }

    #[test]
    fn info_deserializes_with_defaults() {
        let json = r#"{"wid":1,"pid":2,"app_id":"a","title":"t"}"#;
        let info: ToplevelInfo = serde_json::from_str(json).unwrap();
        assert!(info.resizable);
        assert!(!info.is_dialog);
        assert_eq!(info.initial_frame, None);
        assert_eq!(info.min_size, None);
        assert_eq!(info.max_size, None);
    }

    #[test]
    fn size_constraints_round_trip_through_ipc_shape() {
        let info = ToplevelInfo::new(9, "a", "b")
            .with_min_size(Size::new(200.0, 100.0))
            .with_max_size(Size::UNBOUNDED);
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(value["min_size"]["width"], 200.0);
        assert_eq!(value["max_size"]["height"], f64::MAX);
        let back: ToplevelInfo = serde_json::from_value(value).unwrap();
        assert_eq!(back, info);
    }

    #[test]
    fn management_capabilities() {
        let management = ToplevelManagement::standard(9);
        assert!(management.has_capability(ToplevelCapability::Close));
        assert!(management.has_capability(ToplevelCapability::Resize));
        assert!(!management.has_state(ToplevelState::Maximized));
    }

    #[test]
    fn tab_info_round_trip() {
        let tab = TabInfo {
            wid: 5,
            title: "tab".into(),
        };
        let json = serde_json::to_string(&tab).unwrap();
        assert_eq!(serde_json::from_str::<TabInfo>(&json).unwrap(), tab);
    }

    #[test]
    fn state_names_are_snake_case() {
        let json = serde_json::to_string(&ToplevelState::TiledLeft).unwrap();
        assert_eq!(json, "\"tiled_left\"");
        let json = serde_json::to_string(&ToplevelCapability::Fullscreen).unwrap();
        assert_eq!(json, "\"fullscreen\"");
    }
}
