// SPDX-License-Identifier: GPL-3.0-or-later
//! Focus navigation, focus history and wrap-around.

mod common;

use common::*;
use cosmic_mac_protocol::{Action, Config, Direction, Rect, ToplevelInfo};

fn three_horizontal() -> Harness {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.add();
    harness
}

/// One full-height window on the left half and two windows splitting the right half — the
/// arrangement from the VM integration test.  The two right-hand windows are exactly
/// equidistant from the left window's centre, so directional focus must break the tie by
/// position (topmost wins), never by window-id iteration order.
fn left_and_two_on_the_right() -> Harness {
    let mut harness = Harness::with(Config::default(), Rect::new(0.0, 0.0, 1728.0, 975.0));
    harness.add();
    harness.add();
    harness.add();
    harness
}

#[test]
fn focus_right_from_left_half_picks_the_top_right_window() {
    let mut harness = left_and_two_on_the_right();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Focus {
        direction: Direction::Right,
    });
    // Windows 2 and 3 tie on distance; the top one wins no matter which wid it got.
    let second = harness.frame(2);
    let third = harness.frame(3);
    let expected = if second.y <= third.y { 2 } else { 3 };
    assert_eq!(harness.layout.focused(), Some(expected));
}

fn three_nested() -> Harness {
    let mut harness = Harness::with(Config::default(), Rect::new(0.0, 0.0, 1000.0, 1000.0));
    harness.add();
    harness.add();
    harness.add();
    harness
}

#[test]
fn focus_right_moves_to_next_sibling() {
    let mut harness = three_horizontal();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Focus {
        direction: Direction::Right,
    });
    assert_eq!(harness.layout.focused(), Some(2));
}

#[test]
fn focus_right_twice_reaches_last_window() {
    let mut harness = three_horizontal();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Focus {
        direction: Direction::Right,
    });
    harness.act(Action::Focus {
        direction: Direction::Right,
    });
    assert_eq!(harness.layout.focused(), Some(3));
}

#[test]
fn focus_right_wraps_to_first_window() {
    let mut harness = three_horizontal();
    harness.act(Action::FocusWindow { wid: 3 });
    let commands = harness.act(Action::Focus {
        direction: Direction::Right,
    });
    assert_eq!(harness.layout.focused(), Some(1));
    assert_eq!(focus_command(&commands), Some(1));
}

#[test]
fn focus_left_moves_to_previous_sibling() {
    let mut harness = three_horizontal();
    harness.act(Action::FocusWindow { wid: 3 });
    harness.act(Action::Focus {
        direction: Direction::Left,
    });
    assert_eq!(harness.layout.focused(), Some(2));
}

#[test]
fn focus_left_wraps_to_last_window() {
    let mut harness = three_horizontal();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Focus {
        direction: Direction::Left,
    });
    assert_eq!(harness.layout.focused(), Some(3));
}

#[test]
fn focus_perpendicular_direction_is_a_noop() {
    let mut harness = three_horizontal();
    harness.act(Action::FocusWindow { wid: 2 });
    let commands = harness.act(Action::Focus {
        direction: Direction::Up,
    });
    assert!(commands.is_empty());
    assert_eq!(harness.layout.focused(), Some(2));
}

#[test]
fn focus_on_single_window_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::Focus {
        direction: Direction::Left,
    });
    assert!(commands.is_empty());
    assert_eq!(harness.layout.focused(), Some(1));
}

#[test]
fn focus_with_no_windows_is_a_noop() {
    let mut harness = Harness::new();
    let commands = harness.act(Action::Focus {
        direction: Direction::Left,
    });
    assert!(commands.is_empty());
    assert_eq!(harness.layout.focused(), None);
}

#[test]
fn focus_none_selects_the_first_window() {
    let mut harness = Harness::new();
    harness.add();
    // Simulate an external unfocus by moving the only window to another workspace and back.
    harness.act(Action::Workspace { index: 2 });
    harness.act(Action::Workspace { index: 1 });
    assert_eq!(harness.layout.focused(), Some(1));
}

#[test]
fn focus_direction_enters_nested_group() {
    let mut harness = three_nested();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Focus {
        direction: Direction::Right,
    });
    assert_eq!(harness.layout.focused(), Some(2));
}

#[test]
fn focus_direction_within_nested_group() {
    let mut harness = three_nested();
    harness.act(Action::FocusWindow { wid: 2 });
    harness.act(Action::Focus {
        direction: Direction::Down,
    });
    assert_eq!(harness.layout.focused(), Some(3));
    harness.act(Action::Focus {
        direction: Direction::Up,
    });
    assert_eq!(harness.layout.focused(), Some(2));
}

#[test]
fn focus_direction_leaves_nested_group() {
    let mut harness = three_nested();
    harness.act(Action::FocusWindow { wid: 2 });
    harness.act(Action::Focus {
        direction: Direction::Left,
    });
    assert_eq!(harness.layout.focused(), Some(1));
}

#[test]
fn focus_wrap_from_nested_group() {
    let mut harness = three_nested();
    harness.act(Action::FocusWindow { wid: 3 });
    harness.act(Action::Focus {
        direction: Direction::Right,
    });
    assert_eq!(harness.layout.focused(), Some(1));
}

#[test]
fn focus_wrap_backwards_from_nested_group() {
    let mut harness = three_nested();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Focus {
        direction: Direction::Left,
    });
    assert_eq!(harness.layout.focused(), Some(3));
}

#[test]
fn focus_direction_activates_stack_tab() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    // Add a window next to the stack, then move left into the stack.
    harness.add();
    assert_eq!(harness.layout.focused(), Some(3));
    harness.act(Action::Focus {
        direction: Direction::Left,
    });
    assert_eq!(harness.layout.focused(), Some(2));
    let stack = harness.layout.active_stack().unwrap();
    assert_eq!(stack.active, 1);
    assert!(harness.layout.is_visible(2));
    assert!(!harness.layout.is_visible(1));
}

#[test]
fn focus_direction_leaves_stack() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.add();
    harness.act(Action::Focus {
        direction: Direction::Left,
    });
    harness.act(Action::Focus {
        direction: Direction::Right,
    });
    assert_eq!(harness.layout.focused(), Some(3));
}

#[test]
fn focus_direction_from_floating_wraps() {
    let mut harness = Harness::new();
    harness.add();
    harness.add_with(dialog(2));
    assert_eq!(harness.layout.focused(), Some(2));
    harness.act(Action::Focus {
        direction: Direction::Left,
    });
    assert_eq!(harness.layout.focused(), Some(1));
}

#[test]
fn focus_direction_from_floating_picks_nearest() {
    let mut harness = Harness::new();
    harness.add();
    harness.add_with(
        ToplevelInfo::new(2, "com.example.Float", "float")
            .with_resizable(false)
            .with_initial_frame(Rect::new(0.0, 0.0, 200.0, 100.0)),
    );
    harness.add_with(
        ToplevelInfo::new(3, "com.example.Float", "float")
            .with_resizable(false)
            .with_initial_frame(Rect::new(0.0, 0.0, 200.0, 100.0)),
    );
    assert_eq!(harness.layout.focused(), Some(3));
    harness.act(Action::Focus {
        direction: Direction::Left,
    });
    assert_eq!(harness.layout.focused(), Some(1));
}

#[test]
fn focus_window_switches_to_other_workspace() {
    let mut harness = Harness::new();
    harness.add();
    harness.act(Action::Workspace { index: 2 });
    harness.add();
    harness.act(Action::FocusWindow { wid: 1 });
    assert_eq!(harness.layout.active_workspace(), 1);
    assert_eq!(harness.layout.focused(), Some(1));
    assert!(harness.layout.is_visible(1));
    harness.assert_synced();
}

#[test]
fn focus_window_unknown_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::FocusWindow { wid: 99 });
    assert!(commands.is_empty());
    assert_eq!(harness.layout.focused(), Some(1));
}

#[test]
fn focus_next_cycles_through_all_windows() {
    let mut harness = three_horizontal();
    assert_eq!(harness.layout.focused(), Some(3));
    harness.act(Action::FocusNext);
    assert_eq!(harness.layout.focused(), Some(2));
    harness.act(Action::FocusNext);
    assert_eq!(harness.layout.focused(), Some(1));
    harness.act(Action::FocusNext);
    assert_eq!(harness.layout.focused(), Some(3));
}

#[test]
fn focus_previous_cycles_backwards() {
    let mut harness = three_horizontal();
    harness.act(Action::FocusPrevious);
    assert_eq!(harness.layout.focused(), Some(1));
    harness.act(Action::FocusPrevious);
    assert_eq!(harness.layout.focused(), Some(2));
    harness.act(Action::FocusPrevious);
    assert_eq!(harness.layout.focused(), Some(3));
}

#[test]
fn focus_next_with_one_window_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    let commands = harness.act(Action::FocusNext);
    assert!(commands.is_empty());
    assert_eq!(harness.layout.focused(), Some(1));
}

#[test]
fn focus_next_after_removal_uses_mru() {
    let mut harness = three_horizontal();
    harness.remove(3);
    harness.act(Action::FocusNext);
    assert_eq!(harness.layout.focused(), Some(1));
}

#[test]
fn focus_changed_activates_stack_member() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    assert_eq!(harness.layout.focused(), Some(2));
    let commands = harness.layout.focus_changed(1);
    harness.platform.apply(&commands);
    assert_eq!(harness.layout.focused(), Some(1));
    assert!(harness.layout.is_visible(1));
    assert!(!harness.layout.is_visible(2));
    assert_eq!(harness.layout.active_stack().unwrap().active, 0);
    harness.assert_synced();
}

#[test]
fn focus_after_toggle_orientation_uses_new_axis() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::ToggleOrientation);
    harness.act(Action::Focus {
        direction: Direction::Down,
    });
    assert_eq!(harness.layout.focused(), Some(2));
}

#[test]
fn focus_direction_does_not_cross_workspaces() {
    let mut harness = Harness::new();
    harness.add();
    harness.act(Action::Workspace { index: 2 });
    harness.add();
    harness.act(Action::Focus {
        direction: Direction::Left,
    });
    assert_eq!(harness.layout.active_workspace(), 2);
    assert_eq!(harness.layout.focused(), Some(2));
}

#[test]
fn focus_direction_with_floating_and_tiling_keeps_tiling_focus() {
    let mut harness = Harness::new();
    harness.add();
    harness.add_with(dialog(2));
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Focus {
        direction: Direction::Right,
    });
    // Tiled focus stays tree-based; floating windows are not picked up directionally.
    assert_eq!(harness.layout.focused(), Some(1));
}
