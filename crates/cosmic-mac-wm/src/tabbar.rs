// SPDX-License-Identifier: GPL-3.0-or-later
//! Stack tab-bar overlay.
//!
//! The platform crate's [`cosmic_mac_platform::OverlayWindow`] can fill and
//! stroke, but it does not expose its `CGContext` or a text primitive.  Rather
//! than modify another agent's crate, this module redeclares the handful of
//! SkyLight functions needed for a raw overlay window and draws the tab bar
//! with Core Graphics + Core Text, following the same JankyBorders technique
//! documented in `docs/SKYLIGHT.md`.
//!
//! The bar lives on its own thread because a raw SkyLight window belongs to the
//! connection that created it.  The daemon sends it the current
//! [`StackInfo`] whenever the focused stack or its frame changes.
//!
//! Milestone 1 is display-only: tab hit-testing and the close button's click
//! handling need a cursor tap, which is milestone-2 work.  The close glyph is
//! drawn for affordance only.
//!
//! Colours come from [`TabBarPalette`]: COSMIC's default dark or light theme,
//! with the user's COSMIC accent when configured, for the mode resolved by
//! [`cosmic_mac_protocol::theme`] (COSMIC's `com.system76.CosmicTheme.Mode`,
//! else the macOS appearance).  The thread resolves it when it shows the bar
//! and, while the bar is visible, every [`THEME_RECHECK`]; a change redraws
//! the visible bar.  Polling covers both sources (a file and a global
//! default) without a run-loop observer or a file watcher in the daemon.
//!
//! The same thread draws the drag-to-stack highlight ([`DropHint`]): a
//! translucent accent-coloured rounded rectangle over the tile a dragged
//! window would be stacked onto, in an overlay window of its own that is
//! ordered just above the target window.

use std::ffi::c_int;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cosmic_mac_protocol::theme::{self, Mode, Rgba, TabBarPalette};
use cosmic_mac_protocol::{Rect, STACK_BAR_HEIGHT, StackInfo};

use crate::backend::DropHint;
use objc2_core_foundation::{
    CFAttributedString, CFDictionary, CFPreferencesAppSynchronize, CFPreferencesCopyAppValue,
    CFRetained, CFString, CFType, CGAffineTransform, CGPoint, CGRect, CGSize,
    kCFPreferencesAnyApplication,
};
use objc2_core_graphics::CGContext;
use objc2_core_text::{CTFont, CTLine, kCTFontAttributeName, kCTForegroundColorAttributeName};

const SKYLIGHT_PATH: &str = "/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight";
const TAB_MAX_WIDTH: f64 = 220.0;
const TAB_MIN_WIDTH: f64 = 56.0;
const OVERLAY_TAGS: u64 = (1u64 << 1) | (1u64 << 9);
const ORDER_ABOVE: c_int = 1;
/// How long a resolved theme is trusted: a visible bar re-resolves it this
/// often, and a show re-resolves it when it is older.
const THEME_RECHECK: Duration = Duration::from_secs(2);
/// The drop highlight sits this far inside the target's tile.
const HINT_INSET: f64 = 6.0;
const HINT_RADIUS: f64 = 10.0;
const HINT_LINE_WIDTH: f64 = 3.0;
const HINT_FILL_ALPHA: f64 = 0.3;
const HINT_STROKE_ALPHA: f64 = 0.9;

/// What the daemon sends the tab-bar thread.
enum Update {
    /// The focused stack; `None` hides the bar.
    Stack(Option<StackInfo>),
    /// The drag-to-stack highlight; `None` hides it.
    Hint(Option<DropHint>),
}

/// Handle to the tab-bar thread.
pub struct TabBarHandle {
    tx: mpsc::Sender<Update>,
}

impl TabBarHandle {
    /// Start the tab-bar thread.  Returns an error when SkyLight is unavailable
    /// or the overlay window cannot be created; the daemon then runs without a
    /// tab bar instead of failing.
    pub fn spawn() -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("stack-tab-bar".into())
            .spawn(move || match TabBar::new() {
                Ok(mut bar) => {
                    let _ = ready_tx.send(Ok(()));
                    bar.run(rx);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            })
            .map_err(|error| format!("cannot start tab-bar thread: {error}"))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(TabBarHandle { tx }),
            Ok(Err(error)) => Err(error),
            Err(error) => Err(format!("tab-bar thread died during startup: {error}")),
        }
    }

    /// Update the bar.  `None` hides it.
    pub fn update(&self, stack: Option<StackInfo>) -> Result<(), String> {
        self.tx
            .send(Update::Stack(stack))
            .map_err(|_| "tab-bar thread stopped".to_string())
    }

    /// Show, move or hide the drop highlight.
    pub fn drop_hint(&self, hint: Option<DropHint>) -> Result<(), String> {
        self.tx
            .send(Update::Hint(hint))
            .map_err(|_| "tab-bar thread stopped".to_string())
    }
}

struct TabBar {
    overlay: Option<Overlay>,
    /// The stack the visible bar shows, for a redraw after a theme change.
    shown: Option<StackInfo>,
    theme: ThemeWatch,
    /// The drop highlight's window, kept (ordered out) between drags.
    hint: Option<Overlay>,
    /// Whether the highlight is on screen.
    hint_visible: bool,
}

impl TabBar {
    fn new() -> Result<Self, String> {
        Ok(TabBar {
            overlay: None,
            shown: None,
            theme: ThemeWatch::new(),
            hint: None,
            hint_visible: false,
        })
    }

    fn run(&mut self, rx: mpsc::Receiver<Update>) {
        loop {
            // Only a visible bar wakes up to re-check the theme.
            let update = if self.overlay.is_some() {
                match rx.recv_timeout(self.theme.until_next(Instant::now())) {
                    Ok(update) => update,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if let Err(error) = self.recheck_theme() {
                            log::warn!("tab bar redraw failed: {error}");
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(update) => update,
                    Err(_) => break,
                }
            };
            let result = match update {
                Update::Stack(None) => self.hide(),
                Update::Stack(Some(stack)) => self.show(stack),
                Update::Hint(None) => self.hide_hint(),
                Update::Hint(Some(hint)) => self.show_hint(hint),
            };
            if let Err(error) = result {
                log::warn!("tab bar update failed: {error}");
            }
        }
        let _ = self.hide();
        let _ = self.hide_hint();
    }

    fn show_hint(&mut self, hint: DropHint) -> Result<(), String> {
        self.theme.refresh(Instant::now(), resolve_palette);
        let frame = hint.frame.inset(HINT_INSET);
        log::debug!("drop hint over window {} at {frame:?}", hint.target);
        if self.hint.is_none() {
            self.hint = Some(Overlay::new(frame)?);
        }
        let overlay = self.hint.as_mut().expect("hint overlay just created");
        overlay.set_frame(frame)?;
        // Just above the target: under the dragged window, like cosmic-comp.
        overlay.order_above(hint.target as u32)?;
        self.hint_visible = true;
        overlay.draw_hint(&self.theme.palette)
    }

    fn hide_hint(&mut self) -> Result<(), String> {
        if !std::mem::replace(&mut self.hint_visible, false) {
            return Ok(());
        }
        log::debug!("drop hint hidden");
        match &self.hint {
            Some(overlay) => overlay.hide(),
            None => Ok(()),
        }
    }

    /// Re-resolves the theme and redraws the visible bar if it changed.
    fn recheck_theme(&mut self) -> Result<(), String> {
        if !self.theme.refresh(Instant::now(), resolve_palette) {
            return Ok(());
        }
        match (&self.overlay, &self.shown) {
            (Some(overlay), Some(stack)) => overlay.draw(stack, &self.theme.palette),
            _ => Ok(()),
        }
    }

    fn hide(&mut self) -> Result<(), String> {
        self.shown = None;
        if let Some(overlay) = self.overlay.take() {
            overlay.hide()?;
        }
        Ok(())
    }

    fn show(&mut self, stack: StackInfo) -> Result<(), String> {
        self.theme.refresh(Instant::now(), resolve_palette);
        let result = self.show_stack(&stack);
        self.shown = Some(stack);
        result
    }

    fn show_stack(&mut self, stack: &StackInfo) -> Result<(), String> {
        let frame = bar_frame(stack.frame);
        log::debug!(
            "tab bar: stack frame {:?} -> bar {:?}, {} tabs, active {}",
            stack.frame,
            frame,
            stack.tabs.len(),
            stack.active
        );
        if self.overlay.is_none() {
            self.overlay = Some(Overlay::new(frame)?);
        }
        let overlay = self.overlay.as_mut().expect("overlay just created");
        overlay.set_frame(frame)?;
        let active = stack.active_tab().map(|tab| tab.wid).unwrap_or(0);
        overlay.order_above(active as u32)?;
        overlay.draw(stack, &self.theme.palette)?;
        Ok(())
    }
}

/// The resolved tab-bar palette and when it was resolved.
struct ThemeWatch {
    palette: TabBarPalette,
    mode: Option<Mode>,
    checked: Option<Instant>,
}

impl ThemeWatch {
    fn new() -> Self {
        ThemeWatch {
            palette: TabBarPalette::DARK,
            mode: None,
            checked: None,
        }
    }

    /// Time until the palette is due for a re-check (zero when due).
    fn until_next(&self, now: Instant) -> Duration {
        match self.checked {
            Some(checked) => THEME_RECHECK.saturating_sub(now.saturating_duration_since(checked)),
            None => Duration::ZERO,
        }
    }

    /// Re-resolves the palette when it is due; returns whether it changed.
    fn refresh(&mut self, now: Instant, resolve: impl FnOnce() -> (Mode, TabBarPalette)) -> bool {
        if !self.until_next(now).is_zero() {
            return false;
        }
        self.checked = Some(now);
        let (mode, palette) = resolve();
        let changed = palette != self.palette;
        if self.mode != Some(mode) || changed {
            log::info!("tab bar theme: {mode}, accent {:?}", palette.tab_active);
        }
        self.mode = Some(mode);
        self.palette = palette;
        changed
    }
}

/// The palette for the current mode and accent.
fn resolve_palette() -> (Mode, TabBarPalette) {
    TabBarPalette::current(&crate::paths::cosmic_dir(), macos_prefers_dark)
}

/// The macOS appearance from the global default `AppleInterfaceStyle`.  The
/// daemon has no `NSApplication`, so it cannot ask for the effective
/// appearance.
fn macos_prefers_dark() -> bool {
    // SAFETY: an immutable CF constant.
    let any_application = unsafe { kCFPreferencesAnyApplication };
    // Re-reads the global domain, so a change another process (System
    // Settings) made is seen.
    CFPreferencesAppSynchronize(any_application);
    let key = CFString::from_static_str(theme::MACOS_STYLE_KEY);
    let style = CFPreferencesCopyAppValue(&key, any_application)
        .and_then(|value| value.downcast::<CFString>().ok())
        .map(|value| value.to_string());
    theme::macos_style_is_dark(style.as_deref())
}

/// Fill, title and close-glyph colours of one tab.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TabColors {
    fill: Rgba,
    text: Rgba,
    close: Rgba,
}

fn tab_colors(palette: &TabBarPalette, active: bool) -> TabColors {
    if active {
        TabColors {
            fill: palette.tab_active,
            text: palette.text_active,
            close: palette.text_active,
        }
    } else {
        TabColors {
            fill: palette.tab_inactive,
            text: palette.text,
            close: palette.close,
        }
    }
}

/// Fill and outline of the drop highlight: the accent, translucent.
fn hint_colors(palette: &TabBarPalette) -> (Rgba, Rgba) {
    let accent = palette.tab_active;
    (
        Rgba::new(accent.red, accent.green, accent.blue, HINT_FILL_ALPHA),
        Rgba::new(accent.red, accent.green, accent.blue, HINT_STROKE_ALPHA),
    )
}

/// Add a rounded rectangle to the context's path.
fn add_rounded_rect(context: &CGContext, rect: CGRect, radius: f64) {
    let (x, y) = (rect.origin.x, rect.origin.y);
    let (width, height) = (rect.size.width, rect.size.height);
    let radius = radius.min(width / 2.0).min(height / 2.0).max(0.0);
    CGContext::begin_path(Some(context));
    CGContext::move_to_point(Some(context), x + radius, y);
    CGContext::add_arc_to_point(Some(context), x + width, y, x + width, y + height, radius);
    CGContext::add_arc_to_point(Some(context), x + width, y + height, x, y + height, radius);
    CGContext::add_arc_to_point(Some(context), x, y + height, x, y, radius);
    CGContext::add_arc_to_point(Some(context), x, y, x + width, y, radius);
    CGContext::close_path(Some(context));
}

fn set_fill(context: &CGContext, color: Rgba) {
    CGContext::set_rgb_fill_color(
        Some(context),
        color.red,
        color.green,
        color.blue,
        color.alpha,
    );
}

fn bar_frame(frame: Rect) -> Rect {
    let width = frame.width.max(TAB_MIN_WIDTH);
    let height = STACK_BAR_HEIGHT;
    // The bar sits directly above the active member's frame.  Layout reserves
    // that strip inside the stack's tile, so no neighbouring window can occupy
    // it and the member's own title bar stays visible below the tabs.
    Rect::new(frame.x, frame.y - height, width, height)
}

struct Overlay {
    cid: c_int,
    wid: u32,
    context: *mut CGContext,
    frame: Rect,
}

impl Overlay {
    fn new(frame: Rect) -> Result<Self, String> {
        let cid = ffi::new_connection()?;
        let region = ffi::region(frame)?;
        let created = ffi::new_window(cid, region);
        // SAFETY: region is a +1 CFType created by CGSNewRegionWithRect.
        unsafe { ffi::cf_release(region) };
        let wid = created?;
        log::info!("tab bar overlay wid={wid} frame={frame:?}");

        // Retina resolution keeps text crisp; a wrong value only affects
        // sharpness, never correctness.
        let _ = ffi::set_resolution(cid, wid, 2.0);
        let _ = ffi::set_tags(cid, wid);
        let _ = ffi::set_opaque(cid, wid, false);
        let context = ffi::context_create(cid, wid)?;
        if context.is_null() {
            let _ = ffi::release_window(cid, wid);
            let _ = ffi::release_connection(cid);
            return Err("SLWindowContextCreate returned null".into());
        }
        let overlay = Overlay {
            cid,
            wid,
            context,
            frame,
        };
        overlay.move_to(frame)?;
        Ok(overlay)
    }

    fn set_frame(&mut self, frame: Rect) -> Result<(), String> {
        if frame == self.frame {
            return Ok(());
        }
        let region = ffi::region(frame)?;
        let result = ffi::set_shape(self.cid, self.wid, region);
        // SAFETY: region is +1.
        unsafe { ffi::cf_release(region) };
        result?;
        self.frame = frame;
        self.move_to(frame)
    }

    fn move_to(&self, frame: Rect) -> Result<(), String> {
        ffi::move_window(self.cid, self.wid, frame.x, frame.y)
    }

    fn order_above(&self, target: u32) -> Result<(), String> {
        if target == 0 {
            return ffi::order(self.cid, self.wid, ORDER_ABOVE, 0);
        }
        let level = ffi::window_level(target).unwrap_or(0);
        let sub_level = ffi::window_sub_level(target).unwrap_or(0);
        let _ = ffi::set_level(self.cid, self.wid, level);
        let _ = ffi::set_sub_level(self.cid, self.wid, sub_level);
        // Follow the target to its managed Space; a raw SLS window is not
        // automatically composited on one.
        let space = ffi::move_to_space_of(self.cid, self.wid, target);
        log::debug!("tab bar: space follow -> {space:?}");
        let result = ffi::order(self.cid, self.wid, ORDER_ABOVE, target);
        log::debug!("tab bar: order above {target} -> {result:?}");
        result
    }

    fn draw(&self, stack: &StackInfo, palette: &TabBarPalette) -> Result<(), String> {
        let Some(context) = std::ptr::NonNull::new(self.context).map(|ptr| unsafe { ptr.as_ref() })
        else {
            return Err("null CGContext".into());
        };
        let bounds = CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(self.frame.width, self.frame.height),
        );
        let _ = ffi::freeze(self.cid, self.wid);
        CGContext::clear_rect(Some(context), bounds);
        set_fill(context, palette.background);
        CGContext::fill_rect(Some(context), bounds);

        let count = stack.tabs.len().max(1);
        let tab_width = (self.frame.width / count as f64).clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH);
        for (index, tab) in stack.tabs.iter().enumerate() {
            let x = index as f64 * tab_width;
            let tab_rect = CGRect::new(
                CGPoint::new(x + 1.0, 1.0),
                CGSize::new(
                    (tab_width - 2.0).max(1.0),
                    (self.frame.height - 2.0).max(1.0),
                ),
            );
            let colors = tab_colors(palette, index == stack.active);
            set_fill(context, colors.fill);
            CGContext::fill_rect(Some(context), tab_rect);
            draw_close_glyph(
                context,
                x + tab_width - 12.0,
                self.frame.height / 2.0,
                colors.close,
            );
            let title = shorten(&tab.title, tab_width);
            draw_text(
                context,
                &title,
                x + 8.0,
                self.frame.height / 2.0,
                11.0,
                colors.text,
            );
        }
        CGContext::flush(Some(context));
        let _ = ffi::flush(self.cid, self.wid);
        let _ = ffi::thaw(self.cid, self.wid);
        Ok(())
    }

    /// Draw the drop highlight over the whole window.
    fn draw_hint(&self, palette: &TabBarPalette) -> Result<(), String> {
        let Some(context) = std::ptr::NonNull::new(self.context).map(|ptr| unsafe { ptr.as_ref() })
        else {
            return Err("null CGContext".into());
        };
        let bounds = CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(self.frame.width, self.frame.height),
        );
        let half = HINT_LINE_WIDTH / 2.0;
        let shape = CGRect::new(
            CGPoint::new(half, half),
            CGSize::new(
                (self.frame.width - HINT_LINE_WIDTH).max(1.0),
                (self.frame.height - HINT_LINE_WIDTH).max(1.0),
            ),
        );
        let (fill, stroke) = hint_colors(palette);
        let _ = ffi::freeze(self.cid, self.wid);
        CGContext::clear_rect(Some(context), bounds);
        set_fill(context, fill);
        add_rounded_rect(context, shape, HINT_RADIUS);
        CGContext::fill_path(Some(context));
        CGContext::set_rgb_stroke_color(
            Some(context),
            stroke.red,
            stroke.green,
            stroke.blue,
            stroke.alpha,
        );
        CGContext::set_line_width(Some(context), HINT_LINE_WIDTH);
        add_rounded_rect(context, shape, HINT_RADIUS);
        CGContext::stroke_path(Some(context));
        CGContext::flush(Some(context));
        let _ = ffi::flush(self.cid, self.wid);
        let _ = ffi::thaw(self.cid, self.wid);
        Ok(())
    }

    fn hide(&self) -> Result<(), String> {
        ffi::order(self.cid, self.wid, 0, 0)
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: we own the window, context and connection.
        unsafe {
            let _ = ffi::release_window(self.cid, self.wid);
            ffi::cf_release(self.context.cast());
            let _ = ffi::release_connection(self.cid);
        }
    }
}

fn shorten(title: &str, width: f64) -> String {
    let max_chars = ((width - 34.0) / 6.5).max(3.0) as usize;
    let mut out: String = title.chars().take(max_chars).collect();
    if title.chars().count() > max_chars {
        out.pop();
        out.push('…');
    }
    out
}

fn draw_close_glyph(context: &CGContext, cx: f64, cy: f64, color: Rgba) {
    CGContext::set_rgb_stroke_color(
        Some(context),
        color.red,
        color.green,
        color.blue,
        color.alpha,
    );
    CGContext::set_line_width(Some(context), 1.3);
    CGContext::move_to_point(Some(context), cx - 3.5, cy - 3.5);
    CGContext::add_line_to_point(Some(context), cx + 3.5, cy + 3.5);
    CGContext::move_to_point(Some(context), cx + 3.5, cy - 3.5);
    CGContext::add_line_to_point(Some(context), cx - 3.5, cy + 3.5);
    CGContext::stroke_path(Some(context));
}

/// Draw `text` starting at `x`, its capitals centred on `middle`.
fn draw_text(context: &CGContext, text: &str, x: f64, middle: f64, size: f64, color: Rgba) {
    if text.is_empty() {
        return;
    }
    let font_name = CFString::from_static_str("Helvetica Neue");
    // SAFETY: font name is a valid CFString, matrix is null.
    let font = unsafe { CTFont::with_name(&font_name, size, std::ptr::null()) };
    // SAFETY: the attribute-name statics are immutable CFStrings.
    let (font_key, color_key) = unsafe { (kCTFontAttributeName, kCTForegroundColorAttributeName) };
    let color = objc2_core_graphics::CGColor::new_generic_rgb(
        color.red,
        color.green,
        color.blue,
        color.alpha,
    );
    let values: [&CFType; 2] = [font.as_ref(), color.as_ref()];
    let attributes: CFRetained<CFDictionary<CFString, CFType>> =
        CFDictionary::from_slices(&[font_key, color_key], &values);
    let string = CFString::from_str(text);
    // `CFAttributedStringCreate` takes an untyped dictionary; the dictionary's
    // key/value callback contract is what matters, not the Rust phantom types.
    let attributes_ptr = CFRetained::as_ptr(&attributes).as_ptr();
    let attributes: &CFDictionary = unsafe { &*(attributes_ptr as *const CFDictionary) };
    // SAFETY: both CF objects are valid for the duration of the call; the
    // attributed string copies them.
    let Some(attributed) =
        (unsafe { CFAttributedString::new(None, Some(&string), Some(attributes)) })
    else {
        return;
    };
    // SAFETY: attributed string is a valid CFAttributedString.
    let line = unsafe { CTLine::with_attributed_string(&attributed) };
    // The SkyLight window context has a bottom-left origin with y growing
    // upwards, so the baseline sits half the cap height below the middle.
    // SAFETY: valid font.
    let baseline = middle - unsafe { font.cap_height() } / 2.0;
    CGContext::set_text_matrix(
        Some(context),
        CGAffineTransform {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            tx: 0.0,
            ty: 0.0,
        },
    );
    CGContext::set_text_position(Some(context), x, baseline);
    // SAFETY: valid line and context.
    unsafe { line.draw(context) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(mode: Mode) -> impl FnOnce() -> (Mode, TabBarPalette) {
        move || (mode, TabBarPalette::defaults(mode))
    }

    fn not_due() -> (Mode, TabBarPalette) {
        panic!("resolved before THEME_RECHECK elapsed")
    }

    #[test]
    fn active_tab_uses_the_accent() {
        for palette in [TabBarPalette::DARK, TabBarPalette::LIGHT] {
            assert_eq!(
                tab_colors(&palette, true),
                TabColors {
                    fill: palette.tab_active,
                    text: palette.text_active,
                    close: palette.text_active,
                }
            );
            assert_eq!(
                tab_colors(&palette, false),
                TabColors {
                    fill: palette.tab_inactive,
                    text: palette.text,
                    close: palette.close,
                }
            );
        }
    }

    #[test]
    fn drop_hint_is_the_translucent_accent() {
        for palette in [TabBarPalette::DARK, TabBarPalette::LIGHT] {
            let (fill, stroke) = hint_colors(&palette);
            for color in [fill, stroke] {
                assert_eq!(
                    (color.red, color.green, color.blue),
                    (
                        palette.tab_active.red,
                        palette.tab_active.green,
                        palette.tab_active.blue
                    )
                );
            }
            assert!(fill.alpha < stroke.alpha && stroke.alpha < 1.0);
        }
    }

    #[test]
    fn tab_colors_follow_the_mode() {
        let dark = tab_colors(&TabBarPalette::DARK, false);
        let light = tab_colors(&TabBarPalette::LIGHT, false);
        assert!(dark.fill.luminance() < light.fill.luminance());
        assert!(dark.text.luminance() > light.text.luminance());
        assert_ne!(
            tab_colors(&TabBarPalette::DARK, true).fill,
            tab_colors(&TabBarPalette::LIGHT, true).fill
        );
    }

    #[test]
    fn first_show_resolves_the_theme() {
        let mut watch = ThemeWatch::new();
        let now = Instant::now();
        assert_eq!(watch.until_next(now), Duration::ZERO);
        assert!(watch.refresh(now, resolved(Mode::Light)));
        assert_eq!(watch.palette, TabBarPalette::LIGHT);
        assert_eq!(watch.mode, Some(Mode::Light));
        assert_eq!(watch.until_next(now), THEME_RECHECK);
    }

    #[test]
    fn theme_is_rechecked_after_the_interval() {
        let mut watch = ThemeWatch::new();
        let start = Instant::now();
        // Dark is the initial palette, so resolving dark is no change.
        assert!(!watch.refresh(start, resolved(Mode::Dark)));
        assert_eq!(watch.mode, Some(Mode::Dark));

        let soon = start + THEME_RECHECK / 2;
        assert!(!watch.refresh(soon, not_due));
        assert_eq!(watch.until_next(soon), THEME_RECHECK / 2);

        let due = start + THEME_RECHECK;
        assert_eq!(watch.until_next(due), Duration::ZERO);
        assert!(!watch.refresh(due, resolved(Mode::Dark)));
        let later = due + THEME_RECHECK;
        assert!(watch.refresh(later, resolved(Mode::Light)));
        assert_eq!(watch.palette, TabBarPalette::LIGHT);
        assert!(!watch.refresh(later + THEME_RECHECK / 4, not_due));
    }

    #[test]
    fn accent_change_counts_as_a_change() {
        let mut watch = ThemeWatch::new();
        let start = Instant::now();
        watch.refresh(start, resolved(Mode::Dark));
        let mut orange = TabBarPalette::DARK;
        orange.tab_active = Rgba::new(1.0, 0.5, 0.0, 1.0);
        assert!(watch.refresh(start + THEME_RECHECK, move || (Mode::Dark, orange)));
        assert_eq!(watch.palette, orange);
    }

    #[test]
    fn macos_appearance_matches_defaults() {
        let output = std::process::Command::new("/usr/bin/defaults")
            .args(["read", "-g", theme::MACOS_STYLE_KEY])
            .output()
            .expect("run defaults");
        let style = String::from_utf8_lossy(&output.stdout);
        let expected = output.status.success() && theme::macos_style_is_dark(Some(&style));
        assert_eq!(macos_prefers_dark(), expected, "defaults said {style:?}");
    }
}

// ---------------------------------------------------------------------------
// Local SkyLight FFI
// ---------------------------------------------------------------------------

mod ffi {
    use std::ffi::{CString, c_int, c_void};
    use std::sync::OnceLock;

    use objc2_core_foundation::{CFArray, CFNumber, CFRetained, CFType, CGPoint, CGRect};

    use super::SKYLIGHT_PATH;

    type ConnectionId = c_int;
    type WindowId = u32;

    fn handle() -> Result<*mut c_void, String> {
        static HANDLE: OnceLock<usize> = OnceLock::new();
        let handle = HANDLE.get_or_init(|| unsafe {
            let path = CString::new(SKYLIGHT_PATH).expect("static path");
            libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) as usize
        });
        if *handle == 0 {
            Err(format!("dlopen({SKYLIGHT_PATH}) failed"))
        } else {
            Ok(*handle as *mut c_void)
        }
    }

    fn resolve<T: Copy>(name: &'static str) -> Result<T, String> {
        let handle = handle()?;
        let cname = CString::new(name).expect("symbol names are NUL-free");
        let symbol = unsafe { libc::dlsym(handle, cname.as_ptr()) };
        if symbol.is_null() {
            return Err(format!("symbol {name} not found in SkyLight"));
        }
        debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
        // SAFETY: T is a function-pointer type and the size assertion holds.
        Ok(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&symbol) })
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
    }

    /// Release a +1 CF reference.
    ///
    /// # Safety
    /// `ptr` must be a +1 CF reference or null.
    pub unsafe fn cf_release(ptr: *const c_void) {
        if !ptr.is_null() {
            unsafe { CFRelease(ptr) };
        }
    }

    pub fn new_connection() -> Result<ConnectionId, String> {
        type F = unsafe extern "C" fn(c_int, *mut ConnectionId) -> i32;
        let f: F = resolve("SLSNewConnection")?;
        let mut cid = 0;
        // SAFETY: valid out-pointer.
        let code = unsafe { f(0, &mut cid) };
        if code == 0 && cid != 0 {
            Ok(cid)
        } else {
            Err(format!("SLSNewConnection failed ({code})"))
        }
    }

    pub fn release_connection(cid: ConnectionId) -> Result<(), String> {
        type F = unsafe extern "C" fn(ConnectionId) -> i32;
        let f: F = resolve("SLSReleaseConnection")?;
        // SAFETY: cid was created by SLSNewConnection.
        let code = unsafe { f(cid) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("SLSReleaseConnection({code})"))
        }
    }

    pub fn region(frame: super::Rect) -> Result<*mut c_void, String> {
        type F = unsafe extern "C" fn(*const CGRect, *mut *mut c_void) -> i32;
        let f: F = resolve("CGSNewRegionWithRect")?;
        let cg = CGRect::new(
            CGPoint::new(frame.x, frame.y),
            objc2_core_foundation::CGSize::new(frame.width, frame.height),
        );
        let mut region: *mut c_void = std::ptr::null_mut();
        // SAFETY: valid rect and out-pointer.
        let code = unsafe { f(&cg, &mut region) };
        if code == 0 && !region.is_null() {
            Ok(region)
        } else {
            Err(format!("CGSNewRegionWithRect failed ({code})"))
        }
    }

    pub fn new_window(cid: ConnectionId, region: *mut c_void) -> Result<WindowId, String> {
        type F =
            unsafe extern "C" fn(ConnectionId, c_int, f32, f32, *mut c_void, *mut WindowId) -> i32;
        let f: F = resolve("SLSNewWindow")?;
        let mut wid = 0;
        // SAFETY: valid connection/region and out-pointer.
        let code = unsafe { f(cid, 2, -9999.0, -9999.0, region, &mut wid) };
        if code == 0 && wid != 0 {
            Ok(wid)
        } else {
            Err(format!("SLSNewWindow failed ({code})"))
        }
    }

    pub fn release_window(cid: ConnectionId, wid: WindowId) -> Result<(), String> {
        type F = unsafe extern "C" fn(ConnectionId, WindowId) -> i32;
        let f: F = resolve("SLSReleaseWindow")?;
        // SAFETY: we own the window.
        let code = unsafe { f(cid, wid) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("SLSReleaseWindow({code})"))
        }
    }

    pub fn set_resolution(cid: ConnectionId, wid: WindowId, scale: f64) -> Result<(), String> {
        type F = unsafe extern "C" fn(ConnectionId, WindowId, f64) -> i32;
        let f: F = resolve("SLSSetWindowResolution")?;
        // SAFETY: we own the window.
        let code = unsafe { f(cid, wid, scale) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("SLSSetWindowResolution({code})"))
        }
    }

    pub fn set_tags(cid: ConnectionId, wid: WindowId) -> Result<(), String> {
        type F = unsafe extern "C" fn(ConnectionId, WindowId, *mut u64, c_int) -> i32;
        let f: F = resolve("SLSSetWindowTags")?;
        let mut tags = super::OVERLAY_TAGS;
        // SAFETY: we own the window; tags points to one u64.
        let code = unsafe { f(cid, wid, &mut tags, 64) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("SLSSetWindowTags({code})"))
        }
    }

    pub fn set_opaque(cid: ConnectionId, wid: WindowId, opaque: bool) -> Result<(), String> {
        type F = unsafe extern "C" fn(ConnectionId, WindowId, bool) -> i32;
        let f: F = resolve("SLSSetWindowOpacity")?;
        // SAFETY: we own the window.
        let code = unsafe { f(cid, wid, opaque) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("SLSSetWindowOpacity({code})"))
        }
    }

    pub fn context_create(
        cid: ConnectionId,
        wid: WindowId,
    ) -> Result<*mut super::CGContext, String> {
        type F = unsafe extern "C" fn(ConnectionId, WindowId, *mut c_void) -> *mut c_void;
        let f: F = resolve("SLWindowContextCreate")?;
        // SAFETY: we own the window.
        let context = unsafe { f(cid, wid, std::ptr::null_mut()) };
        if context.is_null() {
            Err("SLWindowContextCreate returned null".into())
        } else {
            Ok(context.cast())
        }
    }

    pub fn move_window(cid: ConnectionId, wid: WindowId, x: f64, y: f64) -> Result<(), String> {
        type F = unsafe extern "C" fn(ConnectionId, WindowId, *mut CGPoint) -> i32;
        let f: F = resolve("SLSMoveWindow")?;
        let mut point = CGPoint::new(x, y);
        // SAFETY: we own the window; point is valid.
        let code = unsafe { f(cid, wid, &mut point) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("SLSMoveWindow({code})"))
        }
    }

    pub fn set_shape(cid: ConnectionId, wid: WindowId, region: *mut c_void) -> Result<(), String> {
        type F = unsafe extern "C" fn(ConnectionId, WindowId, f32, f32, *mut c_void) -> i32;
        let f: F = resolve("SLSSetWindowShape")?;
        // SAFETY: we own the window and the region.
        let code = unsafe { f(cid, wid, 0.0, 0.0, region) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("SLSSetWindowShape({code})"))
        }
    }

    pub fn order(
        cid: ConnectionId,
        wid: WindowId,
        order: c_int,
        relative: WindowId,
    ) -> Result<(), String> {
        type F = unsafe extern "C" fn(ConnectionId, WindowId, c_int, WindowId) -> i32;
        let f: F = resolve("SLSOrderWindow")?;
        // SAFETY: we own the window.
        let code = unsafe { f(cid, wid, order, relative) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("SLSOrderWindow({code})"))
        }
    }

    pub fn window_level(wid: WindowId) -> Result<i32, String> {
        type F = unsafe extern "C" fn(ConnectionId, WindowId, *mut i32) -> i32;
        let f: F = resolve("SLSGetWindowLevel")?;
        let cid = super::ffi::main_connection()?;
        let mut level = 0;
        // SAFETY: valid out-pointer.
        let code = unsafe { f(cid, wid, &mut level) };
        if code == 0 {
            Ok(level)
        } else {
            Err(format!("SLSGetWindowLevel({code})"))
        }
    }

    pub fn window_sub_level(wid: WindowId) -> Result<i32, String> {
        type F = unsafe extern "C" fn(ConnectionId, WindowId) -> i32;
        let f: F = resolve("SLSGetWindowSubLevel")?;
        let cid = main_connection()?;
        // SAFETY: value-returning private call.
        Ok(unsafe { f(cid, wid) })
    }

    pub fn set_level(cid: ConnectionId, wid: WindowId, level: i32) -> Result<(), String> {
        type F = unsafe extern "C" fn(ConnectionId, WindowId, i32) -> i32;
        let f: F = resolve("SLSSetWindowLevel")?;
        // SAFETY: we own the window.
        let code = unsafe { f(cid, wid, level) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("SLSSetWindowLevel({code})"))
        }
    }

    pub fn set_sub_level(cid: ConnectionId, wid: WindowId, level: i32) -> Result<(), String> {
        type F = unsafe extern "C" fn(ConnectionId, WindowId, i32) -> i32;
        let f: F = resolve("SLSSetWindowSubLevel")?;
        // SAFETY: we own the window.
        let code = unsafe { f(cid, wid, level) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("SLSSetWindowSubLevel({code})"))
        }
    }

    pub fn move_to_space_of(
        cid: ConnectionId,
        wid: WindowId,
        target: WindowId,
    ) -> Result<(), String> {
        let space = space_of_window(target)?;
        let wid_number = CFNumber::new_i32(wid as i32);
        let array: CFRetained<CFArray<CFType>> = CFArray::from_objects(&[&*wid_number]);
        type F = unsafe extern "C" fn(ConnectionId, *const c_void, u64);
        let f: F = resolve("SLSMoveWindowsToManagedSpace")?;
        // SAFETY: valid connection and CFArray; the call returns void.
        unsafe {
            f(cid, CFRetained::as_ptr(&array).as_ptr().cast(), space);
        }
        Ok(())
    }

    fn space_of_window(wid: WindowId) -> Result<u64, String> {
        type F = unsafe extern "C" fn(ConnectionId, c_int, *const c_void) -> *mut c_void;
        let f: F = resolve("SLSCopySpacesForWindows")?;
        let cid = main_connection()?;
        let wid_number = CFNumber::new_i32(wid as i32);
        let array: CFRetained<CFArray<CFType>> = CFArray::from_objects(&[&*wid_number]);
        // SAFETY: valid connection and CFArray.
        let spaces = unsafe { f(cid, 0x7, CFRetained::as_ptr(&array).as_ptr().cast()) };
        if spaces.is_null() {
            return Err(format!("window {wid} is not on a managed Space"));
        }
        // SAFETY: +1 CFArray of CFNumbers.
        let spaces_ref: &CFArray<CFType> =
            unsafe { (&*(spaces as *const CFArray)).cast_unchecked() };
        let space = spaces_ref
            .get(0)
            .and_then(|value| value.downcast::<CFNumber>().ok())
            .and_then(|value| value.as_i64());
        // SAFETY: we own the +1 reference.
        unsafe { cf_release(spaces) };
        space
            .map(|space| space as u64)
            .ok_or_else(|| format!("window {wid} has no managed Space"))
    }

    pub fn main_connection() -> Result<ConnectionId, String> {
        type F = unsafe extern "C" fn() -> ConnectionId;
        let f: F = resolve("SLSMainConnectionID")?;
        // SAFETY: no arguments.
        let cid = unsafe { f() };
        if cid == 0 {
            Err("SLSMainConnectionID returned 0 (no GUI session?)".into())
        } else {
            Ok(cid)
        }
    }

    pub fn freeze(cid: ConnectionId, wid: WindowId) -> i32 {
        type F = unsafe extern "C" fn(ConnectionId, WindowId, *mut c_void) -> i32;
        match resolve::<F>("SLSWindowFreezeWithOptions") {
            Ok(f) => unsafe { f(cid, wid, std::ptr::null_mut()) },
            Err(_) => 0,
        }
    }

    pub fn thaw(cid: ConnectionId, wid: WindowId) -> i32 {
        type F = unsafe extern "C" fn(ConnectionId, WindowId) -> i32;
        match resolve::<F>("SLSWindowThaw") {
            Ok(f) => unsafe { f(cid, wid) },
            Err(_) => 0,
        }
    }

    pub fn flush(cid: ConnectionId, wid: WindowId) -> i32 {
        type F = unsafe extern "C" fn(ConnectionId, WindowId, *mut c_void) -> i32;
        match resolve::<F>("SLSFlushWindowContentRegion") {
            Ok(f) => unsafe { f(cid, wid, std::ptr::null_mut()) },
            Err(_) => 0,
        }
    }
}
