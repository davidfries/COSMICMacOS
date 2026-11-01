// SPDX-License-Identifier: GPL-3.0-or-later
//! Command-line options.

use std::ffi::OsString;

pub const USAGE: &str = "\
usage: cosmic-mac-launcher [--show]

Resident COSMIC-style launcher. Without options it starts hidden and waits
for `toggle`, `show` or `hide` lines on
~/Library/Application Support/cosmic/cosmic-mac-launcher.sock (the window
manager sends `toggle` when Super is tapped).

  --show      show the launcher right away (used when started on demand);
              if a launcher is already running, ask it to show and exit
  -h, --help  print this help";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    pub show: bool,
    pub help: bool,
}

/// Parses the arguments after the program name. LaunchServices may add a
/// `-psn_…` process serial number argument; it is ignored.
pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Options, String> {
    let mut options = Options::default();
    for arg in args {
        let text = arg.to_string_lossy();
        match text.as_ref() {
            "--show" => options.show = true,
            "-h" | "--help" => options.help = true,
            psn if psn.starts_with("-psn_") => {}
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Options, String> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn defaults_to_hidden() {
        assert_eq!(parse_strs(&[]), Ok(Options::default()));
    }

    #[test]
    fn show_help_and_psn() {
        assert_eq!(
            parse_strs(&["--show", "-psn_0_12345"]),
            Ok(Options {
                show: true,
                help: false
            })
        );
        assert!(parse_strs(&["-h"]).unwrap().help);
        assert!(parse_strs(&["--help"]).unwrap().help);
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(parse_strs(&["--hide"]).unwrap_err().contains("--hide"));
        assert!(parse_strs(&["show"]).is_err());
    }
}
