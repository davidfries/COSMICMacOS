// SPDX-License-Identifier: GPL-3.0-or-later
//! Workspace pool, switching, moving windows and dynamic workspace removal.

mod common;

use common::*;
use cosmic_mac_protocol::Action;

#[test]
fn initial_layout_has_a_single_workspace() {
    let harness = Harness::new();
    assert_eq!(harness.layout.workspaces().len(), 1);
    assert_eq!(harness.layout.active_workspace(), 1);
}

#[test]
fn workspace_action_creates_intermediate_workspaces() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::Workspace { index: 3 });
    let workspaces = harness.layout.workspaces();
    assert_eq!(workspaces.len(), 3);
    assert_eq!(harness.layout.active_workspace(), 3);
    assert_eq!(workspaces[1].window_count, 0);
    assert_eq!(workspaces[2].window_count, 0);
    assert_eq!(workspace_activations(&commands), vec![3]);
    harness.assert_synced();
}

#[test]
fn workspace_switch_hides_and_shows_windows() {
    let mut harness = Harness::new();
    harness.add();
    harness.act(Action::Workspace { index: 2 });
    harness.add();
    let commands = harness.act(Action::Workspace { index: 1 });
    assert_eq!(harness.layout.active_workspace(), 1);
    assert_eq!(hides(&commands), vec![2]);
    assert_eq!(shows(&commands), vec![1]);
    assert_eq!(workspace_activations(&commands), vec![1]);
    assert_eq!(focus_command(&commands), Some(1));
    harness.assert_synced();
}

#[test]
fn switching_to_the_same_workspace_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::Workspace { index: 1 });
    assert!(commands.is_empty());
}

#[test]
fn workspace_zero_is_rejected() {
    let mut harness = Harness::new();
    let commands = harness.act(Action::Workspace { index: 0 });
    assert!(commands.is_empty());
    assert_eq!(harness.layout.active_workspace(), 1);
}

#[test]
fn workspace_above_the_limit_is_rejected() {
    let mut harness = Harness::new();
    let commands = harness.act(Action::Workspace { index: 10 });
    assert!(commands.is_empty());
    assert_eq!(harness.layout.workspaces().len(), 1);
}

#[test]
fn move_to_workspace_moves_the_focused_window() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    let commands = harness.act(Action::MoveToWorkspace { index: 2 });
    assert_eq!(harness.layout.active_workspace(), 1);
    assert_eq!(harness.layout.focused(), Some(1));
    assert_eq!(hides(&commands), vec![2]);
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
    let windows = harness.layout.windows();
    let moved = windows.iter().find(|window| window.info.wid == 2).unwrap();
    assert_eq!(moved.workspace, 2);
    assert!(!moved.visible);
    harness.assert_synced();
}

#[test]
fn move_to_current_workspace_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::MoveToWorkspace { index: 1 });
    assert!(commands.is_empty());
    assert_eq!(harness.layout.focused(), Some(1));
}

#[test]
fn move_to_workspace_creates_the_target() {
    let mut harness = Harness::new();
    harness.add();
    harness.act(Action::MoveToWorkspace { index: 3 });
    assert_eq!(harness.layout.workspaces().len(), 3);
    assert_eq!(harness.layout.workspaces()[2].window_count, 1);
    assert_eq!(harness.layout.active_workspace(), 1);
}

#[test]
fn move_to_workspace_focuses_the_next_window_on_the_source() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.add();
    harness.act(Action::MoveToWorkspace { index: 2 });
    assert_eq!(harness.layout.focused(), Some(2));
}

#[test]
fn moving_the_only_window_leaves_an_empty_active_workspace() {
    let mut harness = Harness::new();
    harness.add();
    harness.act(Action::MoveToWorkspace { index: 2 });
    assert_eq!(harness.layout.active_workspace(), 1);
    assert_eq!(harness.layout.focused(), None);
    assert_eq!(harness.layout.workspaces()[0].window_count, 0);
    assert_eq!(harness.layout.workspaces()[1].window_count, 1);
    assert_eq!(harness.platform.focus, None);
}

#[test]
fn move_floating_window_to_another_workspace_keeps_its_frame() {
    let mut harness = Harness::new();
    harness.add_with(dialog(1));
    harness.act(Action::MoveToWorkspace { index: 2 });
    harness.act(Action::Workspace { index: 2 });
    harness.assert_frame(1, 300.0, 150.0, 400.0, 200.0);
    assert!(harness.layout.windows()[0].floating);
    harness.assert_synced();
}

#[test]
fn next_workspace_wraps_around() {
    let mut harness = Harness::new();
    harness.add();
    harness.act(Action::Workspace { index: 2 });
    harness.act(Action::Workspace { index: 1 });
    harness.act(Action::NextWorkspace);
    assert_eq!(harness.layout.active_workspace(), 2);
    harness.act(Action::NextWorkspace);
    assert_eq!(harness.layout.active_workspace(), 1);
}

#[test]
fn previous_workspace_wraps_around() {
    let mut harness = Harness::new();
    harness.add();
    harness.act(Action::Workspace { index: 2 });
    harness.act(Action::Workspace { index: 1 });
    harness.act(Action::PreviousWorkspace);
    assert_eq!(harness.layout.active_workspace(), 2);
    harness.act(Action::PreviousWorkspace);
    assert_eq!(harness.layout.active_workspace(), 1);
}

#[test]
fn next_workspace_with_one_workspace_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::NextWorkspace);
    assert!(commands.is_empty());
    assert_eq!(harness.layout.active_workspace(), 1);
}

#[test]
fn previous_workspace_with_one_workspace_is_a_noop() {
    let mut harness = Harness::new();
    let commands = harness.act(Action::PreviousWorkspace);
    assert!(commands.is_empty());
}

#[test]
fn last_workspace_returns_to_the_previous_one() {
    let mut harness = Harness::new();
    harness.add();
    harness.act(Action::Workspace { index: 2 });
    harness.act(Action::Workspace { index: 1 });
    harness.act(Action::LastWorkspace);
    assert_eq!(harness.layout.active_workspace(), 2);
}

#[test]
fn last_workspace_without_history_is_a_noop() {
    let mut harness = Harness::new();
    let commands = harness.act(Action::LastWorkspace);
    assert!(commands.is_empty());
    assert_eq!(harness.layout.active_workspace(), 1);
}

#[test]
fn move_to_next_workspace_moves_without_following() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::Workspace { index: 2 });
    harness.act(Action::Workspace { index: 1 });
    harness.act(Action::MoveToNextWorkspace);
    assert_eq!(harness.layout.active_workspace(), 1);
    assert_eq!(harness.layout.focused(), Some(1));
    assert_eq!(harness.layout.workspaces()[1].window_count, 1);
}

#[test]
fn move_to_previous_workspace_moves_without_following() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::Workspace { index: 2 });
    harness.act(Action::Workspace { index: 1 });
    harness.act(Action::MoveToPreviousWorkspace);
    assert_eq!(harness.layout.active_workspace(), 1);
    assert_eq!(harness.layout.workspaces()[1].window_count, 1);
}

#[test]
fn move_to_last_workspace_moves_to_the_previous_one() {
    let mut harness = Harness::new();
    harness.add();
    harness.act(Action::Workspace { index: 2 });
    harness.act(Action::Workspace { index: 1 });
    harness.add();
    harness.act(Action::MoveToLastWorkspace);
    assert_eq!(harness.layout.active_workspace(), 1);
    assert_eq!(harness.layout.workspaces()[1].window_count, 1);
}

#[test]
fn auto_removes_trailing_workspace_when_it_becomes_empty() {
    let mut harness = Harness::new();
    harness.add();
    harness.act(Action::Workspace { index: 2 });
    harness.add();
    harness.remove(2);
    let workspaces = harness.layout.workspaces();
    assert_eq!(workspaces.len(), 1);
    assert_eq!(harness.layout.active_workspace(), 1);
    assert_eq!(harness.layout.focused(), Some(1));
    harness.assert_synced();
}

#[test]
fn auto_removes_multiple_trailing_workspaces() {
    let mut harness = Harness::new();
    harness.add();
    harness.act(Action::Workspace { index: 3 });
    harness.add();
    harness.remove(2);
    assert_eq!(harness.layout.workspaces().len(), 1);
    assert_eq!(harness.layout.active_workspace(), 1);
}

#[test]
fn does_not_remove_empty_workspace_with_windows_after_it() {
    let mut harness = Harness::new();
    harness.add();
    harness.act(Action::Workspace { index: 2 });
    harness.add();
    harness.act(Action::Workspace { index: 3 });
    harness.add();
    harness.remove(3);
    let workspaces = harness.layout.workspaces();
    assert_eq!(workspaces.len(), 2);
    assert_eq!(harness.layout.active_workspace(), 2);
    assert_eq!(workspaces[0].window_count, 1);
    assert_eq!(workspaces[1].window_count, 1);
}

#[test]
fn workspace_window_counts_are_reported() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::Workspace { index: 2 });
    harness.add();
    let workspaces = harness.layout.workspaces();
    assert_eq!(workspaces[0].window_count, 2);
    assert_eq!(workspaces[1].window_count, 1);
    assert!(workspaces[1].active);
    assert!(!workspaces[0].active);
}

#[test]
fn switching_to_an_empty_workspace_unfocuses() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::Workspace { index: 2 });
    assert!(has_unfocus(&commands));
    assert_eq!(harness.layout.focused(), None);
    assert_eq!(harness.platform.focus, None);
}

#[test]
fn workspace_switch_restores_each_workspace_focus() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::Workspace { index: 2 });
    harness.add();
    harness.act(Action::Workspace { index: 1 });
    assert_eq!(harness.layout.focused(), Some(2));
    harness.act(Action::Workspace { index: 2 });
    assert_eq!(harness.layout.focused(), Some(3));
}

#[test]
fn moving_a_window_to_another_workspace_and_back_retiles() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::MoveToWorkspace { index: 2 });
    harness.act(Action::Workspace { index: 2 });
    harness.act(Action::MoveToWorkspace { index: 1 });
    harness.act(Action::Workspace { index: 1 });
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    harness.assert_synced();
}

#[test]
fn moving_a_middle_window_collapses_the_source_group() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 2 });
    harness.act(Action::MoveToWorkspace { index: 2 });
    assert_eq!(harness.layout.focused(), Some(3));
    let visible = harness.layout.visible_frames();
    assert_eq!(visible.len(), 2);
    assert!(visible.contains_key(&1));
    assert!(visible.contains_key(&3));
    harness.assert_synced();
}

#[test]
fn move_to_next_workspace_with_one_workspace_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::MoveToNextWorkspace);
    assert!(commands.is_empty());
    assert_eq!(harness.layout.focused(), Some(1));
}
