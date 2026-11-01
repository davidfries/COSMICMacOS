//! Diagnostic: trace what native full screen and Space changes look like to
//! the platform layer (R11).
//!
//! ```text
//! fs-trace [--seconds N] [--wid W]... [--mouse]
//! ```
//!
//! Prints every [`PlatformEvent`] with a millisecond timestamp and, for events
//! naming a window, what the window server, CoreGraphics and Accessibility
//! report about it at that moment.  The active Space is polled every 50 ms
//! (`SLSGetActiveSpace`) and `NSWorkspaceActiveSpaceDidChangeNotification` is
//! observed, so both can be compared.  Windows passed with `--wid` are probed
//! every 100 ms and printed when their state changes.  `--mouse` also prints
//! the left-button events of a listen-only tap (drag-to-stack timing).
//!
//! Run with `RUST_LOG=cosmic_mac_platform::observers=trace` to also see the
//! raw SkyLight event ids and AX notification names.

use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use block2::RcBlock;
use cosmic_mac_platform::{MouseTap, PlatformEvent, PlatformObserver, skylight, window};
use objc2_app_kit::{NSWorkspace, NSWorkspaceActiveSpaceDidChangeNotification};
use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
use objc2_foundation::NSNotification;

fn start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

fn stamp() -> String {
    format!("{:>7}ms", start().elapsed().as_millis())
}

/// One line describing everything the platform can tell about `wid` now.
fn probe(wid: u32) -> String {
    let cg = match window::cg_window_frame(wid) {
        Some(frame) => format!(
            "cg={:.0},{:.0} {:.0}x{:.0}",
            frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
        ),
        None => "cg=none".to_string(),
    };
    let owner = match skylight::window_owner_pid(wid) {
        Ok(pid) => format!("owner={pid}"),
        Err(error) => format!("owner=err({error})"),
    };
    let level = match skylight::window_level(wid) {
        Ok(level) => format!("level={level}"),
        Err(_) => "level=err".to_string(),
    };
    let spaces = match skylight::window_spaces(wid) {
        Ok(spaces) => {
            let typed: Vec<String> = spaces
                .iter()
                .map(|sid| format!("{sid}/t{}", skylight::space_type(*sid).unwrap_or(-1)))
                .collect();
            format!("spaces=[{}]", typed.join(","))
        }
        Err(error) => format!("spaces=err({error})"),
    };
    let ordered = match skylight::window_is_ordered_in(wid) {
        Ok(ordered) => format!("ordered_in={ordered}"),
        Err(_) => "ordered_in=err".to_string(),
    };
    let ax = match window::ax_window(wid) {
        Ok(ax) => {
            let fullscreen = ax
                .element
                .bool_attribute("AXFullScreen")
                .ok()
                .flatten()
                .map_or("?".to_string(), |value| value.to_string());
            let frame = ax.frame().ok().flatten().map_or("?".to_string(), |frame| {
                format!(
                    "{:.0},{:.0} {:.0}x{:.0}",
                    frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
                )
            });
            format!("ax=ok fs={fullscreen} frame={frame}")
        }
        Err(error) => format!("ax=err({error})"),
    };
    format!("{cg} {owner} {level} {spaces} {ordered} {ax}")
}

fn event_wid(event: &PlatformEvent) -> Option<u32> {
    match event {
        PlatformEvent::WindowCreated { wid, .. }
        | PlatformEvent::WindowDestroyed { wid }
        | PlatformEvent::WindowMoved { wid, .. }
        | PlatformEvent::WindowResized { wid, .. }
        | PlatformEvent::WindowOrdered { wid }
        | PlatformEvent::WindowLevelChanged { wid }
        | PlatformEvent::WindowTitleChanged { wid, .. }
        | PlatformEvent::WindowHidden { wid }
        | PlatformEvent::WindowShown { wid }
        | PlatformEvent::WindowMinimized { wid }
        | PlatformEvent::WindowDeminimized { wid } => Some(*wid),
        PlatformEvent::FocusChanged { wid, .. } => *wid,
        _ => None,
    }
}

fn space_line() -> String {
    match skylight::active_space() {
        Ok(sid) => format!("{sid} (type {})", skylight::space_type(sid).unwrap_or(-1)),
        Err(error) => format!("err({error})"),
    }
}

/// `fs-trace cached-move <wid> <x> <y> <delay-secs>`: look the window's AX element up now,
/// wait, then move it through that element and report what CoreGraphics sees.
fn cached_move(args: &[String]) {
    let wid: u32 = args[0].parse().expect("wid");
    let x: f64 = args[1].parse().expect("x");
    let y: f64 = args[2].parse().expect("y");
    let delay: f64 = args[3].parse().expect("delay");
    let ax = window::ax_window(wid).expect("AX element now");
    println!("{} looked up; {}", stamp(), probe(wid));
    std::thread::sleep(Duration::from_secs_f64(delay));
    println!("{} before move; {}", stamp(), probe(wid));
    let frame = ax.frame().ok().flatten();
    println!("{} cached element frame read: {frame:?}", stamp());
    let result = ax.set_frame(cosmic_mac_platform::Rect::new(x, y, 700.0, 500.0));
    println!("{} set_frame via cached element: {result:?}", stamp());
    std::thread::sleep(Duration::from_millis(300));
    println!("{} after move; {}", stamp(), probe(wid));
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("cached-move") {
        start();
        cached_move(&args[1..]);
        return;
    }
    let seconds = args
        .iter()
        .position(|arg| arg == "--seconds")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(60.0);
    let watched: Vec<u32> = args
        .iter()
        .enumerate()
        .filter(|(_, arg)| *arg == "--wid")
        .filter_map(|(index, _)| args.get(index + 1))
        .filter_map(|value| value.parse().ok())
        .collect();
    start();

    let handler = Arc::new(|event: PlatformEvent| {
        if matches!(
            event,
            PlatformEvent::UnhandledSls { .. } | PlatformEvent::WindowOrdered { .. }
        ) {
            println!("{} {event:?}", stamp());
            return;
        }
        match event_wid(&event) {
            Some(wid) => println!("{} {event:?}\n           -> {}", stamp(), probe(wid)),
            None => println!("{} {event:?}", stamp()),
        }
    });
    let mut observer = PlatformObserver::new(handler);
    if let Err(error) = observer.start_sls() {
        eprintln!("SkyLight observer: {error}");
        std::process::exit(1);
    }
    match observer.refresh_apps() {
        Ok(count) => println!("AX observers for {count} apps"),
        Err(error) => println!("AX observers skipped: {error}"),
    }

    let _mouse = args.iter().any(|arg| arg == "--mouse").then(|| {
        MouseTap::new(Box::new(|event| {
            println!(
                "{} mouse {:?} at {:.0},{:.0}",
                stamp(),
                event.kind,
                event.location.x,
                event.location.y
            );
        }))
        .unwrap_or_else(|error| {
            eprintln!("mouse tap: {error}");
            std::process::exit(1);
        })
    });

    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    let block = RcBlock::new(|_: NonNull<NSNotification>| {
        println!(
            "{} NSWorkspaceActiveSpaceDidChangeNotification; active space {}",
            stamp(),
            space_line()
        );
    });
    // SAFETY: the name is a valid static NSString; the block is 'static and
    // runs on the posting (main) thread.
    let _token = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSWorkspaceActiveSpaceDidChangeNotification),
            None,
            None,
            &block,
        )
    };

    println!("{} start; active space {}", stamp(), space_line());
    let last_probe: Mutex<HashMap<u32, String>> = Mutex::new(HashMap::new());
    for &wid in &watched {
        let line = probe(wid);
        println!("{} watch {wid}: {line}", stamp());
        last_probe.lock().unwrap().insert(wid, line);
    }

    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let mut last_space = skylight::active_space().ok();
    let mut ticks = 0u32;
    let mode = unsafe { kCFRunLoopDefaultMode };
    while Instant::now() < deadline {
        CFRunLoop::run_in_mode(mode, 0.05, false);
        ticks = ticks.wrapping_add(1);
        let space = skylight::active_space().ok();
        if space != last_space {
            println!("{} polled active space change -> {}", stamp(), space_line());
            last_space = space;
        }
        if ticks.is_multiple_of(2) {
            for &wid in &watched {
                let line = probe(wid);
                let mut last = last_probe.lock().unwrap();
                if last.get(&wid) != Some(&line) {
                    println!("{} watch {wid}: {line}", stamp());
                    last.insert(wid, line);
                }
            }
        }
        if ticks.is_multiple_of(40) {
            let _ = observer.refresh_apps();
        }
    }
    println!("{} done", stamp());
}
