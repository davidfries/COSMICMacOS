// SPDX-License-Identifier: GPL-3.0-or-later
//! Post one keyboard chord as a synthetic HID event, spelled like a COSMIC
//! binding, so the daemon's event tap can be exercised from a script.
//!
//! ```sh
//! send-key Super+Shift+y        # post Cmd+Shift+Y (down, then up)
//! send-key --dry-run Super+Left # resolve the chord without posting it
//! send-key --list               # print the armed default bindings
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

use cosmic_mac_platform::overlay::post_key;
use cosmic_mac_wm::keybindings::{Binding, KeyMap, Modifier};
use cosmic_mac_wm::paths;

const USAGE: &str = "\
usage: send-key [--dry-run] <chord>
       send-key --list

  <chord>    modifiers and a key joined by '+', e.g. Super+Shift+y, Cmd+Left
  --dry-run  resolve the chord and print the action without posting
  --list     print every armed binding with its keycode and modifier mask";

/// Delay between the key-down and key-up events, and after the chord.
const KEY_GAP: Duration = Duration::from_millis(30);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dry_run = false;
    let mut chord = None;
    for arg in &args {
        match arg.as_str() {
            "--list" => return list(),
            "--dry-run" => dry_run = true,
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
