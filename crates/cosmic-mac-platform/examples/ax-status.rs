//! `ax-status`: print the TCC / SkyLight / display state of the VM.
//!
//! This is the binary `scripts/deploy.sh` runs after every deploy. Exit code 2
//! means "Accessibility is not granted" so the script can print a loud warning.

use cosmic_mac_platform::{display, tcc, window};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    println!("cosmicMac ax-status");
    println!(
        "binary: {}",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".into())
    );
    println!("macOS:  {}", macos_version());

    let status = tcc::status();
    println!("accessibility_trusted: {}", status.accessibility);
    println!("screen_recording_trusted: {}", status.screen_recording);
    println!("sky_light_connected: {}", status.skylight);
    println!(
        "displays_have_separate_spaces: {}",
        match status.separate_spaces {
            Some(value) => value.to_string(),
            None => "unknown".to_string(),
        }
    );

    match display::list_displays() {
        Ok(displays) => {
            println!("displays: {}", displays.len());
            for display in displays {
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
        }
        Err(err) => println!("displays: ERROR {err}"),
    }

    match window::list_windows() {
        Ok(windows) => {
            let ax_backed = windows.iter().filter(|window| window.ax_backed).count();
            println!("windows: {} (ax_backed: {})", windows.len(), ax_backed);
            for window in windows.iter().take(40) {
                println!(
                    "  wid={:<8} pid={:<6} app={:<24} title={:?}",
                    window.wid,
                    window.pid,
                    window.app_name.as_deref().unwrap_or("?"),
                    window.title.as_deref().unwrap_or("")
                );
            }
            if windows.len() > 40 {
                println!("  ... {} more", windows.len() - 40);
            }
        }
        Err(err) => println!("windows: ERROR {err}"),
    }

    if status.accessibility {
        println!("\nAccessibility is granted. Window control and the event tap are available.");
    } else {
        eprintln!(
            "\n!! Accessibility is NOT granted for this binary.\n\
             !! Window move/resize/minimize/close and the CGEventTap will fail.\n\
             !! Grant it once in System Settings > Privacy & Security > Accessibility.\n\
             !! The grant survives rebuilds only when the binary is signed by scripts/sign.sh."
        );
        std::process::exit(2);
    }
}

fn macos_version() -> String {
    let output = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output();
    match output {
        Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
        Err(_) => "unknown".into(),
    }
}
