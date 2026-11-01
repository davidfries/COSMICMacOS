// SPDX-License-Identifier: GPL-3.0-or-later
//! Unix-socket request/response envelope shared by the daemon and `cosmic-mac-ctl`.
//!
//! The socket path is `~/Library/Application Support/cosmic/cosmic-mac-wm.sock`.  One JSON
//! document per line is written in each direction; this crate only defines the payloads.
//!
//! Every request gets exactly one [`Response`] line, and a connection may carry any number of
//! requests, except [`Request::Subscribe`]: it turns the connection into a one-way stream of
//! [`Response::State`] lines (the current state at once, then one line per change) until
//! either side closes it.  The daemon ignores anything else the client writes on a subscribed
//! connection.
//!
//! ```text
//! -> {"request":"subscribe"}
//! <- {"response":"state","data":{"workspaces":[{"index":1,"name":"1","active":true,"window_count":2}],"tiling":true,"focused_floating":false,"max_workspaces":9}}
//! <- {"response":"state","data":{"workspaces":[...],"tiling":false,"focused_floating":true,"max_workspaces":9}}
//! ```

use serde::{Deserialize, Serialize};

use crate::{Action, Config, ShellState, TreeNode, WindowId, WindowState, WorkspaceInfo};

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
    /// Stream the shell state: [`Response::State`] now and after every change, on this
    /// connection, until it is closed.  See the module documentation.
    Subscribe,
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
    /// A shell-state snapshot, streamed after [`Request::Subscribe`].
    State(ShellState),
}

#[cfg(test)]
mod tests {
    use super::{Request, Response};
    use crate::{Action, Config, Direction, ShellState, WorkspaceInfo};

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
            Request::Subscribe,
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
            Response::State(ShellState {
                workspaces: Vec::new(),
                tiling: false,
                focused_floating: None,
                max_workspaces: 9,
            }),
        ] {
            let json = serde_json::to_string(&response).unwrap();
            assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), response);
        }
    }

    #[test]
    fn subscribe_request_wire_format() {
        let json = serde_json::to_string(&Request::Subscribe).unwrap();
        assert_eq!(json, r#"{"request":"subscribe"}"#);
        assert_eq!(
            serde_json::from_str::<Request>(&json).unwrap(),
            Request::Subscribe
        );
    }

    #[test]
    fn state_response_wire_format() {
        let response = Response::State(ShellState {
            workspaces: vec![WorkspaceInfo::new(1, "1", true, 2)],
            tiling: true,
            focused_floating: Some(false),
            max_workspaces: 9,
        });
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(
            json,
            r#"{"response":"state","data":{"workspaces":[{"index":1,"name":"1","active":true,"window_count":2}],"tiling":true,"focused_floating":false,"max_workspaces":9}}"#
        );
        assert_eq!(serde_json::from_str::<Response>(&json).unwrap(), response);
    }

    /// Clients written before subscriptions existed still decode every reply they can
    /// receive: the new variants only appear on a subscribed connection.
    #[test]
    fn existing_wire_formats_are_unchanged() {
        assert_eq!(
            serde_json::to_string(&Request::ListWorkspaces).unwrap(),
            r#"{"request":"list_workspaces"}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::Ok).unwrap(),
            r#"{"response":"ok"}"#
        );
        assert_eq!(
            serde_json::to_string(&Response::Workspaces(vec![WorkspaceInfo::new(
                1, "1", true, 0
            )]))
            .unwrap(),
            r#"{"response":"workspaces","data":[{"index":1,"name":"1","active":true,"window_count":0}]}"#
        );
    }
}
