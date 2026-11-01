// SPDX-License-Identifier: GPL-3.0-or-later
//! Drag-to-stack: the drop zone under the pointer and `Layout::stack_onto`.

mod common;

use common::*;
use cosmic_mac_layout::{DropZone, stack_region};
use cosmic_mac_protocol::{Action, Command, Config, Rect, WindowId};

/// Stack `wid` onto `target` and apply the commands to the fake platform.
fn stack_onto(harness: &mut Harness, wid: WindowId, target: WindowId) -> Vec<Command> {
    let commands = harness.layout.stack_onto(wid, target);
    harness.platform.apply(&commands);
    commands
}

fn tabs(harness: &Harness) -> Vec<WindowId> {
    harness
        .layout
        .active_stack()
        .map(|stack| stack.tabs.iter().map(|tab| tab.wid).collect())
        .unwrap_or_default()
}

/// Tiles including each stack's tab-bar strip; they must exactly cover the work area.
fn tiles(harness: &Harness) -> Vec<Rect> {
    visible_tiles(&harness.layout)
}

/// `H[1 2]`: two side-by-side 500×500 tiles.
fn pair() -> Harness {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    assert_eq!(harness.shape(), "H[1 2]");
    harness
}

/// `H[1 V[2 3]]` on [`BIG_WORK`] (1200×800): window 1 (600×800) beside 2 over 3
/// (600×400 each).
fn nested() -> Harness {
    let mut harness = Harness::with(Config::default(), BIG_WORK);
    harness.add();
    harness.add();
    harness.add();
    assert_eq!(harness.shape(), "H[1 V[2 3]]");
    harness
}

#[test]
fn window_onto_window_creates_a_stack() {
    let mut harness = pair();
    let commands = stack_onto(&mut harness, 1, 2);
    assert_eq!(harness.shape(), "S[2 1]");
    assert_eq!(tabs(&harness), vec![2, 1]);
    assert_eq!(harness.layout.active_stack().unwrap().active, 1);
    assert_eq!(harness.layout.focused(), Some(1));
    assert_eq!(hides(&commands), vec![2]);
    // The stack's tile minus the tab-bar strip.
    assert_eq!(
        set_frames(&commands).get(&1),
        Some(&Rect::new(0.0, 26.0, 1000.0, 474.0))
    );
    assert_eq!(focus_command(&commands), Some(1));
    assert!(stack_changes(&commands).last().unwrap().is_some());
    assert!(harness.layout.is_visible(1));
    assert!(!harness.layout.is_visible(2));
    harness.assert_synced();
}

#[test]
fn window_onto_an_existing_stack_becomes_its_last_tab() {
    let mut harness = nested();
    stack_onto(&mut harness, 3, 2);
    assert_eq!(harness.shape(), "H[1 S[2 3]]");
    stack_onto(&mut harness, 1, 2);
    assert_eq!(harness.shape(), "S[2 3 1]");
    assert_eq!(tabs(&harness), vec![2, 3, 1]);
    assert_eq!(harness.layout.active_stack().unwrap().active, 2);
    assert_eq!(harness.layout.focused(), Some(1));
    harness.assert_frame(1, 0.0, 26.0, 1200.0, 774.0);
    harness.assert_synced();
}

#[test]
fn naming_any_member_of_a_stack_targets_the_whole_stack() {
    let mut harness = nested();
    stack_onto(&mut harness, 3, 2);
    // 2 is the hidden member now; the drop still joins its stack.
    stack_onto(&mut harness, 1, 2);
    assert_eq!(harness.shape(), "S[2 3 1]");
    harness.assert_synced();
}

#[test]
fn a_member_leaving_a_stack_of_two_dissolves_it() {
    let mut harness = nested();
    stack_onto(&mut harness, 3, 2);
    assert_eq!(harness.shape(), "H[1 S[2 3]]");
    let commands = stack_onto(&mut harness, 3, 1);
    assert_eq!(harness.shape(), "H[S[1 3] 2]");
    assert_eq!(tabs(&harness), vec![1, 3]);
    assert_eq!(harness.layout.focused(), Some(3));
    // The member left behind is a plain, visible window again.
    let two = harness
        .layout
        .windows()
        .into_iter()
        .find(|state| state.info.wid == 2)
        .unwrap();
    assert!(!two.in_stack);
    assert!(two.visible);
    assert_eq!(shows(&commands), vec![2]);
    assert_eq!(hides(&commands), vec![1]);
    assert_eq!(harness.layout.stacks().len(), 1);
    assert_tiles(BIG_WORK, &tiles(&harness));
    harness.assert_synced();
}

#[test]
fn a_member_leaving_a_larger_stack_keeps_the_rest_stacked() {
    let mut harness = nested();
    stack_onto(&mut harness, 3, 2);
    stack_onto(&mut harness, 1, 2);
    assert_eq!(harness.shape(), "S[2 3 1]");
    // Nothing else to drop onto: everything is in the one stack.
    assert!(stack_onto(&mut harness, 3, 1).is_empty());
    harness.add();
    assert_eq!(harness.shape(), "H[S[2 3 1] 4]");
    stack_onto(&mut harness, 1, 4);
    assert_eq!(harness.shape(), "H[S[2 3] S[4 1]]");
    assert_tiles(BIG_WORK, &tiles(&harness));
    harness.assert_synced();
}

#[test]
fn dropping_onto_the_own_stack_or_itself_does_nothing() {
    let mut harness = nested();
    stack_onto(&mut harness, 3, 2);
    let shape = harness.shape();
    assert!(stack_onto(&mut harness, 3, 2).is_empty());
    assert!(stack_onto(&mut harness, 2, 3).is_empty());
    assert!(stack_onto(&mut harness, 1, 1).is_empty());
    assert_eq!(harness.shape(), shape);
    harness.assert_synced();
}

#[test]
fn floating_unknown_or_other_workspace_windows_are_not_stacked() {
    let mut harness = pair();
    let dialog = harness.add_with(dialog(9));
    assert!(stack_onto(&mut harness, dialog, 1).is_empty());
    assert!(stack_onto(&mut harness, 1, dialog).is_empty());
    assert!(stack_onto(&mut harness, 1, 42).is_empty());
    harness.act(Action::FocusWindow { wid: 2 });
    harness.act(Action::MoveToWorkspace { index: 2 });
    assert!(stack_onto(&mut harness, 1, 2).is_empty());
    assert_eq!(harness.shape(), "1");
    harness.assert_synced();
}

#[test]
fn a_window_from_a_nested_group_stacks_onto_a_top_level_window() {
    let mut harness = nested();
    stack_onto(&mut harness, 2, 1);
    assert_eq!(harness.shape(), "H[S[1 2] 3]");
    assert_eq!(harness.layout.focused(), Some(2));
    harness.assert_frame(2, 0.0, 26.0, 600.0, 774.0);
    harness.assert_frame(3, 600.0, 0.0, 600.0, 800.0);
    assert_tiles(BIG_WORK, &tiles(&harness));
    harness.assert_synced();
}

#[test]
fn a_top_level_window_stacks_onto_a_window_in_a_nested_group() {
    let mut harness = nested();
    harness.add();
    assert_eq!(harness.shape(), "H[1 V[2 H[3 4]]]");
    // The root group collapses into its remaining child.
    stack_onto(&mut harness, 1, 4);
    assert_eq!(harness.shape(), "V[2 H[3 S[4 1]]]");
    assert_eq!(harness.layout.focused(), Some(1));
    harness.assert_frame(1, 600.0, 426.0, 600.0, 374.0);
    harness.assert_frame(2, 0.0, 0.0, 1200.0, 400.0);
    assert_tiles(BIG_WORK, &tiles(&harness));
    harness.assert_synced();
    // Out of the nested stack (dissolving it) onto another window.
    stack_onto(&mut harness, 1, 2);
    assert_eq!(harness.shape(), "V[S[2 1] H[3 4]]");
    assert_tiles(BIG_WORK, &tiles(&harness));
    harness.assert_synced();
}

#[test]
fn stacking_ends_the_maximize_of_the_dragged_window() {
    let mut harness = nested();
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Maximize);
    harness.assert_frame(1, 0.0, 0.0, 1200.0, 800.0);
    stack_onto(&mut harness, 1, 2);
    assert_eq!(harness.shape(), "V[S[2 1] 3]");
    // The stack's tile minus the tab-bar strip, not the work area.
    harness.assert_frame(1, 0.0, 26.0, 1200.0, 374.0);
    harness.assert_synced();
}

// --- drop zones -----------------------------------------------------------------------------

#[test]
fn the_stack_region_is_the_middle_third() {
    let region = stack_region(Rect::new(300.0, 30.0, 600.0, 900.0));
    assert_eq!(region, Rect::new(500.0, 330.0, 200.0, 300.0));
}

#[test]
fn the_middle_third_of_another_tile_is_a_stack_zone() {
    let harness = pair();
    let tile = Rect::new(500.0, 0.0, 500.0, 500.0);
    let zone = Some(DropZone::Stack { target: 2, tile });
    assert_eq!(harness.layout.drop_zone(1, 750.0, 250.0), zone);
    // Edges: the region is [w/3, 2w/3) on both axes, as cosmic-comp's contains().
    let left = 500.0 + 500.0 / 3.0;
    let right = 500.0 + 1000.0 / 3.0;
    let top = 500.0 / 3.0;
    let bottom = 1000.0 / 3.0;
    assert_eq!(harness.layout.drop_zone(1, left, top), zone);
    assert_eq!(
        harness.layout.drop_zone(1, right - 0.01, bottom - 0.01),
        zone
    );
    assert_eq!(harness.layout.drop_zone(1, left - 0.01, 250.0), None);
    assert_eq!(harness.layout.drop_zone(1, right, 250.0), None);
    assert_eq!(harness.layout.drop_zone(1, 750.0, top - 0.01), None);
    assert_eq!(harness.layout.drop_zone(1, 750.0, bottom), None);
    // Inside the tile but outside its middle third (a future split zone).
    assert_eq!(harness.layout.drop_zone(1, 520.0, 20.0), None);
}

#[test]
fn the_dragged_window_s_own_tile_is_never_a_target() {
    let harness = pair();
    assert_eq!(harness.layout.drop_zone(1, 250.0, 250.0), None);
    assert_eq!(
        harness.layout.drop_zone(2, 250.0, 250.0),
        Some(DropZone::Stack {
            target: 1,
            tile: Rect::new(0.0, 0.0, 500.0, 500.0)
        })
    );
}

#[test]
fn a_stack_is_one_target_named_by_its_visible_member() {
    let mut harness = nested();
    stack_onto(&mut harness, 3, 2);
    // The stack fills the right half; 3 is its visible member.
    assert_eq!(
        harness.layout.drop_zone(1, 900.0, 400.0),
        Some(DropZone::Stack {
            target: 3,
            // The window area below the tab bar.
            tile: Rect::new(600.0, 26.0, 600.0, 774.0)
        })
    );
    // A stack member's own stack is its own tile.
    assert_eq!(harness.layout.drop_zone(3, 900.0, 400.0), None);
    assert!(harness.layout.drop_zone(3, 300.0, 400.0).is_some());
}

#[test]
fn nested_tiles_have_their_own_zones() {
    let harness = nested();
    // 2 is the top right quarter, 3 the bottom right one.
    let two = Rect::new(600.0, 0.0, 600.0, 400.0);
    let three = Rect::new(600.0, 400.0, 600.0, 400.0);
    assert_eq!(
        harness.layout.drop_zone(1, 900.0, 200.0),
        Some(DropZone::Stack {
            target: 2,
            tile: two
        })
    );
    assert_eq!(
        harness.layout.drop_zone(1, 900.0, 600.0),
        Some(DropZone::Stack {
            target: 3,
            tile: three
        })
    );
    // Between the two middle thirds.
    assert_eq!(harness.layout.drop_zone(1, 900.0, 400.0), None);
    // 2 may drop onto 3 or 1, never onto its own tile.
    assert_eq!(harness.layout.drop_zone(2, 900.0, 200.0), None);
    assert!(harness.layout.drop_zone(2, 900.0, 600.0).is_some());
}

#[test]
fn no_zones_for_floating_windows_or_while_a_window_is_maximized() {
    let mut harness = pair();
    let dialog = harness.add_with(dialog(9));
    assert_eq!(harness.layout.drop_zone(dialog, 750.0, 250.0), None);
    assert_eq!(harness.layout.drop_zone(42, 750.0, 250.0), None);
    harness.act(Action::FocusWindow { wid: 1 });
    harness.act(Action::Maximize);
    assert_eq!(harness.layout.drop_zone(1, 750.0, 250.0), None);
}
