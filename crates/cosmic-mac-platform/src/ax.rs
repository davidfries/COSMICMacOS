//! Accessibility (AX) API bindings and a small safe wrapper.
//!
//! The Accessibility API is the only supported way to read window titles,
//! roles, subroles and to move/resize windows of other applications. It
//! requires the TCC "Accessibility" grant for the calling binary. The grant is
//! keyed on the code signature's designated requirement, which is why
//! `scripts/sign.sh` signs every build with a stable self-signed identity.
//!
//! AX calls are safe to make from any thread (Apple documents `AXUIElement`
//! functions as thread-safe). Observers deliver callbacks on the run loop their
//! source is added to; see [`AxObserver`].

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_foundation::CGPoint;
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFRunLoop, CFString, CFType,
};

use crate::error::{AX_ERROR_API_DISABLED, PlatformError, Result, ax_ok};
use crate::geometry::{Point, Size};

/// `AXUIElementRef` (`CFTypeRef`).
pub type AxElementRef = *const c_void;
/// `AXObserverRef` (`CFTypeRef`).
pub type AxObserverRef = *const c_void;
/// `AXError`.
pub type AxError = i32;

/// `kAXValueCGPointType`.
pub const AX_VALUE_CG_POINT: u32 = 1;
/// `kAXValueCGSizeType`.
pub const AX_VALUE_CG_SIZE: u32 = 2;

// Attribute names used throughout the crate. Creating CFStrings for these on
// every call is cheap and avoids depending on exported symbols that differ
// between SDK versions.
pub(crate) const ATTR_WINDOWS: &str = "AXWindows";
pub(crate) const ATTR_FOCUSED_WINDOW: &str = "AXFocusedWindow";
#[allow(dead_code)]
pub(crate) const ATTR_FOCUSED_APPLICATION: &str = "AXFocusedApplication";
pub(crate) const ATTR_ROLE: &str = "AXRole";
pub(crate) const ATTR_SUBROLE: &str = "AXSubrole";
pub(crate) const ATTR_TITLE: &str = "AXTitle";
pub(crate) const ATTR_POSITION: &str = "AXPosition";
pub(crate) const ATTR_SIZE: &str = "AXSize";
pub(crate) const ATTR_MINIMIZED: &str = "AXMinimized";
pub(crate) const ATTR_CLOSE_BUTTON: &str = "AXCloseButton";
#[allow(dead_code)]
pub(crate) const ATTR_FULLSCREEN: &str = "AXFullScreen";
#[allow(dead_code)]
pub(crate) const ATTR_MAIN: &str = "AXMain";
#[allow(dead_code)]
pub(crate) const ATTR_FOCUSED: &str = "AXFocused";
#[allow(dead_code)]
pub(crate) const ATTR_APPLICATION_ACTIVATED: &str = "AXApplicationActivated";
#[allow(dead_code)]
pub(crate) const ATTR_HIDDEN: &str = "AXHidden";
pub(crate) const ACTION_PRESS: &str = "AXPress";
pub(crate) const ACTION_RAISE: &str = "AXRaise";

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: libc::pid_t) -> AxElementRef;
    fn AXUIElementCreateSystemWide() -> AxElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AxElementRef,
        attribute: *const c_void,
        value: *mut *const c_void,
    ) -> AxError;
    fn AXUIElementSetAttributeValue(
        element: AxElementRef,
        attribute: *const c_void,
        value: *const c_void,
    ) -> AxError;
    fn AXUIElementIsAttributeSettable(
        element: AxElementRef,
        attribute: *const c_void,
        settable: *mut u8,
    ) -> AxError;
    fn AXUIElementPerformAction(element: AxElementRef, action: *const c_void) -> AxError;

    fn AXValueCreate(value_type: u32, value: *const c_void) -> *const c_void;
    fn AXValueGetValue(value: *const c_void, value_type: u32, out: *mut c_void) -> bool;

    fn AXObserverCreate(
        pid: libc::pid_t,
        callback: AxObserverCallback,
        observer: *mut AxObserverRef,
    ) -> AxError;
    fn AXObserverAddNotification(
        observer: AxObserverRef,
        element: AxElementRef,
        notification: *const c_void,
        refcon: *mut c_void,
    ) -> AxError;
    fn AXObserverRemoveNotification(
        observer: AxObserverRef,
        element: AxElementRef,
        notification: *const c_void,
    ) -> AxError;
    fn AXObserverGetRunLoopSource(observer: AxObserverRef) -> *const c_void;

    /// Public: `Boolean AXIsProcessTrusted(void)`.
    fn AXIsProcessTrusted() -> u8;
    /// Public: `Boolean AXIsProcessTrustedWithOptions(CFDictionaryRef)`.
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;

    /// Private but stable: maps an AX window element to its `CGWindowID`.
    fn _AXUIElementGetWindow(element: AxElementRef, wid: *mut u32) -> AxError;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(cf: *const c_void) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

/// AX observer callback signature.
pub type AxObserverCallback = unsafe extern "C" fn(
    observer: AxObserverRef,
    element: AxElementRef,
    notification: *const c_void,
    refcon: *mut c_void,
);

fn cf_string_ptr(string: &CFString) -> *const c_void {
    string as *const CFString as *const c_void
}

/// An owned `AXUIElementRef`.
///
/// AX elements are CF objects. Cloning retains, dropping releases.
#[derive(Debug)]
pub struct AxElement {
    ptr: AxElementRef,
}

// AXUIElement functions are documented as thread-safe; the ref is immutable.
unsafe impl Send for AxElement {}
unsafe impl Sync for AxElement {}

impl AxElement {
    /// Take ownership of a +1 reference returned by an AX function.
    ///
    /// # Safety
    /// `ptr` must be a valid AX element with a +1 retain count, or null.
    pub unsafe fn from_owned(ptr: AxElementRef) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr })
        }
    }

    /// Retain a borrowed AX element (for example one returned from an array).
    ///
    /// # Safety
    /// `ptr` must be a valid AX element or null.
    pub unsafe fn from_borrowed(ptr: AxElementRef) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            // SAFETY: valid AX element, retain balances our Drop.
            unsafe { CFRetain(ptr) };
            Some(Self { ptr })
        }
    }

    /// Raw element reference, for FFI.
    pub fn as_ptr(&self) -> AxElementRef {
        self.ptr
    }

    /// Create the application element for `pid`.
    pub fn application(pid: libc::pid_t) -> Result<Self> {
        // SAFETY: AXUIElementCreateApplication returns +1 or null.
        let ptr = unsafe { AXUIElementCreateApplication(pid) };
        // SAFETY: +1 reference.
        unsafe { Self::from_owned(ptr) }.ok_or(PlatformError::Other(format!(
            "AXUIElementCreateApplication({pid}) returned null"
        )))
    }

    /// Create the system-wide element.
    pub fn system_wide() -> Result<Self> {
        // SAFETY: +1 reference or null.
        let ptr = unsafe { AXUIElementCreateSystemWide() };
        // SAFETY: +1 reference.
        unsafe { Self::from_owned(ptr) }
            .ok_or_else(|| PlatformError::Other("AXUIElementCreateSystemWide returned null".into()))
    }

    /// Copy an attribute. Returns the raw CF value with a +1 retain count.
    pub fn copy_attribute(&self, name: &str) -> Result<CFRetained<CFType>> {
        let attribute = CFString::from_str(name);
        let mut value: *const c_void = std::ptr::null();
        // SAFETY: valid element, attribute and out-pointer.
        let err = unsafe {
            AXUIElementCopyAttributeValue(self.ptr, cf_string_ptr(&attribute), &mut value)
        };
        ax_ok("AXUIElementCopyAttributeValue", err)?;
        let Some(value) = NonNull::new(value as *mut CFType) else {
            return Err(PlatformError::Other(format!(
                "AX attribute {name} returned a null value"
            )));
        };
        // SAFETY: AX returns a +1 CF value.
        Ok(unsafe { CFRetained::from_raw(value) })
    }

    /// Copy a string attribute.
    pub fn string_attribute(&self, name: &str) -> Result<Option<String>> {
        match self.copy_attribute(name) {
            Ok(value) => match value.downcast::<CFString>() {
                Ok(string) => Ok(Some(string.to_string())),
                Err(_) => Ok(None),
            },
            Err(PlatformError::Ax { code: -25205, .. }) => Ok(None), // unsupported
            Err(PlatformError::Ax { code: -25212, .. }) => Ok(None), // no value
            Err(err) => Err(err),
        }
    }

    /// Copy a boolean attribute.
    pub fn bool_attribute(&self, name: &str) -> Result<Option<bool>> {
        match self.copy_attribute(name) {
            Ok(value) => match value.downcast::<CFBoolean>() {
                Ok(boolean) => Ok(Some(boolean.as_bool())),
                Err(_) => Ok(None),
            },
            Err(PlatformError::Ax {
                code: -25205 | -25212,
                ..
            }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Copy an AXValue-backed point attribute (`AXPosition`).
    pub fn point_attribute(&self, name: &str) -> Result<Option<Point>> {
        match self.copy_attribute(name) {
            Ok(value) => {
                let mut point = CGPoint::new(0.0, 0.0);
                // SAFETY: AXValueGetValue validates the type.
                let ok = unsafe {
                    AXValueGetValue(
                        CFRetained::as_ptr(&value).as_ptr().cast(),
                        AX_VALUE_CG_POINT,
                        (&raw mut point).cast(),
                    )
                };
                if ok {
                    Ok(Some(Point::new(point.x, point.y)))
                } else {
                    Ok(None)
                }
            }
            Err(PlatformError::Ax {
                code: -25205 | -25212,
                ..
            }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Copy an AXValue-backed size attribute (`AXSize`).
    pub fn size_attribute(&self, name: &str) -> Result<Option<Size>> {
        match self.copy_attribute(name) {
            Ok(value) => {
                let mut size = objc2_core_foundation::CGSize::new(0.0, 0.0);
                // SAFETY: AXValueGetValue validates the type.
                let ok = unsafe {
                    AXValueGetValue(
                        CFRetained::as_ptr(&value).as_ptr().cast(),
                        AX_VALUE_CG_SIZE,
                        (&raw mut size).cast(),
                    )
                };
                if ok {
                    Ok(Some(Size::new(size.width, size.height)))
                } else {
                    Ok(None)
                }
            }
            Err(PlatformError::Ax {
                code: -25205 | -25212,
                ..
            }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Set a string attribute.
    pub fn set_string_attribute(&self, name: &str, value: &str) -> Result<()> {
        let attribute = CFString::from_str(name);
        let value = CFString::from_str(value);
        // SAFETY: valid element and CF values.
        let err = unsafe {
            AXUIElementSetAttributeValue(self.ptr, cf_string_ptr(&attribute), cf_string_ptr(&value))
        };
        ax_ok("AXUIElementSetAttributeValue", err)
    }

    /// Set a boolean attribute.
    pub fn set_bool_attribute(&self, name: &str, value: bool) -> Result<()> {
        let attribute = CFString::from_str(name);
        let value = CFBoolean::new(value);
        // SAFETY: valid element and CF values.
        let err = unsafe {
            AXUIElementSetAttributeValue(
                self.ptr,
                cf_string_ptr(&attribute),
                value as *const CFBoolean as *const c_void,
            )
        };
        ax_ok("AXUIElementSetAttributeValue", err)
    }

    /// Set a point attribute (`AXPosition`).
    pub fn set_point_attribute(&self, name: &str, point: Point) -> Result<()> {
        let attribute = CFString::from_str(name);
        let cg_point = CGPoint::new(point.x, point.y);
        // SAFETY: AXValueCreate copies the value.
        let value = unsafe { AXValueCreate(AX_VALUE_CG_POINT, (&raw const cg_point).cast()) };
        if value.is_null() {
            return Err(PlatformError::Other("AXValueCreate(point) failed".into()));
        }
        // SAFETY: valid element and CF values; release our +1 value afterwards.
        let err =
            unsafe { AXUIElementSetAttributeValue(self.ptr, cf_string_ptr(&attribute), value) };
        unsafe { CFRelease(value) };
        ax_ok("AXUIElementSetAttributeValue(AXPosition)", err)
    }

    /// Set a size attribute (`AXSize`).
    pub fn set_size_attribute(&self, name: &str, size: Size) -> Result<()> {
        let attribute = CFString::from_str(name);
        let cg_size = objc2_core_foundation::CGSize::new(size.width, size.height);
        // SAFETY: AXValueCreate copies the value.
        let value = unsafe { AXValueCreate(AX_VALUE_CG_SIZE, (&raw const cg_size).cast()) };
        if value.is_null() {
            return Err(PlatformError::Other("AXValueCreate(size) failed".into()));
        }
        // SAFETY: valid element and CF values; release our +1 value afterwards.
        let err =
            unsafe { AXUIElementSetAttributeValue(self.ptr, cf_string_ptr(&attribute), value) };
        unsafe { CFRelease(value) };
        ax_ok("AXUIElementSetAttributeValue(AXSize)", err)
    }

    /// Whether an attribute can be set on this element.
    pub fn is_settable(&self, name: &str) -> Result<bool> {
        let attribute = CFString::from_str(name);
        let mut settable: u8 = 0;
        // SAFETY: valid element and out-pointer.
        let err = unsafe {
            AXUIElementIsAttributeSettable(self.ptr, cf_string_ptr(&attribute), &mut settable)
        };
        ax_ok("AXUIElementIsAttributeSettable", err)?;
        Ok(settable != 0)
    }

    /// Perform an AX action (`AXPress`, `AXRaise`, ...).
    pub fn perform_action(&self, action: &str) -> Result<()> {
        let action = CFString::from_str(action);
        // SAFETY: valid element and CF string.
        let err = unsafe { AXUIElementPerformAction(self.ptr, cf_string_ptr(&action)) };
        ax_ok("AXUIElementPerformAction", err)
    }

    /// Copy an array attribute into owned elements.
    pub fn elements_attribute(&self, name: &str) -> Result<Vec<AxElement>> {
        let value = self.copy_attribute(name)?;
        let array = value
            .downcast::<CFArray>()
            .map_err(|_| PlatformError::Other(format!("AX attribute {name} is not an array")))?;
        // SAFETY: an AX array contains CFType-compatible elements.
        let array: &CFArray<CFType> = unsafe { array.cast_unchecked() };
        let mut elements = Vec::with_capacity(array.len());
        for index in 0..array.len() {
            if let Some(value) = array.get(index) {
                // SAFETY: borrowed element, from_borrowed retains it.
                if let Some(element) =
                    unsafe { AxElement::from_borrowed(CFRetained::as_ptr(&value).as_ptr().cast()) }
                {
                    elements.push(element);
                }
            }
        }
        Ok(elements)
    }

    /// The `CGWindowID` behind an AX window element, when it has one.
    pub fn window_id(&self) -> Option<u32> {
        let mut wid: u32 = 0;
        // SAFETY: valid element and out-pointer.
        let err = unsafe { _AXUIElementGetWindow(self.ptr, &mut wid) };
        if err == 0 && wid != 0 {
            Some(wid)
        } else {
            None
        }
    }

    /// The current frame (`AXPosition` + `AXSize`) of a window element.
    pub fn frame(&self) -> Result<Option<crate::Rect>> {
        let Some(origin) = self.point_attribute(ATTR_POSITION)? else {
            return Ok(None);
        };
        let Some(size) = self.size_attribute(ATTR_SIZE)? else {
            return Ok(None);
        };
        Ok(Some(crate::Rect::new(
            origin.x,
            origin.y,
            size.width,
            size.height,
        )))
    }

    /// Copy a numeric attribute as `i64`.
    pub fn number_attribute(&self, name: &str) -> Result<Option<i64>> {
        match self.copy_attribute(name) {
            Ok(value) => match value.downcast::<CFNumber>() {
                Ok(number) => Ok(number.as_i64()),
                Err(_) => Ok(None),
            },
            Err(PlatformError::Ax {
                code: -25205 | -25212,
                ..
            }) => Ok(None),
            Err(err) => Err(err),
        }
    }
}

impl Clone for AxElement {
    fn clone(&self) -> Self {
        // SAFETY: valid element.
        unsafe { CFRetain(self.ptr) };
        Self { ptr: self.ptr }
    }
}

impl Drop for AxElement {
    fn drop(&mut self) {
        // SAFETY: we own a +1 reference.
        unsafe { CFRelease(self.ptr) };
    }
}

/// Whether this process is trusted for Accessibility.
pub fn is_process_trusted() -> bool {
    // SAFETY: no arguments.
    unsafe { AXIsProcessTrusted() != 0 }
}

/// Ask the system to show the Accessibility prompt (or just report the state).
///
/// The prompt is only shown once per binary identity; after that the user has to
/// toggle the entry in System Settings. Returns the current trust state.
pub fn is_process_trusted_with_prompt(prompt: bool) -> bool {
    let key = CFString::from_static_str("AXTrustedCheckOptionPrompt");
    let value = CFBoolean::new(prompt);
    let dict: CFRetained<CFDictionary<CFString, CFType>> =
        CFDictionary::from_slices(&[&key], &[value.as_ref()]);
    // SAFETY: valid CFDictionary.
    unsafe { AXIsProcessTrustedWithOptions(CFRetained::as_ptr(&dict).as_ptr().cast()) != 0 }
}

/// An AX observer for one process.
///
/// The observer's run-loop source must be added to a run loop that is actually
/// running. [`AxObserver::add_to_current_run_loop`] does that for the calling
/// thread; the daemon runs it on the main thread.
#[derive(Debug)]
pub struct AxObserver {
    ptr: AxObserverRef,
    pid: libc::pid_t,
}

// The observer object is immutable after creation; callbacks are serialized by
// the run loop it is attached to.
unsafe impl Send for AxObserver {}
unsafe impl Sync for AxObserver {}

impl AxObserver {
    /// Create an observer for `pid`.
    pub fn new(pid: libc::pid_t, callback: AxObserverCallback) -> Result<Self> {
        let mut ptr: AxObserverRef = std::ptr::null();
        // SAFETY: valid pid, callback and out-pointer.
        let err = unsafe { AXObserverCreate(pid, callback, &mut ptr) };
        ax_ok("AXObserverCreate", err)?;
        if ptr.is_null() {
            return Err(PlatformError::Other(
                "AXObserverCreate returned null".into(),
            ));
        }
        Ok(Self { ptr, pid })
    }

    /// PID this observer belongs to.
    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// Raw observer reference, for FFI (used by the callback to register
    /// notifications for windows created after the observer was installed).
    pub fn as_ptr(&self) -> AxObserverRef {
        self.ptr
    }

    /// Add a notification for an element.
    ///
    /// `refcon` is passed through to the callback untouched; it is the caller's
    /// responsibility to keep it valid while the notification is registered.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn add_notification(
        &self,
        element: &AxElement,
        notification: &str,
        refcon: *mut c_void,
    ) -> Result<()> {
        let name = CFString::from_str(notification);
        // SAFETY: valid observer, element and CF string.
        let err = unsafe {
            AXObserverAddNotification(self.ptr, element.as_ptr(), cf_string_ptr(&name), refcon)
        };
        // kAXErrorNotificationAlreadyRegistered (-25209) is harmless.
        if err == -25209 {
            return Ok(());
        }
        ax_ok("AXObserverAddNotification", err)
    }

    /// Remove a notification.
    pub fn remove_notification(&self, element: &AxElement, notification: &str) -> Result<()> {
        let name = CFString::from_str(notification);
        // SAFETY: valid observer, element and CF string.
        let err = unsafe {
            AXObserverRemoveNotification(self.ptr, element.as_ptr(), cf_string_ptr(&name))
        };
        ax_ok("AXObserverRemoveNotification", err)
    }

    /// Add the observer's source to the calling thread's current run loop.
    pub fn add_to_current_run_loop(&self) -> Result<()> {
        // SAFETY: valid observer.
        let source = unsafe { AXObserverGetRunLoopSource(self.ptr) };
        if source.is_null() {
            return Err(PlatformError::Other(
                "AXObserverGetRunLoopSource returned null".into(),
            ));
        }
        let run_loop = CFRunLoop::current()
            .ok_or_else(|| PlatformError::Other("no current CFRunLoop".into()))?;
        // SAFETY: the source is owned by the observer, which outlives the run loop.
        let source = unsafe { &*(source as *const objc2_core_foundation::CFRunLoopSource) };
        run_loop.add_source(Some(source), unsafe {
            objc2_core_foundation::kCFRunLoopCommonModes
        });
        Ok(())
    }
}

impl Drop for AxObserver {
    fn drop(&mut self) {
        // SAFETY: we own the +1 reference from AXObserverCreate.
        unsafe { CFRelease(self.ptr) };
    }
}

/// Add a notification through a raw observer reference.
///
/// Used by the AX callback when a window is created after the observer was
/// installed: the new window element needs its own move/resize/title
/// notifications, and the callback only has the raw observer pointer.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub(crate) fn add_notification_raw(
    observer: AxObserverRef,
    element: &AxElement,
    notification: &str,
    refcon: *mut c_void,
) -> Result<()> {
    let name = CFString::from_str(notification);
    // SAFETY: valid observer, element and CF string.
    let err = unsafe {
        AXObserverAddNotification(observer, element.as_ptr(), cf_string_ptr(&name), refcon)
    };
    if err == -25209 {
        return Ok(());
    }
    ax_ok("AXObserverAddNotification", err)
}

/// Whether `err` means the process lacks the Accessibility grant.
pub fn is_permission_error(err: &PlatformError) -> bool {
    err.is_permission_denied()
        || matches!(err, PlatformError::Ax { code, .. } if *code == AX_ERROR_API_DISABLED)
}
