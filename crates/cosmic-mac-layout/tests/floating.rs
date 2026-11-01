// SPDX-License-Identifier: GPL-3.0-or-later
//! Floating layer, default-float rules and the workspace tiling toggle.

mod common;

use common::*;
use cosmic_mac_protocol::{Action, Config, Rect, TilingException, ToplevelInfo};

fn floating(wid: u64) -> ToplevelInfo {
    ToplevelInfo::new(wid, "com.example.Float", format!("Float {wid}")).with_resizable(false)
}

#[test]
fn dialog_floats_centred_with_its_initial_size() {
    let mut harness = Harness::new();
    harness.add_with(dialog(1));
    harness.assert_frame(1, 300.0, 150.0, 400.0, 200.0);
    assert!(harness.layout.is_visible(1));
    let windows = harness.layout.windows();
    assert!(windows[0].floating);
}

#[test]
fn non_resizable_window_floats() {
    let mut harness = Harness::new();
    harness.add_with(
        ToplevelInfo::new(1, "com.example.Util", "util")
            .with_resizable(false)
            .with_initial_frame(Rect::new(0.0, 0.0, 300.0, 200.0)),
    );
    harness.assert_frame(1, 350.0, 150.0, 300.0, 200.0);
    assert!(harness.layout.windows()[0].floating);
}

#[test]
fn floating_without_initial_frame_uses_default_size() {
    let mut harness = Harness::new();
    harness.add_with(ToplevelInfo::new(1, "com.example.Float", "float").with_resizable(false));
    harness.assert_frame(1, 200.0, 100.0, 600.0, 300.0);
}

#[test]
fn autotile_false_floats_new_windows() {
    let mut harness = Harness::with(
        Config {
            autotile: false,
            ..Config::default()
        },
        WORK,
    );
    harness.add();
    assert!(harness.layout.windows()[0].floating);
    assert!(!harness.layout.tiling_enabled());
    harness.assert_frame(1, 200.0, 100.0, 600.0, 300.0);
}

#[test]
fn tiling_exception_floats_matching_windows() {
    let mut harness = Harness::with(
        Config {
            tiling_exceptions: vec![TilingException::new("com.example.App")],
            ..Config::default()
        },
        WORK,
    );
    harness.add();
    assert!(harness.layout.windows()[0].floating);
}

#[test]
fn floating_windows_cascade() {
    let mut harness = Harness::new();
    harness.add_with(
        ToplevelInfo::new(1, "com.example.Float", "a")
            .with_resizable(false)
            .with_initial_frame(Rect::new(0.0, 0.0, 200.0, 100.0)),
    );
    harness.add_with(
        ToplevelInfo::new(2, "com.example.Float", "b")
            .with_resizable(false)
            .with_initial_frame(Rect::new(0.0, 0.0, 200.0, 100.0)),
    );
    harness.add_with(
        ToplevelInfo::new(3, "com.example.Float", "c")
            .with_resizable(false)
            .with_initial_frame(Rect::new(0.0, 0.0, 200.0, 100.0)),
    );
    harness.assert_frame(1, 400.0, 200.0, 200.0, 100.0);
    harness.assert_frame(2, 432.0, 232.0, 200.0, 100.0);
    harness.assert_frame(3, 464.0, 264.0, 200.0, 100.0);
}

#[test]
fn floating_cascade_wraps_after_eight() {
    let mut harness = Harness::new();
    for _ in 0..8 {
        harness.add_with(floating(harness.next_wid()));
    }
    // The eighth cascade step (index 8) wraps back to offset 0.
    harness.add_with(floating(harness.next_wid()));
    let last = harness
        .layout
        .visible_frames()
        .keys()
        .copied()
        .max()
        .unwrap();
    assert_eq!(
        harness.layout.frame_of(last),
        Some(Rect::new(200.0, 100.0, 600.0, 300.0))
    );
}

#[test]
fn floating_is_clamped_into_the_work_area() {
    let mut harness = Harness::new();
    harness.add_with(
        ToplevelInfo::new(1, "com.example.Huge", "huge")
            .with_resizable(false)
            .with_initial_frame(Rect::new(0.0, 0.0, 2000.0, 2000.0)),
    );
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
}

#[test]
fn floating_cascade_is_clamped_at_the_edge() {
    let mut harness = Harness::new();
    for index in 0..8 {
        harness.add_with(floating(index + 1));
    }
    harness.assert_frame(8, 400.0, 200.0, 600.0, 300.0);
}

#[test]
fn toggle_window_floating_moves_window_to_floating_layer() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleWindowFloating);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
    assert_eq!(harness.layout.focused(), Some(2));
    assert!(harness.layout.windows()[1].floating);
    harness.assert_synced();
}

#[test]
fn toggle_window_floating_tiles_a_floating_window() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleWindowFloating);
    harness.act(Action::ToggleWindowFloating);
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    assert!(!harness.layout.windows()[1].floating);
    harness.assert_synced();
}

#[test]
fn is_floating_follows_the_floating_layer() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    assert!(!harness.layout.is_floating(1));
    assert!(!harness.layout.is_floating(2));
    harness.act(Action::ToggleWindowFloating);
    assert!(harness.layout.is_floating(2));
    assert!(!harness.layout.is_floating(1));
    // A window on another workspace is still reported.
    harness.act(Action::MoveToWorkspace { index: 2 });
    assert!(harness.layout.is_floating(2));
    // With tiling disabled every window floats; unknown windows never do.
    harness.act(Action::ToggleTiling);
    assert!(harness.layout.is_floating(1));
    assert!(!harness.layout.is_floating(99));
}

#[test]
fn toggle_window_floating_keeps_focus() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleWindowFloating);
    assert_eq!(harness.layout.focused(), Some(2));
    harness.act(Action::ToggleWindowFloating);
    assert_eq!(harness.layout.focused(), Some(2));
}

#[test]
fn toggle_window_floating_on_stack_member() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.act(Action::ToggleWindowFloating);
    assert!(harness.layout.active_stack().is_none());
    assert!(harness.layout.windows()[1].floating);
    assert!(harness.layout.is_visible(1));
    assert!(harness.layout.is_visible(2));
    // Floated at the frame it had: below the tab bar.
    harness.assert_frame(2, 0.0, 26.0, 1000.0, 474.0);
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
    harness.assert_synced();
}

#[test]
fn toggle_window_floating_when_tiling_disabled_is_a_noop() {
    let mut harness = Harness::new();
    harness.add();
    harness.act(Action::ToggleTiling);
    let commands = harness.act(Action::ToggleWindowFloating);
    assert!(commands.is_empty());
    assert!(harness.layout.windows()[0].floating);
}

#[test]
fn toggle_tiling_off_preserves_frames() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    let commands = harness.act(Action::ToggleTiling);
    assert!(!harness.layout.tiling_enabled());
    assert!(harness.layout.tree_snapshot().is_none());
    assert!(set_frames(&commands).is_empty());
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    assert!(
        harness
            .layout
            .windows()
            .iter()
            .all(|window| window.floating)
    );
    harness.assert_synced();
}

#[test]
fn toggle_tiling_on_retiles_all_windows() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleTiling);
    harness.act(Action::ToggleTiling);
    assert!(harness.layout.tiling_enabled());
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    assert!(
        harness
            .layout
            .windows()
            .iter()
            .all(|window| !window.floating)
    );
    harness.assert_synced();
}

#[test]
fn toggle_tiling_on_rebuilds_a_balanced_tree_in_the_previous_order() {
    // Inserting next to the focus built a halving chain: 1/2, 1/4 … 1/32 of the work area.
    // Re-enabling tiling inserts next to the largest tile instead (as the daemon's startup
    // does) and keeps the previous window order.
    let mut harness = Harness::new();
    for _ in 0..6 {
        harness.add();
    }
    assert_eq!(harness.shape(), "H[1 2 V[3 H[4 V[5 6]]]]");
    harness.act(Action::ToggleTiling);
    harness.act(Action::ToggleTiling);

    // Ties go to the last window, so the focused (last) window ends up paired with a window,
    // which ToggleStacking needs.
    assert_eq!(harness.shape(), "H[1 2 V[3 4] V[5 6]]");
    harness.assert_frame(1, 0.0, 0.0, 250.0, 500.0);
    harness.assert_frame(2, 250.0, 0.0, 250.0, 500.0);
    harness.assert_frame(3, 500.0, 0.0, 250.0, 250.0);
    harness.assert_frame(4, 500.0, 250.0, 250.0, 250.0);
    harness.assert_frame(5, 750.0, 0.0, 250.0, 250.0);
    harness.assert_frame(6, 750.0, 250.0, 250.0, 250.0);
    let total = WORK.area();
    for wid in 1..=6 {
        assert!(harness.frame(wid).area() >= total / 8.0, "window {wid}");
    }
    assert_eq!(harness.layout.focused(), Some(6));
    harness.assert_synced();
}

#[test]
fn toggle_stacking_works_after_toggle_tiling_on() {
    // The VM scenario stacks right after Super+Y twice; with three windows the focused one
    // must have a window neighbour.  The VM's work area: its near-square halves split
    // vertically, which is what put the focused window beside a group.
    let mut harness = Harness::with(Config::default(), Rect::new(0.0, 25.0, 1920.0, 970.0));
    for _ in 0..3 {
        harness.add();
    }
    harness.act(Action::ToggleTiling);
    harness.act(Action::ToggleTiling);
    harness.act(Action::ToggleStacking);
    let stack = harness.layout.active_stack().expect("a stack formed");
    assert_eq!(stack.tabs.len(), 2);
    harness.assert_synced();
}

#[test]
fn toggle_tiling_off_cascades_hidden_stack_members() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.act(Action::ToggleTiling);
    assert!(
        harness
            .layout
            .windows()
            .iter()
            .all(|window| window.floating)
    );
    assert!(harness.layout.is_visible(1));
    assert!(harness.layout.is_visible(2));
    // Floated at the frame it had: below the tab bar.
    harness.assert_frame(2, 0.0, 26.0, 1000.0, 474.0);
    harness.assert_frame(1, 200.0, 100.0, 600.0, 300.0);
    harness.assert_synced();
}

#[test]
fn toggle_tiling_off_with_three_windows_keeps_frames() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.add();
    harness.act(Action::ToggleTiling);
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 250.0, 500.0);
    harness.assert_frame(3, 750.0, 0.0, 250.0, 500.0);
    harness.assert_synced();
}

#[test]
fn floating_focus_cycles_with_tiling_windows() {
    let mut harness = Harness::new();
    harness.add();
    harness.add_with(dialog(2));
    assert_eq!(harness.layout.focused(), Some(2));
    harness.act(Action::FocusNext);
    assert_eq!(harness.layout.focused(), Some(1));
    harness.act(Action::FocusNext);
    assert_eq!(harness.layout.focused(), Some(2));
    harness.assert_synced();
}

#[test]
fn removing_floating_window_empties_workspace() {
    let mut harness = Harness::new();
    harness.add_with(dialog(1));
    harness.remove(1);
    assert!(harness.layout.visible_frames().is_empty());
    assert_eq!(harness.layout.focused(), None);
}

#[test]
fn floating_windows_are_listed_with_their_frames() {
    let mut harness = Harness::new();
    harness.add();
    harness.add_with(dialog(2));
    let windows = harness.layout.windows();
    let floating = windows.iter().find(|window| window.info.wid == 2).unwrap();
    assert!(floating.floating);
    assert_eq!(floating.frame, Rect::new(300.0, 150.0, 400.0, 200.0));
    assert_eq!(floating.workspace, 1);
}

#[test]
fn toggle_tiling_enables_tiling_even_when_autotile_is_false() {
    let mut harness = Harness::with(
        Config {
            autotile: false,
            ..Config::default()
        },
        WORK,
    );
    harness.add();
    assert!(!harness.layout.tiling_enabled());
    assert!(harness.layout.windows()[0].floating);
    harness.act(Action::ToggleTiling);
    assert!(harness.layout.tiling_enabled());
    assert!(!harness.layout.windows()[0].floating);
    harness.add();
    assert!(!harness.layout.windows()[1].floating);
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    harness.assert_synced();
}

#[test]
fn floating_moved_adopts_the_user_frame() {
    let mut harness = Harness::new();
    harness.add();
    harness.add_with(dialog(2));
    harness.assert_frame(2, 300.0, 150.0, 400.0, 200.0);

    let moved = Rect::new(40.0, 30.0, 420.0, 260.0);
    assert!(harness.layout.floating_moved(2, moved));
    // The user put the window there; the fake platform follows.
    harness.platform.frames.insert(2, moved);
    harness.assert_frame(2, 40.0, 30.0, 420.0, 260.0);
    let state = harness
        .layout
        .windows()
        .into_iter()
        .find(|window| window.info.wid == 2)
        .unwrap();
    assert_eq!(state.frame, moved);

    // The next diff must not echo the frame back (or snap it to the old placement).
    let commands = harness.update(dialog(2));
    assert!(!set_frames(&commands).contains_key(&2), "{commands:?}");
    harness.assert_synced();

    // A workspace round trip re-shows the window at the moved frame, not the old one.
    harness.act(Action::Workspace { index: 2 });
    let commands = harness.act(Action::Workspace { index: 1 });
    assert!(shows(&commands).contains(&2), "{commands:?}");
    assert_eq!(set_frames(&commands).get(&2), Some(&moved), "{commands:?}");
    harness.assert_synced();
}

#[test]
fn floating_moved_ignores_tiled_and_unknown_windows() {
    let mut harness = Harness::new();
    harness.add();
    let tiled = harness.frame(1);
    assert!(
        !harness
            .layout
            .floating_moved(1, Rect::new(5.0, 5.0, 100.0, 100.0))
    );
    assert!(
        !harness
            .layout
            .floating_moved(99, Rect::new(5.0, 5.0, 100.0, 100.0))
    );
    assert_eq!(harness.frame(1), tiled);
    let commands = harness.update(info(1));
    assert!(commands.is_empty(), "{commands:?}");
}

#[test]
fn floating_moved_with_the_same_frame_reports_no_change() {
    let mut harness = Harness::new();
    harness.add_with(dialog(1));
    let frame = harness.frame(1);
    assert!(!harness.layout.floating_moved(1, frame));
}

#[test]
fn floating_moved_while_maximized_updates_the_restore_frame() {
    let mut harness = Harness::new();
    harness.add_with(dialog(1));
    harness.act(Action::Maximize);
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);

    let moved = Rect::new(40.0, 30.0, 1000.0, 500.0);
    assert!(harness.layout.floating_moved(1, moved));
    // Still maximized: the layout keeps its frame and the next diff leaves the window alone.
    harness.assert_frame(1, 0.0, 0.0, 1000.0, 500.0);
    let commands = harness.update(dialog(1));
    assert!(set_frames(&commands).is_empty(), "{commands:?}");

    // Un-maximizing returns to where the user dragged it.
    let commands = harness.act(Action::Maximize);
    assert_eq!(set_frames(&commands).get(&1), Some(&moved), "{commands:?}");
    harness.assert_frame(1, 40.0, 30.0, 1000.0, 500.0);
    harness.assert_synced();
}
