// SPDX-License-Identifier: GPL-3.0-or-later
//! Post one keyboard chord as a synthetic HID event, spelled like a COSMIC
//! binding, so the daemon's event tap can be exercised from a script.
//!
//! ```sh
//! send-key Super+Shift+y        # post Cmd+Shift+Y (down, then up)
//! send-key --dry-run Super+Left # resolve the chord without posting it
//! send-key --list               # print the armed default bindings
//! send-key --tap-super          # press and release Command alone (launcher)
//! send-key --tap-super --hold-ms 600   # too long for a tap: nothing happens
//! send-key --drag 400,35 1300,500      # drag with the left button (title bar)
//! send-key --drag 400,35 1300,500 --steps 30 --step-ms 20 --hold-ms 3000
//! ```
//!
//! `Super` is Command on macOS (`Cmd`/`Command` are accepted spellings).  The
//! chord is resolved against the embedded defaults plus `custom.ron` and the
//! resulting action is printed, so a typo is visible before the key is sent.
//! Posting needs the Accessibility grant of the signed binary; the daemon's
//! tap sees the event like a real key press and swallows it when bound.

use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use cosmic_mac_platform::eventtap::{post_flags_changed, post_left_mouse};
use cosmic_mac_platform::overlay::post_key;
use cosmic_mac_platform::{HotkeySpec, MouseEventKind, Point};
use cosmic_mac_wm::keybindings::{Binding, KeyMap, Modifier};
use cosmic_mac_wm::paths;
use cosmic_mac_wm::super_tap::KEYCODE_COMMAND;

const USAGE: &str = "\
usage: send-key [--dry-run] <chord>
       send-key --tap-super [--hold-ms N]
       send-key --drag X1,Y1 X2,Y2 [--steps N] [--step-ms M] [--hold-ms H]
       send-key --list

  <chord>      modifiers and a key joined by '+', e.g. Super+Shift+y, Cmd+Left
  --dry-run    resolve the chord and print the action without posting
  --tap-super  press and release the left Command key alone (the daemon's
               Super tap toggles the launcher); held --hold-ms N (default 80)
  --drag       press the left button at X1,Y1 (global top-left points), move
               to X2,Y2 in --steps N (default 20) dragged events --step-ms M
               apart (default 16), wait --hold-ms H (default 100), release;
               prints a millisecond timestamp for each phase
  --list       print every armed binding with its keycode and modifier mask";

/// Default hold of `--tap-super`.
const TAP_HOLD: Duration = Duration::from_millis(80);

/// Delay between the key-down and key-up events, and after the chord.
const KEY_GAP: Duration = Duration::from_millis(30);

/// Default hold of `--drag` at the end point before the release.
const DRAG_HOLD: Duration = Duration::from_millis(100);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dry_run = false;
    let mut chord = None;
    let mut tap_super = false;
    let mut hold = None;
    let mut drag = None;
    let mut steps = 20u32;
    let mut step_ms = 16u64;
    let mut args_iter = args.iter();
    while let Some(arg) = args_iter.next() {
        match arg.as_str() {
            "--list" => return list(),
            "--dry-run" => dry_run = true,
            "--tap-super" => tap_super = true,
            "--hold-ms" => match args_iter.next().map(|value| value.parse::<u64>()) {
                Some(Ok(ms)) => hold = Some(Duration::from_millis(ms)),
                _ => {
                    eprintln!("--hold-ms needs a number of milliseconds\n{USAGE}");
                    return ExitCode::from(64);
                }
            },
            "--drag" => {
                let from = args_iter.next().and_then(|value| parse_point(value));
                let to = args_iter.next().and_then(|value| parse_point(value));
                match (from, to) {
                    (Some(from), Some(to)) => drag = Some((from, to)),
                    _ => {
                        eprintln!("--drag needs two points X1,Y1 X2,Y2\n{USAGE}");
                        return ExitCode::from(64);
                    }
                }
            }
            "--steps" => match args_iter.next().map(|value| value.parse::<u32>()) {
                Some(Ok(value)) if value > 0 => steps = value,
                _ => {
                    eprintln!("--steps needs a positive number\n{USAGE}");
                    return ExitCode::from(64);
                }
            },
            "--step-ms" => match args_iter.next().map(|value| value.parse::<u64>()) {
                Some(Ok(value)) => step_ms = value,
                _ => {
                    eprintln!("--step-ms needs a number of milliseconds\n{USAGE}");
                    return ExitCode::from(64);
                }
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("unknown option {other:?}\n{USAGE}");
                return ExitCode::from(64);
            }
            other => {
                if chord.replace(other.to_string()).is_some() {
                    eprintln!("exactly one chord is expected\n{USAGE}");
                    return ExitCode::from(64);
                }
            }
        }
    }
    if let Some((from, to)) = drag {
        if chord.is_some() || tap_super {
            eprintln!("--drag takes no chord\n{USAGE}");
            return ExitCode::from(64);
        }
        let step = Duration::from_millis(step_ms);
        return post_drag(from, to, steps, step, hold.unwrap_or(DRAG_HOLD), dry_run);
    }
    if tap_super {
        if chord.is_some() {
            eprintln!("--tap-super takes no chord\n{USAGE}");
            return ExitCode::from(64);
        }
        return post_super_tap(hold.unwrap_or(TAP_HOLD), dry_run);
    }
    let Some(chord) = chord else {
        eprintln!("{USAGE}");
        return ExitCode::from(64);
    };

    let binding = match parse_chord(&chord) {
        Ok(binding) => binding,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(64);
        }
    };
    let Some(hotkey) = binding.hotkey() else {
        eprintln!("no macOS keycode is known for key {:?}", binding.key);
        return ExitCode::from(64);
    };

    let (keymap, warnings) = KeyMap::load(Some(&paths::keybindings_path()));
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    match keymap.lookup(hotkey.keycode, hotkey.modifiers) {
        Some(action) => println!(
            "{} -> keycode {} flags {:#x} -> {action:?}",
            binding.display(),
            hotkey.keycode,
            hotkey.modifiers
        ),
        None => println!(
            "{} -> keycode {} flags {:#x} -> (not bound; the daemon passes it through)",
            binding.display(),
            hotkey.keycode,
            hotkey.modifiers
        ),
    }
    if dry_run {
        return ExitCode::SUCCESS;
    }

    if let Err(error) = post_key(hotkey.keycode, hotkey.modifiers, true) {
        eprintln!("key down failed: {error}");
        return ExitCode::FAILURE;
    }
    thread::sleep(KEY_GAP);
    if let Err(error) = post_key(hotkey.keycode, hotkey.modifiers, false) {
        eprintln!("key up failed: {error}");
        return ExitCode::FAILURE;
    }
    thread::sleep(KEY_GAP);
    println!("posted");
    ExitCode::SUCCESS
}

/// Posts Command down, waits `hold`, posts Command up (modifier changes,
/// like the real key).
fn post_super_tap(hold: Duration, dry_run: bool) -> ExitCode {
    println!(
        "Super tap: left Command (keycode {KEYCODE_COMMAND}) held {} ms",
        hold.as_millis()
    );
    if dry_run {
        return ExitCode::SUCCESS;
    }
    if let Err(error) = post_flags_changed(KEYCODE_COMMAND, HotkeySpec::MASK_COMMAND) {
        eprintln!("Command down failed: {error}");
        return ExitCode::FAILURE;
    }
    thread::sleep(hold);
    if let Err(error) = post_flags_changed(KEYCODE_COMMAND, 0) {
        eprintln!("Command up failed: {error}");
        return ExitCode::FAILURE;
    }
    thread::sleep(KEY_GAP);
    println!("posted");
    ExitCode::SUCCESS
}

/// Posts a left-button drag from `from` to `to`: down, `steps` dragged
/// events `step` apart, a `hold` at the end point, up.
fn post_drag(
    from: Point,
    to: Point,
    steps: u32,
    step: Duration,
    hold: Duration,
    dry_run: bool,
) -> ExitCode {
    println!(
        "drag {:.0},{:.0} -> {:.0},{:.0} in {steps} steps of {} ms, hold {} ms",
        from.x,
        from.y,
        to.x,
        to.y,
        step.as_millis(),
        hold.as_millis()
    );
    if dry_run {
        return ExitCode::SUCCESS;
    }
    let start = std::time::Instant::now();
    let stamp = |what: &str| println!("{:>6}ms {what}", start.elapsed().as_millis());
    let post = |kind, point: Point| {
        post_left_mouse(kind, point).map_err(|error| eprintln!("{kind:?} failed: {error}"))
    };
    if post(MouseEventKind::Down, from).is_err() {
        return ExitCode::FAILURE;
    }
    stamp("down");
    // Give the window server a moment to start tracking, like a real press.
    thread::sleep(Duration::from_millis(60));
    for index in 1..=steps {
        let t = f64::from(index) / f64::from(steps);
        let point = Point::new(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
        if post(MouseEventKind::Dragged, point).is_err() {
            let _ = post(MouseEventKind::Up, point);
            return ExitCode::FAILURE;
        }
        thread::sleep(step);
    }
    stamp("last dragged");
    thread::sleep(hold);
    if post(MouseEventKind::Up, to).is_err() {
        return ExitCode::FAILURE;
    }
    stamp("up");
    thread::sleep(KEY_GAP);
    println!("posted");
    ExitCode::SUCCESS
}

/// `X,Y` in points.
fn parse_point(value: &str) -> Option<Point> {
    let (x, y) = value.split_once(',')?;
    Some(Point::new(x.trim().parse().ok()?, y.trim().parse().ok()?))
}

fn list() -> ExitCode {
    let (keymap, warnings) = KeyMap::load(Some(&paths::keybindings_path()));
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    for (binding, action) in keymap.table() {
        match binding.hotkey() {
            Some(hotkey) => println!(
                "{:<20} keycode {:>3} flags {:#x}  {action:?}",
                binding.display(),
                hotkey.keycode,
                hotkey.modifiers
            ),
            None => println!("{:<20} (no keycode)  {action:?}", binding.display()),
        }
    }
    ExitCode::SUCCESS
}

/// Parse `Super+Shift+y` into a normalised binding.
fn parse_chord(chord: &str) -> Result<Binding, String> {
    let parts: Vec<&str> = chord.split('+').collect();
    let Some((key, modifiers)) = parts.split_last() else {
        return Err(format!("empty chord {chord:?}"));
    };
    if key.is_empty() {
        return Err(format!("chord {chord:?} has no key"));
    }
    let mut parsed = Vec::with_capacity(modifiers.len());
    for modifier in modifiers {
        parsed.push(match modifier.to_ascii_lowercase().as_str() {
            "super" | "cmd" | "command" | "meta" => Modifier::Super,
            "shift" => Modifier::Shift,
            "ctrl" | "control" => Modifier::Ctrl,
            "alt" | "option" | "opt" => Modifier::Alt,
            other => return Err(format!("unknown modifier {other:?} in chord {chord:?}")),
        });
    }
    Ok(Binding::new(parsed, *key))
}
