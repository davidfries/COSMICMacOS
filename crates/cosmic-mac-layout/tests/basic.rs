// SPDX-License-Identifier: GPL-3.0-or-later
//! Basic tree construction, removal and event handling.

mod common;

use common::*;
use cosmic_mac_layout::Layout;
use cosmic_mac_protocol::{Action, Command, Config, Direction, Orientation, Rect, TreeNode};

#[test]
fn new_layout_has_one_empty_workspace() {
    let harness = Harness::new();
    let workspaces = harness.layout.workspaces();
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0].index, 1);
    assert_eq!(workspaces[0].name, "1");
    assert!(workspaces[0].active);
    assert_eq!(workspaces[0].window_count, 0);
    assert_eq!(harness.layout.active_workspace(), 1);
    assert_eq!(harness.layout.focused(), None);
    assert_eq!(harness.layout.tree_snapshot(), None);
}

#[test]
fn first_window_fills_work_area() {
    let mut harness = Harness::new();
    let wid = harness.add();
    assert_eq!(wid, 1);
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
    assert_eq!(harness.layout.focused(), Some(1));
    assert_eq!(harness.layout.visible_frames().len(), 1);
    harness.assert_synced();
}

#[test]
fn first_window_commands_show_set_frame_focus_raise() {
    let mut layout = Layout::with_work_area(Config::default(), WORK);
    let commands = layout.window_added(info(1));
    assert_eq!(shows(&commands), vec![1]);
    assert_eq!(set_frames(&commands).get(&1), Some(&WORK));
    assert_eq!(focus_command(&commands), Some(1));
    assert!(commands.contains(&Command::Raise { wid: 1 }));
}

#[test]
fn second_window_splits_side_by_side() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    harness.assert_synced();
}

#[test]
fn second_window_commands_resize_the_first() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::Focus {
        direction: Direction::Left,
    });
    assert!(commands.is_empty());
    let commands = harness.layout.window_added(info(2));
    let frames = set_frames(&commands);
    assert_eq!(frames.get(&1), Some(&Rect::new(0.0, 0.0, 500.0, 500.0)));
    assert_eq!(frames.get(&2), Some(&Rect::new(500.0, 0.0, 500.0, 500.0)));
    assert_eq!(shows(&commands), vec![2]);
}

#[test]
fn third_window_inserts_next_to_focus_with_equal_split() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.add();
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 250.0, 500.0);
    harness.assert_frame(3, 750.0, 0.0, 250.0, 500.0);
    harness.assert_synced();
}

#[test]
fn insertion_uses_tall_aspect_for_vertical_split() {
    let mut harness = Harness::tall();
    harness.add();
    harness.add();
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 0.0, 500.0, 500.0, 500.0);
}

#[test]
fn nested_group_created_when_orientation_differs() {
    let mut harness = Harness::with(Config::default(), Rect::new(0.0, 0.0, 1000.0, 1000.0));
    harness.add();
    harness.add();
    harness.add();
    harness.assert_frame(1, 0.0, 0.0, 500.0, 1000.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    harness.assert_frame(3, 500.0, 500.0, 500.0, 500.0);
}

#[test]
fn insertion_after_focused_nested_window() {
    let mut harness = Harness::with(Config::default(), Rect::new(0.0, 0.0, 1000.0, 1000.0));
    harness.add();
    harness.add();
    harness.add();
    harness.add();
    harness.assert_frame(1, 0.0, 0.0, 500.0, 1000.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    harness.assert_frame(3, 500.0, 500.0, 250.0, 500.0);
    harness.assert_frame(4, 750.0, 500.0, 250.0, 500.0);
}

#[test]
fn insertion_follows_focus() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.add();
    // Focus the first window, then insert a fourth: it should split window 1's slot.
    harness.act(Action::FocusWindow { wid: 1 });
    harness.add();
    harness.assert_frame(1, 0.0, 0.0, 250.0, 500.0);
    harness.assert_frame(4, 250.0, 0.0, 250.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 250.0, 500.0);
    harness.assert_frame(3, 750.0, 0.0, 250.0, 500.0);
}

#[test]
fn window_info_is_stored() {
    let mut harness = Harness::new();
    harness.add();
    let stored = harness.layout.window_info(1).unwrap();
    assert_eq!(stored.app_id, "com.example.App");
    assert_eq!(stored.title, "Window 1");
    assert_eq!(harness.layout.window_info(99), None);
}

#[test]
fn update_window_title_changes_tab_titles() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    let mut updated = info(1);
    updated.title = "renamed".into();
    let commands = harness.layout.update_window(updated);
    let stack = harness.layout.active_stack().unwrap();
    assert_eq!(stack.tabs[0].title, "renamed");
    assert!(
        stack_changes(&commands)
            .iter()
            .any(|stacks| !stacks.is_empty())
    );
}

#[test]
fn remove_only_window_empties_workspace() {
    let mut harness = Harness::new();
    harness.add();
    harness.remove(1);
    assert!(harness.layout.visible_frames().is_empty());
    assert_eq!(harness.layout.focused(), None);
    assert_eq!(harness.layout.workspaces()[0].window_count, 0);
    assert_eq!(harness.platform.focus, None);
    harness.assert_synced();
}

#[test]
fn remove_window_collapses_group_to_full_frame() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.add();
    harness.remove(1);
    harness.assert_frame(2, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(3, 500.0, 0.0, 500.0, 500.0);
    harness.assert_synced();
}

#[test]
fn remove_non_focused_window_keeps_focus() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.remove(1);
    assert_eq!(harness.layout.focused(), Some(2));
    harness.assert_frame(2, 0.0, 0.0, 1000.0, 500.0);
    harness.assert_synced();
}

#[test]
fn remove_focused_window_moves_focus_to_next() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.add();
    harness.remove(3);
    assert_eq!(harness.layout.focused(), Some(2));
    harness.assert_synced();
}

#[test]
fn remove_unknown_window_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.layout.window_removed(99);
    assert!(commands.is_empty());
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
}

#[test]
fn close_action_emits_close_without_removing() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::Close);
    assert_eq!(closes(&commands), vec![1]);
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
    assert_eq!(harness.layout.visible_frames().len(), 1);
}

#[test]
fn close_without_focus_is_a_noop() {
    let mut harness = Harness::new();
    let commands = harness.act(Action::Close);
    assert!(commands.is_empty());
}

#[test]
fn focus_changed_switches_workspace() {
    let mut harness = Harness::new();
    harness.add();
    harness.act(Action::Workspace { index: 2 });
    harness.add();
    assert_eq!(harness.layout.active_workspace(), 2);
    let commands = harness.layout.focus_changed(1);
    harness.platform.apply(&commands);
    assert_eq!(harness.layout.active_workspace(), 1);
    assert_eq!(harness.layout.focused(), Some(1));
    assert_eq!(harness.platform.workspace, Some(1));
    harness.assert_synced();
}

#[test]
fn focus_changed_unknown_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.layout.focus_changed(42);
    assert!(commands.is_empty());
}

#[test]
fn work_area_change_recomputes_frames() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.set_work_area(Rect::new(0.0, 0.0, 800.0, 400.0));
    harness.assert_frame(1, 0.0, 0.0, 400.0, 400.0);
    harness.assert_frame(2, 400.0, 0.0, 400.0, 400.0);
    harness.assert_synced();
}

#[test]
fn work_area_change_clamps_floating_windows() {
    let mut harness = Harness::new();
    harness.add_with(dialog(1));
    harness.assert_frame(1, 300.0, 150.0, 400.0, 200.0);
    harness.set_work_area(Rect::new(0.0, 0.0, 300.0, 200.0));
    harness.assert_frame(1, 0.0, 0.0, 300.0, 200.0);
    harness.assert_synced();
}

#[test]
fn config_update_changes_gaps() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.update_config(Config::default().with_gaps(10, 20));
    harness.assert_frame(1, 10.0, 10.0, 480.0, 480.0);
    harness.assert_frame(2, 510.0, 10.0, 480.0, 480.0);
    harness.assert_synced();
}

#[test]
fn tree_snapshot_describes_groups_and_windows() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    let snapshot = harness.layout.tree_snapshot().unwrap();
    match snapshot {
        TreeNode::Group {
            orientation,
            children,
            frame,
        } => {
            assert_eq!(orientation, Orientation::Horizontal);
            assert_eq!(frame, WORK);
            assert_eq!(children.len(), 2);
            assert!(matches!(&children[0], TreeNode::Window { wid: 1, .. }));
            assert!(matches!(&children[1], TreeNode::Window { wid: 2, .. }));
        }
        other => panic!("unexpected snapshot {other:?}"),
    }
}

#[test]
fn windows_listing_reports_state() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.add_with(dialog(3));
    let windows = harness.layout.windows();
    assert_eq!(windows.len(), 3);
    assert_eq!(windows[0].info.wid, 1);
    assert_eq!(windows[0].workspace, 1);
    assert!(windows[0].visible);
    assert!(!windows[0].focused);
    assert!(!windows[0].floating);
    assert_eq!(windows[1].info.wid, 2);
    assert!(!windows[1].focused);
    assert!(windows[2].focused);
    assert!(windows[2].floating);
    assert!(windows[2].visible);
}

#[test]
fn new_layout_without_work_area_has_zero_frames() {
    let mut layout = Layout::new(Config::default());
    layout.window_added(info(1));
    assert_eq!(layout.frame_of(1), Some(Rect::ZERO));
    layout.set_work_area(WORK);
    assert_eq!(layout.frame_of(1), Some(WORK));
}

#[test]
fn set_work_area_before_adding_windows_is_enough() {
    let mut layout = Layout::with_work_area(Config::default(), Rect::new(5.0, 6.0, 800.0, 600.0));
    layout.window_added(info(1));
    assert_eq!(layout.frame_of(1), Some(Rect::new(5.0, 6.0, 800.0, 600.0)));
}
