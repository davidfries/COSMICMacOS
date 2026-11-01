// SPDX-License-Identifier: GPL-3.0-or-later
//! Putting a window back where it was (`slot_of` + `window_restored`): minimized, hidden and
//! full-screen windows leave the layout for a while and come back to their old tile.

mod common;

use std::collections::BTreeMap;

use common::*;
use cosmic_mac_layout::WindowSlot;
use cosmic_mac_protocol::{Action, Direction, Rect, WindowId};

/// Take `wid` out of the layout the way the daemon suspends a window.
fn take_out(harness: &mut Harness, wid: WindowId) -> WindowSlot {
    let slot = harness
        .layout
        .slot_of(wid)
        .expect("managed window has a slot");
    harness.remove(wid);
    slot
}

fn put_back(harness: &mut Harness, wid: WindowId, slot: &WindowSlot) {
    let commands = harness.layout.window_restored(info(wid), slot);
    harness.platform.apply(&commands);
}

fn frames(harness: &Harness) -> BTreeMap<WindowId, Rect> {
    harness.layout.visible_frames()
}

/// `1 | (2 / 3)`: on the 1200×800 area window 2's tile is taller than wide, so 3 splits it
/// vertically.
fn three() -> Harness {
    let mut harness = Harness::with(cosmic_mac_protocol::Config::default(), BIG_WORK);
    harness.add();
    harness.add();
    harness.add();
    assert_eq!(harness.shape(), "H[1 V[2 3]]");
    harness
}

#[test]
fn every_window_of_a_split_returns_to_its_tile() {
    for wid in 1..=3 {
        let mut harness = three();
        let before = frames(&harness);
        let slot = take_out(&mut harness, wid);
        assert_ne!(frames(&harness), before, "the rest re-flowed without {wid}");

        put_back(&mut harness, wid, &slot);

        assert_eq!(harness.shape(), "H[1 V[2 3]]", "window {wid}");
        assert_eq!(frames(&harness), before, "window {wid}");
        assert_eq!(harness.layout.focused(), Some(wid));
        harness.assert_synced();
    }
}

#[test]
fn resized_tiles_come_back_at_their_sizes() {
    let mut harness = three();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Resize {
        direction: Direction::Right,
    });
    harness.act(Action::Resize {
        direction: Direction::Right,
    });
    harness.act(Action::FocusWindow { wid: 3 });
    harness.act(Action::Resize {
        direction: Direction::Down,
    });
    let before = frames(&harness);
    assert_ne!(before[&1].width, 600.0, "the resize took effect");
    assert_ne!(before[&3].height, 400.0, "the resize took effect");

    for wid in [1, 3, 2] {
        let slot = take_out(&mut harness, wid);
        put_back(&mut harness, wid, &slot);
        assert_eq!(frames(&harness), before, "window {wid}");
    }
}

#[test]
fn a_window_between_two_siblings_keeps_its_position() {
    // On a very wide area every insert splits side by side: four windows in one group,
    // with unequal weights.
    let mut harness = Harness::with(
        cosmic_mac_protocol::Config::default(),
        Rect::new(0.0, 0.0, 4000.0, 500.0),
    );
    harness.add();
    harness.add();
    harness.add();
    harness.add();
    let shape = harness.shape();
    assert_eq!(shape, "H[1 2 3 4]");
    let before = frames(&harness);

    let slot = take_out(&mut harness, 2);
    put_back(&mut harness, 2, &slot);

    assert_eq!(harness.shape(), shape);
    assert_eq!(frames(&harness), before);
}

#[test]
fn the_last_window_comes_back_as_the_root() {
    let mut harness = Harness::new();
    harness.add();
    let slot = take_out(&mut harness, 1);
    assert_eq!(harness.shape(), "");

    put_back(&mut harness, 1, &slot);

    assert_eq!(harness.shape(), "1");
    harness.assert_frame(1, WORK.x, WORK.y, WORK.width, WORK.height);
    harness.assert_synced();
}

#[test]
fn neighbours_that_are_gone_fall_back_to_a_normal_insert() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    let slot = take_out(&mut harness, 2);
    harness.remove(1);
    harness.add_with(info(5));

    put_back(&mut harness, 2, &slot);

    assert!(harness.layout.window_info(2).is_some());
    assert_eq!(harness.shape(), "H[5 2]");
    assert_eq!(harness.layout.focused(), Some(2));
    harness.assert_synced();
}

#[test]
fn neighbours_rearranged_among_other_windows_get_the_window_beside_the_nearest() {
    let mut harness = three();
    // 3's neighbour is 2 (it came after 2 in the vertical group).
    let slot = take_out(&mut harness, 3);
    assert_eq!(harness.shape(), "H[1 2]");
    // 2 joins a new window in a group.
    harness.act(Action::FocusWindow { wid: 2 });
    harness.add_with(info(4));
    assert_eq!(harness.shape(), "H[1 V[2 4]]");

    put_back(&mut harness, 3, &slot);

    assert_eq!(harness.shape(), "H[1 V[2 3 4]]");
    harness.assert_synced();
}

#[test]
fn a_stack_member_returns_to_its_stack_at_its_index() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.add();
    harness.act(Action::ToggleStacking);
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Focus {
        direction: Direction::Right,
    });
    // Stack the third window into the stack too.
    assert_eq!(harness.shape(), "H[1 S[2 3]]");
    let before = frames(&harness);

    let slot = take_out(&mut harness, 2);
    assert_eq!(harness.shape(), "H[1 3]", "a stack of one collapses");

    put_back(&mut harness, 2, &slot);

    assert_eq!(harness.shape(), "H[1 S[2 3]]");
    assert_eq!(
        harness.layout.focused(),
        Some(2),
        "the restored tab is active"
    );
    assert_eq!(frames(&harness).get(&2), before.get(&3));
    harness.assert_synced();
}

#[test]
fn a_floating_window_returns_to_its_frame() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.act(Action::ToggleWindowFloating);
    let floating = harness.frame(2);

    let slot = take_out(&mut harness, 2);
    put_back(&mut harness, 2, &slot);

    assert_eq!(harness.frame(2), floating);
    assert!(
        harness
            .layout
            .windows()
            .iter()
            .any(|state| state.info.wid == 2 && state.floating)
    );
    harness.assert_synced();
}

#[test]
fn a_maximized_window_comes_back_maximized() {
    let mut harness = three();
    harness.act(Action::FocusWindow { wid: 2 });
    harness.act(Action::Maximize);
    let area = BIG_WORK;
    harness.assert_frame(2, area.x, area.y, area.width, area.height);

    let slot = take_out(&mut harness, 2);
    put_back(&mut harness, 2, &slot);

    harness.assert_frame(2, area.x, area.y, area.width, area.height);
    harness.assert_synced();
}

#[test]
fn a_slot_on_another_workspace_adds_the_window_to_the_active_one() {
    let mut harness = three();
    let slot = take_out(&mut harness, 3);
    harness.act(Action::Workspace { index: 2 });
    harness.add_with(info(7));

    put_back(&mut harness, 3, &slot);

    let state = harness
        .layout
        .windows()
        .into_iter()
        .find(|state| state.info.wid == 3)
        .expect("managed again");
    assert_eq!(state.workspace, 2);
    assert!(state.visible);
    assert_eq!(harness.shape(), "H[7 3]");
    harness.assert_synced();
}

#[test]
fn restoring_a_managed_window_changes_nothing() {
    let mut harness = three();
    let slot = harness.layout.slot_of(2).unwrap();
    let before = frames(&harness);

    let commands = harness.layout.window_restored(info(2), &slot);

    assert!(set_frames(&commands).is_empty(), "{commands:?}");
    assert_eq!(frames(&harness), before);
    assert_eq!(harness.shape(), "H[1 V[2 3]]");
}
