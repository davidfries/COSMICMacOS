//! Error type shared by the platform primitives.

/// Errors returned by the macOS platform layer.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// The private SkyLight framework could not be loaded, or a symbol is missing.
    #[error("SkyLight unavailable: {0}")]
    SkyLightUnavailable(String),

    /// A SkyLight call returned a non-zero `CGError`.
    #[error("SkyLight call {call} failed (CGError {code})")]
    SkyLight {
        /// Name of the failing private function.
        call: &'static str,
        /// Raw `CGError` value.
        code: i32,
    },

    /// The process is not trusted for Accessibility (TCC).
    #[error(
        "Accessibility permission is not granted. Grant it in System Settings > Privacy & \
         Security > Accessibility (and keep the same code signature across rebuilds)."
    )]
    AccessibilityDenied,

    /// An AX call returned a non-zero `AXError`.
    #[error("Accessibility API error {code} in {call}")]
    Ax {
        /// Name of the failing AX function or attribute.
        call: &'static str,
        /// Raw `AXError` value.
        code: i32,
    },

    /// The requested window is not known to the window server.
    #[error("window {0} not found")]
    WindowNotFound(u32),

    /// Invalid argument supplied by the caller.
    #[error("invalid argument: {0}")]
    InvalidArgument(&'static str),

    /// A public macOS API failed.
    #[error("{context}: {message}")]
    Os {
        /// What was being attempted.
        context: &'static str,
        /// OS-provided description.
        message: String,
    },

    /// Anything else.
    #[error("{0}")]
    Other(String),
}

impl PlatformError {
    /// True when the failure is caused by a missing TCC grant.
    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Self::AccessibilityDenied)
            || matches!(self, Self::Ax { code, .. } if *code == AX_ERROR_API_DISABLED)
    }
}

/// `AX_ERROR_API_DISABLED` from `AXError.h`.
pub const AX_ERROR_API_DISABLED: i32 = -25211;

/// Convenience alias.
pub type Result<T> = std::result::Result<T, PlatformError>;

/// Convert a `CGError` into a `Result`.
pub(crate) fn cg_ok(call: &'static str, code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(PlatformError::SkyLight { call, code })
    }
}

/// Convert an `AXError` into a `Result`.
pub(crate) fn ax_ok(call: &'static str, code: i32) -> Result<()> {
    match code {
        0 => Ok(()),
        AX_ERROR_API_DISABLED => Err(PlatformError::AccessibilityDenied),
        code => Err(PlatformError::Ax { call, code }),
    }
}
