# HANDOFF.md — cosmicMac

> State as of 2026-10-31. Milestone 0 (spikes) **complete**, milestone 1 (window
> manager) **implemented and passing the full VM integration scenario**.
> Written after two rounds of parallel DeepSeek agent implementation plus manual
> review, debugging and VM verification.

## What this is

A macOS port of the Pop!_OS COSMIC desktop's window management: tiling, stacks
(tabbed groups), virtual workspaces, floating toggles — driven through the
Accessibility (AX) API + private SkyLight calls with **SIP on**. Repo layout and
design rationale live in `PLAN.md` and `docs/ARCHITECTURE.md`; SkyLight FFI
notes in `docs/SKYLIGHT.md`.

```
crates/
  cosmic-mac-protocol/   serde IPC types mirroring cosmic-protocols (toplevel, workspace,
                         Action, Command, Request/Response JSON-line envelope)
  cosmic-mac-layout/     PURE Rust tiling tree, stacks, floating, workspaces, per-window
                         size constraints. Zero macOS deps. 194 tests.
  cosmic-mac-platform/   objc2 + AX + private SkyLight FFI: enumeration, move/resize with
                         honest AppliedFrame results, park/unpark, focus-without-raise,
                         CGEventTap, overlay windows, size-constraint probes. 15 tests.
  cosmic-mac-wm/         the daemon: tokio event loop, COSMIC keybindings.ron, learned
                         per-app size constraints, tab-bar overlay, frame journal, IPC. 65 tests.
  cosmic-mac-ctl/        CLI client (list-windows/tree/action/restore...), 70 tests incl.
                         a real Unix-socket mock daemon harness.
```

~17k lines of Rust. All 384 workspace tests pass; clippy `-D warnings` and fmt clean.

## Reference revisions (upstream)

cosmic-epoch pins each submodule at a tested-compatible commit — Epoch 1 shipped in
Pop!_OS 24.04. Our semantics documentation should cite these:

| Component | Pin | Used for |
|---|---|---|
| cosmic-comp | `8cb1802` | tiling tree semantics (`src/shell/layout/tiling/mod.rs`) |
| cosmic-settings-daemon | `655b14f` | keybinding `Binding`/`Action` types + `keybindings.ron` format |
| cosmic-panel / cosmic-launcher / pop-launcher / cosmic-applets / cosmic-workspaces-epoch / cosmic-osd / cosmic-bg | see cosmic-epoch master | milestone 2 |

## The wins

1. **SIP-on window management works.** Every private API survived contact with
   macOS 15.6: SLS enumeration, `SLSRegisterNotifyProc` events, park-at-corner
   (AeroSpace style), focus-without-raise (`_SLPSSetFrontProcessWithOptions` +
   `SLPSPostEventRecordTo`), overlay windows ordered above foreign windows
   (JankyBorders technique) — including above an **AppKit NSWindow by
   `windowNumber`**, which de-risks the milestone-2 libcosmic tab bar.
2. **TCC grants survive rebuilds.** Signing with a self-signed "cosmicMac Dev"
   identity in a dedicated keychain (`scripts/sign.sh`); Accessibility granted
   once and still granted after many redeploys.
3. **The full scripted milestone-1 scenario passes in the VM**
   (`bash scripts/vm-test.sh`): auto-tiling, directional focus, stacking with
   tab bar, workspace switching/moving, floating toggles, maximize, resize,
   orientation toggle, swap, close, `restore` matching AX-verified baseline
   frames, and clean SIGTERM shutdown. Screenshots in `scratch/vm-test-*.png`.
4. **Crash safety proven**: `kill -9` leaves `frame-journal.ron`; next start
   replays it and restores every frame. SIGTERM restores and removes the journal.
5. **Keybindings**: COSMIC's RON format embedded as defaults (45 armed),
   `custom.ron` overrides merged, per-app passthrough honoured via the event tap
   handler (bound combos swallowed, everything else passes; verified Cmd+Shift+Y
   swallow / Cmd+C pass in the VM).

## The finds (things that cost debugging time — don't relearn them)

### macOS platform facts

- **Apps silently clamp AX resizes.** `AXUIElementSetAttributeValue(AXSize)`
  returns success even when the app refuses. Verified per-app: System Settings
  width **frozen** at 715px (grow and shrink both rejected; height fine); Notes
  clamps width to 642; Finder to 445; Terminal snaps to its character grid;
  TextEdit resizes freely. This was the source of the mysterious "wallpaper gap
  between tiles": the layout's tiles were flush, the windows weren't.
- **A single rejected resize cannot distinguish min-clamp from frozen.** Notes'
  min equals its natural size but it grows fine — only a **shrink+grow probe**
  tells `Min{642}` from `Frozen{715}` apart. Probe algorithm: shrink toward 1×1,
  read back, restore; grow +128px capped at 50% of the display, read back,
  restore; classify `Free` / `Min{v}` / `Frozen{v}` (+ real max from a grow
  clamp, plus `AXMaxSize` when published).
- **Probe location matters**: growing in place near a screen edge gets clamped
  by the *display*, which records a false app maximum (Notes "max height 485").
  The grow probe runs at the display's visible origin instead.
- **SLS notify callbacks do not fire** unless the connection event port is
  drained (`SLSGetEventPort` + `SLSLEventCreateNextEvent`). This was the
  difference between zero events and a working observer.
- `SLSMoveWindowsToManagedSpace` returns `void`; treating its return as CGError
  yields random errors.
- Raw SLS overlays need a dedicated `SLSNewConnection` and must be moved to the
  target's managed Space; transaction move/order calls can fail for plain
  `SLSNewWindow` windows on 15.6 (fall back to `SLSMoveWindow`/`SLSOrderWindow`).
- `screencapture` over SSH is TCC-filtered (Screen Recording granted to
  `sshd-keygen-wrapper`); `CGWindowListCreateImage` for *your own* window is not.
- Sequoia re-prompts Screen Recording every ~30 days; the
  `ScreenCaptureApprovals.plist` date-bump workaround belongs in
  `scripts/screenshot.sh`.
- `Raise` on some system windows (System Settings) fails with SkyLight
  `CGError 1000`; logged and ignored.

### Daemon/layout lessons

- **Echo suppression is essential.** Every programmatic move echoes back through
  SLS/AX. Suppress frames matching the programmed one *forever* (duplicates
  come in bursts), suppress differing frames for ~150 ms, and stop suppressing
  on the first genuinely-different frame. Programmatic focus opens a ~250 ms
  window ignoring `AppActivated`/`FocusChanged` — without it macOS's activation
  burst ping-pongs focus across every open app.
- **Tree-based focus, not spatial.** `act_focus` for tiled windows walks the
  tiling tree (`focus_neighbor` with wrap-around at the root axis), exactly like
  cosmic-comp; the spatial search is only the floating fallback. Early VM test
  failures came from asserting spatial expectations against tree semantics.
- **Deterministic tie-break**: two windows equidistant in a focus direction are
  common (full-height left window + two stacked right windows). Ties resolve
  primary → perpendicular → **topmost** → leftmost → lowest wid; anything else
  flips between runs with map iteration order.
- **Insert next to the largest tile**, not "next to the previously focused
  window" — the latter produces ½, ¼, ⅛ chains that shrink real windows into
  app-minimum territory.
- **`restore` must stop managing.** Restoring frames while the layout still holds
  the windows means the next event re-tiles them back. `Request::Restore` now
  drains the layout (discarding its commands) after restoring.
- **Constraint learning must be probe-confirmed.** A single clamped resize is
  only a directional *hint* (requested < applied → min candidate). Pinning
  min==max from one observation poisoned the layout (Terminal snapped to the
  whole work area at startup and was "learned" as min=max=1725×959). Bounds are
  promoted only after a successful probe; failed probes never pin.
- **Layout honours per-window limits**: tiles clamp to [min,max], leftover space
  redistributes to siblings that can flex, an un-absorbable leftover centres the
  group (symmetric gaps, never one-sided wallpaper), and impossible minima yield
  to the non-overlap invariant.

### Test-harness lessons

- **Bare `open -a TextEdit` creates an "Untitled" document window** that is not
  titled like the test windows, survives prefix-based cleanup, joins the tiling,
  and breaks every focus/move assertion (the focused window legitimately becomes
  the Untitled one). Cleanup now `pkill -x TextEdit` first, and windows are only
  opened as `open -n -a TextEdit <file>` (one instance per document; plain
  `open -a` merges into tabs).
- **Assertions need a faithful oracle.** The focus assertions now snapshot
  list-windows *and* `tree --json` before each action, and a Python port of
  `Tree::focus_neighbor` computes the expected target. Naive min/max-over-frames
  oracles only pass against one specific arrangement.
- **Resize is axis-relative** to the focused window's parent group: `resize
  left` is a no-op when that group is vertical. The scenario probes `left` and
  falls back to `up`.
- `spike-b move` prints "ok" unconditionally; the daemon's `set_frame` is the
  one with read-back verification.

## Verification status

- Host: `cargo test --workspace` (384), `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo fmt --check` — all green.
- VM (`dj@192.168.64.14`, macOS 15.6, SIP on, fullscreen 1728×1080): full
  scenario green; evidence in `scratch/vm-test-0{1..6}-*.png`.
- Not yet verified in the VM: **manual Super-key bindings** (the event tap is
  armed and unit-tested; osascript-based key injection hung on the Automation
  prompt, and synthetic keys need their own TCC grant). `spike-b tap --post`
  verified the tap swallows/posts synthetic events.

## Known gaps / backlog

1. **Transient probe failures are cached as "probed"** — a window whose probe
   fails once (AX lookup can return `CGError 1000` right after a resize) is
   never re-probed. Bounded retry on `Err` (not on a definitive answer) would
   fix it; observed once on Finder (74 px gap).
2. `cosmic-mac-ctl list-windows` reports the **layout's** desired frames, not
   AX ground truth (use `spike-b list` for that). Worth an `--ax` flag or an
   actual-frame column.
3. Probe calls `display::main_display()` from the WM thread — logs
   `list_displays() called off the main thread` at WARN each probe. Cache the
   display frame on the main thread instead.
4. No `Ping`/`CloseWindow{wid}`/tree-selector request variants in the protocol;
   `tree --workspace N` is composed by switching workspaces first (state-changing).
5. Tab bar: drawn inside the active member's top edge (placing it above the
   frame hid it behind the neighbour — focus-without-raise preserves z-order).
   Tab clicks and the × glyph are display-only (milestone 2).
6. Multi-display: parking falls back to the cached main display frame; the
   layout is a single workspace pool (`assign_display` not implemented).
7. Carbon hotkey fallback for Secure Event Input is documented
   (`SECURE_INPUT_FALLBACK_NOTE`), not implemented.
8. Docs: cite the cosmic-epoch submodule pins above in ARCHITECTURE.md; note the
   layout's orientation naming flip (our `Horizontal` = children side-by-side,
   opposite of cosmic-comp's split-axis naming).

## Manual setup (one-time, already done on this VM)

- Accessibility granted to the signed binaries; Screen Recording granted to
  `sshd-keygen-wrapper` (re-prompt every ~30 days on Sequoia).
- If TCC ever drops after a rebuild: trust the signing cert
  (`security add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db
  "$HOME/Library/Application Support/cosmicMac/signing/cert.pem"`).
- `scripts/vm-prep.sh --apply` remains confirm-gated for: auto-login,
  `pmset displaysleep 0`, "Displays have separate Spaces" off, native-tiling
  defaults off. Not yet applied on this VM.

## Next steps

1. Manual acceptance of the Super-key bindings in the VM (exit criteria of
   milestone 1) — needs a working key-injection path or hands on the keyboard.
2. Fix backlog items 1–3 above (small, high value).
3. Milestone 2: `cosmic-mac-session` (launchd agents, defaults), panel/dock
   (libcosmic Spike A first), launcher (pop-launcher in-process). Spike C's
   AppKit result already proves the tab-bar anchor path.
4. First ship: `gh repo create` (private), push, tag.
