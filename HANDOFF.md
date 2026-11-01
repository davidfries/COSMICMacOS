# HANDOFF.md — cosmicMac

> State as of 2026-11-01. Milestone 0 (spikes) **complete**, milestone 1
> (window manager) **implemented and passing the full VM integration scenario**
> (71 assertions incl. native full screen). Code-review backlog R1, R4, R5, R6, R6a
> and R11 (native full screen) are fixed; R12 remains. Milestone 2: the COSMIC panel
> is dropped in favour of the native menu bar; the session installer (3 launchd
> agents), the menu-bar status item and the resident launcher (Super tap) are
> implemented and verified in the VM; theming is in progress. Drag to stack
> (drop a tiled window on another tile's middle third) is implemented and
> verified; the full VM scenario passes with 78 assertions.
> Branch `milestone-2a`: commit `18f9597` holds the R-fixes, session installer and
> launcher spike; R11, the status item and the resident launcher are uncommitted
> on top of it.

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

~18k lines of Rust. All 405 workspace tests pass; clippy `-D warnings` and fmt clean.

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
6. **Super-key bindings verified end to end in the VM.** `examples/send-key.rs`
   (wm crate) posts a chord spelled like a COSMIC binding (`send-key Super+Shift+2`)
   as a synthetic HID event from the signed binary; the daemon's tap swallows it
   and runs the action. The scenario's "keybindings via the event tap" phase
   exercises Super+Y, Super+S, Super+1/2, Super+Shift+1/2, Super+Left/Right and
   Super+M and asserts the layout effect of each. Super+Q is deliberately not
   exercised: a missed tap would let Cmd+Q quit the single-document TextEdit,
   which is indistinguishable from Close.

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
- **A sleeping display enumerates as zero displays.** After the VM's 10-minute
  display sleep, `CGGetActiveDisplayList` returns nothing and `NSScreen` is
  empty; a daemon started then falls back to the hardcoded 1920×1080 work area,
  tiles past the real 1728 px edge, and "learns" clamps that are really the
  screen edge. `vm-test.sh` now wakes the display (`caffeinate -u -t 3`) before
  starting the daemon; `scripts/vm-prep.sh --apply` (`pmset displaysleep 0`)
  removes the cause. The daemon now waits for an active display before it
  manages anything (R6a).
- **`AXWindowMiniaturized` fires before `AXMinimized` reads true.** The
  attribute reads false for about 0.5 s after the notification (the minimize
  animation); the SkyLight window-hidden event (816) arrives at the same moment.
  Re-reading the window on the notification therefore always says "not
  minimized". The WM treats the AX minimize/deminimize notifications as
  authoritative (`PlatformEvent::WindowMinimized/WindowDeminimized`) and ignores
  read-based "it is back" triggers for 1.5 s after a minimize (`MINIMIZE_SETTLE`).
- **Native full screen moves the window to a Space of its own; mid-transition
  it is unreadable but not gone.** Traced on macOS 15.6 with the `fs-trace`
  example (`cargo build --release -p cosmic-mac-platform --example fs-trace`;
  TextEdit, `AXFullScreen` true/false and Ctrl+Cmd+F; desktop Space id 1 type
  0, full-screen Spaces type **4** from `SLSSpaceGetType`). Entering, in order:
  the app creates a **helper window** (level 0, the whole display, AX-readable,
  `fullscreen` false, **on no Space**: `SLSCopySpacesForWindows(0x7)` returns
  `[]`); the window itself becomes unreadable (no CGWindowList record, not in
  `AXWindows`, on no Space) **but `SLSGetWindowOwner` still returns its pid**;
  `AXWindowResized` to the display frame (the re-read fails); SLS **1326**
  (space-window-destroyed, payload Space 1) → `WindowDestroyed`, owner still
  fine, no 804; `NSWorkspaceActiveSpaceDidChangeNotification` about 0.55 s after
  the start (polling `SLSGetActiveSpace` sees it ~50 ms later, so the
  notification is prompt; also for Ctrl+Left/Right Space switches); SLS 1325
  (the full-screen Space) → `WindowCreated`, readable, `AXFullScreen` true, on
  the type-4 Space. Leaving mirrors it (helper on no Space, 1326, resize back
  while unreadable, 1325 on Space 1 readable within ~10 ms, then the
  notification). Other helpers: a 1728×44 strip on the full-screen Space that
  AX never reads, Dock windows at negative levels. A **real close** sends 1326
  too, but then `SLSGetWindowOwner` fails (CGError 1000) — owner lookup is the
  discriminator for "destroyed" vs "left its Space"; "on no Space" is the one
  for helper windows (an ordinary new window is already on Space 1 at
  `AXWindowCreated`). AX `AXWindows` only lists windows on the **current**
  Space, so `ax_window()` fails ("window N not found") for every desktop window
  while the full-screen Space is active; CGWindowList still lists them. An AX
  element looked up while the window was on the desktop **can** still move it
  from the full-screen Space (checked with `fs-trace cached-move`), but TextEdit
  resets the frame again when the desktop returns, so the WM does not cache
  elements (see R11).
- **TextEdit resets its other windows' frames across a full-screen round trip**
  (also without the WM): windows set through AX come back at their cascade
  defaults (185,75 646×418 …), right after the full-screen Space activates and
  again when the desktop returns; it sends `AXWindowMoved` for all of them at
  every Space change.
- **A focus change to a desktop window while a full-screen Space is active
  makes it key**: after the WM focused a neighbour of the window that went full
  screen, the next Ctrl+Cmd+F went to that neighbour (on the inactive desktop)
  and made it full screen as well.
- **An app stopped with SIGSTOP makes every AX call wait the full messaging
  timeout** (1 s, error -25204). The journal replay's "1 s budget" is per pass,
  and one pass over three windows took 9 s; observer registration for that pid
  took another 5 s and failed, so the WM gets no AX notifications from that app
  until it is re-observed.
- **A sleeping VM display swallows synthetic clicks**: the cursor moves, but
  no drag happens and no tap sees the events. Wake it first with
  `(caffeinate -u -t 30 >/dev/null 2>&1 &)`; without the redirect the ssh
  call waits for caffeinate to exit.
- **A title-bar drag needs time before the release**: dragged events and a
  release posted within ~3 ms move nothing. Clicks on the tab-bar overlay
  reach the window under it (a title-bar drag of a stack's visible member that
  starts under the bar works). See "Drag to stack" for the event timing.
- **Accessibility is charged per launching context.** Over ssh the grant is
  sshd's, so a hand-started WM works; started by launchd, `cosmic-mac-wm` needs
  its own entry in System Settings > Privacy & Security > Accessibility or it
  exits 1 ("Accessibility permission is not granted").

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
- **The focus oracle must not fall back to the spatial search for tiled windows.**
  `Layout::act_focus` leaves focus unchanged when a tiled window has no tree
  neighbour (e.g. `Left` from the leftmost member of a horizontal pair under a
  vertical root: no wrap, because the root does not split on that axis). The
  Python oracle used to return None there and fall through to the geometric
  search, which only the floating path uses; it now returns `from_wid`. The bug
  was latent until the keybinding phase hit that tree shape.
- Synthetic keys: `CGEventPost` to the HID tap from a trusted (Accessibility)
  process is seen by another process's `CGEventTap` like a real key press. No
  Automation prompt, no extra TCC grant beyond Accessibility.

## Verification status

- Host: `cargo test --workspace` (637), `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo fmt --check` — all green (clippy only
  prints the known `block v0.1.6` future-incompat note).
- R11 (2026-11-01, VM, three TextEdit documents in one instance, daemon at
  `RUST_LOG=debug`): `AXFullScreen` true on one window through System Events →
  "destroy notification, but it still exists (Detached)" → suspended, the other
  two re-flow; the animation's helper windows are deferred and given up on,
  never managed; `AXFullScreen` false → "active Space changed to Some(1)",
  drifted siblings framed again, the window "is back", and `list-windows` shows
  all three `=` in their **original** tiles. Same with Ctrl+Cmd+F on and off,
  including Ctrl+Left to the desktop while in full screen (siblings
  re-asserted, `=`) and Ctrl+Right back. Screenshots
  `scratch/r11-fs-on.png`, `scratch/r11-fs-off.png`. Each core unit test of
  R11 was checked to fail with its fix reverted.
- VM checks of the review fixes (2026-10-31, hand-driven over ssh with
  `RUST_LOG=debug`):
  - R5 minimize: `AXMinimized` set through System Events and the real
    minimize button (AXPress) both suspend the window at once and re-flow its
    neighbours; it stays suspended past the settle time; setting `AXMinimized`
    false re-tiles it. Cmd+H suspends and un-hiding resumes.
  - R5 native full screen: failed then (R11); fixed with R11, see above.
  - R1 with Terminal: only `min 150×105` learned, no max; Terminal sits at
    962×483 in its 960×485 tile (its character grid; no centring gaps). An
    external move snaps back with no re-probe and keeps the constraints (one
    extra clamp+retile, as expected).
  - R6 crash: window parked on workspace 2, WM `kill -9`, TextEdit
    `kill -STOP`, WM restarted (replay fails, entries kept), `kill -CONT`,
    SIGTERM: every window back at its original frame, journal removed; the
    corner frame was never recorded as an original.
  - R4: Spotlight not tiled; the panel is not managed.
  - R6a: the daemon waits for the display and starts when it wakes.
  - `bash scripts/vm-test.sh` passes (66 assertions) on this uncommitted state,
    including the minimize fix; with the R11 full-screen phase it passes with
    71 assertions (2026-11-01).
- VM (`dj@192.168.64.14`, macOS 15.6, SIP on, fullscreen 1728×1080): full
  scenario green; evidence in `scratch/vm-test-0{1..6}-*.png`.
- Drag to stack (2026-11-01): `bash scripts/vm-test.sh` passes with **78
  assertions** (71 + the 7 of the drag phase), with the VM's Terminal app
  hidden for the run; evidence `scratch/vm-test-09-drag-stack.png`, log
  `scratch/vm-test-drag.log`. Hand-driven checks: see "Drag to stack".
- VM: the keybinding phase (synthetic chords via `send-key`) passes; evidence in
  `scratch/vm-test-07-key-stack.png`. The daemon's `list-windows` ACTUAL column
  shows `=` for every tiled TextEdit window against the live daemon.
- Not verified at runtime: the display snapshot used by the probe (backlog 3
  fix) only matters when an app clamps a resize, and the scenario's TextEdit
  windows never do. Compile/clippy/unit-test covered only.

## Known gaps / backlog

1. ~~Transient probe failures cached as "probed"~~ **done**: `Err` from the
   probe now calls `LearnedConstraints::defer_probe()`; the window is re-probed
   on later clamps up to `MAX_PROBE_ATTEMPTS` (3), then given up on (unconfirmed,
   never pinned). `Ok(None)` is still definitive.
2. ~~`list-windows` reports only layout frames~~ **done**: `WindowState.actual_frame`
   (serde-optional, filled by the daemon from AX) and an ACTUAL column in the
   CLI (`=` when within 1 px of FRAME, the real frame otherwise, `?` if unknown).
3. ~~Probe calls `NSScreen` from the WM thread~~ **done**: `display::list_displays()`
   on the main thread refreshes a snapshot; `cached_main_display()` /
   `cached_displays()` serve the probe's grow origin and `display_frame`.
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

## Code-review backlog (2026-10-31 review of milestone 1)

A full read of the committed code by a review agent, spot-checked in the main
session. Items fixed the same day are struck through. The rest are ordered by
impact; none blocks milestone 1, but R1–R3 should land before milestone 2
depends on the daemon being long-running.

**Major**

- ~~R1. Post-probe clamps still pin bounds~~ **done** (candidate promotion
  needs a probe explanation or a repeat, `SNAP_TOLERANCE` 24, a grow shortfall
  ≤ `GROW_STEP/2` is a snap; VM-checked with Terminal). Original:
- R1 (original). **Post-probe clamps still pin bounds** (`core.rs learn_constraints`,
  `constraints.rs promote_candidates`, `window.rs` grow-probe max). After one
  successful probe every later directional candidate is promoted, so a
  grid-snapping app (Terminal: 900→896, 500→504) ends up with `max=896×496`
  and is centred with gaps for the session. The grow probe itself records
  a snapped +126 as a maximum. Fix: promote a post-probe candidate only when it
  lies inside the probe-confirmed range or is seen twice; treat a grow shortfall
  as a max only when it exceeds half of `GROW_STEP` or repeats.
- ~~R2. Echo suppression and the focus debounce are timed at processing, not
  capture~~ **done**: `Message::Platform(event, Instant)` stamps capture time
  in the platform callback; `is_echo_at`/`is_recent_focus_event_at` compare
  against `programmed.at + debounce`; `SetFrame`/`reapply` mark the window in
  flight *before* the first AX call; `show()` marks the unparked frame; startup
  batches all inserts and applies one compacted command list (6 windows: 6
  `set_frame` calls, 1 focus). Original description kept below for context.
- R2 (original). Echo suppression and the focus debounce are timed at processing, not
  capture** (`commands.rs is_echo`, `core.rs recent_focus`; `PlatformEvent`
  has no timestamp). During a long AX batch (startup, workspace switch, probe)
  queued intermediate frames age past 150 ms and are treated as user drags:
  constraints invalidated, snap-backs, re-probes with visible flicker; a late
  `AppActivated` can steal focus. Fix: stamp `Instant` in the platform
  callback, compare capture time against `programmed.at + debounce`, mark the
  window in flight before the first AX set, and batch startup into one sync.
- ~~R3. The event tap shares the main run loop with blocking AX IPC~~ **done**:
  the tap runs on its own `event-tap` thread/run loop (creation errors still
  fail startup); `refresh_apps` enumerates owner pids only
  (`window::list_window_pids`, no AX) and prunes observers of exited
  processes (run-loop source invalidated before the context is freed);
  `AXUIElementSetMessagingTimeout` is set globally to 1 s at startup.
  Original description kept below for context.
- R3 (original). The event tap shares the main run loop with blocking AX IPC**
  (`main.rs refresh_apps` every 2 s → `enrich_with_ax`, ~7 AX calls per window;
  AX callbacks call `element.frame()`; no `AXUIElementSetMessagingTimeout`,
  default ~6 s). A beach-balling app freezes keyboard input system-wide and
  gets the tap disabled. Fix: tap on its own thread/run loop; `refresh_apps`
  collects pids without AX enrichment; set a 0.25–0.5 s messaging timeout.
- ~~R4. Windows created after startup skip the level and minimized filters~~
  **done** (`ToplevelInfo.level/minimized/fullscreen`, `rules::is_manageable`,
  `WindowLevelChanged` drops a managed window). Original:
- R4 (original). **Windows created after startup skip the level and minimized filters**
  (`core.rs` window-created → `MacPlatform::window_info`; `to_toplevel` drops
  `level`/`is_minimized`; startup filters `level == 0 && !is_minimized`).
  Spotlight, launcher panels and overlays ≥40×30 get tiled. Fix: carry level
  and minimized into `ToplevelInfo` and require level 0 plus an AX role.
- ~~R5. Minimize, Cmd+H and native full screen~~ **done** (full screen with
  R11). Suspended windows leave the layout and come back with their learned
  constraints (`Core::suspended`, `AppHidden/AppShown`,
  `WindowMinimized/WindowDeminimized`, `CommandApplier::release_window`), into
  the **slot they left** (`Layout::slot_of` / `Layout::window_restored`: same
  split position and weights, stack index, floating frame; next to the nearest
  surviving neighbour if the tree changed; like `window_added` when the slot's
  workspace is not active). Windows suspended together come back in reverse
  order. Original:
- R5 (original). **Minimize, Cmd+H and native fullscreen are ignored** (`WindowHidden`/
  `WindowShown` dropped in `core.rs`; `AXApplicationHidden/Shown` dropped in
  `observers.rs`). Cmd+M leaves a hole, focus lands on the invisible window,
  and a re-tile of a minimized window can probe it and pin `Frozen`. Fix:
  remove/suspend on hide, re-insert on show, never probe a minimized window.
- ~~R6a. Startup with no active display uses a hardcoded 1920×1080 work area~~
  **done** (startup waits for an active display; zero displays keep the last
  area). Original:
- R6a (original). **Startup with no active display uses a hardcoded 1920×1080 work area**
  (`macos.rs main_display_frames_now` fallback). Seen when the VM display was
  asleep: wrong tiles, false clamps learned. Fix: when enumeration yields no
  display, wait for the display-reconfiguration callback (or poll) before
  managing anything, and never persist constraints learned before the first
  real work area.
- ~~R6. Journal replay discards entries it failed to restore~~ **done**
  (failed entries kept and retried at startup, corner frames never recorded,
  entries carry the owner pid; VM crash scenario passes). Original:
- R6 (original). **Journal replay discards entries it failed to restore**
  (`journal.rs restore`). A transient AX error after `kill -9` leaves a window
  at the park corner, the new daemon records the corner as its original frame,
  and a clean shutdown later parks it for good. Fix: write failed entries back
  and retry; prefer a stale entry in `record_original`; treat a startup frame
  at the corner as parked.
- ~~R11. Native full screen loses the window~~ **done**. As built
  (`core.rs` module docs, "Native full screen and Space changes"):
  - `PlatformBackend::window_presence` (`Gone` / `Detached` / `OnSpace {
    fullscreen }`, from `SLSGetWindowOwner` + `SLSCopySpacesForWindows` +
    `SLSSpaceGetType`). `WindowDestroyed` only forgets a window that is `Gone`;
    one that still exists is suspended as away in full screen (constraints,
    journal entry and slot kept). A full-screen suspension does not move
    platform focus (the full-screen window keeps the keyboard).
  - Re-checks: core queues `(wid, delay)` (`Core::take_rechecks`, back-off
    `RECHECK_DELAYS` 0.5/1.5/4 s); `daemon.rs` turns them into tokio timers
    (`Message::Recheck`) and calls `Core::recheck`. Used for unreadable reads
    (display-sized resize mid-transition), full-screen-suspended windows,
    failed snap-backs and windows not admitted yet. After the last delay a
    pending window is dropped; a suspended one that still exists is kept
    (the exit transition detaches it again) and waits for the next Space
    change or an event about it.
  - New windows: managed only when readable, AX-backed, on an ordinary Space
    and not within `TRANSITION_SETTLE` (1 s) of a transition sign; otherwise
    pending until a re-check. Keeps the animation's helpers out.
  - `PlatformEvent::ActiveSpaceChanged { space, fullscreen }` from
    `NSWorkspaceActiveSpaceDidChangeNotification` (block observer on the main
    run loop, `PlatformObserver::start_space_observer`). On it the core
    re-reads full-screen-suspended windows (resuming those that are back, last
    suspended first) and, on a desktop Space, **re-asserts** the layout: every
    managed window that drifted from its tile is framed again
    (`CommandApplier::try_reapply_frame`), every window that should be parked
    is parked again (`CommandApplier::repark`). While a full-screen Space is
    active, moves of managed (desktop) windows are ignored: they are the app's,
    not the user's, and AX cannot undo them.
  - Remaining: while the full-screen Space is active the app's other windows
    sit at its cascade frames (list-windows ACTUAL ≠ FRAME); invisible to the
    user and corrected when the desktop returns. Caching AX elements would
    allow moving them from the full-screen Space (verified), but TextEdit
    resets them again on the way back, so it was not worth the stale-element
    risk. A window moved to another ordinary desktop Space (Mission Control)
    is also treated as suspended until it is back on the active Space
    (multi-desktop is not modelled). Only the main display is modelled.
  Original:
- R11 (original). **Native full screen loses the window** (found in the VM while checking
  R5). Entering full screen: the move to the display-sized frame triggers the
  full-screen re-read, but AX cannot read the window mid-transition, so it is
  not suspended; SkyLight's `WindowDestroyed` for it then makes the WM forget it
  (constraints and journal entry too), and the later `WindowCreated` read fails
  ("disappeared before it could be inspected"). After leaving full screen the
  window stays unmanaged. The app's other windows are moved by the app while
  their Space is inactive, the snap-back fails (AX cannot see other Spaces) and
  is never retried, so they stay out of their tiles. Transition helper windows
  can be managed briefly. Needs a design decision, roughly: treat a destroy that
  follows a display-sized frame (or of a window SkyLight still knows) as
  "left for a full-screen Space" and suspend instead of forgetting; a delayed
  re-check (`Message::Recheck(wid)` from a tokio timer in `daemon.rs`) for
  reads that fail during a transition; re-assert the active workspace's tiles
  when the desktop Space becomes active again (a Space-change notification);
  ignore windows created during a transition until they settle.
- **R12. Commands that fail while an app is unresponsive are never retried.**
  In the R6 run the startup `SetFrame`s for the stopped TextEdit failed
  (-25204); after `kill -CONT` the layout believed the windows were tiled while
  one was still at the park corner (ACTUAL column shows it). Also its AX
  observers failed to register and are not retried for a live pid.
- ~~R7. Closing a window trims the active empty workspace~~ fixed (layout).
  **Semantics changed with it:** the active workspace is never trimmed, and an
  empty trailing workspace is trimmed when the user *leaves* it (`switch_to`).
  So `MoveToWorkspace` out of workspace 2 keeps you on an empty workspace 2,
  and `Workspace(1)` then removes it. The VM scenario and nine layout tests
  were updated. cosmic-comp additionally keeps one empty workspace at the
  end at all times; not modelled yet (backlog).
- ~~R8. User moves of floating windows are not written back~~ fixed (layout +
  core).
- ~~R9. Parking uses the startup display frame after a resolution change~~
  fixed (`main_frame` refreshed by the display handler).
- ~~R10. A panic in the runtime thread leaves a zombie that swallows keys~~
  fixed (drop guard sets the shutdown flag; tap passes keys through once the
  daemon channel is gone).

**Minor**

- ~~`Request::Restore` leaves the tab bar on screen~~ fixed.
- ~~Position-only moves wipe learned sizes; invalidation never reached the
  layout~~ fixed.
- ~~Key autorepeat re-fires one-shot actions (holding Super+Q closes window
  after window)~~ fixed (repeats only pass for Focus/Move/Resize).
- ~~Moving a window out of a nested group moves the whole group~~ fixed (i3
  semantics, including at the ancestor edge).
- ~~ToggleTiling off→on rebuilds a halving chain~~ fixed (re-insert next to
  the largest tile, ties → last window, then relabel to keep the order).
  ToggleStacking beside a group now stacks with the group's nearest leaf
  (`Tree::edge_leaf`).
- ~~Startup is quadratic~~ fixed with R2 (batched inserts, one sync).
- Windows rejected at creation (0×0, AX not ready) are never re-checked.
  Re-evaluate unknown wids on later Resized/Title events.
- ~~Drop order in `EventTap` / `PlatformObserver`~~ fixed (invalidate first).
- ~~AX observers are never removed for dead pids; a reused pid is never
  observed again~~ fixed (dead pids pruned; reuse detected by process start
  time).
- SLS callbacks run on the pump thread, not the main run loop as documented;
  title events do AX work there; the 10 ms sleep loop wakes 100×/s. Block on
  the event port with `mach_msg` instead.
- ~~A failed `window_bounds` read becomes a `WindowMoved` to `Rect::default()`~~
  fixed (the event is dropped).
- IPC: line length unbounded; invalid UTF-8 closes the connection without an
  error reply (socket is 0600, low impact).
- ~~Hollow test `user_move_of_a_tiled_window_snaps_back`~~ fixed (zeroes the
  applier's debounce).
- ~~Missing coverage: minimize, journal restore errors, post-probe candidate
  promotion (R1)~~ covered with R1/R5/R6. Stale echoes after the debounce:
  `move_captured_after_the_debounce_is_a_user_move`.
- Terminal costs one extra clamp+retile per layout change (its grid never
  matches the tile; R1 note).

**Confirmed solid by the review** (no need to re-check): journal atomic writes
and corrupt-file handling; CF ownership in `ax.rs` and the tab bar; tap
disable-by-timeout re-enable and key-up swallowing; layout clamping cannot
panic and tiles never overlap; stack active-index bookkeeping; SLS/AX
double-`WindowCreated` dedup; IPC malformed-line handling and socket
permissions; bounded `handle_clamp` recursion.

## Milestone 2 status

- **Decision (2026-10-31): no COSMIC top panel.** Replacing the macOS menu bar
  means hiding it, which hides every app's menus. The native menu bar and Dock
  stay; a menu-bar `NSStatusItem` showing the active workspace (menu: switch
  workspace, toggle tiling) replaces the panel. PLAN.md milestone 2 items 1–2
  are rewritten accordingly. `crates/cosmic-mac-panel` stays as Spike A
  reference code, and the WM keeps its shell-strip reservation (`reserve.rs`,
  `rules::SHELL_APP_IDS`) for a possible COSMIC dock.
- **Launcher** (`crates/cosmic-mac-launcher`, uncommitted): a libcosmic
  overlay with its own `Plugin` trait (pop-launcher's plugins crate does not
  build on macOS). The spike rendered in the VM (centred, app icons, key
  focus, `=2^10` + Return copies 1024) but started in ~1.1 s, flashed a Dock
  icon (the embedded `Info.plist` gives no bundle id) and had no trigger. Now
  resident, bundled, opened by a Super tap and with files/terminal plugins;
  see "Launcher" below. **Not yet verified in the VM.**
- **Theming** (uncommitted): dark/light from COSMIC's Mode config, else the
  macOS appearance, for the launcher (libcosmic theme) and the WM tab bar
  (COSMIC palette). See "Theming" below. Verified in the VM (2026-11-01).
- **Spike A passed** and is merged into `main` (`ad1ef56`, `6cc9230`, merge
  `5fbec1c`): libcosmic pinned at `95236a9` (master 2026-10-27)
  with `default-features = false` + winit/tokio/multi-window/wgpu builds on
  macOS **unpatched** (clean release build 77 s) and renders through wgpu on
  Metal in the VM ("Apple Paravirtual device"). `crates/cosmic-mac-panel` is
  a 32 px bar with a label and a clock; after `window::open_events` fires,
  `window::run_with_handle` reaches the winit `NSWindow` via the
  raw-window-handle `NSView` and sets borderless, `NSStatusWindowLevel`,
  CanJoinAllSpaces|Stationary and the top-edge frame. Evidence:
  `scratch/spike-a-panel.png`, `scratch/spike-a-panel-tick.png`.
- Done in `6cc9230`: `setActivationPolicy(Accessory)` (no Dock icon) and
  private `-[NSWindow _setPreventsActivation:YES]` (guarded by
  `respondsToSelector:`) so clicks on the bar do not activate it. An `NSPanel`
  class swap does not work and is unsafe with winit's KVO subclass.
- Open: the panel still takes focus for ~100 ms at launch, because iced builds
  winit's event loop with no activation policy. Fix with a small `iced_winit`
  patch or by launching it as a `.app` with `LSBackgroundOnly`/`LSUIElement`
  (planned for the `cosmic-mac-session` installer).
- The WM must learn to ignore the panel window (R4: level filter for windows
  created after startup) and reserve its height from the work area.

### Session installer (`cosmic-mac-session`)

- **Verified end to end in the VM (2026-11-01)** once `cosmic-mac-wm` had its
  own Accessibility grant (without it the launchd-started WM exits 1; see the
  platform facts): `install` → WM running under launchd, macOS shows
  "Background Items Added"; window parked on ws2 + `kill -9` → launchd restarted
  the WM within ~1 s, the journal restored all 3 windows and they were re-tiled;
  `uninstall` → clean shutdown, frames restored, journal removed. The grant
  survives redeploys because the binaries are signed with the stable identity
  (an unchanged binary was used for this check). Installs the WM
  as a launchd agent; it changes no macOS defaults (the native menu bar and Dock
  stay as they are).
- `install [--bin-dir DIR] [--dry-run]` writes
  `~/Library/LaunchAgents/com.cosmicmac.wm.plist` atomically (mode 0644):
  `<bin-dir>/cosmic-mac-wm`, `RunAtLoad`, `KeepAlive { SuccessfulExit = false }`
  (restart after a crash, not after the clean exit 0 that follows SIGTERM),
  `ThrottleInterval 5`, `ProcessType Interactive`,
  `LimitLoadToSessionType Aqua`, `RUST_LOG=info`, stdout/stderr to
  `~/Library/Logs/cosmicMac/wm.log`. Then it runs
  `launchctl bootstrap gui/<uid>`. If the label is already loaded, it boots it
  out first and polls `launchctl print` until it is gone, because bootout can
  return before the process exits ("36: Operation now in progress") and
  bootstrap then fails with "5: Input/output error". That makes reinstall
  idempotent. `--bin-dir` defaults to the directory of the running
  `cosmic-mac-session`.
- `uninstall [--dry-run]` runs `launchctl bootout` (SIGTERM, so the WM restores
  every window) in reverse agent order and removes the plists. `status` shows
  for each agent whether its plist is installed, whether launchd has it loaded,
  and its state, pid, last exit code and program.
- `--dry-run` prints every change (commands, file writes, directories)
  without making it. Read-only `launchctl print` queries still run, so the
  plan matches the machine.
- The agent list is `AGENTS` in `crates/cosmic-mac-session/src/agents.rs`. The
  launcher, or a menu-bar helper, is one more entry each. A bundled app names
  its executable relative to `--bin-dir`
  (`Foo.app/Contents/MacOS/foo`). Every `launchctl` call goes through a
  `CommandRunner` trait. 36 host tests use a scripted fake runner and a temp
  home, plus one `plutil` lint/round-trip of the generated plist.
- In the VM: first stop any WM started by hand or by `vm-test.sh`
  (`bash /tmp/cosmicmac-vm-helpers.sh stop-daemon`, or
  `kill -TERM $(pgrep -x cosmic-mac-wm)`), because two daemons would fight
  over the windows. Then:

  ```
  scripts/deploy.sh -p cosmic-mac-wm -p cosmic-mac-session
  ssh dj@192.168.64.14 '~/cosmicMac/bin/cosmic-mac-session install --dry-run'
  ssh dj@192.168.64.14 '~/cosmicMac/bin/cosmic-mac-session install'
  ssh dj@192.168.64.14 '~/cosmicMac/bin/cosmic-mac-session status'
  ssh dj@192.168.64.14 'tail ~/Library/Logs/cosmicMac/wm.log'
  ```

  Checked (2026-11-01): Accessibility holds when launchd (not sshd) starts the
  binary; macOS shows a "Background Items Added" notice and lists the agent
  under Login Items > Allow in the Background; after `kill -9` launchd restarts
  the WM (at most one start per 5 s) and the new process replays the frame
  journal; `uninstall` restores the frames;
  a redeploy followed by `install` swaps in the new binary. `vm-test.sh`
  reuses a reachable daemon and leaves it running, so it also works against
  the agent.

### Menu-bar status item (`cosmic-mac-status`) and IPC subscriptions

**Implemented and verified in the VM (2026-11-01):** launchd agent running and
subscribed; title follows the active workspace (1 → 2); menu shows workspaces
with window counts, Tiling and Float items; clicking "Workspace 2" and "Tiling"
acts; opening the menu does not take focus from the frontmost app; after
`kill -9` of the WM it re-subscribes ~1 s after launchd restarts the WM.
System Events reaches it as `menu bar item 1 of menu bar 1` of process
`cosmic-mac-status` (useful for scripted tests).

- **Separate process**, not part of the WM. The WM's main thread pumps a plain
  CFRunLoop for AX/SkyLight observers and has no `NSApplication`, and a
  status-item crash must not take the WM down. `crates/cosmic-mac-status` is
  an AppKit app (objc2 0.6 / objc2-app-kit 0.3). It sets the `Accessory`
  activation policy before `-[NSApplication run]`, so it has no Dock icon and
  no app menu, and it creates the `NSStatusItem` in
  `applicationDidFinishLaunching:`.
- **Threads.** The main thread runs only AppKit. A subscription thread owns
  the `Request::Subscribe` connection and reconnects with backoff (250 ms,
  doubling to 2 s, reset after each received state). It hands each report to
  the main thread through `dispatch2::DispatchQueue::main().exec_async`, which
  AppKit drains in the common modes, so updates also arrive while the menu is
  open. An action thread sends the menu's one-shot requests. AppKit is never
  touched off the main thread.
- **Thin glue.** `src/model.rs` maps the connection state to a `MenuModel`
  (title, tooltip, entries with checkmarks, enabled flags and commands), as
  pure functions with unit tests. `src/appkit.rs` copies a model into AppKit,
  skipping models that did not change, and rebuilds the menu, which has at
  most about 15 items. The menu-item tag carries the command:
  workspace `n` → `n`, Tiling 1001, Float 1002, Quit 1003.
- **What it shows.** The title is the active workspace's name (`1`…`9`, cut to
  12 characters), and the tooltip reads
  `cosmicMac: workspace 2 of 3, tiling on`. The menu has:
  - one item per workspace, `Workspace 1 — 3 windows`, with a checkmark on the
    active one; a click sends `Action::Workspace`;
  - one more, empty `Workspace N+1` when the last workspace has windows and
    the pool holds fewer than 9. The layout trims empty trailing workspaces,
    so without it the menu could not reach a new workspace;
  - a separator;
  - "Tiling", checked when the active workspace tiles (sends `ToggleTiling`);
  - "Float Focused Window", checked when the focused window floats and
    disabled when nothing is focused or tiling is off, because the layout
    ignores the toggle then (sends `ToggleWindowFloating`);
  - a separator, then "Quit", which quits only the status item.
- **When the WM is not reachable** (no socket, or the WM refused the
  subscription), the title is `◌` and the menu shows a disabled "Window
  manager not running" item plus Quit. The status item retries in the
  background. It logs the first failure at info and the rest at debug.
- **Session agent** `com.cosmicmac.status` (second entry in `AGENTS`):
  `<bin-dir>/cosmic-mac-status`, logs to `~/Library/Logs/cosmicMac/status.log`,
  with the same KeepAlive policy as the WM. The menu's Quit exits 0, so launchd
  does not restart it until the next login, `install`, or
  `launchctl kickstart gui/$(id -u)/com.cosmicmac.status`. `install` refuses
  to proceed while any agent's program is missing, so deploy `cosmic-mac-status`
  along with the WM from now on.

**Subscription protocol** (`Request::Subscribe` / `Response::State`, see
`crates/cosmic-mac-protocol/src/ipc.rs` and `shell.rs`):

```
-> {"request":"subscribe"}
<- {"response":"state","data":{"workspaces":[{"index":1,"name":"1","active":true,"window_count":2}],"tiling":true,"focused_floating":false,"max_workspaces":9}}
<- {"response":"state","data":{"workspaces":[{"index":1,"name":"1","active":false,"window_count":2},{"index":2,"name":"2","active":true,"window_count":0}],"tiling":true,"focused_floating":null,"max_workspaces":9}}
```

- After `subscribe`, the connection carries `state` lines only: the current
  state at once, then one line per change, until either side closes it.
  `focused_floating` is `null` when no window is focused. The daemon ignores
  any input on a subscribed connection and uses it only to notice the
  disconnect. Every other request keeps the one-reply-per-line framing, so old
  clients are unaffected. A WM that predates subscriptions answers
  `{"response":"error","data":"invalid request: unknown variant ..."}`.
- In the daemon, `shell_state::Publisher` wraps a `tokio::sync::watch`
  channel. `daemon.rs` calls `update(core.layout())` after startup and after
  every handled message. The snapshot compares with the last one and notifies
  only on a change; it costs a ≤ 9-entry workspace list and computes no frames.
  `ipc.rs` answers `Subscribe` itself (`Core` only has a match arm returning a
  one-shot snapshot). A watch channel is used rather than a broadcast one so
  that a slow subscriber skips intermediate states instead of lagging, and the
  daemon loop never blocks on a subscriber.
- `cosmic-mac-ctl watch [--json]` prints the stream: one
  `workspace=2 workspaces=3 tiling=on focused=tiled windows=1:2,2*:1,3:0` line
  per state, or one compact JSON payload per line with `--json`. It exits 3
  when the daemon closes the stream. `Client::subscribe` in `cosmic-mac-ctl`
  is the shared client; the status item uses it too.
- Host tests:
  - `ipc.rs`: initial state, each change, nothing when unchanged, one
    subscriber leaving while the others keep receiving (its task ends), one-shot
    requests next to a subscription, input on a subscribed connection ignored,
    publisher drop closing the stream.
  - `tests/subscription.rs`: the real `daemon::run` with an in-memory backend.
  - `shell_state.rs`: snapshot and publisher.
  - The status crate (19 tests): menu model, backoff, reconnect after
    EOF, late WM start, refused subscription, action worker.
  - ctl client and CLI `watch` tests, and the session agent entry.

**VM test steps** (not run yet). First stop any hand-started WM
(`bash /tmp/cosmicmac-vm-helpers.sh stop-daemon`):

```
scripts/deploy.sh -p cosmic-mac-wm -p cosmic-mac-ctl -p cosmic-mac-session -p cosmic-mac-status
ssh dj@192.168.64.14 '~/cosmicMac/bin/cosmic-mac-session install --dry-run'   # lists both agents
ssh dj@192.168.64.14 '~/cosmicMac/bin/cosmic-mac-session install'
ssh dj@192.168.64.14 '~/cosmicMac/bin/cosmic-mac-session status'   # wm and status running
ssh dj@192.168.64.14 'tail ~/Library/Logs/cosmicMac/status.log'    # "subscribed to ..."
```

1. Stream: run `ssh dj@192.168.64.14 '~/cosmicMac/bin/cosmic-mac-ctl watch'`
   in one terminal. Run `cosmic-mac-ctl action workspace 2`, then
   `action toggle-tiling`, `action workspace 1` and `list-windows` in another.
   Expect one line at once, one line per action, and no line for
   `list-windows`.
2. Title: run `just screenshot`. The right side of the menu bar shows `1`.
   After `cosmic-mac-ctl action workspace 2` it shows `2` within about 100 ms
   (take a second screenshot).
3. Menu (click the item in the UTM window): the workspaces are listed with a
   checkmark on the active one, plus an empty next workspace; Tiling is
   checked; then Float Focused Window and Quit. Choosing `Workspace 2`
   switches, choosing Tiling un-tiles and the checkmark clears, and choosing
   Float Focused Window floats the window that was focused before the click.
   Opening the menu must not take focus from that window, so check
   `cosmic-mac-ctl list-windows` for the focused flag.
4. No Dock icon, and not managed: no Dock icon appears;
   `lsappinfo list | grep -A6 cosmic-mac-status` shows `type="UIElement"`; and
   `cosmic-mac-ctl list-windows` lists no cosmic-mac-status window.
5. Reconnect: run `kill -9 $(pgrep -x cosmic-mac-wm)`. The title turns `◌`
   and the menu shows "Window manager not running". Once launchd restarts the
   WM (within ≤ 5 s), the title returns within ≤ 2 s more.
6. Quit: choose Quit. `cosmic-mac-session status` shows the agent not running
   with last exit 0, and it is not restarted. Then
   `launchctl kickstart gui/$(id -u)/com.cosmicmac.status` brings it back.
7. `uninstall` stops both agents; the status item disappears.

Starting the status item by hand from an ssh shell is untested (sshd is
outside the Aqua session); use the launchd agent as above.

### Launcher (`cosmic-mac-launcher`)

**Verified in the VM (2026-11-01):** the launchd agent starts hidden (warm-up
~1 s, gives focus back); a socket `toggle` shows it with the first frame 65 ms
after the request, bundle id `com.cosmicmac.cosmic-mac-launcher`, Accessory
policy (no Dock icon); typed keys reach it (`=2+2` → 4) while the menu bar
still shows the previous app; Escape hides it and focus returns; `send-key
--tap-super` toggles it, a 600 ms hold and Cmd+C do not; with the agent booted
out a Super tap starts it on demand via the bundle. Not yet exercised: `find`,
`run`, clicking outside to hide.

- **Resident mode.** The launchd agent `com.cosmicmac.launcher` starts it
  hidden; the process stays. Escape, activating a result and losing key focus
  hide it. Hiding orders the window out, hides the app (so AppKit gives focus
  back to the previous app) and **closes** the iced window; showing opens a
  fresh one. With `no_main_window`, libcosmic runs an iced daemon, which
  neither exits nor drops its wgpu compositor when its last window closes, so
  fonts, GPU pipelines, the app list and the icon cache stay warm. A shown
  launcher costs one window creation, expected well under 100 ms against the
  ~1.1 s cold start. Why not keep a hidden window: iced redraws every window
  after every update, and an off-screen `CAMetalLayer` runs out of drawables,
  `nextDrawable` blocks 1 s, and iced's present-error branch re-requests
  redraws forever. While hidden, only the socket subscription runs. A hidden
  start opens one never-shown warm-up window (the compositor is created with
  the first window) and closes it after its first frame (3 s timeout).
  winit activates the app at `didFinishLaunching` (iced has no hook to stop
  it), so a hidden start calls `NSApp.hide` at 0/400/1200 ms while nothing
  is shown. Each show logs `window opened` / `window visible` /
  `first frame … ms after the show request` (info, `launcher.log`).
- **Socket protocol** (`cosmic_mac_protocol::launcher`):
  `~/Library/Application Support/cosmic/cosmic-mac-launcher.sock` (mode 0600,
  next to the WM socket). One command per line: `toggle`, `show`, `hide`
  (case-insensitive, trimmed, ≤ 256 bytes). Each line gets `ok` or
  `error: <reason>`, and a connection may send several lines. `ok` means the
  command was queued, not that it has run yet. `show` clears the query,
  centres on the main screen and takes focus (also when the launcher is
  already shown). `toggle` hides a shown launcher and otherwise shows it.
  Try it with `echo toggle | nc -U ~/Library/Application\ Support/cosmic/cosmic-mac-launcher.sock`.
- **Single instance:** `flock` on `cosmic-mac-launcher.lock` next to the
  socket, taken before binding, so a socket file found then is stale and is
  replaced. A second launcher started with `--show` forwards `show` (retrying
  for 3 s while the first starts) and exits 0. Without `--show` it just exits
  0, which the agent's `KeepAlive { SuccessfulExit = false }` does not
  restart. Flags: `--show` and `-h/--help`. LaunchServices' `-psn_…`
  argument is ignored.
- **Bundle:** `scripts/bundle.sh` assembles
  `target/release/bundles/cosmic-mac-launcher.app/Contents/{Info.plist,PkgInfo,MacOS/cosmic-mac-launcher}`
  from `crates/cosmic-mac-launcher/Info.plist`
  (`CFBundleIdentifier com.cosmicmac.cosmic-mac-launcher`, `LSUIElement`,
  `NSHighResolutionCapable`, version 0.1.0; tests in `src/main.rs`
  check them). It lints the bundle with `plutil` and signs it with `sign.sh`
  (which now takes `.app` directories; identifier = `com.cosmicmac.<name>`).
  `deploy.sh` runs it whenever `target/release/cosmic-mac-launcher` exists
  and rsyncs the bundle (`--delete`) next to the binaries. With a bundle id
  present, winit leaves the policy to `LSUIElement` (Accessory from launch,
  no Dock icon). The plain binary still works, switches to Accessory on show
  and may flash a Dock icon. The log line `window visible …` prints
  `policy=X→Y bundleId=…`, which shows which case applies.
- **Super tap (WM side):** the event tap now passes FlagsChanged events to
  the handler. Modifier events are never swallowed. `super_tap::SuperTap` is
  a pure state machine (12 tests). It arms when Command (0x37/0x36) goes down
  with no other held modifier (Shift/Ctrl/Option/Fn; Caps Lock is ignored).
  Any key, another modifier or the other Command key spoils it. Releasing
  with no modifiers held counts as a tap if Command was held ≤ 400 ms. A
  click, scroll or key-down since the press also cancels it
  (`CGEventSourceSecondsSinceLastEventType`, 25 ms slack, so the tap needs
  no mouse events). The per-app `passthrough` list applies.
  `launcher::LauncherClient` (thread `launcher-client`; `toggle()` never
  blocks the tap) sends `toggle`. If that fails it starts the launcher with
  `--show`: first `open -a <wm dir>/cosmic-mac-launcher.app --args --show`
  (output to `launcher.log`), else `<wm dir>/cosmic-mac-launcher --show`. It
  does not start another one within 5 s of a start. Config:
  `super_tap_launcher: false` in
  `~/Library/Application Support/cosmic/com.system76.CosmicComp/config.ron`
  turns it off (default on; `cosmic-mac-ctl config` shows it). `send-key
  --tap-super [--hold-ms N]` posts a tap (default hold 80 ms).
- **Plugins and prefixes** (pop-launcher's prefixes; a prefix owns the
  query):

  | Prefix | Plugin | Does |
  |---|---|---|
  | `=` (or a bare `2+2`) | calc | evaluate; Return copies |
  | `g `, `ddg ` | web | Google / DuckDuckGo search |
  | `run `, `t:` | terminal | run in a new Terminal.app window |
  | `:` | terminal | run in the background, no output |
  | `find ` | files | Spotlight file-name search in `~` |
  | (none) | windows, apps | WM windows and applications by name |

  Because a prefix owns the query, the app "Find My" is found with `find` or
  `my`, not `find my` (pop-launcher behaves the same way).
  Terminal: the command goes into a fresh owner-only `.command` file in
  `$TMPDIR` that deletes itself, runs `$SHELL -l -c '<cmd>'` and waits for
  Return. Opening it runs it in Terminal, so no Automation grant is needed.
  Files: `mdfind -0 -onlyin ~ -name <name>` (runner injectable for tests),
  3 s timeout, 64 paths. Hidden paths and `~/Library` are dropped, results
  are ranked by file name, and Return opens the file with its default app.
  The search runs off the UI thread after a 150 ms typing pause. File icons
  come from `iconForFile:` and are dropped on hide.
- **Session:** `AGENTS` has a second entry `com.cosmicmac.launcher` →
  `cosmic-mac-launcher.app/Contents/MacOS/cosmic-mac-launcher` (no
  arguments, so it starts hidden; `launcher.log`;
  `RUST_LOG=warn,cosmic_mac_launcher=info`). `install` now refuses to run
  unless the launcher bundle is deployed too.
- **VM test steps** (stop a hand-started WM first, see above):

  ```
  scripts/deploy.sh -p cosmic-mac-wm -p cosmic-mac-session -p cosmic-mac-launcher
  ssh dj@192.168.64.14 '~/cosmicMac/bin/cosmic-mac-session install'
  ssh dj@192.168.64.14 '~/cosmicMac/bin/cosmic-mac-session status'   # wm + launcher running
  ssh dj@192.168.64.14 'tail -n 30 ~/Library/Logs/cosmicMac/launcher.log'
  #   expect: "warmed up", "gave focus back" (maybe), no Dock icon in screenshots
  ssh dj@192.168.64.14 'echo toggle | nc -U ~/Library/Application\ Support/cosmic/cosmic-mac-launcher.sock'
  #   expect "ok"; launcher visible; log: policy=Accessory→Accessory bundleId=Some("com.cosmicmac.cosmic-mac-launcher"),
  #   "first frame N ms after the show request" (N well under 1100)
  ssh dj@192.168.64.14 'echo toggle | nc -U ~/Library/Application\ Support/cosmic/cosmic-mac-launcher.sock'   # hidden again
  ssh dj@192.168.64.14 '~/cosmicMac/bin/send-key --tap-super'               # toggles
  ssh dj@192.168.64.14 '~/cosmicMac/bin/send-key --tap-super --hold-ms 600' # no change (held too long)
  ssh dj@192.168.64.14 '~/cosmicMac/bin/cosmic-mac-ctl config | grep super_tap'   # super_tap_launcher: true
  # On-demand start: launchctl bootout gui/$(id -u)/com.cosmicmac.launcher; pgrep -x cosmic-mac-launcher (none);
  #   send-key --tap-super → started via open -a … --args --show and visible; next tap hides it.
  #   (Reinstall with cosmic-mac-session install afterwards.)
  # Plugins (type in the VM GUI or with send-key): "find <name>", "run ls", ":say hi", "=2+2";
  #   Escape / Return / clicking another window hide it; pgrep -x cosmic-mac-launcher still shows one process.
  ```

  Check that the real Command key still works for shortcuts (Cmd+Tab,
  Cmd+C) and that a quick Cmd tap in the passthrough apps does nothing.

### Theming (dark/light)

**Verified in the VM (2026-11-01).** Tab bar: dark/light and back within ~1 s of `osascript -e 'tell application "System Events" to tell appearance preferences to set dark mode to …'` (works over ssh without an extra grant). Launcher, after the fix below: light at start in light mode, live switches both ways while visible, dark after hide → switch → show, COSMIC `is_dark=false` wins live over macOS dark, `auto_switch=true` follows macOS again. Evidence: `scratch/theme-fix-*.png`.

- **Rule** (`cosmic_mac_protocol::theme`, shared by the WM and the launcher;
  no libcosmic in the WM): COSMIC's
  `~/Library/Application Support/cosmic/com.system76.CosmicTheme.Mode/v1/is_dark`
  decides when it holds a readable RON bool and `auto_switch` (same
  directory) is not `true`. Otherwise (no config, garbage `is_dark`, or
  `auto_switch: true`) the macOS appearance decides: the global default
  `AppleInterfaceStyle` is `Dark` in dark mode and absent in light mode. A
  missing or garbage `auto_switch` counts as `false` (cosmic-config's
  default). The layout is cosmic-config's: `dirs::config_dir()` (=
  `~/Library/Application Support` on macOS) + `cosmic/<id>/v<version>/<key>`,
  one RON value per file. cosmic-config's system defaults
  (`$XDG_DATA_DIRS/cosmic/...`) are not read.
- **Why libcosmic alone is not enough:** it reads `is_dark` only (on Linux
  cosmic-settings-daemon implements `auto_switch` by rewriting `is_dark`),
  its macOS-appearance equivalent is the Linux-only XDG portal, and its
  `Settings::default()` theme is `system_preference()` (just `is_dark`).
  It does watch the Mode config on macOS (cosmic-config's `notify` watcher,
  kqueue backend) and would switch on its own `is_dark`.
- **Launcher** (`crates/cosmic-mac-launcher/src/theme.rs`): `load_theme`
  starts from libcosmic's `dark_default()`/`light_default()` for the mode and
  lays every key file found in `com.system76.CosmicTheme.Dark/v1` (or
  `.Light`) over it (`CosmicConfigEntry::update_keys`). `cosmic_theme` hands
  the result to libcosmic as `ThemeType::Custom`. The mode is re-resolved on
  every show (covers changes while hidden), on
  `AppleInterfaceThemeChangedNotification` (a block observer on
  `NSDistributedNotificationCenter`, installed in `main` before the app
  runs; in the VM it arrived at once even while the launcher was hidden) and
  on any Mode config change (`Core::watch_config::<ThemeMode>`). The theme is
  rebuilt on a mode change and when the current mode's theme config changes
  (`watch_config::<Theme>(mode.theme_id())`, updates with keys only). A change
  sends `cosmic::command::set_theme` and logs `theme: dark -> light` (info).
  The macOS side is read with `NSUserDefaults.standardUserDefaults` (global
  domain in its search list). Startup logs `theme: <mode>`.
- **Pitfall (fixed 2026-11-01, found in the VM):** do not use
  `cosmic::theme::system_light()` or `ThemeType::System` on macOS.
  cosmic-config fills missing keys from `cosmic_theme::Theme::default()`,
  which is `preferred_theme()` = **`dark_default()`**, so with an empty or
  missing `com.system76.CosmicTheme.Light` (always the case on a Mac: libcosmic
  creates the directories but nothing writes them; Linux ships defaults in
  `/usr/share/cosmic`) the "light" theme is dark. libcosmic's own
  theme-config subscription also pushes that dark entry over any
  `ThemeType::System` theme, because its `is_dark` check uses the Mode
  config's default `true`. The first version had both problems: the log said
  `theme: dark -> light` while the window stayed dark. `ThemeType::Custom` is
  never replaced by libcosmic's Mode or theme subscriptions.
- **Tab bar** (`crates/cosmic-mac-wm/src/tabbar.rs`): colours come from
  `TabBarPalette::DARK`/`LIGHT`, taken from COSMIC's default themes at the
  pinned libcosmic rev (launcher test `tab_bar_palette_matches_cosmic_defaults`
  guards them): bar = `background.base` at 0.94 alpha, inactive tab =
  `background.component.base`, title = `background.component.on`, close
  glyph = `palette.neutral_7`, active tab = `accent.base` with title and
  close glyph in `accent.on`. Dark: `#1B1B1B` bar, `#2E2E2E` tabs, accent
  `#63D0DF` with black text. Light: `#D7D7D7` bar, `#F5F5F5` tabs, accent
  `#00525A` with white text. When
  `com.system76.CosmicTheme.{Dark,Light}/v1/accent` (a cosmic-theme
  `Component`, hex strings such as `"#63D0DFFF"` or older
  `(red:, green:, blue:, alpha:)` structs) is readable, its `base` and `on`
  replace the accent (black/white by luminance when `on` is missing).
- **Live changes in the WM:** the tab-bar thread resolves the palette when it
  shows the bar (if the last check is ≥ 2 s old) and, while the bar is
  visible, every 2 s (`recv_timeout`); a change redraws the visible bar. A
  hidden bar does not wake up. The macOS side is
  `CFPreferencesAppSynchronize` + `CFPreferencesCopyAppValue` on
  `kCFPreferencesAnyApplication`; the daemon has no `NSApplication`, so it
  cannot ask for the effective appearance. Polling was chosen over a
  distributed-notification observer on the main run loop because it also
  covers the COSMIC file and needs no hook in `main.rs`, `macos.rs` or
  `daemon.rs` (none of them changed). Logs
  `tab bar theme: light, accent Rgba { .. }` (info) on each change.
- **Status item:** unchanged. Its title is a plain `NSStatusBarButton`
  title, which AppKit draws in the menu bar's colour.
- **Unverified assumption:** with macOS Appearance set to *Auto*, both
  processes rely on macOS updating `AppleInterfaceStyle` when it switches.
- **Host tests:** protocol `theme` (17: rule incl. auto_switch, missing and
  garbage config, file fixtures, hex/struct accents, palette), WM `tabbar`
  (6: tab colour mapping, re-check timing, change detection, CF read vs
  `defaults read -g`), launcher `theme` (7: palette vs libcosmic defaults,
  parsing files written by cosmic-config itself into a temp dir, light theme
  without config is light (regression), user keys over the mode's default,
  custom theme type, NSUserDefaults vs `defaults`, resolution from a home
  dir).
  Tests must not call libcosmic's `Theme::with_accent`, `system_dark()` or
  `ThemeMode::config()`: they create directories under the real
  `~/Library/Application Support/cosmic`.
- **VM test steps** (all passed 2026-11-01). The agents must already be installed (see
  above); deploying does not restart them:

  ```
  scripts/deploy.sh -p cosmic-mac-wm -p cosmic-mac-launcher
  ssh dj@192.168.64.14 'launchctl kickstart -k gui/$(id -u)/com.cosmicmac.wm; launchctl kickstart -k gui/$(id -u)/com.cosmicmac.launcher'
  ssh dj@192.168.64.14 'grep "theme" ~/Library/Logs/cosmicMac/launcher.log | tail -3'   # "theme: dark" or "light" = VM appearance
  ```

  1. **Follow macOS (no COSMIC config):** in the VM, open two Terminal
     windows side by side and run `cosmic-mac-ctl action toggle-stacking`
     (stack them; the tab bar appears). `just screenshot`. Toggle System
     Settings → Appearance (Light/Dark) in the UTM window, or over ssh
     `osascript -e 'tell application "System Events" to tell appearance preferences to set dark mode to not dark mode'`
     (needs an Automation grant; prefer the GUI). Within ≤ 2 s the tab bar
     switches palette (dark: near-black bar, cyan active tab, black title;
     light: light-grey bar, dark-teal active tab, white title). `grep "tab bar theme" ~/Library/Logs/cosmicMac/wm.log`
     shows one line per switch.
  2. **Launcher follows:** `echo toggle | nc -U ~/Library/Application\ Support/cosmic/cosmic-mac-launcher.sock`
     after each switch: the launcher is light or dark to match, and
     `launcher.log` shows `theme: dark -> light` (or the reverse) once.
     Live path: show the launcher over ssh (ssh does not take focus in the
     VM, so it stays visible), then switch with the `osascript` command
     above; it recolours while visible, via the distributed notification.
  3. **COSMIC config wins:** with macOS in Dark,
     ```
     D=~/Library/Application\ Support/cosmic/com.system76.CosmicTheme.Mode/v1
     mkdir -p "$D"; echo false > "$D/is_dark"; echo false > "$D/auto_switch"
     ```
     The visible tab bar turns light within ≤ 2 s; the launcher shows light
     (live if it is visible: `launcher.log` logs the switch at once from the
     config watcher). `echo true > "$D/is_dark"` → dark again.
  4. **auto_switch:** `echo true > "$D/auto_switch"` → both follow macOS
     again (step 1 toggling works). `rm -r ~/Library/Application\ Support/cosmic/com.system76.CosmicTheme.Mode`
     → still follows macOS (deleting files may only be noticed at the next
     launcher show).
  5. **Accent:** `mkdir -p ~/Library/Application\ Support/cosmic/com.system76.CosmicTheme.Dark/v1 && echo '(base: "#FF8800FF", on: "#000000FF")' > ~/Library/Application\ Support/cosmic/com.system76.CosmicTheme.Dark/v1/accent`
     → in dark mode the active tab turns orange within ≤ 2 s. libcosmic
     needs every `Component` field, so the launcher ignores this partial
     file and keeps its default accent. Remove the file afterwards.
  6. Status item: its title stays readable in both appearances.

### Drag to stack

Dragging a tiled window by its title bar and releasing it with the pointer in
the **middle third** (both axes) of another tile stacks it onto that tile
(cosmic-comp's `TargetZone::WindowStack`): a plain target becomes a stack, a
stack gains a tab. The dropped window is the active, focused tab; a stack of
two it left turns back into a plain window. Any other release snaps back as
before. **Verified in the VM (2026-11-01)**: window → window, a stack's
visible member → a third window (the old stack dissolved), a corner drop snaps
back, an edge resize is not fought while held and snaps back on release, fast
drags (grace path below), and the vm-test drag phase. Evidence:
`scratch/drag-stack-highlight.png` (mid-drag highlight),
`scratch/drag-stack.png`, `scratch/drag-restack.png`,
`scratch/vm-test-09-drag-stack.png`.

- **Input**: a second, listen-only CGEventTap (`MouseTap` in platform
  `eventtap.rs`) for left down/dragged/up, on the WM's tap thread; dragged
  events are throttled to ~30 Hz in the callback. It cannot delay or swallow
  clicks. The core gets `Message::Mouse(event, captured)`.
- **Core** (`core.rs`, module doc "Drag to stack"): while the button is held, a
  user move of a visible tiled window whose tile (+8 px) the button went down
  on is a drag and is not snapped back (edge resizes too). On release a pure
  move (size unchanged) whose release point is in another tile's stack zone
  (`Layout::drop_zone`) goes to `Layout::stack_onto`; everything else snaps
  back through the old clamp/err/re-check code (`Core::snap_back`). Moves with
  the button up, floating, parked, suspended and full-screen-Space windows
  behave as before.
- **Late moves**: a move captured within `DROP_GRACE` (250 ms) after a release
  that ended no drag, of a window whose tile the button went down on, is taken
  as a drag that ended at the release point.
- **Highlight**: while the pointer is over a stack zone the tab-bar thread
  shows a translucent accent rounded rectangle over the target tile, ordered
  just above the target (under the dragged window); the core sends it through
  `PlatformBackend::set_drop_hint` only when the target changes.
- **Measured in the VM** (synthetic drags with `send-key --drag`, traced with
  `fs-trace --mouse` and the daemon's debug log):
  - `WindowMoved` arrives live during a title-bar drag, ~10–20 ms after each
    dragged event, and a duplicate of the final frame ~60 ms after the last
    movement. With a hold before the release every move precedes it.
  - Release right after the last dragged event (`--steps 1–4 --step-ms 4–10
    --hold-ms 0`): in 7 of 7 trials the daemon saw the release first and the
    window's first move 6–21 ms later; the grace path stacked every one. With
    `--steps 6 --step-ms 8` the moves came before the release and the normal
    path stacked.
  - Dragged events and release within ~3 ms (`--steps 3 --step-ms 0
    --hold-ms 0`): the window does not move at all (no `WindowMoved`, whether
    or not its app was active); the daemon sees only down and up and does
    nothing. This was the "fast drag does not stack" case of the first VM
    session: no WM bug.
  - An edge resize sends live `WindowResized` with stale `WindowMoved` echoes
    in between.
- **Out of scope**: split zones (a drop outside the middle third splits next
  to the target in cosmic-comp; here it snaps back, and `DropZone` has room for
  a `Split` variant), dragging tabs out of the tab bar, tab-bar clicks.
- Tests: `crates/cosmic-mac-layout/tests/drag_stack.rs` (16), 10 core tests in
  `core::tests`, a tab-bar colour test; `cargo test --workspace` 801 passed.
  VM: the drag phase of `scripts/vm-test.sh` (below).

### VM scenario changes (`scripts/vm-test.sh`)

- Keeps the display awake while it waits for the daemon.
- Closes Finder windows through the WM, because AppleScript `tell Finder` needs
  an Automation grant over ssh and times out (a permission prompt may be
  pending in the VM).
- Focuses a visible test window explicitly before move-to-workspace.
- Native full-screen phase (R11): puts an unfocused test window into full
  screen through System Events (`set-fullscreen <pid> <title> true|false` in
  the remote helper), asserts it is not listed while the rest still tile, then
  leaves full screen and asserts every window is back in its old tile
  (`geometry-unchanged`) with ACTUAL = FRAME (`in-tiles`). Screenshot
  `scratch/vm-test-08-fullscreen.png`.
- Drag-to-stack phase (after the keybinding phase; needs `send-key` and two
  test windows): wakes the display, drags one test window's title bar to the
  centre of another tile (`--steps 20 --step-ms 16 --hold-ms 300`, so every
  move arrives before the release) and asserts `stacked-onto <wid> <target>`
  and `tiled COUNT-1` (screenshot `scratch/vm-test-09-drag-stack.png`),
  unstacks with `toggle-stacking`, then drags to a point a sixth into another
  tile (outside its middle third) and asserts `unstacked`,
  `geometry-unchanged` and `in-tiles` (snapped back).
- Other apps' windows still take part in the tiling: an extra Terminal window
  left open made the Super+S stacking assertion pair a test window with it
  ("found 1"). Close stray windows before a run, or hide their app
  (`osascript -e 'tell application "System Events" to set visible of process
  "Terminal" to false'`; a hidden app's windows are suspended, not tiled).
- `deploy.sh` no longer copies `.d` dep-info files; `--help` fixed.

## Newer macOS VMs

The planned macOS 27.2 VM cannot be installed from UTM on this host: the
Virtualization framework reports "software update required", which means the
**host** (macOS 15.6) must run at least the guest's major version to restore
that IPSW. Testing on 26/27 therefore needs a host upgrade or another Mac.
`docs/SKYLIGHT.md` already notes the macOS 26 SLS message-ID differences.

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

1. ~~Super-key acceptance~~ done (scripted). Milestone 1 exit criteria are met.
2. ~~Backlog 1–3~~ done; backlog 4–8 remain.
3. ~~Review backlog R1–R6 and R11~~ done; **R12** remains (retry commands
   and observer registration after an unresponsive app recovers). The R11
   re-check timers (`Message::Recheck`) could carry R12's retries.
4. ~~Session installer under launchd~~ verified in the VM (install, `kill -9`
   restart + journal replay, uninstall).
5. Milestone 2: ~~menu-bar status item, WM IPC subscription~~ (separate process
   `cosmic-mac-status`, `Request::Subscribe`), ~~resident launcher, Super tap,
   bundle~~ and ~~theming~~ (dark/light for the launcher and the tab bar)
   implemented; run their VM test steps (sections above).
6. Minor backlog: SLS pump `mach_msg`, IPC line limit / UTF-8 error reply,
   windows rejected at creation never re-checked.
7. Commit the uncommitted work, then first ship: `gh repo create` (private),
   push, tag.
