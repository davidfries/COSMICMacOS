# cosmicMac

COSMIC-style tiling window management for macOS. cosmicMac brings the window
management of the [COSMIC desktop](https://github.com/pop-os/cosmic-epoch)
(Pop!_OS) to a Mac: auto-tiling, stacks with tabs, workspaces and COSMIC's
keybindings, plus a COSMIC launcher and a menu-bar workspace indicator.

It works with System Integrity Protection **on**. Windows are moved through
the Accessibility API and a few private SkyLight calls; nothing is injected
into other apps. The macOS menu bar and Dock stay as they are.

> **Status:** early and experimental. Milestones 1 (window manager) and 2
> (menu-bar item, launcher, theming) are done and tested on macOS 15. Expect
> rough edges, and try it on a test machine or VM first.

## Features

- **Auto-tiling** with COSMIC's layout rules; toggle tiling per workspace and
  float single windows.
- **Stacks**: windows grouped into one tile with a tab bar. Stack with Cmd+S,
  or drag a window by its title bar onto the middle of another window.
- **Workspaces** 1–9, switched and moved between with Cmd+number.
- **COSMIC keybindings**, with Cmd as the Super key, read from COSMIC's
  `keybindings.ron` format.
- **Launcher**: tap Cmd on its own to search apps and windows, calculate
  (`=`), search the web (`g `, `ddg `), find files (`find `) and run commands
  (`run `, `t:`, `:`).
- **Menu-bar item** showing the active workspace, with a menu to switch
  workspaces, toggle tiling and float the focused window.
- **Theming**: the launcher and tab bar follow macOS dark/light mode, or
  COSMIC's theme settings when present.
- **Crash safety**: windows go back to their original frames when the window
  manager quits, and after a crash on the next start.
- **Native full screen**, minimize and Cmd+H: windows leave their tile and
  come back to the same spot.

## Requirements

- macOS 15 (Sequoia) on Apple silicon. macOS 26 (Tahoe) is not supported yet.
- Rust (edition 2024; a recent stable toolchain).
- Accessibility permission for `cosmic-mac-wm` (System Settings > Privacy &
  Security > Accessibility).

## Build

```sh
cargo build --release --workspace
scripts/bundle.sh        # assembles target/release/bundles/cosmic-mac-launcher.app
```

macOS ties the Accessibility grant to the binary's code signature. Sign the
binaries with a stable identity (see `scripts/sign.sh`) so the grant survives
rebuilds.

## Install

Copy the binaries (`cosmic-mac-wm`, `cosmic-mac-ctl`, `cosmic-mac-session`,
`cosmic-mac-status`) and `cosmic-mac-launcher.app` into one directory, then
install the launchd agents from there:

```sh
./cosmic-mac-session install     # starts the WM, menu-bar item and launcher at login
./cosmic-mac-session status
./cosmic-mac-session uninstall   # stops them; windows return to their original frames
```

The agents are `com.cosmicmac.wm`, `com.cosmicmac.status` and
`com.cosmicmac.launcher`. Logs go to `~/Library/Logs/cosmicMac/`. macOS does
not prompt for the Accessibility permission: add `cosmic-mac-wm` yourself in
System Settings > Privacy & Security > Accessibility. Until then the agent
exits and launchd retries every few seconds.

## Default keybindings

Super is the Command key.

| Keys | Action |
|---|---|
| Cmd+←/→/↑/↓, Cmd+H/J/K/L | Focus left / right / up / down |
| Cmd+Shift+arrows, Cmd+Shift+H/J/K/L | Move the window |
| Cmd+S | Stack with the neighbour (again to unstack) |
| Cmd+U | Take the window out of its stack |
| Cmd+Y | Toggle tiling for the workspace |
| Cmd+G | Float / tile the window |
| Cmd+O | Toggle split orientation |
| Cmd+X | Swap windows |
| Cmd+M | Maximize |
| Cmd+R | Resize mode (arrows resize) |
| Cmd+Q | Close the window |
| Cmd+1…9 | Go to workspace |
| Cmd+Shift+1…9 | Move the window to workspace |
| Tap Cmd | Open / close the launcher |

These shadow the apps' own Cmd shortcuts (for example Cmd+S to save). To keep
an app's shortcuts, add its bundle id to `passthrough` in the config. The full
default table is in `crates/cosmic-mac-wm/src/keybindings.rs`.

## Configuration

Files live in `~/Library/Application Support/cosmic/`, the same layout
cosmic-config uses:

- `com.system76.CosmicComp/config.ron`: gaps, `autotile`,
  `tiling_exceptions` (windows that float by default), `passthrough` (apps
  whose shortcuts are not intercepted), `super_tap_launcher`.
- `com.system76.CosmicSettings.Shortcuts/custom.ron`: keybindings in COSMIC's
  format, merged over the defaults.

Example `config.ron`:

```ron
(
    gaps: (outer: 8, inner: 4),
    autotile: true,
    tiling_exceptions: [(app_id: Some("com.apple.calculator"), title: None)],
    passthrough: ["com.apple.Terminal"],
)
```

## Command line

`cosmic-mac-ctl` talks to the running window manager:

```sh
cosmic-mac-ctl list-windows
cosmic-mac-ctl tree
cosmic-mac-ctl action toggle-stacking
cosmic-mac-ctl watch            # print the workspace state on every change
cosmic-mac-ctl restore          # put every window back where it was
```

## Repository layout

| Crate | Purpose |
|---|---|
| `cosmic-mac-wm` | The window-manager daemon |
| `cosmic-mac-layout` | Tiling tree, stacks, workspaces (pure Rust, no macOS code) |
| `cosmic-mac-platform` | Accessibility, SkyLight and event-tap bindings |
| `cosmic-mac-protocol` | IPC and config types shared by all parts |
| `cosmic-mac-ctl` | Command-line client |
| `cosmic-mac-session` | launchd agent installer |
| `cosmic-mac-status` | Menu-bar workspace item |
| `cosmic-mac-launcher` | The launcher (libcosmic) |
| `cosmic-mac-panel` | Earlier spike of a COSMIC top panel, kept for reference |

Design notes: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md),
[docs/SKYLIGHT.md](docs/SKYLIGHT.md). Plan and milestones:
[PLAN.md](PLAN.md). Detailed status and findings: [HANDOFF.md](HANDOFF.md).

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

The window manager takes over window placement, so it is developed against a
macOS VM. `scripts/deploy.sh` builds, signs and copies the binaries to the VM,
and `scripts/vm-test.sh` runs the end-to-end scenario there.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
