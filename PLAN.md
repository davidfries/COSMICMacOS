# cosmicMac: COSMIC desktop shell for macOS

## Context

Goal: make macOS look and behave like the Pop!_OS COSMIC desktop, including COSMIC window
management (tiling, per-workspace floating toggle, window **stacks** = tabbed window groups,
workspaces) applied to real native macOS apps, with COSMIC's panel, dock and launcher replacing
the macOS menu bar and Dock.

macOS has no Wayland and WindowServer owns the display, so cosmic-comp cannot run as the
display server. Agreed shape (confirmed with user): a **hybrid**. COSMIC UI components run as
ordinary macOS windows built with libcosmic (iced, winit, wgpu/Metal). A separate
window-manager daemon drives native apps through the Accessibility (AX) API plus
SIP-safe private SkyLight calls. Core must work with **SIP on**; SIP-off extras (native Spaces
control, opacity, shadows) are an optional later layer.

Decisions already made:
- Stacking means both COSMIC stacks (tabbed groups) and a floating mode toggle per workspace.
- Super key = **Command**. Bound combos are swallowed globally by a CGEventTap; unbound Cmd
  combos pass through.
- First milestone = **window manager**, CLI-driven. Panel/dock/launcher second.
- Development happens on this host; testing in the UTM macOS VM over SSH.
- Repo: `git init` locally, then a private GitHub repo created with `gh` at first ship.

Research findings that shape the design (all verified from upstream sources on 2026-10-31):
- libcosmic already has deliberate `target_vendor = "apple"` cfg gates: with
  `default-features = false, features = ["winit","tokio","multi-window","wgpu"]` it is meant to
  compile on macOS. Icons come only from the build-script bundled set on macOS (freedesktop
  lookup is a stub). No macOS CI; open issue #615 (rustix pipe in `process.rs`) may bite if the
  `desktop`/`process` features are enabled. cosmic-config (RON files, `notify` watcher) and
  cosmic-theme are portable; on macOS config lands in `~/Library/Application Support/cosmic/`.
- Everything in COSMIC's shell layer is Wayland/D-Bus native: cosmic-panel is itself a nested
  Wayland compositor hosting applets as subsurfaces; launcher/app-library/osd use layer-shell;
  workspaces-epoch uses Vulkan + screencopy. These are **re-architected**, not ported.
  cosmic-launcher's pop-launcher service runs in-process and its plugin core is portable.
- cosmic-comp's tiling tree (`src/shell/layout/tiling/mod.rs`: `id_tree` of
  `Group{orientation,sizes}` / `Mapped` nodes), `workspace.rs` and `element/stack.rs` are
  reference material, too interleaved with Smithay to reuse. Layout engine is re-implemented
  in a platform-free crate following their semantics. cosmic-comp is GPL-3; shell apps GPL-3;
  libcosmic MPL-2. This project: **GPL-3.0**.
- Keybinding config format: cosmic-config id `com.system76.CosmicSettings.Shortcuts` v1, RON
  map `{(modifiers:[Super,Shift], key:"y"): Action}`; defaults in cosmic-comp
  `data/keybindings.ron`. Reuse the format and defaults verbatim.
- macOS WM technique: AeroSpace (Swift, AX only) proves SIP-on virtual workspaces by parking
  hidden windows at a monitor corner. Rust precedents using objc2 + SkyLight FFI with SIP on:
  `acsandmann/rift` and `glide-wm/glide`. JankyBorders shows how to keep an own SLS window
  ordered directly above a foreign window (`SLSTransactionOrderWindow`,
  `SLSRegisterNotifyProc` window events) with SIP on; this is the stack tab-bar mechanism.
  Focus-without-raise via `_SLPSSetFrontProcessWithOptions` + `SLPSPostEventRecordTo`.
  CGEventTap swallows Cmd combos; must re-enable on `kCGEventTapDisabledByTimeout` and fall
  back to Carbon hotkeys during Secure Event Input.
- TCC grants key on the code-signing designated requirement: ad-hoc signing loses the grant on
  every rebuild. Sign with a self-signed cert so Accessibility stays granted across deploys.

VM facts (checked over SSH): `dj@192.168.64.14`, key auth works, macOS 15.6, SIP enabled,
2 CPU / 4 GB / 59 GB, GUI session logged in, no Rust/Xcode/brew, display sleep 10 min,
no auto-login. UTM 4.6.5 on host; `utmctl start|stop|ip-address|start --recovery` available.
Building happens on the host (same `aarch64-apple-darwin` target); binaries are copied in.

## Architecture

Cargo workspace at `/Users/dj/Programming/cosmicMac`:

```
Cargo.toml                 workspace; libcosmic pinned by git rev; [patch] section ready
crates/
  cosmic-mac-layout/       pure Rust, no macOS deps: tiling tree, stacks, floating layer,
                           workspaces, focus stack. Unit-tested exhaustively.
  cosmic-mac-platform/     objc2 + SkyLight FFI: window enumeration/tracking (AX observers +
                           SLSRegisterNotifyProc), move/resize/raise/focus-without-raise,
                           displays, CGEventTap keyboard, overlay window primitives, TCC checks.
  cosmic-mac-protocol/     IPC types (serde): toplevel-info, toplevel-management, workspace,
                           actions, config-changed. Mirrors cosmic-protocols semantics so shell
                           crates map 1:1 to what upstream applets expect.
  cosmic-mac-wm/           the daemon: wires layout + platform, keybindings (cosmic
                           keybindings.ron format via cosmic-config), virtual workspaces,
                           stack tab-bar overlays, Unix-socket IPC server.
  cosmic-mac-ctl/          CLI client (like `cosmic-comp`'s absence of one is a pain): query
                           windows/workspaces, send actions, dump layout tree as JSON. Used by
                           integration tests.
  cosmic-mac-panel/        (milestone 2) libcosmic app: top panel + bottom dock as borderless
                           status-level NSWindows; applets in-process (time, workspaces,
                           app-list, tiling toggle, battery, audio, network, power).
  cosmic-mac-launcher/     (milestone 2) fork of cosmic-launcher UI + pop-launcher service
                           in-process with macOS plugins (apps, calc, web, files, terminal,
                           window switcher).
  cosmic-mac-session/      (milestone 2) installer + launchd agents + macOS defaults
                           (hide Dock/menu bar, disable native tiling, wallpaper).
vendor/                    only if libcosmic needs a local patch (path [patch]) until upstreamed
scripts/                   vm-prep.sh, deploy.sh, screenshot.sh, sign.sh
justfile                   build / deploy / test / screenshot recipes
docs/                      ARCHITECTURE.md, SKYLIGHT.md (FFI notes incl. macOS 26 msg IDs)
```

Key design rules:
- `cosmic-mac-layout` exposes a `Layout` state machine fed by events
  (`WindowAdded/Removed/FocusChanged/Action`) and emitting `Command`s
  (`SetFrame{wid,rect}`, `Raise`, `Hide`, `Show`, `Focus`). No platform calls inside. This is
  what makes tiling/stack logic testable on the host without a VM.
- Virtual workspaces (AeroSpace model): one workspace pool; each display shows one. Hidden
  workspace windows are moved to the display's bottom-right corner (1 px visible). Restore all
  frames on quit/crash (write a journal of original frames; replay on next start if dirty).
- Stacks with SIP on: z-order of foreign windows is **not** controllable (activating an app
  raises all its windows), so inactive stack members are **parked** with the same corner-park
  mechanism as hidden workspaces; only the active member occupies the frame. Switching tabs
  = unpark new member at the frame, park the old one. A tab bar is an own window kept ordered
  just above the active member via `SLSTransactionOrderWindow`, repositioned from SLS
  move/resize notifications. Milestone 1 draws it with Core Graphics on a raw SLS window
  (JankyBorders style, proven); milestone 2 swaps in a libcosmic renderer matching
  `element/stack.rs` once Spike C proves an AppKit `windowNumber` can be ordered the same way.
- Focus: COSMIC focus follows the layout tree; use focus-without-raise where possible.
  External activation of a parked window (Cmd+Tab, Dock, notification click) is detected via
  `NSWorkspace` didActivate / SLS front-change events and switches to that window's workspace
  (AeroSpace behaviour) instead of fighting the user.
- Coordinates: one conversion module in `cosmic-mac-platform` (AX/SLS top-left global vs
  `NSScreen` bottom-left). Work area per display comes from `NSScreen.visibleFrame` in
  milestone 1 (menu bar and Dock still visible) and from panel/dock reservations later.
- Prerequisite: "Displays have separate Spaces" **off** (corner parking must not leak across
  displays); daemon warns at startup if it is on.
- Floating toggle per workspace (`ToggleTiling`, Super+Y) and per window
  (`ToggleWindowFloating`, Super+G) exactly as in cosmic-comp.
- Config: cosmic-config crate as-is. Keybindings file compatible with COSMIC's. The shortcut
  `Binding`/`Action` types come from `cosmic-settings-config` (cosmic-settings-daemon repo);
  if that crate drags in Linux-only deps, vendor just those two types into
  `cosmic-mac-protocol`. Default `keybindings.ron` is embedded with `include_str!`, not read
  from xdg system dirs (which resolve to `/usr/local/share` on macOS). WM config id
  `com.system76.CosmicComp` keys honoured where meaningful (`autotile`, `active_hint`, gaps).
  Until milestone 2 ships them, the Launcher, AppLibrary, WorkspaceOverview and WindowSwitcher
  actions are **not** armed, so Cmd+Tab and Cmd+Space keep working.
- IPC socket at a fixed path `~/Library/Application Support/cosmic/cosmic-mac-wm.sock`
  (non-interactive SSH shells may lack `TMPDIR`).
- Shutdown: SIGTERM/SIGINT handler unparks and restores every frame; the journal covers
  crashes only.
- Code signing: a self-signed "cosmicMac Dev" code-signing cert is created **on the host**;
  `scripts/sign.sh` signs on the host after build with a stable identifier per binary. The
  acceptance test for the signing setup is: grant Accessibility once in the VM, rebuild,
  redeploy, grant still present.
- Licensing hygiene: check `rift` and `glide` licenses before copying anything; SkyLight
  prototypes are redeclared independently from the public yabai/JankyBorders knowledge.

## Milestone 0: Bootstrap and spikes (de-risk before building)

1. `git init`, workspace skeleton, GPL-3 LICENSE, `.gitignore`, `rust-toolchain.toml`.
2. VM prep script `scripts/vm-prep.sh` (run over SSH, user confirms each sudo step):
   enable auto-login, `pmset displaysleep 0`, screensaver off, turn off "Displays have
   separate Spaces", disable macOS native tiling defaults (`com.apple.WindowManager
   EnableTilingByEdgeDrag/EnableTopTilingByEdgeDrag/EnableTilingOptionAccelerator false`),
   open the Accessibility pane (`open x-apple.systempreferences:com.apple.preference.security
   ?Privacy_Accessibility`) for the one-time GUI click. Host side: create the self-signed
   codesign cert. Document the two one-time GUI clicks: Accessibility for the WM daemon,
   Screen Recording for sshd (`screencapture` over SSH; Sequoia re-prompts every 30 days, the
   `ScreenCaptureApprovals.plist` date-bump workaround goes in `screenshot.sh`). Recommend
   bumping the VM to 4 CPU / 8 GB (UTM settings; host has 12 cores / 32 GB). Snapshot = stop
   VM + `cp -c` the `.utm` bundle (APFS clone).
3. `scripts/deploy.sh`: `cargo build --release` on host, `sign.sh`, `rsync` to
   `dj@192.168.64.14:~/cosmicMac/bin`, restart launchd agent, then run `cosmic-mac-ctl
   ax-status` in the VM and print a loud notice if `AXIsProcessTrusted()` is false.
   `scripts/screenshot.sh`: `ssh ... screencapture -x /tmp/s.png` then `scp` back to scratchpad.
4. Spike B (first): `cosmic-mac-platform` enumerates on-screen windows (SLS + AX),
   moves/resizes a TextEdit window, focuses without raising, receives move/resize
   notifications, parks a window at the corner and restores it, and a CGEventTap swallows
   Cmd+Shift+Y while passing Cmd+C. Validates every private API we rely on against macOS 15.6
   in the VM, and that the Accessibility grant survives a rebuild with host signing.
5. Spike C (second): an own overlay window ordered just above a foreign window and tracking it
   (JankyBorders technique) with SIP on. Test both a raw SLS window and an AppKit/winit
   `NSWindow` by `windowNumber`; the result decides the milestone-2 tab-bar renderer.
6. Spike A (last, not blocking milestone 1): libcosmic hello-world (`default-features=false`,
   winit+tokio+multi-window+wgpu) builds on host and renders in VM. If #615 or other breakage
   appears, add a `[patch]` to a local `vendor/libcosmic` with the minimal fix; note for
   upstreaming.

Exit criteria: all three spikes run in the VM with screenshots as evidence.

## Milestone 1: Window manager (first deliverable)

Crates: `cosmic-mac-layout`, `cosmic-mac-platform`, `cosmic-mac-protocol`, `cosmic-mac-wm`,
`cosmic-mac-ctl`.

1. **Layout engine** (host-testable): tiling tree with COSMIC semantics ported from
   `cosmic-comp/src/shell/layout/tiling/mod.rs`: insert next to focused node with orientation
   auto-pick by aspect, `Focus(Direction)`, `Move(Direction)`, `SwapWindow`,
   `ToggleOrientation`, resize mode, `Maximize`, remove+collapse; floating layer with
   placement; per-workspace `tiling_enabled`; stacks as a node kind holding an ordered list and
   active index; focus stack per workspace; workspace list per display with `Workspace(n)`,
   `MoveToWorkspace(n)`, `Next/PreviousWorkspace`, `LastWorkspace`, auto-remove empty trailing
   workspaces like COSMIC. Gaps and active-hint config.
2. **Platform layer**: window model (wid, pid, app id = bundle id, title, role/subtype,
   resizable, is dialog), AX observers per app (thread per app as AeroSpace does), SLS
   notifications for move/resize/order/space, display enumeration and change events, apply
   `Command`s (set position/size with the set-size/set-position/set-size workaround, raise,
   park/unpark, focus-without-raise, minimize, close), keyboard tap with hotkey fallback,
   presence check for Accessibility permission with a clear startup error.
3. **Daemon**: event loop (tokio) joining platform events, layout engine, keybindings from
   `com.system76.CosmicSettings.Shortcuts` (ship COSMIC's `keybindings.ron` as defaults; load
   `custom`), IPC server on `$TMPDIR/cosmic-mac-wm.sock` serving `cosmic-mac-protocol`.
   Float-by-default rules: dialogs, non-resizable, utility panels, picture-in-picture, plus a
   configurable app-id/title rule list (mirrors cosmic-comp `tiling_exceptions`).
   Crash safety: frame journal + restore on start; `cosmic-mac-ctl restore`.
4. **Stacks** (`ToggleStacking` Super+S, stack navigation, `MoveToStack` via Move into a
   stack node, `Unstack`): inactive members parked, active member at the frame; tab-bar
   overlay drawn with Core Graphics on a raw SLS window inside the daemon (JankyBorders
   style). Click on tab activates; close button closes window. The libcosmic-rendered tab bar
   in a sibling `cosmic-mac-overlay` process is milestone 2 work.
5. **CLI**: `cosmic-mac-ctl list-windows|list-workspaces|tree|action <Action>|focus <wid>|
   restore`, JSON output.
6. **Tests**: layout crate unit tests for every action and edge case (last window removed,
   stack of one, move across workspaces, display removed); property test that frames never
   overlap in tiling mode and always cover the work area. Integration script
   `scripts/vm-test.sh`: opens N TextEdit/Terminal windows in the VM via `open -n`, drives
   actions with `cosmic-mac-ctl`, asserts geometry from `list-windows`, takes screenshots.

Exit criteria: in the VM, with Dock/menu bar still visible, open apps tile automatically;
Super+arrows/hjkl focus, Super+Shift+arrows move, Super+S stacks with visible tab bar,
Super+Y floats a workspace, Super+1..4 switch virtual workspaces, Super+Shift+n moves
windows between them, Super+Q closes; quitting the daemon restores every window.

## Milestone 2: Visual shell

1. `cosmic-mac-session`: launchd agents for the WM (and the launcher once it is resident);
   `install`/`uninstall`/`status` subcommands. No macOS defaults are changed (menu bar and Dock
   stay). Wallpaper via `NSWorkspace.setDesktopImageURL` is a possible follow-up.
2. ~~`cosmic-mac-panel`~~ **dropped (2026-10-31)**: Spike A proved a libcosmic bar works, but
   replacing the macOS menu bar means hiding it, and that hides every app's menus (File, Edit,
   View), which COSMIC's panel never has to carry on Linux. The native menu bar and Dock stay.
   Instead: a **menu-bar status item** (`NSStatusItem`) showing the active workspace, with a
   menu to switch workspaces and toggle tiling. The WM keeps its shell-strip reservation
   (`reserve.rs`) for a possible COSMIC dock later. `crates/cosmic-mac-panel` remains as Spike A
   reference code.
3. `cosmic-mac-launcher`: cosmic-launcher UI with pop-launcher service; plugins: apps
   (LaunchServices database + `/Applications`, `~/Applications`, `/System/Applications`),
   calc, web, files (`mdfind`), terminal, window switcher (Alt-Tab, Super+Tab via WM IPC).
   Opened by tap-Super (tap detection in the event tap: Cmd down/up with no other key).
4. Theming: cosmic-theme dark/light from `com.system76.CosmicTheme.Mode`; follow macOS
   appearance as fallback.

## Milestone 3: Completeness

- Workspace overview (Super+W) and app library (Super+A): thumbnails via ScreenCaptureKit
  (needs Screen Recording permission); app library from LaunchServices.
- Settings app: fork cosmic-settings pages that are portable (appearance, panel, dock,
  shortcuts, window management) over cosmic-config.
- OSD for volume/brightness (CoreAudio, DisplayServices) replacing macOS HUD where possible.
- Optional SIP-off layer: native Spaces control, window opacity/shadows, animation-free
  moves (yabai scripting-addition equivalent). Documented as opt-in.
- Multi-monitor polish, laptop lid/display change handling, Tahoe (macOS 26) support
  (SLS sub-level message IDs differ).

## Verification (per milestone, run by main session)

- Host: `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, `cargo fmt --check`.
- VM: `just deploy && just vm-test` (integration script above), then `just screenshot` and
  visually inspect; `cosmic-mac-ctl tree` output matches expected layout after each action.
- Manual acceptance in VM for each exit-criteria list; crash-restore test (kill -9 daemon,
  all windows return to original frames).
- Before each risky shell change: stop VM, APFS-clone the bundle as a snapshot.

## Open risks (tracked, not blocking)

- libcosmic on macOS is unsupported upstream; expect occasional breakage on rev bumps. Pin
  rev; bump deliberately.
- AX latency during user drags makes overlays lag; mitigated by SLS notifications and
  high-rate polling only while a drag is in progress.
- Cmd-as-Super shadows app shortcuts (Cmd+W, Cmd+1..9, Cmd+S). User accepted; provide an
  escape hatch (`passthrough` list in config) in milestone 1.
- Secure Event Input disables the tap; hotkey fallback covers the most-used bindings.
- Mission Control shows parked windows tiny (AeroSpace limitation); documented.

## First implementation steps after approval

1. `git init`; create workspace skeleton and the five milestone-1 crates (empty).
2. Run `scripts/vm-prep.sh` steps over SSH (each sudo step confirmed with the user).
3. Spikes B, C, A in that order; adjust the platform design from what the VM actually allows.
4. Then milestone 1 proper, layout engine first (host-only, TDD), platform layer second,
   daemon and CLI third, stacks last.
