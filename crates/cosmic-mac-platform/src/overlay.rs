//! Own overlay windows, ordered directly above a foreign window.
//!
//! This is the JankyBorders technique, proven with SIP on:
//!
//! 1. create a raw SkyLight window (`SLSNewWindow`) with the *floating* and
//!    *ignore-for-events* tags;
//! 2. draw into it with the `CGContext` returned by `SLWindowContextCreate`;
//! 3. copy the target window's level and sub-level and order the overlay above
//!    it in one `SLSTransaction*` batch, so the overlay cannot flicker between
//!    levels;
//! 4. move it with the target from SkyLight move/resize notifications.
//!
//! The same primitive is used by milestone 1 for stack tab bars. Milestone 2
//! can replace the Core Graphics drawing with a libcosmic/wgpu surface once the
//! AppKit `windowNumber` ordering test in Spike C has proven that path.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_foundation::{
    CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType, CFURL, CGRect,
};
#[allow(deprecated)]
use objc2_core_graphics::{
    CGContext, CGWindowImageOption, CGWindowListCreateImage, CGWindowListOption,
};

use crate::error::{PlatformError, Result};
use crate::geometry::Rect;
use crate::skylight::{self, BACKING_STORE_BUFFERED, ORDER_ABOVE, OVERLAY_TAGS};

/// An RGBA color with components in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    /// Red.
    pub r: f32,
    /// Green.
    pub g: f32,
    /// Blue.
    pub b: f32,
    /// Alpha.
    pub a: f32,
}

impl Color {
    /// Create a color.
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// Opaque COSMIC-like accent blue.
    pub const fn accent() -> Self {
        Self::rgba(0.25, 0.55, 1.0, 1.0)
    }
}

/// A raw SkyLight overlay window with a Core Graphics drawing context.
pub struct OverlayWindow {
    cid: i32,
    owns_connection: bool,
    wid: u32,
    context: *mut c_void,
    frame: Rect,
    hidpi: bool,
}

// The window is only touched from the thread that owns the connection.
unsafe impl Send for OverlayWindow {}

impl OverlayWindow {
    /// Create an overlay at `frame` (top-left global coordinates).
    pub fn new(frame: Rect, hidpi: bool) -> Result<Self> {
        // JankyBorders gives every border window its own SkyLight connection.
        // Windows created on the main connection of a plain CLI process are not
        // reliably shown by the compositor; a dedicated connection is the
        // proven path and also keeps our windows out of the app's own list.
        let mut new_cid: i32 = 0;
        // SAFETY: SLSNewConnection fills the out-pointer and returns a CGError.
        let new_result = unsafe { skylight::SLSNewConnection(0, &mut new_cid) };
        let (cid, owns_connection) = match new_result {
            Ok(0) if new_cid != 0 => (new_cid, true),
            _ => (skylight::main_connection()?, false),
        };
        let cg_frame = frame.to_cg();
        let mut region: *mut c_void = std::ptr::null_mut();
        // SAFETY: valid rect and out-pointer.
        crate::error::cg_ok("CGSNewRegionWithRect", unsafe {
            skylight::CGSNewRegionWithRect(&cg_frame, &mut region)?
        })?;
        if region.is_null() {
            return Err(PlatformError::Other(
                "CGSNewRegionWithRect returned null".into(),
            ));
        }

        let mut wid: u32 = 0;
        // SAFETY: region is a valid CFType; SkyLight copies it.
        let result = unsafe {
            skylight::SLSNewWindow(
                cid,
                BACKING_STORE_BUFFERED,
                -9999.0,
                -9999.0,
                region,
                &mut wid,
            )
        };
        // SAFETY: we own the +1 region.
        unsafe { skylight::cf_release(region) };
        crate::error::cg_ok("SLSNewWindow", result?)?;
        if wid == 0 {
            return Err(PlatformError::Other("SLSNewWindow returned wid 0".into()));
        }

        // SAFETY: valid connection/window.
        unsafe {
            let _ = skylight::SLSSetWindowResolution(cid, wid, if hidpi { 2.0 } else { 1.0 })?;
            let mut tags = OVERLAY_TAGS;
            let _ = skylight::SLSSetWindowTags(cid, wid, &mut tags, 64)?;
            let _ = skylight::SLSSetWindowOpacity(cid, wid, false)?;
        }
        remove_shadow(cid, wid);

        // SAFETY: valid connection/window; options may be null.
        let context = unsafe { skylight::SLWindowContextCreate(cid, wid, std::ptr::null_mut())? };
        if context.is_null() {
            // SAFETY: release the window we just created.
            unsafe {
                let _ = skylight::SLSReleaseWindow(cid, wid);
            };
            return Err(PlatformError::Other(
                "SLWindowContextCreate returned null".into(),
            ));
        }

        let overlay = Self {
            cid,
            owns_connection,
            wid,
            context,
            frame,
            hidpi,
        };
        overlay.move_to(frame.origin)?;
        overlay.show()?;
        Ok(overlay)
    }

    /// Create an overlay ordered directly above `target_wid`.
    pub fn new_above(target_wid: u32, frame: Rect, hidpi: bool) -> Result<Self> {
        let overlay = Self::new(frame, hidpi)?;
        overlay.order_above(target_wid)?;
        if let Err(err) = overlay.move_to_space_of(target_wid) {
            log::warn!("overlay could not follow wid {target_wid} to its Space: {err}");
        }
        Ok(overlay)
    }

    /// Window ID of the overlay.
    pub fn wid(&self) -> u32 {
        self.wid
    }

    /// Current frame.
    pub fn frame(&self) -> Rect {
        self.frame
    }

    /// Move and resize the overlay.
    pub fn set_frame(&mut self, frame: Rect) -> Result<()> {
        if frame == self.frame {
            return Ok(());
        }
        // SAFETY: valid connection.
        unsafe {
            let _ = skylight::SLSDisableUpdate(self.cid);
        };
        let result = (|| -> Result<()> {
            // JankyBorders freezes the window while changing its shape; without
            // this the compositor can keep the old surface.
            unsafe {
                let _ =
                    skylight::SLSWindowFreezeWithOptions(self.cid, self.wid, std::ptr::null_mut());
            }
            let cg_frame = frame.to_cg();
            let mut region: *mut c_void = std::ptr::null_mut();
            // SAFETY: valid rect and out-pointer.
            crate::error::cg_ok("CGSNewRegionWithRect", unsafe {
                skylight::CGSNewRegionWithRect(&cg_frame, &mut region)?
            })?;
            // SAFETY: valid window and region.
            let result =
                unsafe { skylight::SLSSetWindowShape(self.cid, self.wid, 0.0, 0.0, region) };
            // SAFETY: +1 region.
            unsafe { skylight::cf_release(region) };
            crate::error::cg_ok("SLSSetWindowShape", result?)?;
            let moved = self.move_to(frame.origin);
            unsafe {
                let _ = skylight::SLSWindowThaw(self.cid, self.wid);
            }
            moved
        })();
        // SAFETY: valid connection.
        unsafe {
            let _ = skylight::SLSReenableUpdate(self.cid);
        };
        if result.is_ok() {
            self.frame = frame;
        }
        result
    }

    /// Move the overlay without changing its size.
    ///
    /// The atomic path is `SLSTransactionMoveWindowWithGroup` + commit (what
    /// JankyBorders uses). On macOS 15.6 that path can return an opaque
    /// `CGSError` for plain `SLSNewWindow` windows, so we fall back to
    /// `SLSMoveWindow`, which is non-atomic but reliable.
    pub fn move_to(&self, origin: crate::Point) -> Result<()> {
        let point = objc2_core_foundation::CGPoint::new(origin.x, origin.y);
        // SAFETY: valid connection; transaction is +1.
        let transaction_result = unsafe {
            let transaction = skylight::SLSTransactionCreate(self.cid)?;
            if transaction.is_null() {
                Err(PlatformError::Other(
                    "SLSTransactionCreate returned null".into(),
                ))
            } else {
                let moved =
                    skylight::SLSTransactionMoveWindowWithGroup(transaction, self.wid, point);
                let committed = skylight::SLSTransactionCommit(transaction, 0);
                skylight::cf_release(transaction);
                match (moved, committed) {
                    (Err(err), _) | (_, Err(err)) => Err(err),
                    (Ok(moved_code), Ok(committed_code))
                        if moved_code == 0 && committed_code == 0 =>
                    {
                        Ok(())
                    }
                    (Ok(moved_code), Ok(committed_code)) => Err(PlatformError::SkyLight {
                        call: "SLSTransactionMoveWindowWithGroup",
                        code: if moved_code != 0 {
                            moved_code
                        } else {
                            committed_code
                        },
                    }),
                }
            }
        };
        match transaction_result {
            Ok(()) => Ok(()),
            Err(err) => {
                log::debug!("transaction move failed ({err}); falling back to SLSMoveWindow");
                // SAFETY: valid connection/window and point.
                let mut point = point;
                crate::error::cg_ok("SLSMoveWindow", unsafe {
                    skylight::SLSMoveWindow(self.cid, self.wid, &mut point)?
                })
            }
        }
    }

    /// Order the overlay directly above `target_wid`, copying its level and
    /// sub-level so that it stays glued to the target's stacking group.
    ///
    /// Falls back to the non-transactional setters when the transaction path
    /// is rejected (see [`OverlayWindow::move_to`]).
    pub fn order_above(&self, target_wid: u32) -> Result<()> {
        let level = skylight::window_level(target_wid).unwrap_or(0);
        let sub_level = skylight::window_sub_level(target_wid).unwrap_or(0);
        // SAFETY: valid connection; transaction is +1.
        let transaction_result = unsafe {
            let transaction = skylight::SLSTransactionCreate(self.cid)?;
            if transaction.is_null() {
                Err(PlatformError::Other(
                    "SLSTransactionCreate returned null".into(),
                ))
            } else {
                let _ = skylight::SLSTransactionSetWindowLevel(transaction, self.wid, level);
                let _ = skylight::SLSTransactionSetWindowSubLevel(transaction, self.wid, sub_level);
                let ordered = skylight::SLSTransactionOrderWindow(
                    transaction,
                    self.wid,
                    ORDER_ABOVE,
                    target_wid,
                );
                let committed = skylight::SLSTransactionCommit(transaction, 0);
                skylight::cf_release(transaction);
                match (ordered, committed) {
                    (Err(err), _) | (_, Err(err)) => Err(err),
                    (Ok(ordered_code), Ok(committed_code))
                        if ordered_code == 0 && committed_code == 0 =>
                    {
                        Ok(())
                    }
                    (Ok(ordered_code), Ok(committed_code)) => Err(PlatformError::SkyLight {
                        call: "SLSTransactionOrderWindow",
                        code: if ordered_code != 0 {
                            ordered_code
                        } else {
                            committed_code
                        },
                    }),
                }
            }
        };
        match transaction_result {
            Ok(()) => Ok(()),
            Err(err) => {
                log::debug!("transaction order failed ({err}); using non-atomic ordering");
                // SAFETY: valid connection/window.
                unsafe {
                    let _ = skylight::SLSSetWindowLevel(self.cid, self.wid, level);
                    let _ = skylight::SLSSetWindowSubLevel(self.cid, self.wid, sub_level);
                }
                crate::error::cg_ok("SLSOrderWindow", unsafe {
                    skylight::SLSOrderWindow(self.cid, self.wid, ORDER_ABOVE, target_wid)?
                })
            }
        }
    }

    /// Move the overlay to the managed Space of `target_wid`.
    ///
    /// A newly created raw SkyLight window is not necessarily on a managed
    /// Space, so it can be invisible even though `SLSGetWindowBounds` reports a
    /// frame. JankyBorders sends every border window to the target's Space for
    /// exactly this reason.
    pub fn move_to_space_of(&self, target_wid: u32) -> Result<()> {
        let space = space_of_window(target_wid)?;
        let wid_number = CFNumber::new_i32(self.wid as i32);
        let overlay_array: CFRetained<CFArray<CFType>> = CFArray::from_objects(&[&*wid_number]);
        // SAFETY: valid connection and CFArray of CFNumbers. The call returns
        // void; failures are only observable through the resulting Space.
        unsafe {
            skylight::SLSMoveWindowsToManagedSpace(
                self.cid,
                CFRetained::as_ptr(&overlay_array).as_ptr().cast(),
                space,
            )?;
        }
        Ok(())
    }

    /// Move the overlay to the Space of the frontmost on-screen window.
    ///
    /// Used by Spike C's own-window mode, where the target itself was just
    /// created and does not belong to a managed Space yet.
    pub fn move_to_active_space(&self) -> Result<()> {
        let anchor = crate::window::list_windows()?
            .into_iter()
            .next()
            .ok_or_else(|| {
                PlatformError::Other("no on-screen window to anchor the Space".into())
            })?;
        self.move_to_space_of(anchor.wid)
    }

    /// Draw a border inside the overlay.
    pub fn draw_border(&self, color: Color, width: f64) -> Result<()> {
        let context =
            NonNull::new(self.context as *mut CGContext).map(|context| unsafe { context.as_ref() });
        let Some(context) = context else {
            return Err(PlatformError::Other("null CGContext".into()));
        };
        let bounds = CGRect::new(
            objc2_core_foundation::CGPoint::new(0.0, 0.0),
            objc2_core_foundation::CGSize::new(self.frame.size.width, self.frame.size.height),
        );
        // SAFETY: valid connection/window.
        unsafe {
            let _ = skylight::SLSWindowFreezeWithOptions(self.cid, self.wid, std::ptr::null_mut());
        }
        CGContext::clear_rect(Some(context), bounds);
        CGContext::set_should_antialias(Some(context), true);
        CGContext::set_line_width(Some(context), width);
        CGContext::set_rgb_stroke_color(
            Some(context),
            color.r as f64,
            color.g as f64,
            color.b as f64,
            color.a as f64,
        );
        let inset = width / 2.0;
        let border = CGRect::new(
            objc2_core_foundation::CGPoint::new(inset, inset),
            objc2_core_foundation::CGSize::new(
                (self.frame.size.width - width).max(0.0),
                (self.frame.size.height - width).max(0.0),
            ),
        );
        CGContext::stroke_rect(Some(context), border);
        CGContext::flush(Some(context));
        // SAFETY: valid connection/window.
        unsafe {
            let _ = skylight::SLSFlushWindowContentRegion(self.cid, self.wid, std::ptr::null_mut());
            let _ = skylight::SLSWindowThaw(self.cid, self.wid);
        };
        Ok(())
    }

    /// Fill the whole overlay with a solid color (used by Spike C's target
    /// window and by future tab bars).
    pub fn fill(&self, color: Color) -> Result<()> {
        let Some(context) =
            NonNull::new(self.context as *mut CGContext).map(|context| unsafe { context.as_ref() })
        else {
            return Err(PlatformError::Other("null CGContext".into()));
        };
        let bounds = CGRect::new(
            objc2_core_foundation::CGPoint::new(0.0, 0.0),
            objc2_core_foundation::CGSize::new(self.frame.size.width, self.frame.size.height),
        );
        // SAFETY: valid connection/window.
        unsafe {
            let _ = skylight::SLSWindowFreezeWithOptions(self.cid, self.wid, std::ptr::null_mut());
        }
        CGContext::clear_rect(Some(context), bounds);
        CGContext::set_rgb_fill_color(
            Some(context),
            color.r as f64,
            color.g as f64,
            color.b as f64,
            color.a as f64,
        );
        CGContext::fill_rect(Some(context), bounds);
        CGContext::flush(Some(context));
        // SAFETY: valid connection/window.
        unsafe {
            let _ = skylight::SLSFlushWindowContentRegion(self.cid, self.wid, std::ptr::null_mut());
            let _ = skylight::SLSWindowThaw(self.cid, self.wid);
        };
        Ok(())
    }

    /// Order the overlay in (front of its level).
    pub fn show(&self) -> Result<()> {
        // SAFETY: valid connection/window.
        crate::error::cg_ok("SLSOrderWindow", unsafe {
            skylight::SLSOrderWindow(self.cid, self.wid, ORDER_ABOVE, 0)?
        })
    }

    /// Order the overlay out (hidden, but keeps its content).
    pub fn hide(&self) -> Result<()> {
        // SAFETY: valid connection/window.
        crate::error::cg_ok("SLSOrderWindow", unsafe {
            skylight::SLSOrderWindow(self.cid, self.wid, 0, 0)?
        })
    }

    /// Whether this is a HiDPI overlay.
    pub fn is_hidpi(&self) -> bool {
        self.hidpi
    }
}

impl Drop for OverlayWindow {
    fn drop(&mut self) {
        // SAFETY: we own the window and context.
        unsafe {
            let _ = skylight::SLSReleaseWindow(self.cid, self.wid);
            skylight::cf_release(self.context);
            if self.owns_connection {
                let _ = skylight::SLSReleaseConnection(self.cid);
            }
        }
    }
}

/// Managed Space ID of a window, via `SLSCopySpacesForWindows` (selector 0x7).
fn space_of_window(wid: u32) -> Result<u64> {
    let cid = skylight::main_connection()?;
    let wid_number = CFNumber::new_i32(wid as i32);
    let target_array: CFRetained<CFArray<CFType>> = CFArray::from_objects(&[&*wid_number]);
    // SAFETY: valid connection and CFArray of CFNumbers.
    let spaces = unsafe {
        skylight::SLSCopySpacesForWindows(
            cid,
            0x7,
            CFRetained::as_ptr(&target_array).as_ptr().cast(),
        )?
    };
    if spaces.is_null() {
        return Err(PlatformError::Other(format!(
            "SLSCopySpacesForWindows({wid}) returned null"
        )));
    }
    // SAFETY: +1 CFArray of CFNumbers; release after reading.
    let spaces_ref: &CFArray<CFType> = unsafe { (&*(spaces as *const CFArray)).cast_unchecked() };
    let space = spaces_ref
        .get(0)
        .and_then(|value| value.downcast::<CFNumber>().ok())
        .and_then(|value| value.as_i64());
    // SAFETY: we own the +1 reference.
    unsafe { skylight::cf_release(spaces) };
    space
        .map(|space| space as u64)
        .ok_or_else(|| PlatformError::Other(format!("window {wid} is not on a managed Space")))
}

fn remove_shadow(cid: i32, wid: u32) {
    let key = CFString::from_static_str("com.apple.WindowShadowDensity");
    let value = CFNumber::new_f64(0.0);
    let dict: CFRetained<CFDictionary<CFString, CFType>> =
        CFDictionary::from_slices(&[&key], &[value.as_ref()]);
    // SAFETY: valid window and CFDictionary.
    unsafe {
        let _ =
            skylight::SLSWindowSetShadowProperties(wid, CFRetained::as_ptr(&dict).as_ptr().cast());
    }
    let _ = cid;
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    fn CGImageDestinationCreateWithURL(
        url: *const c_void,
        type_: *const c_void,
        count: usize,
        properties: *const c_void,
    ) -> *mut c_void;
    fn CGImageDestinationAddImage(
        destination: *mut c_void,
        image: *const c_void,
        properties: *const c_void,
    );
    fn CGImageDestinationFinalize(destination: *mut c_void) -> bool;
}

/// Capture one window (including our own raw SLS windows) to a PNG file.
///
/// `CGWindowListCreateImage` with `OptionIncludingWindow` does not need the
/// Screen Recording grant for windows owned by the calling process, which makes
/// this the evidence path for Spike C when `screencapture` over SSH is still
/// waiting for the one-time Screen Recording grant. It is deprecated in favour
/// of ScreenCaptureKit but still functional on macOS 15.6.
#[allow(deprecated)]
pub fn capture_window_png(wid: u32, path: &std::path::Path) -> Result<()> {
    // CGRectNull: the bounds are ignored for a single-window capture.
    let null_rect = CGRect::new(
        objc2_core_foundation::CGPoint::new(f64::INFINITY, f64::INFINITY),
        objc2_core_foundation::CGSize::new(0.0, 0.0),
    );
    let image = CGWindowListCreateImage(
        null_rect,
        CGWindowListOption::OptionIncludingWindow,
        wid,
        CGWindowImageOption::BoundsIgnoreFraming,
    )
    .ok_or_else(|| PlatformError::Other(format!("CGWindowListCreateImage({wid}) returned null")))?;
    let url = CFURL::from_file_path(path)
        .ok_or_else(|| PlatformError::Other(format!("invalid path {path:?}")))?;
    let png = CFString::from_static_str("public.png");
    // SAFETY: valid CFURL/CFString and a +1 destination.
    let destination = unsafe {
        CGImageDestinationCreateWithURL(
            CFRetained::as_ptr(&url).as_ptr().cast(),
            CFRetained::as_ptr(&png).as_ptr().cast(),
            1,
            std::ptr::null(),
        )
    };
    if destination.is_null() {
        return Err(PlatformError::Other(
            "CGImageDestinationCreateWithURL failed".into(),
        ));
    }
    // SAFETY: valid destination and image.
    let finalized = unsafe {
        CGImageDestinationAddImage(
            destination,
            CFRetained::as_ptr(&image).as_ptr().cast(),
            std::ptr::null(),
        );
        CGImageDestinationFinalize(destination)
    };
    // SAFETY: we own the +1 destination.
    unsafe { skylight::cf_release(destination) };
    if finalized {
        Ok(())
    } else {
        Err(PlatformError::Other(format!(
            "CGImageDestinationFinalize failed for {path:?}"
        )))
    }
}

/// Send a synthetic key event (used by tests to verify the event tap).
pub fn post_key(keycode: u16, flags: u64, down: bool) -> Result<()> {
    use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventTapLocation};
    let event = CGEvent::new_keyboard_event(None, keycode, down)
        .ok_or_else(|| PlatformError::Other("CGEventCreateKeyboardEvent failed".into()))?;
    CGEvent::set_flags(Some(&event), CGEventFlags(flags));
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    Ok(())
}
