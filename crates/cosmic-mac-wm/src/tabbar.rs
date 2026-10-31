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

use std::ffi::c_int;
use std::sync::mpsc;

use cosmic_mac_protocol::{Rect, StackInfo};
use objc2_core_foundation::{
    CFAttributedString, CFDictionary, CFRetained, CFString, CFType, CGAffineTransform, CGPoint,
    CGRect, CGSize,
};
use objc2_core_graphics::CGContext;
use objc2_core_text::{CTFont, CTLine, kCTFontAttributeName, kCTForegroundColorAttributeName};

const SKYLIGHT_PATH: &str = "/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight";
const BAR_HEIGHT: f64 = 26.0;
const TAB_MAX_WIDTH: f64 = 220.0;
const TAB_MIN_WIDTH: f64 = 56.0;
const OVERLAY_TAGS: u64 = (1u64 << 1) | (1u64 << 9);
const ORDER_ABOVE: c_int = 1;

/// Handle to the tab-bar thread.
pub struct TabBarHandle {
    tx: mpsc::Sender<Option<StackInfo>>,
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
            .send(stack)
            .map_err(|_| "tab-bar thread stopped".to_string())
    }
}

struct TabBar {
    overlay: Option<Overlay>,
}

impl TabBar {
    fn new() -> Result<Self, String> {
        Ok(TabBar { overlay: None })
    }

    fn run(&mut self, rx: mpsc::Receiver<Option<StackInfo>>) {
        while let Ok(stack) = rx.recv() {
            let result = match stack {
                None => self.hide(),
                Some(stack) => self.show(&stack),
            };
            if let Err(error) = result {
                log::warn!("tab bar update failed: {error}");
            }
        }
        let _ = self.hide();
    }

    fn hide(&mut self) -> Result<(), String> {
        if let Some(overlay) = self.overlay.take() {
            overlay.hide()?;
        }
        Ok(())
    }

    fn show(&mut self, stack: &StackInfo) -> Result<(), String> {
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
        overlay.draw(stack)?;
        Ok(())
    }
}

fn bar_frame(frame: Rect) -> Rect {
    let width = frame.width.max(TAB_MIN_WIDTH);
    let height = BAR_HEIGHT;
    // Draw the bar over the top edge of the active member.  Placing it *above*
    // the frame would put it in the neighbouring tile's area, where any window
    // stacked above the active member (focus-without-raise keeps z-order) would
    // hide it.  Inside the frame it is always visible relative to its anchor.
    Rect::new(frame.x, frame.y, width, height)
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

    fn draw(&self, stack: &StackInfo) -> Result<(), String> {
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
        CGContext::set_rgb_fill_color(Some(context), 0.10, 0.10, 0.12, 0.94);
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
            let active = index == stack.active;
            if active {
                CGContext::set_rgb_fill_color(Some(context), 0.25, 0.55, 1.0, 1.0);
            } else {
                CGContext::set_rgb_fill_color(Some(context), 0.18, 0.18, 0.21, 1.0);
            }
            CGContext::fill_rect(Some(context), tab_rect);
            draw_close_glyph(context, x + tab_width - 12.0, self.frame.height / 2.0);
            let title = shorten(&tab.title, tab_width);
            draw_text(context, &title, x + 8.0, self.frame.height - 8.0, 11.0);
        }
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

fn draw_close_glyph(context: &CGContext, cx: f64, cy: f64) {
    CGContext::set_rgb_stroke_color(Some(context), 0.78, 0.78, 0.82, 1.0);
    CGContext::set_line_width(Some(context), 1.3);
    CGContext::move_to_point(Some(context), cx - 3.5, cy - 3.5);
    CGContext::add_line_to_point(Some(context), cx + 3.5, cy + 3.5);
    CGContext::move_to_point(Some(context), cx + 3.5, cy - 3.5);
    CGContext::add_line_to_point(Some(context), cx - 3.5, cy + 3.5);
    CGContext::stroke_path(Some(context));
}

fn draw_text(context: &CGContext, text: &str, x: f64, baseline: f64, size: f64) {
    if text.is_empty() {
        return;
    }
    let font_name = CFString::from_static_str("Helvetica Neue");
    // SAFETY: font name is a valid CFString, matrix is null.
    let font = unsafe { CTFont::with_name(&font_name, size, std::ptr::null()) };
    // SAFETY: the attribute-name statics are immutable CFStrings.
    let (font_key, color_key) = unsafe { (kCTFontAttributeName, kCTForegroundColorAttributeName) };
    let color = objc2_core_graphics::CGColor::new_generic_rgb(0.94, 0.94, 0.96, 1.0);
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
    // The SkyLight window context already has a top-left origin with y growing
    // downwards; Core Text's default text matrix draws upright glyphs there.
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
