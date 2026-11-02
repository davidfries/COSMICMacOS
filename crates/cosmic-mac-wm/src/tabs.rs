// SPDX-License-Identifier: GPL-3.0-or-later
//! Stack tab-bar geometry, shared by the drawing ([`crate::tabbar`]) and the
//! click handling ([`crate::core`]) so the two can never disagree.
//!
//! A bar sits directly above its stack's visible window ([`bar_frame`]).  Its
//! tabs share the width equally, clamped to [`TAB_MIN_WIDTH`]..[`TAB_MAX_WIDTH`],
//! from the left edge; tabs past the right edge are cut off.  Each tab has a
//! close glyph [`CLOSE_INSET`] in from its right edge.  A press is a
//! [`BarHit`]: a tab, its close glyph, or the empty bar right of the last tab.
//!
//! The tab-bar thread publishes the bars it shows ([`ShownBar`]) to a
//! [`SharedBars`] snapshot; the click tap reads it to decide, in its callback,
//! whether a press belongs to a bar ([`claim`]).

use std::sync::{Arc, RwLock};

use cosmic_mac_protocol::{Rect, STACK_BAR_HEIGHT, WindowId};

/// Widest a tab gets.
pub const TAB_MAX_WIDTH: f64 = 220.0;
/// Narrowest a tab gets; a bar is never narrower.
pub const TAB_MIN_WIDTH: f64 = 56.0;
/// Distance of the close glyph's centre from the tab's right edge.
pub const CLOSE_INSET: f64 = 12.0;
/// Side of the square around the close glyph's centre that counts as a press on it.
pub const CLOSE_HIT: f64 = 16.0;

/// The bar of a stack whose visible window has the frame `member`.
///
/// Layout reserves the strip above the member inside the stack's tile, so no
/// neighbouring window covers the bar and the member's own title bar stays
/// visible below the tabs.
pub fn bar_frame(member: Rect) -> Rect {
    Rect::new(
        member.x,
        member.y - STACK_BAR_HEIGHT,
        member.width.max(TAB_MIN_WIDTH),
        STACK_BAR_HEIGHT,
    )
}

/// Width of each of `count` tabs in a bar `bar_width` wide.
pub fn tab_width(bar_width: f64, count: usize) -> f64 {
    (bar_width / count.max(1) as f64).clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH)
}

/// One tab's horizontal extent, from the bar's left edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TabSpan {
    /// Left edge.
    pub x: f64,
    /// Width.
    pub width: f64,
}

impl TabSpan {
    /// Centre of the close glyph, from the bar's left edge.
    pub fn close_x(self) -> f64 {
        self.x + self.width - CLOSE_INSET
    }
}

/// Tab `index` of `count` in a bar `bar_width` wide.
pub fn tab_span(bar_width: f64, count: usize, index: usize) -> TabSpan {
    let width = tab_width(bar_width, count);
    TabSpan {
        x: index as f64 * width,
        width,
    }
}

/// What a press in a bar lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarHit {
    /// Tab `index` (outside its close glyph).
    Tab(usize),
    /// The close glyph of tab `index`.
    Close(usize),
    /// The bar right of the last tab.
    Empty,
}

/// What the point `(x, y)` (global) hits in `bar`, a bar with `count` tabs;
/// `None` outside the bar.
pub fn hit_test(bar: Rect, count: usize, x: f64, y: f64) -> Option<BarHit> {
    if !bar.contains_point(x, y) {
        return None;
    }
    let (local_x, local_y) = (x - bar.x, y - bar.y);
    let index = (local_x / tab_width(bar.width, count)).floor() as usize;
    if index >= count {
        return Some(BarHit::Empty);
    }
    let span = tab_span(bar.width, count, index);
    let half = CLOSE_HIT / 2.0;
    let on_close =
        (local_x - span.close_x()).abs() <= half && (local_y - bar.height / 2.0).abs() <= half;
    Some(if on_close {
        BarHit::Close(index)
    } else {
        BarHit::Tab(index)
    })
}

/// A bar the tab-bar thread shows.
#[derive(Debug, Clone, PartialEq)]
pub struct ShownBar {
    /// Its overlay window (`CGWindowID`).
    pub overlay: u32,
    /// Its frame (global, top-left origin).
    pub frame: Rect,
    /// The stack's active tab when the bar was drawn; names the stack.
    pub stack: WindowId,
    /// The windows of the tabs it was drawn with, in order.
    pub tabs: Vec<WindowId>,
}

/// The bars on screen, written by the tab-bar thread and read by the click tap.
pub type SharedBars = Arc<RwLock<Vec<ShownBar>>>;

/// A press in a shown bar, for the core.  The core hit-tests it against the
/// tabs the bar was drawn with ([`BarClick::tabs`]), the ones the user saw.
#[derive(Debug, Clone, PartialEq)]
pub struct BarClick {
    /// Pointer location (global, top-left origin).
    pub x: f64,
    /// See `x`.
    pub y: f64,
    /// A window of the stack whose bar was pressed ([`ShownBar::stack`]).
    pub stack: WindowId,
    /// The windows of the tabs the bar was drawn with ([`ShownBar::tabs`]).
    pub tabs: Vec<WindowId>,
    /// The press's place in a run of clicks: 2 for the second press of a
    /// double click; 0 when the event does not say (as 1).
    pub click_count: u32,
}

/// The click for a press at `(x, y)` (the `click_count`th of a run of
/// clicks), when it lands in one of `bars` and that bar is the topmost window
/// there (`topmost(overlay)`; a menu or another window can cover a bar).
/// `topmost` is only called for a bar that contains the point.
pub fn claim(
    bars: &[ShownBar],
    x: f64,
    y: f64,
    click_count: u32,
    topmost: impl FnOnce(u32) -> bool,
) -> Option<BarClick> {
    let bar = bars.iter().find(|bar| bar.frame.contains_point(x, y))?;
    topmost(bar.overlay).then_some(BarClick {
        x,
        y,
        stack: bar.stack,
        tabs: bar.tabs.clone(),
        click_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAR: Rect = Rect::new(100.0, 25.0, 1000.0, STACK_BAR_HEIGHT);

    fn middle() -> f64 {
        BAR.y + BAR.height / 2.0
    }

    #[test]
    fn the_bar_sits_directly_above_the_member() {
        let bar = bar_frame(Rect::new(10.0, 51.0, 800.0, 600.0));
        assert_eq!(bar, Rect::new(10.0, 25.0, 800.0, STACK_BAR_HEIGHT));
        // A bar is never narrower than one tab.
        assert_eq!(
            bar_frame(Rect::new(0.0, 51.0, 20.0, 600.0)).width,
            TAB_MIN_WIDTH
        );
    }

    #[test]
    fn tabs_split_the_bar_within_their_limits() {
        assert_eq!(tab_width(1000.0, 1), TAB_MAX_WIDTH);
        assert_eq!(tab_width(1000.0, 5), 200.0);
        assert_eq!(tab_width(1000.0, 50), TAB_MIN_WIDTH);
        assert_eq!(tab_width(1000.0, 0), TAB_MAX_WIDTH);
        assert_eq!(
            tab_span(1000.0, 5, 3),
            TabSpan {
                x: 600.0,
                width: 200.0
            }
        );
        assert_eq!(tab_span(1000.0, 5, 3).close_x(), 788.0);
    }

    #[test]
    fn tab_boundaries() {
        // Five 200pt tabs.
        let y = middle();
        assert_eq!(hit_test(BAR, 5, BAR.x, y), Some(BarHit::Tab(0)));
        assert_eq!(hit_test(BAR, 5, BAR.x + 199.9, BAR.y), Some(BarHit::Tab(0)));
        assert_eq!(hit_test(BAR, 5, BAR.x + 200.0, BAR.y), Some(BarHit::Tab(1)));
        assert_eq!(hit_test(BAR, 5, BAR.x + 999.9, BAR.y), Some(BarHit::Tab(4)));
        // Outside the bar: left, right, above, below.
        assert_eq!(hit_test(BAR, 5, BAR.x - 0.1, y), None);
        assert_eq!(hit_test(BAR, 5, BAR.right(), y), None);
        assert_eq!(hit_test(BAR, 5, BAR.x + 10.0, BAR.y - 0.1), None);
        assert_eq!(hit_test(BAR, 5, BAR.x + 10.0, BAR.bottom()), None);
    }

    #[test]
    fn the_close_glyph_has_a_small_box() {
        let y = middle();
        // Tab 1 of five spans 200..400; its glyph is centred at 388.
        let close = BAR.x + 388.0;
        assert_eq!(hit_test(BAR, 5, close, y), Some(BarHit::Close(1)));
        assert_eq!(
            hit_test(BAR, 5, close - 8.0, y - 8.0),
            Some(BarHit::Close(1))
        );
        assert_eq!(
            hit_test(BAR, 5, close + 8.0, y + 8.0),
            Some(BarHit::Close(1))
        );
        assert_eq!(hit_test(BAR, 5, close - 8.5, y), Some(BarHit::Tab(1)));
        assert_eq!(hit_test(BAR, 5, close + 8.5, y), Some(BarHit::Tab(1)));
        // Above or below the box, still on the tab.
        assert_eq!(hit_test(BAR, 5, close, BAR.y + 1.0), Some(BarHit::Tab(1)));
        assert_eq!(
            hit_test(BAR, 5, close, BAR.bottom() - 1.0),
            Some(BarHit::Tab(1))
        );
    }

    #[test]
    fn right_of_the_last_tab_is_empty_bar() {
        let y = middle();
        // Two tabs at the maximum width end at 440.
        assert_eq!(hit_test(BAR, 2, BAR.x + 439.0, y), Some(BarHit::Tab(1)));
        assert_eq!(hit_test(BAR, 2, BAR.x + 440.0, y), Some(BarHit::Empty));
        assert_eq!(hit_test(BAR, 2, BAR.x + 999.0, y), Some(BarHit::Empty));
        assert_eq!(hit_test(BAR, 0, BAR.x + 10.0, y), Some(BarHit::Empty));
    }

    #[test]
    fn a_single_tab_is_at_most_the_maximum_width() {
        let y = middle();
        let close = BAR.x + TAB_MAX_WIDTH - CLOSE_INSET;
        assert_eq!(hit_test(BAR, 1, BAR.x + 100.0, y), Some(BarHit::Tab(0)));
        assert_eq!(hit_test(BAR, 1, close, y), Some(BarHit::Close(0)));
        assert_eq!(
            hit_test(BAR, 1, BAR.x + TAB_MAX_WIDTH, y),
            Some(BarHit::Empty)
        );
    }

    #[test]
    fn many_tabs_stay_at_the_minimum_width_and_are_cut_off() {
        let bar = Rect::new(0.0, 0.0, 300.0, STACK_BAR_HEIGHT);
        let y = bar.height / 2.0;
        // Ten tabs need 560pt; the bar shows 300 of it.
        assert_eq!(hit_test(bar, 10, 0.0, y), Some(BarHit::Tab(0)));
        assert_eq!(hit_test(bar, 10, 56.0, y), Some(BarHit::Tab(1)));
        assert_eq!(hit_test(bar, 10, 280.0, y), Some(BarHit::Tab(5)));
        assert_eq!(
            hit_test(bar, 10, 56.0 * 5.0 - CLOSE_INSET, y),
            Some(BarHit::Close(4))
        );
        assert_eq!(hit_test(bar, 10, 300.0, y), None);
    }

    fn shown(overlay: u32, frame: Rect, stack: WindowId) -> ShownBar {
        ShownBar {
            overlay,
            frame,
            stack,
            tabs: vec![stack, stack + 1],
        }
    }

    #[test]
    fn a_press_in_an_uncovered_bar_is_claimed() {
        let other = Rect::new(1200.0, 25.0, 500.0, STACK_BAR_HEIGHT);
        let bars = [shown(7, BAR, 11), shown(8, other, 22)];
        let mut asked = None;
        let click = claim(&bars, 1300.0, 30.0, 1, |overlay| {
            asked = Some(overlay);
            true
        });
        assert_eq!(
            click,
            Some(BarClick {
                x: 1300.0,
                y: 30.0,
                stack: 22,
                tabs: vec![22, 23],
                click_count: 1,
            })
        );
        assert_eq!(asked, Some(8));
    }

    #[test]
    fn a_claimed_press_keeps_its_click_count() {
        let bars = [shown(7, BAR, 11)];
        for count in [0, 1, 2, 3] {
            let click = claim(&bars, 150.0, 30.0, count, |_| true);
            assert_eq!(click.map(|click| click.click_count), Some(count));
        }
    }

    #[test]
    fn a_covered_bar_or_a_press_elsewhere_is_not_claimed() {
        let bars = [shown(7, BAR, 11)];
        assert_eq!(claim(&bars, 150.0, 30.0, 1, |_| false), None);
        assert_eq!(
            claim(&bars, 150.0, 60.0, 1, |_| panic!("asked outside every bar")),
            None
        );
        assert_eq!(claim(&[], 150.0, 30.0, 1, |_| panic!("no bars")), None);
    }
}
