// SPDX-License-Identifier: GPL-3.0-or-later
//! The shell state streamed to `Request::Subscribe` clients (the menu-bar status item).
//!
//! The daemon loop calls [`Publisher::update`] after every message it handles.  The
//! publisher keeps the latest [`ShellState`] in a `tokio::sync::watch` channel and wakes
//! subscribers only when it differs from the previous one; the IPC server
//! (`crate::ipc`) writes the current state to each new subscriber and then every change.
//!
//! A watch channel rather than a broadcast one fits a state stream: a subscriber that falls
//! behind skips intermediate states instead of lagging or erroring, and a new subscriber
//! starts from the current state.  Nothing here can block the daemon loop.

use cosmic_mac_layout::{Layout, MAX_WORKSPACES};
use cosmic_mac_protocol::ShellState;
use tokio::sync::watch;

/// The shell state for `layout`.  Cheap enough to run after every message: the workspace
/// pool has at most [`MAX_WORKSPACES`] entries and no frames are computed.
pub fn snapshot(layout: &Layout) -> ShellState {
    ShellState {
        workspaces: layout.workspaces(),
        tiling: layout.tiling_enabled(),
        focused_floating: layout.focused().map(|wid| layout.is_floating(wid)),
        max_workspaces: MAX_WORKSPACES,
    }
}

/// Holds the latest shell state and notifies subscribers of changes.
pub struct Publisher {
    tx: watch::Sender<ShellState>,
}

impl Publisher {
    /// A publisher whose current state is `layout`'s.
    pub fn new(layout: &Layout) -> Self {
        Publisher::with_state(snapshot(layout))
    }

    /// A publisher with an explicit initial state.
    pub fn with_state(state: ShellState) -> Self {
        let (tx, _rx) = watch::channel(state);
        Publisher { tx }
    }

    /// A receiver for one IPC connection.  Its current value is the latest state.
    pub fn subscribe(&self) -> watch::Receiver<ShellState> {
        self.tx.subscribe()
    }

    /// Publish `layout`'s shell state.  Returns whether it changed.
    pub fn update(&self, layout: &Layout) -> bool {
        self.publish(snapshot(layout))
    }

    /// Store `state` and wake the subscribers, unless it equals the current state.
    /// Returns whether it changed.  Works with or without subscribers.
    pub fn publish(&self, state: ShellState) -> bool {
        self.tx.send_if_modified(|current| {
            if *current == state {
                false
            } else {
                *current = state;
                true
            }
        })
    }

    /// The current state.
    pub fn current(&self) -> ShellState {
        self.tx.borrow().clone()
    }

    /// Live receivers: the IPC server's own plus one per open connection
    /// (diagnostics and tests).
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use cosmic_mac_protocol::{Action, Config, Rect, ToplevelInfo, WorkspaceInfo};

    fn layout_with(windows: u64) -> Layout {
        let mut layout =
            Layout::with_work_area(Config::default(), Rect::new(0.0, 0.0, 1000.0, 500.0));
        for wid in 1..=windows {
            layout.window_added(ToplevelInfo::new(wid, "com.example.App", format!("w{wid}")));
        }
        layout
    }

    #[test]
    fn snapshot_of_an_empty_layout() {
        let state = snapshot(&layout_with(0));
        assert_eq!(state.workspaces, vec![WorkspaceInfo::new(1, "1", true, 0)]);
        assert!(state.tiling);
        assert_eq!(state.focused_floating, None);
        assert_eq!(state.max_workspaces, MAX_WORKSPACES);
    }

    #[test]
    fn snapshot_follows_workspaces_tiling_and_floating() {
        let mut layout = layout_with(2);
        let state = snapshot(&layout);
        assert_eq!(state.workspaces, vec![WorkspaceInfo::new(1, "1", true, 2)]);
        assert_eq!(state.focused_floating, Some(false));

        layout.action(Action::ToggleWindowFloating);
        assert_eq!(snapshot(&layout).focused_floating, Some(true));

        layout.action(Action::Workspace { index: 2 });
        let state = snapshot(&layout);
        assert_eq!(
            state.workspaces,
            vec![
                WorkspaceInfo::new(1, "1", false, 2),
                WorkspaceInfo::new(2, "2", true, 0),
            ]
        );
        assert_eq!(state.active_workspace().map(|info| info.index), Some(2));
        assert_eq!(state.focused_floating, None);

        layout.action(Action::ToggleTiling);
        assert!(!snapshot(&layout).tiling);
        layout.action(Action::Workspace { index: 1 });
        assert!(snapshot(&layout).tiling, "tiling is per workspace");
    }

    #[test]
    fn publish_reports_and_notifies_only_changes() {
        let mut layout = layout_with(1);
        let publisher = Publisher::new(&layout);
        let mut rx = publisher.subscribe();
        assert!(!rx.has_changed().unwrap());

        // A query or an event that changes nothing visible: no notification.
        assert!(!publisher.update(&layout));
        assert!(!rx.has_changed().unwrap());

        layout.action(Action::Workspace { index: 2 });
        assert!(publisher.update(&layout));
        assert!(rx.has_changed().unwrap());
        assert_eq!(*rx.borrow_and_update(), snapshot(&layout));
        assert_eq!(publisher.current(), snapshot(&layout));

        // Publishing the same state again does not wake the receiver.
        assert!(!publisher.update(&layout));
        assert!(!rx.has_changed().unwrap());
    }

    #[test]
    fn publish_works_without_subscribers() {
        let publisher = Publisher::new(&layout_with(0));
        let changed = snapshot(&layout_with(3));
        assert!(publisher.publish(changed.clone()));
        assert_eq!(publisher.current(), changed);
        // A subscriber that arrives later starts from the latest state.
        assert_eq!(*publisher.subscribe().borrow(), changed);
    }
}
