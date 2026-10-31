// SPDX-License-Identifier: GPL-3.0-or-later
//! Moving, swapping, orientation and resize behaviour.

mod common;

use common::*;
use cosmic_mac_layout::MIN_WINDOW_SIZE;
use cosmic_mac_protocol::{Action, Direction};

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
