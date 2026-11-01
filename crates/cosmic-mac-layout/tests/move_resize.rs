// SPDX-License-Identifier: GPL-3.0-or-later
//! Moving, swapping, orientation and resize behaviour.

mod common;

use common::*;
use cosmic_mac_layout::MIN_WINDOW_SIZE;
use cosmic_mac_protocol::{Action, Command, Config, Direction};

#[test]
fn move_left_swaps_with_left_sibling() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::Move {
        direction: Direction::Left,
    });
    harness.assert_frame(2, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(1, 500.0, 0.0, 500.0, 500.0);
    assert_eq!(harness.layout.focused(), Some(2));
    harness.assert_synced();
}

#[test]
fn move_right_swaps_with_right_sibling() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Move {
        direction: Direction::Right,
    });
    harness.assert_frame(1, 500.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 0.0, 0.0, 500.0, 500.0);
    assert_eq!(harness.layout.focused(), Some(1));
}

#[test]
fn move_up_swaps_in_vertical_group() {
    let mut harness = Harness::tall();
    harness.add();
    harness.add();
    harness.act(Action::Move {
        direction: Direction::Up,
    });
    harness.assert_frame(2, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(1, 0.0, 500.0, 500.0, 500.0);
}

#[test]
fn move_down_swaps_in_vertical_group() {
    let mut harness = Harness::tall();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Move {
        direction: Direction::Down,
    });
    harness.assert_frame(1, 0.0, 500.0, 500.0, 500.0);
    harness.assert_frame(2, 0.0, 0.0, 500.0, 500.0);
}

#[test]
fn move_without_sibling_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 1 });
    let commands = harness.act(Action::Move {
        direction: Direction::Left,
    });
    assert!(commands.is_empty());
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
}

#[test]
fn move_perpendicular_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    let commands = harness.act(Action::Move {
        direction: Direction::Up,
    });
    assert!(commands.is_empty());
}

#[test]
fn move_floating_window_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    harness.add_with(dialog(2));
    let commands = harness.act(Action::Move {
        direction: Direction::Left,
    });
    assert!(commands.is_empty());
    assert_eq!(harness.layout.focused(), Some(2));
}

#[test]
fn move_into_adjacent_stack_joins_it() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.add();
    harness.act(Action::Move {
        direction: Direction::Left,
    });
    let stack = harness.layout.active_stack().unwrap();
    assert_eq!(stack.tabs.len(), 3);
    assert_eq!(stack.active, 2);
    assert_eq!(harness.layout.focused(), Some(3));
    assert!(harness.layout.is_visible(3));
    assert!(!harness.layout.is_visible(1));
    harness.assert_frame(3, 0.0, 0.0, 1000.0, 500.0);
    harness.assert_synced();
}

#[test]
fn move_stack_member_into_adjacent_stack() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    assert_eq!(harness.layout.active_stack().unwrap().tabs.len(), 2);
    harness.act(Action::Move {
        direction: Direction::Left,
    });
    let stack = harness.layout.active_stack().unwrap();
    assert_eq!(
        stack.tabs.iter().map(|tab| tab.wid).collect::<Vec<_>>(),
        vec![1, 2, 4]
    );
    assert_eq!(harness.layout.focused(), Some(4));
    assert!(harness.layout.is_visible(3));
    assert!(!harness.layout.is_visible(2));
    harness.assert_synced();
}

#[test]
fn move_whole_stack_swaps_with_neighbour() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.add();
    // Move the stack by focusing its active member.
    harness.act(Action::FocusWindow { wid: 2 });
    harness.act(Action::Move {
        direction: Direction::Right,
    });
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    harness.assert_frame(3, 0.0, 0.0, 500.0, 500.0);
    assert_eq!(harness.layout.focused(), Some(2));
}

#[test]
fn toggle_orientation_flips_parent_group() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::ToggleOrientation);
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 250.0);
    harness.assert_frame(2, 0.0, 250.0, 1000.0, 250.0);
    harness.assert_synced();
}

#[test]
fn toggle_orientation_on_single_window_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::ToggleOrientation);
    assert!(commands.is_empty());
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
}

#[test]
fn toggle_orientation_on_floating_window_is_a_noop() {
    let mut harness = Harness::new();
    harness.add_with(dialog(1));
    let commands = harness.act(Action::ToggleOrientation);
    assert!(commands.is_empty());
}

#[test]
fn resize_right_grows_focused_window() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Resize {
        direction: Direction::Right,
    });
    harness.assert_frame(1, 0.0, 0.0, 532.0, 500.0);
    harness.assert_frame(2, 532.0, 0.0, 468.0, 500.0);
    harness.assert_synced();
}

#[test]
fn repeated_resize_accumulates() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Resize {
        direction: Direction::Right,
    });
    harness.act(Action::Resize {
        direction: Direction::Right,
    });
    harness.assert_frame(1, 0.0, 0.0, 564.0, 500.0);
    harness.assert_frame(2, 564.0, 0.0, 436.0, 500.0);
}

#[test]
fn resize_at_left_edge_shrinks_focused_window() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Resize {
        direction: Direction::Left,
    });
    harness.assert_frame(1, 0.0, 0.0, 468.0, 500.0);
    harness.assert_frame(2, 468.0, 0.0, 532.0, 500.0);
}

#[test]
fn resize_down_grows_in_vertical_group() {
    let mut harness = Harness::tall();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Resize {
        direction: Direction::Down,
    });
    harness.assert_frame(1, 0.0, 0.0, 500.0, 532.0);
    harness.assert_frame(2, 0.0, 532.0, 500.0, 468.0);
}

#[test]
fn resize_clamps_to_minimum_size() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 1 });
    for _ in 0..30 {
        harness.act(Action::Resize {
            direction: Direction::Right,
        });
    }
    harness.assert_frame(1, 0.0, 0.0, 900.0, 500.0);
    harness.assert_frame(2, 900.0, 0.0, 100.0, 500.0);
    assert_eq!(MIN_WINDOW_SIZE, 100.0);
    // Further attempts do nothing.
    let commands = harness.act(Action::Resize {
        direction: Direction::Right,
    });
    assert!(commands.is_empty());
}

#[test]
fn resize_single_window_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::Resize {
        direction: Direction::Right,
    });
    assert!(commands.is_empty());
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
}

#[test]
fn resize_floating_window_is_a_noop() {
    let mut harness = Harness::new();
    harness.add_with(dialog(1));
    let commands = harness.act(Action::Resize {
        direction: Direction::Right,
    });
    assert!(commands.is_empty());
}

#[test]
fn resize_nested_group_moves_boundary() {
    let mut harness = Harness::with(
        cosmic_mac_protocol::Config::default(),
        cosmic_mac_protocol::Rect::new(0.0, 0.0, 1000.0, 1000.0),
    );
    harness.add();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 2 });
    harness.act(Action::Resize {
        direction: Direction::Down,
    });
    harness.assert_frame(2, 500.0, 0.0, 500.0, 532.0);
    harness.assert_frame(3, 500.0, 532.0, 500.0, 468.0);
}

#[test]
fn resize_nested_group_resizes_outer_split() {
    let mut harness = Harness::with(
        cosmic_mac_protocol::Config::default(),
        cosmic_mac_protocol::Rect::new(0.0, 0.0, 1000.0, 1000.0),
    );
    harness.add();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 2 });
    harness.act(Action::Resize {
        direction: Direction::Right,
    });
    harness.assert_frame(1, 0.0, 0.0, 532.0, 1000.0);
    harness.assert_frame(2, 532.0, 0.0, 468.0, 500.0);
    harness.assert_frame(3, 532.0, 500.0, 468.0, 500.0);
}

#[test]
fn resize_mode_toggles() {
    let mut harness = Harness::new();
    assert!(!harness.layout.resize_mode());
    harness.act(Action::ResizeMode);
    assert!(harness.layout.resize_mode());
    harness.act(Action::ResizeMode);
    assert!(!harness.layout.resize_mode());
}

#[test]
fn swap_window_exchanges_frames_with_previous_focus() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::SwapWindow);
    harness.assert_frame(1, 500.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 0.0, 0.0, 500.0, 500.0);
    assert_eq!(harness.layout.focused(), Some(2));
    harness.assert_synced();
}

#[test]
fn swap_window_without_previous_focus_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::SwapWindow);
    assert!(commands.is_empty());
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
}

#[test]
fn swap_window_between_tiling_and_floating() {
    let mut harness = Harness::new();
    harness.add();
    harness.add_with(dialog(2));
    harness.act(Action::SwapWindow);
    harness.assert_frame(2, 0.0, 0.0, 1000.0, 500.0);
    harness.assert_frame(1, 300.0, 150.0, 400.0, 200.0);
    assert_eq!(harness.layout.focused(), Some(2));
    assert!(harness.layout.is_visible(1));
    harness.assert_synced();
}

#[test]
fn swap_window_between_two_floating_windows() {
    let mut harness = Harness::new();
    harness.add_with(
        cosmic_mac_protocol::ToplevelInfo::new(1, "com.example.Float", "a")
            .with_resizable(false)
            .with_initial_frame(cosmic_mac_protocol::Rect::new(0.0, 0.0, 200.0, 100.0)),
    );
    harness.add_with(
        cosmic_mac_protocol::ToplevelInfo::new(2, "com.example.Float", "b")
            .with_resizable(false)
            .with_initial_frame(cosmic_mac_protocol::Rect::new(0.0, 0.0, 200.0, 100.0)),
    );
    harness.act(Action::SwapWindow);
    harness.assert_frame(1, 432.0, 232.0, 200.0, 100.0);
    harness.assert_frame(2, 400.0, 200.0, 200.0, 100.0);
}

#[test]
fn swap_window_plain_with_stack_member() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.add();
    // Focus is 3 (plain), previous focus is 2 (stack member).
    harness.act(Action::SwapWindow);
    assert_eq!(harness.layout.focused(), Some(3));
    assert!(harness.layout.is_visible(3));
    assert_eq!(harness.layout.active_stack().unwrap().active, 1);
    harness.assert_frame(3, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    harness.assert_synced();
}

#[test]
fn swap_window_stack_member_with_plain() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.add();
    harness.act(Action::SwapWindow);
    // Now 3 is in the stack and 2 is plain; focus 3, swap back.
    harness.act(Action::SwapWindow);
    assert_eq!(harness.layout.focused(), Some(3));
    harness.assert_frame(3, 500.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 0.0, 0.0, 500.0, 500.0);
    harness.assert_synced();
}

#[test]
fn maximize_fills_work_area_and_restores() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::Maximize);
    harness.assert_frame(2, 0.0, 0.0, 1000.0, 500.0);
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
    harness.act(Action::Maximize);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    harness.assert_synced();
}

#[test]
fn maximize_floating_window_restores_its_frame() {
    let mut harness = Harness::new();
    harness.add_with(dialog(1));
    harness.assert_frame(1, 300.0, 150.0, 400.0, 200.0);
    harness.act(Action::Maximize);
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
    harness.act(Action::Maximize);
    harness.assert_frame(1, 300.0, 150.0, 400.0, 200.0);
    harness.assert_synced();
}

#[test]
fn maximizing_another_window_replaces_the_maximized_one() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::Maximize);
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Maximize);
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    harness.assert_synced();
}

#[test]
fn removing_a_maximized_window_clears_maximize_state() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::Maximize);
    harness.remove(2);
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
    harness.assert_synced();
}

#[test]
fn maximizing_without_focus_is_a_noop() {
    let mut harness = Harness::new();
    let commands = harness.act(Action::Maximize);
    assert!(commands.is_empty());
}

// -----------------------------------------------------------------------------------------
// Moving a window out of a nested group (i3 / cosmic-comp `move_current_node` semantics)
// -----------------------------------------------------------------------------------------

fn move_focused(harness: &mut Harness, wid: u64, direction: Direction) -> Vec<Command> {
    harness.act(Action::FocusWindow { wid });
    harness.act(Action::Move { direction })
}

/// `H[1 V[2 3]]` on a 1200x800 work area: window 1 beside a vertical group of 2 over 3.
fn beside_vertical_group() -> Harness {
    let mut harness = Harness::with(Config::default(), BIG_WORK);
    harness.add();
    harness.add();
    harness.add();
    assert_eq!(harness.shape(), "H[1 V[2 3]]");
    harness
}

/// `H[1 V[2 4] 3]` on a 1000x500 work area.
fn horizontal_with_nested_group() -> Harness {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 2 });
    harness.add();
    assert_eq!(harness.shape(), "H[1 V[2 4] 3]");
    harness
}

/// `V[1 H[2 4] 3]` on a 1000x500 work area.
fn vertical_with_nested_group() -> Harness {
    let mut harness = horizontal_with_nested_group();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::ToggleOrientation);
    harness.act(Action::FocusWindow { wid: 2 });
    harness.act(Action::ToggleOrientation);
    assert_eq!(harness.shape(), "V[1 H[2 4] 3]");
    harness
}

#[test]
fn move_left_out_of_a_nested_group_pulls_the_window_out() {
    // The reported case: in `[A | [B/C]]`, Move(B, Left) used to move the whole group and
    // give `[[B/C] | A]`.  B leaves its group instead, and the group of one collapses.
    let mut harness = beside_vertical_group();
    move_focused(&mut harness, 2, Direction::Left);
    assert_eq!(harness.shape(), "H[1 2 3]");
    harness.assert_frame(1, 0.0, 0.0, 400.0, 800.0);
    harness.assert_frame(2, 400.0, 0.0, 400.0, 800.0);
    harness.assert_frame(3, 800.0, 0.0, 400.0, 800.0);
    assert_eq!(harness.layout.focused(), Some(2));
    harness.assert_synced();
}

#[test]
fn move_lower_window_left_out_of_a_nested_group() {
    let mut harness = beside_vertical_group();
    move_focused(&mut harness, 3, Direction::Left);
    assert_eq!(harness.shape(), "H[1 3 2]");
    assert_eq!(harness.layout.focused(), Some(3));
    harness.assert_synced();
}

#[test]
fn move_left_and_right_out_of_a_nested_group() {
    let mut harness = horizontal_with_nested_group();
    move_focused(&mut harness, 2, Direction::Left);
    assert_eq!(harness.shape(), "H[1 2 4 3]");
    harness.assert_synced();

    let mut harness = horizontal_with_nested_group();
    move_focused(&mut harness, 2, Direction::Right);
    assert_eq!(harness.shape(), "H[1 4 2 3]");
    // The moved window takes an equal share; its new siblings shrink proportionally.
    harness.assert_frame(1, 0.0, 0.0, 375.0, 500.0);
    harness.assert_frame(4, 375.0, 0.0, 187.5, 500.0);
    harness.assert_frame(2, 562.5, 0.0, 250.0, 500.0);
    harness.assert_frame(3, 812.5, 0.0, 187.5, 500.0);
    assert_eq!(harness.layout.focused(), Some(2));
    harness.assert_synced();
}

#[test]
fn move_up_and_down_out_of_a_nested_group() {
    let mut harness = vertical_with_nested_group();
    move_focused(&mut harness, 2, Direction::Up);
    assert_eq!(harness.shape(), "V[1 2 4 3]");
    harness.assert_synced();

    let mut harness = vertical_with_nested_group();
    move_focused(&mut harness, 2, Direction::Down);
    assert_eq!(harness.shape(), "V[1 4 2 3]");
    harness.assert_synced();

    let mut harness = vertical_with_nested_group();
    move_focused(&mut harness, 4, Direction::Down);
    assert_eq!(harness.shape(), "V[1 2 4 3]");
    assert_eq!(harness.layout.focused(), Some(4));
    harness.assert_synced();
}

#[test]
fn move_within_a_nested_group_still_swaps() {
    let mut harness = horizontal_with_nested_group();
    move_focused(&mut harness, 2, Direction::Down);
    assert_eq!(harness.shape(), "H[1 V[4 2] 3]");
    move_focused(&mut harness, 2, Direction::Up);
    assert_eq!(harness.shape(), "H[1 V[2 4] 3]");

    let mut harness = vertical_with_nested_group();
    move_focused(&mut harness, 2, Direction::Right);
    assert_eq!(harness.shape(), "V[1 H[4 2] 3]");
    harness.assert_synced();
}

#[test]
fn move_out_of_a_nested_group_at_the_ancestor_edge() {
    // The vertical group sits at the root's right edge; B still leaves it, to the right.
    let mut harness = beside_vertical_group();
    move_focused(&mut harness, 2, Direction::Right);
    assert_eq!(harness.shape(), "H[1 3 2]");
    harness.assert_frame(1, 0.0, 0.0, 400.0, 800.0);
    harness.assert_frame(3, 400.0, 0.0, 400.0, 800.0);
    harness.assert_frame(2, 800.0, 0.0, 400.0, 800.0);
    assert_eq!(harness.layout.focused(), Some(2));
    harness.assert_synced();

    let mut harness = beside_vertical_group();
    move_focused(&mut harness, 3, Direction::Right);
    assert_eq!(harness.shape(), "H[1 2 3]");
    harness.assert_synced();
}

#[test]
fn move_without_an_axis_aligned_ancestor_is_a_noop() {
    // No group above the vertical one splits vertically, and the root is never wrapped in a
    // new split.
    let mut harness = beside_vertical_group();
    for (wid, direction) in [(2, Direction::Up), (3, Direction::Down)] {
        harness.act(Action::FocusWindow { wid });
        let commands = harness.act(Action::Move { direction });
        assert!(commands.is_empty(), "{wid} {direction:?}");
        assert_eq!(harness.shape(), "H[1 V[2 3]]", "{wid} {direction:?}");
    }
    harness.assert_synced();
}

#[test]
fn move_of_a_direct_child_at_the_outer_edge_is_a_noop() {
    let mut harness = beside_vertical_group();
    harness.act(Action::FocusWindow { wid: 1 });
    let commands = harness.act(Action::Move {
        direction: Direction::Left,
    });
    assert!(commands.is_empty());
    assert_eq!(harness.shape(), "H[1 V[2 3]]");

    let mut harness = vertical_with_nested_group();
    for (wid, direction) in [(1, Direction::Up), (3, Direction::Down)] {
        harness.act(Action::FocusWindow { wid });
        let commands = harness.act(Action::Move { direction });
        assert!(commands.is_empty(), "{wid} {direction:?}");
        assert_eq!(harness.shape(), "V[1 H[2 4] 3]", "{wid} {direction:?}");
    }
    harness.assert_synced();
}

#[test]
fn move_out_of_three_levels_of_nesting() {
    // H[1 V[2 H[3 4]]]: window 4 was inserted next to 3 (600x400, wide).
    let deep = || {
        let mut harness = beside_vertical_group();
        harness.add();
        assert_eq!(harness.shape(), "H[1 V[2 H[3 4]]]");
        harness
    };

    // Left from the innermost group skips the vertical group and lands in the root.
    let mut harness = deep();
    move_focused(&mut harness, 3, Direction::Left);
    assert_eq!(harness.shape(), "H[1 3 V[2 4]]");
    harness.assert_synced();

    // Up leaves the innermost group into the middle (vertical) group.
    let mut harness = deep();
    move_focused(&mut harness, 3, Direction::Up);
    assert_eq!(harness.shape(), "H[1 V[2 3 4]]");
    harness.assert_synced();

    // A same-axis sibling inside the innermost group is still a plain swap.
    let mut harness = deep();
    move_focused(&mut harness, 4, Direction::Left);
    assert_eq!(harness.shape(), "H[1 V[2 H[4 3]]]");

    // At the innermost group's right edge, 4 leaves through the vertical group to the root's
    // right edge.
    let mut harness = deep();
    move_focused(&mut harness, 4, Direction::Right);
    assert_eq!(harness.shape(), "H[1 V[2 3] 4]");
    harness.assert_synced();

    // Down and Up leave the innermost group into the vertical group, on either side of it.
    let mut harness = deep();
    move_focused(&mut harness, 4, Direction::Down);
    assert_eq!(harness.shape(), "H[1 V[2 3 4]]");
    let mut harness = deep();
    move_focused(&mut harness, 4, Direction::Up);
    assert_eq!(harness.shape(), "H[1 V[2 4 3]]");

    // 2 is a direct child at the vertical group's top edge and the root splits horizontally.
    let mut harness = deep();
    harness.act(Action::FocusWindow { wid: 2 });
    let commands = harness.act(Action::Move {
        direction: Direction::Up,
    });
    assert!(commands.is_empty());
    assert_eq!(harness.shape(), "H[1 V[2 H[3 4]]]");
}

#[test]
fn a_stack_moves_out_of_a_nested_group_as_a_whole() {
    let mut harness = beside_vertical_group();
    harness.add();
    harness.act(Action::ToggleStacking);
    assert_eq!(harness.shape(), "H[1 V[2 S[3 4]]]");
    harness.act(Action::Move {
        direction: Direction::Left,
    });
    assert_eq!(harness.shape(), "H[1 S[3 4] 2]");
    assert_eq!(harness.layout.focused(), Some(4));
    harness.assert_synced();
}

#[test]
fn moving_out_collapses_the_group_left_with_one_child() {
    // V[1 H[2 4] 3]: moving 4 out leaves H[2], which must collapse into window 2.
    let mut harness = vertical_with_nested_group();
    move_focused(&mut harness, 4, Direction::Up);
    assert_eq!(harness.shape(), "V[1 4 2 3]");
    // 4 is now a direct child of the root, next to 1, so a further move swaps there.
    harness.act(Action::Move {
        direction: Direction::Up,
    });
    assert_eq!(harness.shape(), "V[4 1 2 3]");
    harness.assert_synced();
}
