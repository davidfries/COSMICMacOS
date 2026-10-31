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
    harness.assert_frame(2, 0.0, 0.0, 1000.0, 500.0);
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
    harness.assert_frame(2, 0.0, 0.0, 1000.0, 500.0);
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
