//! Spike B: exercise every platform primitive the WM relies on.
//!
//! Subcommands that do **not** need Accessibility (useful over SSH before the
//! GUI grant exists):
//!
//! ```text
//! spike-b selftest            SkyLight, displays, window list, park store
//! spike-b list                enumerate on-screen windows
//! spike-b watch [--seconds N] SkyLight + AX events
//! spike-b raise <wid>         SkyLight order-above
//! spike-b focus <wid>         focus-without-raise (SLS/CPS)
//! ```
//!
//! Subcommands that need Accessibility:
//!
//! ```text
//! spike-b move <wid> <x> <y> <w> <h>
//! spike-b park <wid> [--seconds N] [--display N]
//! spike-b tap [--seconds N] [--post]
//! spike-b ax-list
//! ```
//!
//! They print an explicit "needs Accessibility" message when the grant is
//! missing instead of failing with an opaque AX error.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cosmic_mac_platform::{
    DisplayInfo, EventTap, HotkeySpec, ParkStore, PlatformEvent, PlatformObserver, Rect, display,
    eventtap, tcc, window,
};
use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(index) = args.iter().position(|arg| arg == "--probe-window") {
        let Some(wid) = args
            .get(index + 1)
            .and_then(|value| value.parse::<u32>().ok())
        else {
            eprintln!("usage: spike-b --probe-window <wid>");
            std::process::exit(64);
        };
        probe_window(wid);
        return;
    }
    let command = args.first().map(String::as_str).unwrap_or("help");

    match command {
        "help" | "--help" | "-h" => help(),
        "selftest" => selftest(),
        "list" => list_windows(),
        "ax-list" => ax_list(),
        "move" => move_window(&args[1..]),
        "probe" => {
            let wid = parse_wid(&args[1..]);
            probe_window(wid);
        }
        "raise" => with_wid(&args[1..], window::raise),
        "focus" => with_wid(&args[1..], window::focus_without_raise),
        "park" => park(&args[1..]),
        "watch" => watch(&args[1..]),
        "tap" => tap(&args[1..]),
        other => {
            eprintln!("unknown subcommand {other:?}\n");
            help();
            std::process::exit(64);
        }
    }
}

fn help() {
    println!(
        "spike-b: cosmicMac platform spike\n\
         \n\
         USAGE: spike-b <subcommand> [args]\n\
         \n\
         no Accessibility needed:\n\
           selftest                 SkyLight/display/window sanity checks\n\
           list                     enumerate on-screen windows\n\
           raise <wid>              order a window above its peers\n\
           focus <wid>              focus without raising (CPS event records)\n\
           watch [--seconds N]      print SkyLight + AX events\n\
         \n\
         needs Accessibility:\n\
           ax-list                  enumerate through AX only\n\
           move <wid> <x> <y> <w> <h>\n\
           probe <wid>              discover min/max size (momentarily shrinks and grows it)\n\
           --probe-window <wid>     same, as a flag\n\
           park <wid> [--seconds N] [--display N]\n\
           tap [--seconds N] [--post]\n\
         \n\
         Examples:\n\
           spike-b list\n\
           spike-b move 1234 100 100 800 600\n\
           spike-b park 1234 --seconds 10\n\
           spike-b tap --seconds 15\n"
    );
}

fn print_error(action: &str, err: &cosmic_mac_platform::PlatformError) {
    if err.is_permission_denied() {
        eprintln!(
            "!! {action}: needs Accessibility. Grant it in System Settings > Privacy & Security \
             > Accessibility for this binary, then rerun. ({err})"
        );
    } else {
        eprintln!("!! {action}: {err}");
    }
}

fn selftest() {
    println!("== cosmicMac spike-b selftest ==");
    let status = tcc::status();
    print!("{}", status.report());

    match cosmic_mac_platform::skylight::main_connection() {
        Ok(cid) => println!("SkyLight connection: {cid}"),
        Err(err) => {
            eprintln!("!! SkyLight: {err}");
            std::process::exit(1);
        }
    }

    match display::list_displays() {
        Ok(displays) => {
            println!("displays: {}", displays.len());
            for display in &displays {
                print_display(display);
            }
        }
        Err(err) => print_error("list_displays", &err),
    }

    match window::list_windows() {
        Ok(windows) => {
            println!(
                "on-screen windows: {} (AX-backed: {})",
                windows.len(),
                windows.iter().filter(|window| window.ax_backed).count()
            );
            for window in windows.iter().take(8) {
                print_window(window);
            }
        }
        Err(err) => print_error("list_windows", &err),
    }

    let store = ParkStore::new();
    println!("park store: empty={} len={}", store.is_empty(), store.len());

    // Creating a tap is the strongest "everything is wired" check we can do
    // without a GUI grant; failure is expected and reported, not fatal.
    match EventTap::new(Vec::new(), Box::new(|_| false)) {
        Ok(tap) => {
            println!("CGEventTap: created, enabled={}", tap.is_enabled());
        }
        Err(err) => {
            println!("CGEventTap: unavailable ({err})");
        }
    }

    println!("selftest: OK (see permission lines above for what is gated)");
}

fn list_windows() {
    match window::list_windows() {
        Ok(windows) => {
            println!(
                "{} on-screen windows (AX trusted: {})",
                windows.len(),
                tcc::accessibility_trusted()
            );
            for window in windows {
                print_window(&window);
            }
        }
        Err(err) => {
            print_error("list", &err);
            std::process::exit(1);
        }
    }
}

fn ax_list() {
    if !tcc::accessibility_trusted() {
        eprintln!("!! ax-list: needs Accessibility (AXIsProcessTrusted() == false)");
        std::process::exit(3);
    }
    let windows = match window::list_windows() {
        Ok(windows) => windows,
        Err(err) => {
            print_error("list", &err);
            std::process::exit(1);
        }
    };
    let mut pids: Vec<i32> = windows.iter().map(|window| window.pid).collect();
    pids.sort_unstable();
    pids.dedup();
    for pid in pids {
        match window::list_ax_windows(pid) {
            Ok(ax_windows) => {
                println!("pid {pid}: {} AX windows", ax_windows.len());
                for window in ax_windows {
                    print_window(&window);
                }
            }
            Err(err) => print_error(&format!("ax-list pid {pid}"), &err),
        }
    }
}

fn print_window(window: &cosmic_mac_platform::WindowInfo) {
    println!(
        "  wid={:<8} pid={:<6} app={:<22} role={:<10} sub={:<18} resizable={:<5} dialog={:<5} \
         frame=({:.0},{:.0} {:.0}x{:.0}) ax={} title={:?}",
        window.wid,
        window.pid,
        window.app_name.as_deref().unwrap_or("?"),
        window.role.as_deref().unwrap_or("-"),
        window.subrole.as_deref().unwrap_or("-"),
        window.resizable,
        window.is_dialog,
        window.frame.origin.x,
        window.frame.origin.y,
        window.frame.size.width,
        window.frame.size.height,
        window.ax_backed,
        window.title.as_deref().unwrap_or(""),
    );
}

fn print_display(display: &DisplayInfo) {
    println!(
        "  display id={} main={} scale={:.1} frame=({:.0},{:.0} {:.0}x{:.0}) \
         visible=({:.0},{:.0} {:.0}x{:.0}) name={}",
        display.id,
        display.is_main,
        display.scale,
        display.frame.origin.x,
        display.frame.origin.y,
        display.frame.size.width,
        display.frame.size.height,
        display.visible_frame.origin.x,
        display.visible_frame.origin.y,
        display.visible_frame.size.width,
        display.visible_frame.size.height,
        display.name.as_deref().unwrap_or("?"),
    );
}

fn parse_wid(args: &[String]) -> u32 {
    let Some(raw) = args.first() else {
        eprintln!("missing <wid>");
        std::process::exit(64);
    };
    match raw.parse() {
        Ok(wid) => wid,
        Err(_) => {
            eprintln!("invalid wid {raw:?}");
            std::process::exit(64);
        }
    }
}

fn with_wid(args: &[String], op: fn(u32) -> cosmic_mac_platform::Result<()>) {
    let wid = parse_wid(args);
    match op(wid) {
        Ok(()) => println!("ok: wid {wid}"),
        Err(err) => {
            print_error("operation", &err);
            std::process::exit(1);
        }
    }
}

fn move_window(args: &[String]) {
    if args.len() < 5 {
        eprintln!("usage: spike-b move <wid> <x> <y> <w> <h>");
        std::process::exit(64);
    }
    let wid: u32 = args[0].parse().expect("wid");
    let values: Vec<f64> = args[1..5]
        .iter()
        .map(|value| value.parse().expect("number"))
        .collect();
    let target = Rect::new(values[0], values[1], values[2], values[3]);
    if !tcc::accessibility_trusted() {
        eprintln!(
            "!! move: needs Accessibility. This is expected before the GUI grant; \
             `spike-b list` and `spike-b watch` still work."
        );
        std::process::exit(3);
    }
    let before = window::window_info(wid).ok().flatten();
    if let Some(info) = &before {
        println!(
            "before: ({:.0},{:.0} {:.0}x{:.0})",
            info.frame.origin.x, info.frame.origin.y, info.frame.size.width, info.frame.size.height
        );
    }
    match window::set_frame(wid, target) {
        Ok(applied) => {
            if applied.clamped {
                println!(
                    "clamped: width={:?} height={:?} position={:?}",
                    applied.width, applied.height, applied.position
                );
            }
            let after = window::window_info(wid).ok().flatten();
            if let Some(info) = after {
                println!(
                    "after:  ({:.0},{:.0} {:.0}x{:.0})",
                    info.frame.origin.x,
                    info.frame.origin.y,
                    info.frame.size.width,
                    info.frame.size.height
                );
            }
            println!(
                "ok: moved wid {wid} to {target:?} (applied {:?})",
                applied.applied
            );
        }
        Err(err) => {
            print_error("move", &err);
            std::process::exit(1);
        }
    }
}

/// Discover a window's size constraints.  This *momentarily resizes the window*
/// (a 1x1 shrink followed by a small grow, roughly 100-200 ms) and restores its
/// frame afterwards.
fn probe_window(wid: u32) {
    if !tcc::accessibility_trusted() {
        eprintln!("!! probe: needs Accessibility (AXPosition/AXSize are AX attributes)");
        std::process::exit(3);
    }
    let before = window::window_info(wid)
        .ok()
        .flatten()
        .map(|info| info.frame);
    if let Some(frame) = before {
        println!(
            "before: ({:.0},{:.0} {:.0}x{:.0})",
            frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
        );
    }
    match window::probe_size_constraints(wid) {
        Ok(Some(constraints)) => {
            println!(
                "min: {:?} max: {:?} width: {:?} height: {:?}",
                constraints.min, constraints.max, constraints.width, constraints.height
            );
        }
        Ok(None) => println!("no constraints (probe returned none)"),
        Err(err) => {
            print_error("probe", &err);
            std::process::exit(1);
        }
    }
    if let Some(after) = window::window_info(wid)
        .ok()
        .flatten()
        .map(|info| info.frame)
    {
        println!(
            "after:  ({:.0},{:.0} {:.0}x{:.0})",
            after.origin.x, after.origin.y, after.size.width, after.size.height
        );
        if let Some(before) = before
            && ((after.origin.x - before.origin.x).abs() > 1.0
                || (after.origin.y - before.origin.y).abs() > 1.0
                || (after.size.width - before.size.width).abs() > 1.0
                || (after.size.height - before.size.height).abs() > 1.0)
        {
            println!("warning: frame was not restored exactly");
        }
    }
}

fn park(args: &[String]) {
    let wid = parse_wid(args);
    let mut seconds = 5.0f64;
    let mut display_index = 0usize;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--seconds" => {
                seconds = args
                    .get(index + 1)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(5.0);
                index += 2;
            }
            "--display" => {
                display_index = args
                    .get(index + 1)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                index += 2;
            }
            other => {
                eprintln!("unknown option {other:?}");
                std::process::exit(64);
            }
        }
    }
    if !tcc::accessibility_trusted() {
        eprintln!("!! park: needs Accessibility (AXPosition/AXSize are AX attributes)");
        std::process::exit(3);
    }
    let displays = match display::list_displays() {
        Ok(displays) => displays,
        Err(err) => {
            print_error("list_displays", &err);
            std::process::exit(1);
        }
    };
    let Some(display) = displays.get(display_index) else {
        eprintln!("no display at index {display_index}");
        std::process::exit(64);
    };
    let store = ParkStore::new();
    match store.park(wid, display.frame) {
        Ok(saved) => println!("parked wid {wid}; saved frame {saved:?}"),
        Err(err) => {
            print_error("park", &err);
            std::process::exit(1);
        }
    }
    println!("holding for {seconds:.0}s (window should sit at the bottom-right corner)");
    std::thread::sleep(Duration::from_secs_f64(seconds));
    match store.unpark(wid) {
        Ok(true) => println!("unparked wid {wid}: restored saved frame"),
        Ok(false) => println!("nothing to restore for wid {wid}"),
        Err(err) => {
            print_error("unpark", &err);
            std::process::exit(1);
        }
    }
}

fn run_loop_for(seconds: f64) {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    // SAFETY: kCFRunLoopDefaultMode is an immutable CFRunLoopMode static.
    let mode = unsafe { kCFRunLoopDefaultMode };
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let slice = remaining.as_secs_f64().min(0.25);
        CFRunLoop::run_in_mode(mode, slice, false);
    }
}

fn watch(args: &[String]) {
    let seconds = option_seconds(args).unwrap_or(10.0);
    let handler = Arc::new(|event: PlatformEvent| println!("{event:?}"));
    let mut observer = PlatformObserver::new(handler);
    if let Err(err) = observer.start_sls() {
        print_error("start SkyLight observer", &err);
        std::process::exit(1);
    }
    println!("SkyLight observer installed");
    match observer.refresh_apps() {
        Ok(count) => println!("AX observers installed for {count} apps"),
        Err(err) => println!("AX observers skipped: {err}"),
    }
    println!("watching for {seconds:.0}s (move/resize/close a window now)");
    run_loop_for(seconds);
    println!("watch done");
}

fn option_seconds(args: &[String]) -> Option<f64> {
    args.iter()
        .position(|arg| arg == "--seconds")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse().ok())
}

fn tap(args: &[String]) {
    let seconds = option_seconds(args).unwrap_or(10.0);
    let post = args.iter().any(|arg| arg == "--post");
    if !tcc::accessibility_trusted() {
        eprintln!(
            "!! tap: needs Accessibility (and possibly Input Monitoring). \
             The tap will not receive key events without it."
        );
        std::process::exit(3);
    }
    let handler = Box::new(|key: cosmic_mac_platform::KeyEvent| {
        println!(
            "key {} flags=0x{:x} down={} modifiers=0x{:x}",
            key.keycode,
            key.flags,
            key.is_key_down,
            key.modifiers()
        );
        false
    });
    let swallow = vec![HotkeySpec::new(
        16, // kVK_ANSI_Y
        HotkeySpec::MASK_COMMAND | HotkeySpec::MASK_SHIFT,
    )];
    let tap = match EventTap::new(swallow, handler) {
        Ok(tap) => tap,
        Err(err) => {
            print_error("CGEventTapCreate", &err);
            std::process::exit(1);
        }
    };
    println!(
        "event tap installed; swallowing Cmd+Shift+Y, passing everything else (Cmd+C included)"
    );
    println!("{}", eventtap::SECURE_INPUT_FALLBACK_NOTE);

    if post {
        std::thread::sleep(Duration::from_millis(300));
        println!("posting synthetic Cmd+Shift+Y (should be swallowed)...");
        let flags = HotkeySpec::MASK_COMMAND | HotkeySpec::MASK_SHIFT;
        let _ = cosmic_mac_platform::overlay::post_key(16, flags, true);
        let _ = cosmic_mac_platform::overlay::post_key(16, flags, false);
        std::thread::sleep(Duration::from_millis(300));
        println!("posting synthetic Cmd+C (should pass through)...");
        let flags = HotkeySpec::MASK_COMMAND;
        let _ = cosmic_mac_platform::overlay::post_key(8, flags, true);
        let _ = cosmic_mac_platform::overlay::post_key(8, flags, false);
    }

    println!("listening for {seconds:.0}s");
    run_loop_for(seconds);
    println!("tap done (enabled={})", tap.is_enabled());
}
