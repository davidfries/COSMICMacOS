//! Spike C: an own overlay window ordered directly above a foreign window.
//!
//! Three modes:
//!
//! ```text
//! spike-c                         own raw SkyLight target window (default)
//! spike-c --target <wid>          track a real app window by window ID
//! spike-c --appkit                create an AppKit NSWindow and order above it
//! ```
//!
//! The overlay is a raw SkyLight window (`SLSNewWindow`) drawn with Core
//! Graphics and ordered with `SLSTransactionOrderWindow` relative to the target,
//! copying the target's level and sub-level (the JankyBorders technique). This
//! is the milestone-1 stack tab-bar mechanism. `--appkit` answers the
//! milestone-2 question: can an `NSWindow`'s `windowNumber` be used as the
//! relative window in the same transaction?
//!
//! All modes work with SIP on and without Accessibility; `--target` reads the
//! target frame through SkyLight only.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cosmic_mac_platform::coords;
use cosmic_mac_platform::{Color, OverlayWindow, PlatformObserver, Point, Rect, display, window};
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowStyleMask,
};
use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        help();
        return;
    }
    let capture = args
        .iter()
        .position(|arg| arg == "--capture")
        .and_then(|index| args.get(index + 1))
        .cloned();
    let seconds = args
        .iter()
        .position(|arg| arg == "--seconds")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(15.0);
    if let Some(index) = args.iter().position(|arg| arg == "--target") {
        let Some(wid) = args.get(index + 1).and_then(|value| value.parse().ok()) else {
            eprintln!("--target needs a window id");
            std::process::exit(64);
        };
        track_foreign(wid, seconds, capture.as_deref());
    } else if args.iter().any(|arg| arg == "--appkit") {
        appkit(seconds, capture.as_deref());
    } else {
        own(seconds, capture.as_deref());
    }
}

fn help() {
    println!(
        "spike-c: overlay window ordered above a target\n\
         \n\
         USAGE: spike-c [--own] [--seconds N] [--capture PREFIX]\n\
                spike-c --target <wid> [--seconds N]\n\
                spike-c --appkit [--seconds N]\n\
         \n\
         The overlay is a raw SkyLight window drawn with Core Graphics and kept\n\
         above the target with SLSTransactionOrderWindow. No Accessibility needed.\n"
    );
}

fn expanded(frame: Rect, amount: f64) -> Rect {
    Rect::new(
        frame.origin.x - amount,
        frame.origin.y - amount,
        frame.size.width + 2.0 * amount,
        frame.size.height + 2.0 * amount,
    )
}

fn describe(name: &str, overlay: &OverlayWindow) {
    println!(
        "{name}: wid={} frame=({:.0},{:.0} {:.0}x{:.0})",
        overlay.wid(),
        overlay.frame().origin.x,
        overlay.frame().origin.y,
        overlay.frame().size.width,
        overlay.frame().size.height
    );
}

fn capture(prefix: Option<&str>, items: &[(&str, u32)]) {
    let Some(prefix) = prefix else { return };
    for (name, wid) in items {
        let path = format!("{prefix}-{name}.png");
        match cosmic_mac_platform::overlay::capture_window_png(*wid, std::path::Path::new(&path)) {
            Ok(()) => println!("captured {name} window to {path}"),
            Err(err) => eprintln!("!! capture {name}: {err}"),
        }
    }
}

fn own(seconds: f64, capture_prefix: Option<&str>) {
    println!("== spike-c: own raw SkyLight target + overlay ==");
    let display = match display::main_display() {
        Ok(display) => display,
        Err(err) => {
            eprintln!("!! no display: {err}");
            std::process::exit(1);
        }
    };
    let work = display.visible_frame;
    let width = 520.0f64.min(work.size.width * 0.5);
    let height = 340.0f64.min(work.size.height * 0.5);
    let start = Point::new(work.origin.x + 80.0, work.origin.y + 80.0);

    let mut target = match OverlayWindow::new(Rect::new(start.x, start.y, width, height), true) {
        Ok(window) => window,
        Err(err) => {
            eprintln!("!! cannot create the raw SLS target window: {err}");
            std::process::exit(1);
        }
    };
    let _ = target.fill(Color::rgba(0.12, 0.13, 0.16, 1.0));
    if let Err(err) = target.move_to_active_space() {
        eprintln!("!! target could not join the active Space: {err}");
    }
    describe("target", &target);

    let mut overlay =
        match OverlayWindow::new_above(target.wid(), expanded(target.frame(), 4.0), true) {
            Ok(overlay) => overlay,
            Err(err) => {
                eprintln!("!! cannot create the overlay: {err}");
                std::process::exit(1);
            }
        };
    describe("overlay", &overlay);
    if let Err(err) = overlay.draw_border(Color::accent(), 4.0) {
        eprintln!("!! draw_border: {err}");
    }

    capture(
        capture_prefix,
        &[("target", target.wid()), ("overlay", overlay.wid())],
    );
    println!(
        "target wid={} overlay wid={} ordered above; moving for {seconds:.0}s",
        target.wid(),
        overlay.wid()
    );
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let mut dx = 6.0;
    let mut dy = 3.0;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(700));
        let mut frame = target.frame();
        frame.origin.x += dx;
        frame.origin.y += dy;
        if frame.origin.x < work.origin.x + 20.0 || frame.max_x() > work.max_x() - 20.0 {
            dx = -dx;
            frame.origin.x += dx;
        }
        if frame.origin.y < work.origin.y + 20.0 || frame.max_y() > work.max_y() - 20.0 {
            dy = -dy;
            frame.origin.y += dy;
        }
        if let Err(err) = target.set_frame(frame) {
            eprintln!("target set_frame: {err}");
            break;
        }
        let overlay_frame = expanded(frame, 4.0);
        if let Err(err) = overlay.set_frame(overlay_frame) {
            eprintln!("overlay set_frame: {err}");
            break;
        }
        let _ = overlay.order_above(target.wid());
        let _ = overlay.draw_border(Color::accent(), 4.0);
        println!(
            "moved target to ({:.0},{:.0}) overlay wid={}",
            frame.origin.x,
            frame.origin.y,
            overlay.wid()
        );
    }
    println!("spike-c own mode done (windows released)");
}

fn track_foreign(wid: u32, seconds: f64, capture_prefix: Option<&str>) {
    println!("== spike-c: track foreign window wid {wid} ==");
    let frame = match cosmic_mac_platform::skylight::window_bounds(wid) {
        Ok(frame) => Rect::from_cg(frame),
        Err(err) => {
            eprintln!("!! cannot read window {wid}: {err}");
            std::process::exit(1);
        }
    };
    println!(
        "target frame=({:.0},{:.0} {:.0}x{:.0})",
        frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
    );
    let mut overlay = match OverlayWindow::new_above(wid, expanded(frame, 4.0), true) {
        Ok(overlay) => overlay,
        Err(err) => {
            eprintln!("!! cannot create overlay: {err}");
            std::process::exit(1);
        }
    };
    describe("overlay", &overlay);
    let _ = overlay.draw_border(Color::rgba(1.0, 0.35, 0.25, 1.0), 4.0);
    capture(
        capture_prefix,
        &[("overlay", overlay.wid()), ("target", wid)],
    );

    let handler = {
        let overlay_wid = overlay.wid();
        Arc::new(move |event| match event {
            cosmic_mac_platform::PlatformEvent::WindowMoved { wid: moved, frame }
            | cosmic_mac_platform::PlatformEvent::WindowResized { wid: moved, frame }
                if moved == wid =>
            {
                println!(
                    "target event: ({:.0},{:.0} {:.0}x{:.0})",
                    frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
                );
                // The observer callback runs on this thread's run loop. Recreate
                // the overlay geometry through a fresh window handle is not
                // possible here (the overlay is not Send), so the tracking loop
                // below polls the frame instead. This handler only logs.
                let _ = overlay_wid;
            }
            _ => {}
        })
    };
    let mut observer = PlatformObserver::new(handler);
    if let Err(err) = observer.start_sls() {
        eprintln!("!! SkyLight observer: {err}");
    }

    println!("tracking for {seconds:.0}s; move/resize the target now");
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    while Instant::now() < deadline {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.1, false);
        if let Ok(frame) = cosmic_mac_platform::skylight::window_bounds(wid) {
            let frame = Rect::from_cg(frame);
            let _ = overlay.set_frame(expanded(frame, 4.0));
            let _ = overlay.order_above(wid);
        }
    }
    println!("spike-c foreign mode done");
}

fn appkit(seconds: f64, capture_prefix: Option<&str>) {
    println!("== spike-c: AppKit NSWindow + overlay by windowNumber ==");
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("!! must run on the main thread");
        std::process::exit(1);
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();

    let rect = NSRect::new(NSPoint::new(320.0, 320.0), NSSize::new(560.0, 360.0));
    let style =
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Resizable;
    // SAFETY: main thread, valid rect/style.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str("cosmicMac Spike C — AppKit target"));
    window.makeKeyAndOrderFront(None);
    app.activate();
    let wid = window.windowNumber() as u32;
    println!("AppKit windowNumber = {wid}");

    let anchor = display::main_display_height();
    let frame = coords::ns_to_ax(Rect::from_cg(window.frame()), anchor);
    let mut overlay = match OverlayWindow::new_above(wid, expanded(frame, 4.0), true) {
        Ok(overlay) => overlay,
        Err(err) => {
            eprintln!("!! cannot create overlay above NSWindow: {err}");
            std::process::exit(1);
        }
    };
    describe("overlay", &overlay);
    let _ = overlay.draw_border(Color::rgba(0.4, 1.0, 0.5, 1.0), 4.0);
    capture(
        capture_prefix,
        &[("appkit", wid), ("overlay", overlay.wid())],
    );

    println!("moving the NSWindow for {seconds:.0}s; the overlay should follow");
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let mut offset = 0.0f64;
    let mut last = Instant::now();
    while Instant::now() < deadline {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.1, false);
        if last.elapsed() >= Duration::from_millis(700) {
            last = Instant::now();
            offset += 8.0;
            if offset > 220.0 {
                offset = 0.0;
            }
            let mut ns_rect = window.frame();
            ns_rect.origin.x = 320.0 + offset;
            window.setFrameOrigin(ns_rect.origin);
            let ax = coords::ns_to_ax(Rect::from_cg(window.frame()), anchor);
            let _ = overlay.set_frame(expanded(ax, 4.0));
            let _ = overlay.order_above(wid);
            let _ = overlay.draw_border(Color::rgba(0.4, 1.0, 0.5, 1.0), 4.0);
            println!("NSWindow moved to x={:.0}", ns_rect.origin.x);
        }
    }
    println!("spike-c appkit mode done");
    let _ = window::raise(wid);
}
