// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-window minimum/maximum size handling in the geometry walk.

mod common;

use common::*;
use cosmic_mac_protocol::{Action, Command, Config, Direction, Rect};

fn has_set_frame(commands: &[Command], wid: u64) -> bool {
    commands
        .iter()
        .any(|command| matches!(command, Command::SetFrame { wid: other, .. } if *other == wid))
}

fn assert_sane(harness: &Harness, area: Rect) {
    harness.assert_synced();
    let frames: Vec<Rect> = harness.layout.visible_frames().values().copied().collect();
    for frame in &frames {
        assert!(
            frame.x >= area.x - 1e-6
                && frame.y >= area.y - 1e-6
                && frame.right() <= area.right() + 1e-6
                && frame.bottom() <= area.bottom() + 1e-6,
            "frame {frame:?} escapes {area:?}"
        );
        assert!(
            frame.width >= 0.0 && frame.height >= 0.0,
            "negative frame {frame:?}"
        );
    }
    for (index, frame) in frames.iter().enumerate() {
        for other in &frames[index + 1..] {
            assert!(!frame.overlaps(*other), "{frame:?} overlaps {other:?}");
        }
    }
}

#[test]
fn minimum_size_shrinks_its_tile_and_sibling_absorbs_the_rest() {
    // Two 150px columns; the first window needs 200px, so the second gives up 50px.
    let mut harness = Harness::with(Config::default(), Rect::new(0.0, 0.0, 300.0, 200.0));
    harness.add_with(constrained(1, Some((200.0, 100.0)), None));
    harness.add_with(info(2));

    harness.assert_frame(1, 0.0, 0.0, 200.0, 200.0);
    harness.assert_frame(2, 200.0, 0.0, 100.0, 200.0);
    harness.assert_synced();
}

#[test]
fn minimum_size_below_the_tile_does_not_shrink_the_window() {
    // A 416px tile is larger than the 200px minimum; the tile must stay unchanged.
    let mut harness = Harness::with(Config::default(), Rect::new(0.0, 0.0, 832.0, 400.0));
    harness.add_with(constrained(1, Some((200.0, 100.0)), None));
    harness.add_with(info(2));

    harness.assert_frame(1, 0.0, 0.0, 416.0, 400.0);
    harness.assert_frame(2, 416.0, 0.0, 416.0, 400.0);
}

#[test]
fn notes_style_width_minimum_is_taken_from_the_sibling() {
    // The VM bug: Notes clamps its width to 642px in a 1000px two-way split.
    let mut harness = Harness::with(Config::default(), WORK);
    harness.add_with(constrained(1, Some((642.0, 100.0)), None));
    harness.add_with(info(2));

    harness.assert_frame(1, 0.0, 0.0, 642.0, 500.0);
    harness.assert_frame(2, 642.0, 0.0, 358.0, 500.0);
    harness.assert_synced();
}

#[test]
fn all_windows_at_their_maximum_are_centred_in_the_group() {
    // Two columns with a 100px maximum each in a 416px row: 216px is left over and split
    // symmetrically, so no wallpaper appears on only one side.
    let mut harness = Harness::with(Config::default(), Rect::new(0.0, 0.0, 416.0, 200.0));
    harness.add_with(constrained(1, None, Some((100.0, f64::MAX))));
    harness.add_with(constrained(2, None, Some((100.0, f64::MAX))));

    harness.assert_frame(1, 108.0, 0.0, 100.0, 200.0);
    harness.assert_frame(2, 208.0, 0.0, 100.0, 200.0);
    harness.assert_synced();
    assert_sane(&harness, Rect::new(0.0, 0.0, 416.0, 200.0));
}

#[test]
fn maximum_size_clamps_on_the_split_axis() {
    let mut harness = Harness::with(Config::default(), WORK);
    harness.add_with(constrained(1, None, Some((200.0, f64::MAX))));
    harness.add_with(info(2));

    harness.assert_frame(1, 0.0, 0.0, 200.0, 500.0);
    harness.assert_frame(2, 200.0, 0.0, 800.0, 500.0);
    harness.assert_synced();
}

#[test]
fn maximum_size_on_the_cross_axis_centres_the_window_in_its_tile() {
    let mut harness = Harness::with(Config::default(), WORK);
    harness.add_with(constrained(1, None, Some((f64::MAX, 300.0))));
    harness.add_with(info(2));

    harness.assert_frame(1, 0.0, 100.0, 500.0, 300.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    harness.assert_synced();
}

#[test]
fn a_nested_group_reports_its_children_minimum_to_the_parent() {
    // Root is horizontal: window 1 and a vertical group containing windows 2 and 3.
    // Window 2 needs 600px of width, so the group needs 600px and window 1 shrinks to 400px.
    let mut harness = Harness::with(Config::default(), Rect::new(0.0, 0.0, 1000.0, 1000.0));
    harness.add_with(info(1));
    harness.add_with(constrained(2, Some((600.0, 0.0)), None));
    harness.add_with(constrained(3, Some((300.0, 0.0)), None));

    harness.assert_frame(1, 0.0, 0.0, 400.0, 1000.0);
    harness.assert_frame(2, 400.0, 0.0, 600.0, 500.0);
    harness.assert_frame(3, 400.0, 500.0, 600.0, 500.0);
    harness.assert_synced();
}

#[test]
fn update_window_carries_new_constraints_into_the_layout() {
    let mut harness = Harness::new();
    harness.add();
    harness.add();
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);

    let commands = harness.update(constrained(1, Some((642.0, 100.0)), None));
    assert!(has_set_frame(&commands, 1));
    assert!(has_set_frame(&commands, 2));
    harness.assert_frame(1, 0.0, 0.0, 642.0, 500.0);
    harness.assert_frame(2, 642.0, 0.0, 358.0, 500.0);
    harness.assert_synced();

    // Clearing the learned constraint restores the original split.
    let commands = harness.update(info(1));
    assert!(has_set_frame(&commands, 1));
    harness.assert_frame(1, 0.0, 0.0, 500.0, 500.0);
    harness.assert_frame(2, 500.0, 0.0, 500.0, 500.0);
    harness.assert_synced();
}

#[test]
fn constrained_tiling_actions_never_overlap_or_escape_the_work_area() {
    let area = Rect::new(0.0, 0.0, 800.0, 600.0);
    let mut harness = Harness::with(Config::default(), area);
    harness.add_with(constrained(1, Some((300.0, 200.0)), None));
    harness.add_with(constrained(2, None, Some((150.0, 150.0))));
    harness.add_with(constrained(3, Some((250.0, 100.0)), Some((400.0, 300.0))));

    let actions = [
        Action::Focus {
            direction: Direction::Right,
        },
        Action::Resize {
            direction: Direction::Left,
        },
        Action::Move {
            direction: Direction::Right,
        },
        Action::ToggleOrientation,
        Action::ToggleStacking,
        Action::Unstack,
        Action::Focus {
            direction: Direction::Down,
        },
        Action::Resize {
            direction: Direction::Up,
        },
    ];
    for action in actions {
        harness.act(action);
        assert_sane(&harness, area);
    }

    harness.update(constrained(2, Some((500.0, 300.0)), Some((700.0, 500.0))));
    assert_sane(&harness, area);
    harness.remove(1);
    assert_sane(&harness, area);
}
