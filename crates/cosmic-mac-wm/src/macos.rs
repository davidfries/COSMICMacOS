// SPDX-License-Identifier: GPL-3.0-or-later
//! Real macOS backend: a thin adapter over `cosmic-mac-platform`.

use std::sync::Mutex;

use cosmic_mac_platform::window::{self, ParkStore, WindowInfo};
use cosmic_mac_platform::{Rect as PlatformRect, display};
use cosmic_mac_protocol::{Rect, StackInfo, ToplevelInfo, WindowId};

use crate::backend::{PlatformBackend, PlatformResult};
use crate::tabbar::TabBarHandle;

/// The production backend.
pub struct MacPlatform {
    park: ParkStore,
    work_area: Mutex<Rect>,
    /// Full frame of the main display, cached on the main thread.  Used as the
    /// parking corner so `display_frame` does not have to call `NSScreen` from
    /// the layout thread.
    main_frame: Rect,
    tab_bar: Option<TabBarHandle>,
}

impl MacPlatform {
    /// Create the backend and cache the main display's work area.
    ///
    /// Must be called on the main thread so `NSScreen.visibleFrame` is
    /// available.
    pub fn new() -> Self {
        let (work_area, main_frame) = main_display_frames();
        let tab_bar = match TabBarHandle::spawn() {
            Ok(handle) => Some(handle),
            Err(error) => {
                log::warn!("stack tab bar disabled: {error}");
                None
            }
        };
        MacPlatform {
            park: ParkStore::new(),
            work_area: Mutex::new(work_area),
            main_frame,
            tab_bar,
        }
    }

    /// Update the cached work area after a display change.
    pub fn update_work_area(&self, work_area: Rect) {
        *self.work_area.lock().expect("work area poisoned") = work_area;
    }

    fn info(&self, wid: WindowId) -> Option<ToplevelInfo> {
        window::window_info(wid as u32)
            .ok()
            .flatten()
            .map(|info| to_toplevel(&info))
    }
}

impl Default for MacPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl PlatformBackend for MacPlatform {
    fn set_frame(&self, wid: WindowId, rect: Rect) -> PlatformResult<()> {
        window::set_frame(wid as u32, to_platform(rect)).map_err(|error| error.to_string())
    }

    fn raise(&self, wid: WindowId) -> PlatformResult<()> {
        window::raise(wid as u32).map_err(|error| error.to_string())
    }

    fn focus(&self, wid: WindowId) -> PlatformResult<()> {
        window::focus_without_raise(wid as u32).map_err(|error| error.to_string())
    }

    fn close(&self, wid: WindowId) -> PlatformResult<()> {
        window::close(wid as u32).map_err(|error| error.to_string())
    }

    fn park(&self, wid: WindowId, display_frame: Rect) -> PlatformResult<Option<Rect>> {
        self.park
            .park(wid as u32, to_platform(display_frame))
            .map(|saved| saved.map(from_platform))
            .map_err(|error| error.to_string())
    }

    fn unpark(&self, wid: WindowId) -> PlatformResult<Option<Rect>> {
        self.park
            .unpark(wid as u32)
            .map(|restored| restored.then(|| self.frame(wid)).flatten())
            .map_err(|error| error.to_string())
    }

    fn saved_frame(&self, wid: WindowId) -> Option<Rect> {
        self.park.saved_frame(wid as u32).map(from_platform)
    }

    fn parked(&self) -> Vec<(WindowId, Rect)> {
        self.park
            .saved_frames()
            .into_iter()
            .map(|(wid, frame)| (u64::from(wid), from_platform(frame)))
            .collect()
    }

    fn forget(&self, wid: WindowId) {
        self.park.forget(wid as u32);
    }

    fn restore_all(&self) -> Vec<(WindowId, String)> {
        self.park
            .restore_all()
            .into_iter()
            .map(|(wid, error)| (u64::from(wid), error.to_string()))
            .collect()
    }

    fn frame(&self, wid: WindowId) -> Option<Rect> {
        self.info(wid).and_then(|info| info.initial_frame)
    }

    fn window_info(&self, wid: WindowId) -> Option<ToplevelInfo> {
        self.info(wid)
    }

    fn windows(&self) -> Vec<ToplevelInfo> {
        window::list_windows()
            .map(|windows| {
                windows
                    .iter()
                    .filter(|info| info.level == 0 && !info.is_minimized)
                    .map(to_toplevel)
                    .collect()
            })
            .unwrap_or_else(|error| {
                log::warn!("window enumeration failed: {error}");
                Vec::new()
            })
    }

    fn set_stack_bar(&self, stack: Option<StackInfo>) -> PlatformResult<()> {
        match &self.tab_bar {
            Some(tab_bar) => tab_bar.update(stack),
            None => Ok(()),
        }
    }

    fn main_work_area(&self) -> Rect {
        *self.work_area.lock().expect("work area poisoned")
    }

    fn display_frame(&self, wid: WindowId) -> Rect {
        if let Some(frame) = self.frame(wid) {
            let (x, y) = frame.center();
            if self.main_frame.contains_point(x, y) {
                return self.main_frame;
            }
            if let Ok(Some(display)) = display::display_at(cosmic_mac_platform::Point::new(x, y)) {
                return from_platform(display.frame);
            }
        }
        self.main_frame
    }
}

/// Current main-display work area.  Call on the main thread.
pub fn main_work_area_now() -> Rect {
    main_display_frames().0
}

/// `(visible_frame, frame)` of the main display.  Call on the main thread.
fn main_display_frames() -> (Rect, Rect) {
    if let Ok(display) = display::main_display() {
        return (
            from_platform(display.visible_frame),
            from_platform(display.frame),
        );
    }
    if let Ok(displays) = display::list_displays()
        && let Some(display) = displays.first()
    {
        return (
            from_platform(display.visible_frame),
            from_platform(display.frame),
        );
    }
    let fallback = Rect::new(0.0, 0.0, 1920.0, 1080.0);
    (fallback, fallback)
}

/// Convert a platform window record into the protocol shape.
pub fn to_toplevel(info: &WindowInfo) -> ToplevelInfo {
    let title = info.title.clone().unwrap_or_default();
    let subrole = info.subrole.clone();
    let picture_in_picture = subrole.as_deref() == Some("AXPictureInPicture")
        || title.to_ascii_lowercase().contains("picture in picture")
        || title.to_ascii_lowercase().contains("picture-in-picture");
    let floating_subrole = matches!(
        subrole.as_deref(),
        Some("AXFloatingWindow" | "AXSystemDialog" | "AXUtilityWindow" | "AXSheet" | "AXDialog")
    );
    ToplevelInfo {
        wid: u64::from(info.wid),
        pid: info.pid,
        app_id: info.bundle_id.clone().unwrap_or_default(),
        title,
        role: info.role.clone(),
        subrole,
        resizable: info.resizable,
        // `ToplevelInfo` has no `is_floating` field; floating panels and PiP
        // are represented as dialogs so the pure layout engine floats them.
        is_dialog: info.is_dialog || info.is_floating || floating_subrole || picture_in_picture,
        initial_frame: Some(from_platform(info.frame)),
    }
}

fn from_platform(rect: PlatformRect) -> Rect {
    Rect::new(
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    )
}

fn to_platform(rect: Rect) -> PlatformRect {
    PlatformRect::new(rect.x, rect.y, rect.width, rect.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_conversion_round_trips() {
        let rect = Rect::new(1.5, 2.5, 300.0, 200.0);
        assert_eq!(from_platform(to_platform(rect)), rect);
    }

    #[test]
    fn pip_and_floating_panels_become_dialogs() {
        let mut info = WindowInfo {
            wid: 1,
            pid: 2,
            bundle_id: Some("com.example.app".into()),
            app_name: None,
            title: Some("Picture in Picture".into()),
            role: Some("AXWindow".into()),
            subrole: None,
            frame: PlatformRect::new(0.0, 0.0, 320.0, 180.0),
            level: 0,
            resizable: true,
            is_dialog: false,
            is_floating: false,
            is_minimized: false,
            ax_backed: true,
        };
        assert!(to_toplevel(&info).is_dialog);

        info.title = Some("Video".into());
        info.subrole = Some("AXFloatingWindow".into());
        assert!(to_toplevel(&info).is_dialog);

        info.subrole = Some("AXPictureInPicture".into());
        assert!(to_toplevel(&info).is_dialog);

        info.subrole = Some("AXStandardWindow".into());
        assert!(!to_toplevel(&info).is_dialog);
    }
}
