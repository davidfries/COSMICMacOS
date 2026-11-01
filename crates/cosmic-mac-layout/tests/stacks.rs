// SPDX-License-Identifier: GPL-3.0-or-later
//! Stacks (COSMIC tabbed groups): stacking, unstacking, tab visibility and removal.

mod common;

use common::*;
use cosmic_mac_protocol::{Action, Config, Rect, STACK_BAR_HEIGHT, TreeNode};

fn stacked_pair() -> Harness {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness
}

#[test]
fn toggle_stacking_with_sibling_window() {
    let harness = stacked_pair();
    let stack = harness.layout.active_stack().unwrap();
    assert_eq!(
        stack.tabs.iter().map(|tab| tab.wid).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(stack.active, 1);
    assert_eq!(harness.layout.focused(), Some(2));
    harness.assert_frame(2, 0.0, 26.0, 1000.0, 474.0);
    assert!(harness.layout.is_visible(2));
    assert!(!harness.layout.is_visible(1));
    harness.assert_synced();
}

#[test]
fn toggle_stacking_uses_previous_sibling_for_last_window() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    let stack = harness.layout.active_stack().unwrap();
    assert_eq!(
        stack.tabs.iter().map(|tab| tab.wid).collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert!(harness.layout.is_visible(1));
    assert!(harness.layout.is_visible(3));
    assert!(!harness.layout.is_visible(2));
}

#[test]
fn toggle_stacking_without_sibling_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::ToggleStacking);
    assert!(commands.is_empty());
    assert!(harness.layout.active_stack().is_none());
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
}

#[test]
fn toggle_stacking_on_floating_window_is_a_noop() {
    let mut harness = Harness::new();
    harness.add_with(dialog(1));
    let commands = harness.act(Action::ToggleStacking);
    assert!(commands.is_empty());
    assert!(harness.layout.active_stack().is_none());
}

#[test]
fn toggle_stacking_twice_unstacks() {
    let mut harness = stacked_pair();
    harness.act(Action::ToggleStacking);
    assert!(harness.layout.active_stack().is_none());
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    assert_eq!(harness.layout.focused(), Some(2));
    harness.assert_synced();
}

#[test]
fn toggle_stacking_into_existing_stack() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.add();
    harness.act(Action::ToggleStacking);
    let stack = harness.layout.active_stack().unwrap();
    assert_eq!(
        stack.tabs.iter().map(|tab| tab.wid).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(stack.active, 2);
    assert_eq!(harness.layout.focused(), Some(3));
    harness.assert_synced();
}

#[test]
fn toggle_stacking_with_group_sibling_stacks_with_its_nearest_window() {
    // A no-op here made Super+S depend on the tree's shape; the focused window now stacks with
    // the nearest window inside the neighbouring group.
    let mut harness = Harness::with(
        cosmic_mac_protocol::Config::default(),
        cosmic_mac_protocol::Rect::new(0.0, 0.0, 1000.0, 1000.0),
    );
    harness.add();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::ToggleStacking);
    let stack = harness.layout.active_stack().expect("a stack formed");
    let mut members: Vec<_> = stack.tabs.iter().map(|tab| tab.wid).collect();
    members.sort_unstable();
    assert_eq!(members, vec![1, 2]);
    assert_eq!(harness.layout.focused(), Some(1));
    harness.assert_synced();
}

#[test]
fn unstack_creates_a_sibling_frame() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.add();
    harness.act(Action::ToggleStacking);
    let stack = harness.layout.active_stack().unwrap();
    assert_eq!(stack.tabs.len(), 3);
    harness.act(Action::Unstack);
    let stack = harness.layout.active_stack();
    // Focus moved out of the stack, so there is no focused stack any more.
    assert!(stack.is_none());
    assert_eq!(harness.layout.focused(), Some(3));
    harness.assert_frame(3, 500.0, 0.0, 500.0, 500.0);
    // Still stacked with 1, below the tab-bar strip.
    harness.assert_frame(2, 0.0, 26.0, 500.0, 474.0);
    assert!(harness.layout.is_visible(2));
    harness.assert_synced();
}

#[test]
fn unstack_dissolves_stack_of_one() {
    let mut harness = stacked_pair();
    harness.remove(1);
    let snapshot = harness.layout.tree_snapshot().unwrap();
    assert!(matches!(snapshot, TreeNode::Window { wid: 2, .. }));
    assert!(harness.layout.active_stack().is_none());
    harness.assert_frame(2, 0.0, 0.0, 1000.0, 500.0);
}

#[test]
fn removing_inactive_stack_member_keeps_active_tab() {
    let mut harness = stacked_pair();
    harness.remove(1);
    assert_eq!(harness.layout.focused(), Some(2));
    assert!(harness.layout.is_visible(2));
    assert!(!harness.layout.is_visible(1));
    harness.assert_synced();
}

#[test]
fn removing_active_stack_member_activates_remaining_window() {
    let mut harness = stacked_pair();
    harness.remove(2);
    assert_eq!(harness.layout.focused(), Some(1));
    assert!(harness.layout.is_visible(1));
    assert!(harness.layout.active_stack().is_none());
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
    harness.assert_synced();
}

#[test]
fn removing_all_stack_members_empties_the_workspace() {
    let mut harness = stacked_pair();
    harness.remove(1);
    harness.remove(2);
    assert!(harness.layout.visible_frames().is_empty());
    assert_eq!(harness.layout.focused(), None);
    assert_eq!(harness.layout.workspaces()[0].window_count, 0);
}

#[test]
fn stack_only_shows_active_member() {
    let harness = stacked_pair();
    let visible = harness.layout.visible_frames();
    assert_eq!(visible.len(), 1);
    assert!(visible.contains_key(&2));
    assert!(!visible.contains_key(&1));
    assert!(harness.platform.hidden.contains(&1));
    // A parked member keeps the frame below the tab bar.
    harness.assert_frame(1, 0.0, 26.0, 1000.0, 474.0);
    harness.assert_synced();
}

#[test]
fn stacking_emits_hide_for_inactive_member() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    let commands = harness.act(Action::ToggleStacking);
    assert_eq!(hides(&commands), vec![1]);
    assert_eq!(
        set_frames(&commands).get(&2),
        Some(&cosmic_mac_protocol::Rect::new(0.0, 26.0, 1000.0, 474.0))
    );
    let changes = stack_changes(&commands);
    assert!(changes.last().unwrap().is_some());
}

#[test]
fn stack_tabs_carry_titles() {
    let harness = stacked_pair();
    let stack = harness.layout.active_stack().unwrap();
    assert_eq!(stack.tabs[0].title, "Window 1");
    assert_eq!(stack.tabs[1].title, "Window 2");
}

#[test]
fn active_stack_is_none_without_a_stack() {
    let mut harness = Harness::new();
    harness.add();
    assert!(harness.layout.active_stack().is_none());
    assert!(harness.layout.stacks().is_empty());
}

#[test]
fn stacks_lists_all_stacks() {
    let mut harness = stacked_pair();
    harness.add();
    let stacks = harness.layout.stacks();
    assert_eq!(stacks.len(), 1);
    assert_eq!(stacks[0].tabs.len(), 2);
}

#[test]
fn tree_snapshot_contains_stack_node() {
    let harness = stacked_pair();
    let snapshot = harness.layout.tree_snapshot().unwrap();
    match snapshot {
        TreeNode::Stack { active, tabs, .. } => {
            assert_eq!(active, 1);
            assert_eq!(tabs.len(), 2);
        }
        other => panic!("unexpected snapshot {other:?}"),
    }
}

#[test]
fn focus_cycling_activates_stack_tabs() {
    let mut harness = stacked_pair();
    harness.act(Action::FocusNext);
    assert_eq!(harness.layout.focused(), Some(1));
    assert_eq!(harness.layout.active_stack().unwrap().active, 0);
    assert!(harness.layout.is_visible(1));
    assert!(!harness.layout.is_visible(2));
    harness.assert_synced();
}

#[test]
fn unstack_without_stack_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    let before = harness.layout.visible_frames();
    let commands = harness.act(Action::Unstack);
    assert!(commands.is_empty());
    assert_eq!(harness.layout.visible_frames(), before);
}

#[test]
fn unstack_middle_member_keeps_other_tabs() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.act(Action::FocusWindow { wid: 2 });
    harness.act(Action::Unstack);
    let stack = harness.layout.stacks()[0].clone();
    assert_eq!(
        stack.tabs.iter().map(|tab| tab.wid).collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert_eq!(stack.active, 1);
    assert_eq!(harness.layout.focused(), Some(2));
    assert!(harness.layout.is_visible(3));
    assert!(harness.layout.is_visible(2));
    harness.assert_synced();
}

#[test]
fn unstack_last_member_of_three_leaves_two_tabs() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.act(Action::Unstack);
    let stack = harness.layout.stacks()[0].clone();
    assert_eq!(
        stack.tabs.iter().map(|tab| tab.wid).collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(stack.active, 1);
    assert_eq!(harness.layout.focused(), Some(3));
}

#[test]
fn active_stack_frame_follows_maximized_tab() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.add();
    harness.act(Action::FocusWindow { wid: 2 });
    harness.act(Action::Maximize);
    let stack = harness.layout.active_stack().unwrap();
    // The work area minus the tab-bar strip.
    assert_eq!(stack.frame, Rect::new(0.0, 26.0, 1000.0, 474.0));
    harness.assert_frame(2, 0.0, 26.0, 1000.0, 474.0);
    assert!(harness.layout.is_visible(2));
}

#[test]
fn stacking_after_unstack_can_stack_again() {
    let mut harness = stacked_pair();
    harness.act(Action::ToggleStacking);
    harness.act(Action::ToggleStacking);
    let stack = harness.layout.active_stack().unwrap();
    assert_eq!(stack.tabs.len(), 2);
}

#[test]
fn stack_members_get_the_tile_minus_the_tab_bar_strip() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    assert_eq!(harness.shape(), "H[1 S[2 3]]");
    let bar = STACK_BAR_HEIGHT;
    // The plain window beside the stack keeps its full-height tile.
    let plain = harness.frame(1);
    assert_eq!((plain.y, plain.height), (0.0, 500.0));
    // Both members, parked or visible, sit below the strip of the stack's full-height tile.
    let member = harness.frame(3);
    assert_eq!(harness.frame(2), member);
    assert_eq!(member.x, plain.right());
    assert_eq!((member.y, member.height), (bar, 500.0 - bar));
    assert_eq!(harness.layout.visible_frames().len(), 2);
    assert_eq!(harness.layout.active_stack().unwrap().frame, member);
    // The strip still belongs to the stack's tile: tiles cover the work area exactly.
    assert_tiles(WORK, &visible_tiles(&harness.layout));
    harness.assert_synced();
}

#[test]
fn stack_info_frame_is_the_visible_members_frame() {
    let mut harness = stacked_pair();
    for wid in [1, 2] {
        harness.act(Action::FocusWindow { wid });
        let stack = harness.layout.active_stack().unwrap();
        assert_eq!(stack.frame, harness.frame(wid));
        assert_eq!(stack.frame, Rect::new(0.0, 26.0, 1000.0, 474.0));
        assert_eq!(harness.layout.stacks()[0].frame, stack.frame);
        // The bar is drawn directly above this frame, inside the work area.
        assert!(stack.frame.y - STACK_BAR_HEIGHT >= WORK.y);
    }
}

#[test]
fn a_constrained_member_is_centred_below_the_strip() {
    let mut harness = Harness::new();
    harness.add();
    harness.add_with(constrained(2, None, Some((1000.0, 300.0))));
    harness.act(Action::ToggleStacking);
    assert_eq!(harness.shape(), "S[1 2]");
    // The 474pt window area leaves 174pt spare, split above and below.
    harness.assert_frame(2, 0.0, 113.0, 1000.0, 300.0);
    assert_eq!(
        harness.layout.active_stack().unwrap().frame,
        harness.frame(2)
    );
}

#[test]
fn a_stack_in_a_vertical_group_reserves_its_strip_inside_its_tile() {
    let mut harness = Harness::with(Config::default(), BIG_WORK);
    harness.add();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    assert_eq!(harness.shape(), "H[1 S[2 3]]");
    harness.act(Action::ToggleOrientation);
    assert_eq!(harness.shape(), "V[1 S[2 3]]");
    // 800 split in two 400pt tiles; the stack's is the lower one.
    harness.assert_frame(1, 0.0, 0.0, 1200.0, 400.0);
    harness.assert_frame(3, 0.0, 426.0, 1200.0, 374.0);
    harness.assert_frame(2, 0.0, 426.0, 1200.0, 374.0);
    assert_tiles(BIG_WORK, &visible_tiles(&harness.layout));
}

#[test]
fn a_maximized_stack_member_keeps_the_strip_free() {
    let mut harness = stacked_pair();
    harness.act(Action::Maximize);
    let below_bar = Rect::new(0.0, 26.0, 1000.0, 474.0);
    harness.assert_frame(2, 0.0, 26.0, 1000.0, 474.0);
    assert_eq!(harness.layout.active_stack().unwrap().frame, below_bar);
    harness.assert_synced();
    // Un-maximizing returns it to the tile frame, which is the same below-bar frame here.
    harness.act(Action::Maximize);
    harness.assert_frame(2, 0.0, 26.0, 1000.0, 474.0);
}

#[test]
fn a_maximized_stack_member_fills_the_work_area_below_the_strip_beside_others() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    assert_eq!(harness.shape(), "H[1 S[2 3]]");
    let tile_frame = harness.frame(3);
    harness.act(Action::Maximize);
    harness.assert_frame(3, 0.0, 26.0, 1000.0, 474.0);
    assert_eq!(
        harness.layout.active_stack().unwrap().frame,
        Rect::new(0.0, 26.0, 1000.0, 474.0)
    );
    harness.act(Action::Maximize);
    assert_eq!(harness.frame(3), tile_frame);
}

#[test]
fn a_maximized_plain_window_keeps_the_whole_work_area() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::Maximize);
    harness.assert_frame(2, 0.0, 0.0, 1000.0, 500.0);
}

#[test]
fn unstacking_restores_full_height_tiles() {
    let mut harness = stacked_pair();
    harness.act(Action::ToggleStacking);
    assert!(harness.layout.active_stack().is_none());
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    assert_tiles(WORK, &visible_tiles(&harness.layout));
    harness.assert_synced();
}
