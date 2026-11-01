// SPDX-License-Identifier: GPL-3.0-or-later
//! Work-area reservations for shell windows.
//!
//! The cosmicMac panel is a borderless bar along the top edge of the main
//! display.  Like a Wayland layer-shell surface with an exclusive zone, the
//! strip it covers is taken out of the work area the layout tiles into, and
//! given back when the panel closes.  [`reserve_edges`] is the pure geometry;
//! `crate::core` tracks which windows are shell windows
//! ([`crate::rules::is_shell_window`]) and where they are.

use cosmic_mac_protocol::Rect;

/// Slack when deciding whether a strip touches an edge of the work area.
/// Frames read back from the window server can be off by a fraction of a
/// pixel.
const EPS: f64 = 1.0;

/// The part of `base` that is not covered by edge strips.
///
/// Each strip that spans at least half of `base`'s width, is less than half
/// of its height, overlaps it horizontally and touches its top or bottom edge
/// moves that edge inwards:
///
/// - **top-anchored** (`strip.y <= base.y + EPS` and the strip reaches below
///   `base.y`): the top becomes at least the strip's bottom edge.  A strip that
///   starts above the work area (the panel over the native menu bar) still
///   counts;
/// - **bottom-anchored** (`strip.bottom() >= base.bottom() - EPS` and the strip
///   starts above `base.bottom()`): the bottom becomes at most the strip's top
///   edge.
///
/// Anything else (empty, narrow, tall, floating mid-screen, or entirely on a
/// display above or below) is ignored.  The result keeps `base`'s x and
/// width.  If the reservations would leave less than half of `base`'s height,
/// `base` is returned unchanged: a wrongly classified window must not squeeze
/// every tile into a sliver.
pub fn reserve_edges(base: Rect, strips: &[Rect]) -> Rect {
    let mut top = base.y;
    let mut bottom = base.bottom();
    for strip in strips {
        if strip.is_empty()
            || strip.width < base.width / 2.0
            || strip.height >= base.height / 2.0
            || strip.x >= base.right()
            || strip.right() <= base.x
        {
            continue;
        }
        if strip.y <= base.y + EPS && strip.bottom() > base.y {
            top = top.max(strip.bottom());
        } else if strip.bottom() >= base.bottom() - EPS && strip.y < base.bottom() {
            bottom = bottom.min(strip.y);
        }
    }
    let height = bottom - top;
    if height < base.height / 2.0 {
        return base;
    }
    Rect::new(base.x, top, base.width, height)
}

#[cfg(test)]
mod tests {
    use super::reserve_edges;
    use cosmic_mac_protocol::Rect;

    /// Main display 1440×900 with the native menu bar (25 px) visible.
    const VISIBLE: Rect = Rect::new(0.0, 25.0, 1440.0, 875.0);
    /// The same display with the menu bar hidden.
    const FULL: Rect = Rect::new(0.0, 0.0, 1440.0, 900.0);
    /// The panel: 32 px along the top edge, full width.
    const PANEL: Rect = Rect::new(0.0, 0.0, 1440.0, 32.0);

    #[test]
    fn no_strips_keep_the_base() {
        assert_eq!(reserve_edges(VISIBLE, &[]), VISIBLE);
    }

    #[test]
    fn panel_over_the_native_menu_bar_reserves_the_overlap() {
        assert_eq!(
            reserve_edges(VISIBLE, &[PANEL]),
            Rect::new(0.0, 32.0, 1440.0, 868.0)
        );
    }

    #[test]
    fn panel_with_the_menu_bar_hidden_reserves_its_height() {
        assert_eq!(
            reserve_edges(FULL, &[PANEL]),
            Rect::new(0.0, 32.0, 1440.0, 868.0)
        );
    }

    #[test]
    fn panel_hidden_behind_the_menu_bar_reserves_nothing() {
        // A strip entirely inside the native menu bar does not reach the work
        // area.
        let thin = Rect::new(0.0, 0.0, 1440.0, 20.0);
        assert_eq!(reserve_edges(VISIBLE, &[thin]), VISIBLE);
    }

    #[test]
    fn bottom_strip_reserves_from_the_bottom() {
        let dock = Rect::new(0.0, 852.0, 1440.0, 48.0);
        assert_eq!(
            reserve_edges(FULL, &[dock]),
            Rect::new(0.0, 0.0, 1440.0, 852.0)
        );
    }

    #[test]
    fn top_and_bottom_strips_compose() {
        let dock = Rect::new(0.0, 852.0, 1440.0, 48.0);
        assert_eq!(
            reserve_edges(FULL, &[PANEL, dock]),
            Rect::new(0.0, 32.0, 1440.0, 820.0)
        );
        // Order does not matter.
        assert_eq!(
            reserve_edges(FULL, &[dock, PANEL]),
            reserve_edges(FULL, &[PANEL, dock])
        );
    }

    #[test]
    fn edges_within_the_tolerance_count() {
        let almost_top = Rect::new(0.0, 0.5, 1440.0, 32.0);
        assert_eq!(reserve_edges(FULL, &[almost_top]).y, 32.5);
        let almost_bottom = Rect::new(0.0, 852.0, 1440.0, 47.5);
        assert_eq!(reserve_edges(FULL, &[almost_bottom]).bottom(), 852.0);
    }

    #[test]
    fn narrow_strip_is_ignored() {
        let narrow = Rect::new(0.0, 0.0, 600.0, 32.0);
        assert_eq!(reserve_edges(FULL, &[narrow]), FULL);
        // Exactly half the width still counts.
        let half = Rect::new(0.0, 0.0, 720.0, 32.0);
        assert_eq!(reserve_edges(FULL, &[half]).y, 32.0);
    }

    #[test]
    fn mid_screen_strip_is_ignored() {
        // Where the panel window first appears, before it moves to the edge.
        let floating = Rect::new(0.0, 400.0, 1440.0, 32.0);
        assert_eq!(reserve_edges(FULL, &[floating]), FULL);
        assert_eq!(reserve_edges(VISIBLE, &[floating]), VISIBLE);
    }

    #[test]
    fn strip_on_another_display_is_ignored() {
        // A display stacked above the main one: negative y, same x range.
        let above = Rect::new(0.0, -1080.0, 1440.0, 32.0);
        assert_eq!(reserve_edges(VISIBLE, &[above]), VISIBLE);
        // A display below the main one.
        let below = Rect::new(0.0, 900.0, 1440.0, 32.0);
        assert_eq!(reserve_edges(FULL, &[below]), FULL);
        // A display to the right: no horizontal overlap.
        let right = Rect::new(1440.0, 0.0, 1920.0, 32.0);
        assert_eq!(reserve_edges(FULL, &[right]), FULL);
    }

    #[test]
    fn empty_strip_is_ignored() {
        assert_eq!(reserve_edges(FULL, &[Rect::ZERO]), FULL);
        let flat = Rect::new(0.0, 0.0, 1440.0, 0.0);
        assert_eq!(reserve_edges(FULL, &[flat]), FULL);
    }

    #[test]
    fn multiple_top_strips_take_the_lowest_edge() {
        let thin = Rect::new(0.0, 0.0, 1440.0, 28.0);
        let tall = Rect::new(0.0, 0.0, 1440.0, 40.0);
        assert_eq!(reserve_edges(FULL, &[PANEL, tall, thin]).y, 40.0);
        assert_eq!(reserve_edges(FULL, &[thin, PANEL]).y, 32.0);
    }

    #[test]
    fn tall_strip_is_ignored() {
        let tall = Rect::new(0.0, 0.0, 1440.0, 450.0);
        assert_eq!(reserve_edges(FULL, &[tall]), FULL);
    }

    #[test]
    fn absurd_reservation_keeps_the_base() {
        // Each strip is plausible alone, together they would leave a sliver.
        let top = Rect::new(0.0, 0.0, 1440.0, 400.0);
        let bottom = Rect::new(0.0, 500.0, 1440.0, 400.0);
        assert_eq!(reserve_edges(FULL, &[top, bottom]), FULL);
    }

    #[test]
    fn offset_base_keeps_its_x_and_width() {
        let base = Rect::new(-1440.0, 0.0, 1440.0, 900.0);
        let strip = Rect::new(-1440.0, 0.0, 1440.0, 32.0);
        assert_eq!(
            reserve_edges(base, &[strip]),
            Rect::new(-1440.0, 32.0, 1440.0, 868.0)
        );
    }
}
