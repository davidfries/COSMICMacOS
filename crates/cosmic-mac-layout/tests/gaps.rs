// SPDX-License-Identifier: GPL-3.0-or-later
//! Outer/inner gap arithmetic.

mod common;

use common::*;
use cosmic_mac_protocol::{Config, Rect};

#[test]
fn outer_gap_insets_the_first_window() {
    let mut harness = Harness::with(Config::default().with_gaps(10, 0), WORK);
    harness.add();
    harness.assert_frame(1, 10.0, 10.0, 980.0, 480.0);
}

#[test]
fn inner_gap_separates_two_windows() {
    let mut harness = Harness::with(Config::default().with_gaps(0, 10), WORK);
    harness.add();
    harness.add();
    harness.assert_frame(1, 0.0, 0.0, 495.0, 500.0);
    harness.assert_frame(2, 505.0, 0.0, 495.0, 500.0);
}

#[test]
fn inner_gap_separates_three_windows() {
    // Use a wide work area so the aspect heuristic keeps splitting horizontally after the
    // inner gap is applied.
    let mut harness = Harness::with(
        Config::default().with_gaps(0, 10),
        Rect::new(0.0, 0.0, 2000.0, 500.0),
    );
    harness.add();
    harness.add();
    harness.add();
    harness.assert_frame(1, 0.0, 0.0, 990.0, 500.0);
    harness.assert_frame(2, 1000.0, 0.0, 495.0, 500.0);
    harness.assert_frame(3, 1505.0, 0.0, 495.0, 500.0);
}

#[test]
fn outer_and_inner_gaps_combine() {
    let mut harness = Harness::with(Config::default().with_gaps(10, 20), WORK);
    harness.add();
    harness.add();
    harness.assert_frame(1, 10.0, 10.0, 480.0, 480.0);
    harness.assert_frame(2, 510.0, 10.0, 480.0, 480.0);
}

#[test]
fn gaps_apply_vertically_too() {
    let mut harness = Harness::with(Config::default().with_gaps(5, 10), TALL_WORK);
    harness.add();
    harness.add();
    harness.assert_frame(1, 5.0, 5.0, 490.0, 490.0);
    harness.assert_frame(2, 5.0, 505.0, 490.0, 490.0);
}

#[test]
fn outer_gap_larger_than_work_area_clamps_to_zero_size() {
    let mut harness = Harness::with(Config::default().with_gaps(600, 0), WORK);
    harness.add();
    assert_eq!(
        harness.layout.frame_of(1),
        Some(Rect::new(600.0, 600.0, 0.0, 0.0))
    );
}

#[test]
fn inner_gap_larger_than_extent_is_clamped() {
    let mut harness = Harness::with(Config::default().with_gaps(0, 2000), WORK);
    harness.add();
    harness.add();
    harness.assert_frame(1, 0.0, 0.0, 250.0, 500.0);
    harness.assert_frame(2, 750.0, 0.0, 250.0, 500.0);
}

#[test]
fn active_hint_is_carried_in_config() {
    let config = Config {
        active_hint: 2,
        ..Config::default()
    };
    let harness = Harness::with(config, WORK);
    assert_eq!(harness.layout.config().active_hint, 2);
}

#[test]
fn gap_change_recomputes_nested_frames() {
    let mut harness = Harness::with(Config::default(), Rect::new(0.0, 0.0, 1000.0, 1000.0));
    harness.add();
    harness.add();
    harness.add();
    harness.update_config(Config::default().with_gaps(10, 10));
    // Tiling area: (10,10,980,980). Root horizontal split: 485 each with a 10px gap.
    harness.assert_frame(1, 10.0, 10.0, 485.0, 980.0);
    // Right column: (505,10,485,980), split vertically with a 10px gap.
    harness.assert_frame(2, 505.0, 10.0, 485.0, 485.0);
    harness.assert_frame(3, 505.0, 505.0, 485.0, 485.0);
    harness.assert_synced();
}
