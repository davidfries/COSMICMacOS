// SPDX-License-Identifier: GPL-3.0-or-later
//! Unix-socket request/response envelope shared by the daemon and `cosmic-mac-ctl`.
//!
//! The socket path is `~/Library/Application Support/cosmic/cosmic-mac-wm.sock`.  One JSON
//! document per line is written in each direction; this crate only defines the payloads.

use serde::{Deserialize, Serialize};

use crate::{Action, Config, TreeNode, WindowId, WindowState, WorkspaceInfo};

/// A request from a client to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "request", content = "data", rename_all = "snake_case")]
pub enum Request {
    /// List every managed window.
    ListWindows,
    /// List the workspace pool.
    ListWorkspaces,
    /// Dump the active workspace's tiling tree.
    Tree,
    /// Perform a window-management action.
    Action(Action),
    /// Focus a specific window (convenience for `cosmic-mac-ctl focus <wid>`).
    Focus { wid: WindowId },
    /// Read the layout configuration.
    Config,
    /// Unpark every window and restore its pre-manager frame.
    Restore,
}

/// The daemon's reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "response", content = "data", rename_all = "snake_case")]
pub enum Response {
    Windows(Vec<WindowState>),
    Workspaces(Vec<WorkspaceInfo>),
    Tree(Option<TreeNode>),
    Ok,
    Config(Config),
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::{Request, Response};
    use crate::{Action, Config, Direction};

    #[test]
    fn request_action_round_trip() {
        let request = Request::Action(Action::Focus {
            direction: Direction::Right,
        });
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"request":"action","data":{"action":"focus","args":{"direction":"right"}}}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
    }

    #[test]
    fn unit_requests_round_trip() {
        for request in [
            Request::ListWindows,
            Request::ListWorkspaces,
            Request::Tree,
            Request::Config,
            Request::Restore,
        ] {
            let json = serde_json::to_string(&request).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
        }
    }

    #[test]
    fn focus_request_round_trip() {
        let request = Request::Focus { wid: 99 };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(json, r#"{"request":"focus","data":{"wid":99}}"#);
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
    }

    #[test]
    fn response_round_trip() {
        for response in [
            Response::Ok,
            Response::Error("nope".into()),
            Response::Windows(Vec::new()),
            Response::Workspaces(Vec::new()),
            Response::Tree(None),
            Response::Config(Config::default()),
        ] {
            let json = serde_json::to_string(&response).unwrap();
            assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), response);
        }
    }
}
