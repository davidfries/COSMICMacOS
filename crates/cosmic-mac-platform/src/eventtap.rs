//! Keyboard capture with a `CGEventTap`.
//!
//! The WM swallows bound Cmd combinations globally and lets everything else
//! through. The tap is installed at the HID/session level with
//! `kCGEventTapOptionDefault` (active tap) so the callback can return `NULL` to
//! swallow an event.
//!
//! Two failure modes matter:
//!
//! * `kCGEventTapDisabledByTimeout` / `kCGEventTapDisabledByUserInput`: the
//!   system disables a slow tap. The callback re-enables it immediately and
//!   logs a warning. Keep the callback allocation-free and fast.
//! * Secure Event Input (password fields, some terminals): the tap receives no
//!   key events at all. cosmicMac documents a Carbon `RegisterEventHotKey`
//!   fallback for the most important bindings; it is intentionally not wired
//!   up in the platform crate yet because Carbon hotkeys cannot swallow events
//!   and need a different action path in the WM.
//!
//! Accessibility is required for the tap to see key events from other apps.

use std::ffi::c_void;

use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
};

use crate::error::{PlatformError, Result};

/// A key binding the tap should swallow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeySpec {
    /// Virtual key code (`kVK_*`, e.g. 16 = Y).
    pub keycode: u16,
    /// Required modifier bits (`CGEventFlags`).
    pub modifiers: u64,
}

impl HotkeySpec {
    /// Command modifier mask (`kCGEventFlagMaskCommand`).
    pub const MASK_COMMAND: u64 = 1 << 20;
    /// Shift modifier mask (`kCGEventFlagMaskShift`).
    pub const MASK_SHIFT: u64 = 1 << 17;
    /// Option modifier mask (`kCGEventFlagMaskAlternate`).
    pub const MASK_OPTION: u64 = 1 << 19;
    /// Control modifier mask (`kCGEventFlagMaskControl`).
    pub const MASK_CONTROL: u64 = 1 << 18;

    /// Create a spec.
    pub const fn new(keycode: u16, modifiers: u64) -> Self {
        Self { keycode, modifiers }
    }
}

/// A key event observed by the tap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    /// Virtual key code.
    pub keycode: u16,
    /// Raw `CGEventFlags`.
    pub flags: u64,
    /// True for key down, false for key up.
    pub is_key_down: bool,
}

impl KeyEvent {
    /// Modifier bits with device-independent masks removed (caps lock, fn, ...).
    pub fn modifiers(&self) -> u64 {
        self.flags
            & (HotkeySpec::MASK_COMMAND
                | HotkeySpec::MASK_SHIFT
                | HotkeySpec::MASK_OPTION
                | HotkeySpec::MASK_CONTROL)
    }
}

/// Callback: return `true` to swallow the event, `false` to pass it through.
pub type TapHandler = Box<dyn Fn(KeyEvent) -> bool + Send + 'static>;

struct TapContext {
    handler: TapHandler,
    swallow: Vec<HotkeySpec>,
    tap: *mut c_void,
}

unsafe extern "C-unwind" fn tap_callback(
    _proxy: objc2_core_graphics::CGEventTapProxy,
    event_type: CGEventType,
    event: std::ptr::NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    if user_info.is_null() {
        return event.as_ptr();
    }
    // SAFETY: user_info points to the TapContext owned by EventTap.
    let context = unsafe { &mut *(user_info as *mut TapContext) };

    if event_type == CGEventType::TapDisabledByTimeout
        || event_type == CGEventType::TapDisabledByUserInput
    {
        log::warn!(
            "CGEventTap disabled by the system ({}); re-enabling",
            if event_type == CGEventType::TapDisabledByTimeout {
                "timeout"
            } else {
                "user input"
            }
        );
        if !context.tap.is_null() {
            // SAFETY: tap is the CFMachPort this callback belongs to.
            unsafe {
                objc2_core_graphics::CGEvent::tap_enable(&*(context.tap as *const CFMachPort), true)
            };
        }
        return event.as_ptr();
    }

    let is_key_down = event_type == CGEventType::KeyDown;
    let is_key_up = event_type == CGEventType::KeyUp;
    if !is_key_down && !is_key_up {
        return event.as_ptr();
    }

    // SAFETY: valid event reference.
    let keycode = unsafe {
        CGEvent::integer_value_field(Some(event.as_ref()), CGEventField::KeyboardEventKeycode)
    } as u16;
    // SAFETY: valid event reference.
    let flags = unsafe { CGEvent::flags(Some(event.as_ref())) }.0;

    let key = KeyEvent {
        keycode,
        flags,
        is_key_down,
    };
    let matched = context
        .swallow
        .iter()
        .any(|spec| spec.keycode == key.keycode && key.modifiers() == spec.modifiers);

    if matched && is_key_down {
        (context.handler)(key);
        return std::ptr::null_mut(); // swallow
    }
    if matched && is_key_up {
        // Swallow the matching key-up too, otherwise the app sees a stuck key.
        return std::ptr::null_mut();
    }
    if (context.handler)(key) {
        return std::ptr::null_mut();
    }
    event.as_ptr()
}

/// An installed active event tap.
pub struct EventTap {
    tap: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
    context: *mut TapContext,
}

// The tap is only used from the run loop thread; the raw pointers are immutable
// for the lifetime of the object.
unsafe impl Send for EventTap {}

impl EventTap {
    /// Create and enable a tap that swallows `swallow` and reports every other
    /// key event to `handler`.
    ///
    /// `handler` is called for every key down/up, including swallowed ones.
    /// Returning `true` from the handler also swallows that event.
    pub fn new(swallow: Vec<HotkeySpec>, handler: TapHandler) -> Result<Self> {
        let context = Box::into_raw(Box::new(TapContext {
            handler,
            swallow,
            tap: std::ptr::null_mut(),
        }));

        let mask: u64 = (1u64 << CGEventType::KeyDown.0)
            | (1u64 << CGEventType::KeyUp.0)
            | (1u64 << CGEventType::FlagsChanged.0);

        // SAFETY: callback and context outlive the tap (released in Drop).
        let tap = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::SessionEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mask,
                Some(tap_callback),
                context.cast(),
            )
        }
        .ok_or_else(|| {
            // SAFETY: reclaim the context because the tap was never created.
            unsafe { drop(Box::from_raw(context)) };
            PlatformError::Other(
                "CGEventTapCreate failed. Accessibility (and Input Monitoring on some macOS \
                 versions) must be granted to this binary."
                    .into(),
            )
        })?;

        let tap_ptr = CFRetained::as_ptr(&tap).as_ptr();
        // SAFETY: context is still alive.
        unsafe { (*context).tap = tap_ptr.cast() };

        let source = CFMachPort::new_run_loop_source(None, Some(&tap), 0)
            .ok_or_else(|| PlatformError::Other("CFMachPortCreateRunLoopSource failed".into()))?;

        let run_loop = CFRunLoop::current()
            .ok_or_else(|| PlatformError::Other("no current CFRunLoop".into()))?;
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
        CGEvent::tap_enable(&tap, true);

        Ok(Self {
            tap,
            source,
            context,
        })
    }

    /// Whether the tap is currently enabled.
    pub fn is_enabled(&self) -> bool {
        CGEvent::tap_is_enabled(&self.tap)
    }

    /// Enable or disable the tap.
    pub fn set_enabled(&self, enabled: bool) {
        CGEvent::tap_enable(&self.tap, enabled);
    }

    /// The run-loop source, in case the caller wants to add it to another loop.
    pub fn run_loop_source(&self) -> &CFRunLoopSource {
        &self.source
    }

    /// Add the tap's source to the calling thread's run loop.
    pub fn add_to_current_run_loop(&self) -> Result<()> {
        let run_loop = CFRunLoop::current()
            .ok_or_else(|| PlatformError::Other("no current CFRunLoop".into()))?;
        run_loop.add_source(Some(&self.source), unsafe { kCFRunLoopCommonModes });
        Ok(())
    }
}

impl Drop for EventTap {
    fn drop(&mut self) {
        CGEvent::tap_enable(&self.tap, false);
        // SAFETY: the source and tap are released by CFMachPort/CFRunLoopSource
        // Drop; reclaim our callback context.
        unsafe { drop(Box::from_raw(self.context)) };
    }
}

/// A note for the daemon: Secure Event Input disables event taps entirely.
///
/// The milestone-1 fallback plan is Carbon `RegisterEventHotKey` for the
/// bindings that must keep working (workspace switching, close, focus). Carbon
/// hotkeys cannot swallow events, so they are only a degraded mode.
pub const SECURE_INPUT_FALLBACK_NOTE: &str = "When Secure Event Input is active the CGEventTap \
receives no key events. Fall back to Carbon RegisterEventHotKey for critical bindings; those \
cannot swallow events and are a degraded mode.";
