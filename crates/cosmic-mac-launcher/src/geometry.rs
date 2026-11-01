// SPDX-License-Identifier: GPL-3.0-or-later
//! Launcher window size and placement (pure, host-testable).
//!
//! The window always has the height of a full result list
//! ([`window_height`] of [`MAX_ROWS`]); iced draws the visible box at the
//! top and leaves the rest transparent, so the box grows downwards without
//! window resizes. The layout constants here are the ones the view uses, so
//! the box fits exactly.

/// Results shown at most.
pub const MAX_ROWS: usize = 8;

/// Width of the search field and result list (cosmic-launcher's 600 px).
pub const CONTENT_WIDTH: f32 = 600.0;
/// Padding inside the box, vertical and horizontal (cosmic-launcher: 24, 32).
pub const PADDING_Y: f32 = 24.0;
pub const PADDING_X: f32 = 32.0;
/// Height of the search field (libcosmic's search input with its 32 px icon).
pub const SEARCH_HEIGHT: f32 = 32.0;
/// Gap between the search field and the list.
pub const LIST_SPACING: f32 = 16.0;
/// Height of one result row (name + description lines and 8 px padding).
pub const ROW_HEIGHT: f32 = 52.0;
/// Height of the divider between rows.
pub const DIVIDER_HEIGHT: f32 = 1.0;

/// Window width: content plus horizontal padding.
pub const WINDOW_WIDTH: f32 = CONTENT_WIDTH + 2.0 * PADDING_X;

/// Height of the box with `rows` results (at most [`MAX_ROWS`] count).
pub fn box_height(rows: usize) -> f32 {
    let rows = rows.min(MAX_ROWS);
    let mut height = 2.0 * PADDING_Y + SEARCH_HEIGHT;
    if rows > 0 {
        height += LIST_SPACING + rows as f32 * ROW_HEIGHT + (rows - 1) as f32 * DIVIDER_HEIGHT;
    }
    height
}

/// Window height: the box with a full list.
pub fn window_height() -> f32 {
    box_height(MAX_ROWS)
}

/// A rectangle in AppKit screen coordinates (origin bottom-left of the
/// primary screen, y grows upwards), in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Frame {
    pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Frame {
            x,
            y,
            width,
            height,
        }
    }

    pub fn top(&self) -> f64 {
        self.y + self.height
    }
}

/// Frame for a `width`×`height` window centred in `visible` (the screen's
/// `visibleFrame`, which excludes the menu bar and Dock). Screens can have
/// any origin (secondary displays sit at negative or offset coordinates) and
/// any size; a window larger than the visible area is pinned to its
/// top-left corner so the search field stays on screen. Rounded to whole
/// points.
pub fn centered_frame(visible: Frame, width: f64, height: f64) -> Frame {
    let x = visible.x + ((visible.width - width) / 2.0).max(0.0);
    // Cocoa y is the bottom edge; centring the window means equal space
    // above and below. If it does not fit, keep its top at the visible top.
    let y = if height <= visible.height {
        visible.y + (visible.height - height) / 2.0
    } else {
        visible.top() - height
    };
    Frame::new(x.round(), y.round(), width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_height_counts_rows_and_dividers() {
        assert_eq!(box_height(0), 2.0 * PADDING_Y + SEARCH_HEIGHT);
        assert_eq!(box_height(1), box_height(0) + LIST_SPACING + ROW_HEIGHT);
        assert_eq!(
            box_height(3),
            box_height(0) + LIST_SPACING + 3.0 * ROW_HEIGHT + 2.0 * DIVIDER_HEIGHT
        );
        assert_eq!(box_height(MAX_ROWS + 5), box_height(MAX_ROWS));
        assert_eq!(window_height(), box_height(MAX_ROWS));
        assert_eq!(WINDOW_WIDTH, 664.0);
    }

    #[test]
    fn centres_in_the_visible_frame_of_a_non_1080_screen() {
        // The VM's 1728×1117 screen: menu bar 33 px at the top, no Dock.
        let visible = Frame::new(0.0, 0.0, 1728.0, 1084.0);
        let frame = centered_frame(visible, 664.0, 528.0);
        assert_eq!(frame, Frame::new(532.0, 278.0, 664.0, 528.0));
        // Equal space above and below.
        assert_eq!(frame.y - visible.y, visible.top() - frame.top());
    }

    #[test]
    fn respects_offset_origins() {
        // A Dock on the bottom raises the visible origin.
        let visible = Frame::new(0.0, 80.0, 1440.0, 787.0);
        let frame = centered_frame(visible, 664.0, 528.0);
        assert_eq!(frame.x, 388.0);
        assert_eq!(frame.y, (80.0_f64 + (787.0 - 528.0) / 2.0).round());

        // A secondary display left of and above the primary one.
        let visible = Frame::new(-2560.0, 1117.0, 2560.0, 1415.0);
        let frame = centered_frame(visible, 664.0, 528.0);
        assert_eq!(frame.x, -2560.0 + 948.0);
        assert!(frame.y > visible.y && frame.top() < visible.top());
    }

    #[test]
    fn oversized_window_keeps_its_top_on_screen() {
        let visible = Frame::new(0.0, 0.0, 600.0, 400.0);
        let frame = centered_frame(visible, 664.0, 528.0);
        assert_eq!(frame.x, 0.0);
        assert_eq!(frame.top(), visible.top());
    }
}
