# HANDOFF.md — cosmicMac

> State as of 2026-10-31 (late evening). Milestone 0 (spikes) **complete**, milestone 1
> (window manager) **implemented and passing the full VM integration scenario,
> including the Super-key bindings driven through the event tap**. Code-review
> backlog R1, R4, R5 (minimize and Cmd+H; **not** native full screen, see R11),
> R6 and R6a are fixed. Milestone 2: the COSMIC panel is dropped in favour of the
> native menu bar; the session installer and a launcher spike exist.
> **All of this is uncommitted in the main checkout** (nothing committed since
> `5fbec1c`).

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
- **Native full screen moves the window to a new Space, and that breaks the
  usual reads.** Traced in the VM (TextEdit, `AXFullScreen` true/false): the
  window moves/resizes to the whole display (0,0 1920×1080), SkyLight then sends
  **`WindowDestroyed` for it** and later `WindowCreated` with the same wid, and
  `CGWindowList`/AX reads of it fail during the transition. AX `AXWindows` only
  lists windows on the **current** Space, so `ax_window()` fails ("window N not
  found") for every window left on the desktop Space while the full-screen
  Space is active. Several short-lived helper windows (levels change, wids like
  3044/3045) appear and vanish during the animation; one was briefly managed.
- **TextEdit resets its other windows' frames across a full-screen round trip**
  (also without the WM): windows set to 0,25 960×970 through AX came back at
  their cascade defaults (213,75 646×418 …). The WM tried to snap them back
  while they were on the inactive Space, failed (not found) and never retried.
- **An app stopped with SIGSTOP makes every AX call wait the full messaging
  timeout** (1 s, error -25204). The journal replay's "1 s budget" is per pass,
  and one pass over three windows took 9 s; observer registration for that pid
  took another 5 s and failed, so the WM gets no AX notifications from that app
  until it is re-observed.
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

- Host: `cargo test --workspace` (616), `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo fmt --check` — all green (clippy only
  prints the known `block v0.1.6` future-incompat note).
- VM checks of the review fixes (2026-10-31, hand-driven over ssh with
  `RUST_LOG=debug`):
  - R5 minimize: `AXMinimized` set through System Events and the real
    minimize button (AXPress) both suspend the window at once and re-flow its
    neighbours; it stays suspended past the settle time; setting `AXMinimized`
    false re-tiles it. Cmd+H suspends and un-hiding resumes.
  - R5 native full screen: **fails** (R11).
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
    including the minimize fix.
- VM (`dj@192.168.64.14`, macOS 15.6, SIP on, fullscreen 1728×1080): full
  scenario green; evidence in `scratch/vm-test-0{1..6}-*.png`.
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
- R5. **Minimize and Cmd+H done; native full screen still broken (R11).**
  Suspended windows leave the layout and come back on the active workspace
  with their learned constraints (`Core::suspended`, `AppHidden/AppShown`,
  `WindowMinimized/WindowDeminimized`, `CommandApplier::release_window`).
  Original:
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
- **R11. Native full screen loses the window** (found in the VM while checking
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
- **Launcher spike** (`crates/cosmic-mac-launcher`, uncommitted): a libcosmic
  overlay with its own `Plugin` trait (pop-launcher's plugins crate does not
  build on macOS) and plugins apps, calc, web and windows (over WM IPC). In the
  VM it renders centred with app icons, takes keyboard focus, `=2^10` + Return
  copies 1024, Escape quits. Open: startup takes ~1.1 s (a resident mode is
  likely needed); the `Info.plist` embedded by `build.rs` does not take effect
  (bundle id None; it starts with the Regular policy, then switches to
  Accessory, so a Dock icon may flash); the WM does not spawn it on a Super tap
  yet.
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

### VM scenario changes (`scripts/vm-test.sh`)

- Keeps the display awake while it waits for the daemon.
- Closes Finder windows through the WM, because AppleScript `tell Finder` needs
  an Automation grant over ssh and times out (a permission prompt may be
  pending in the VM).
- Focuses a visible test window explicitly before move-to-workspace.
- Other apps' windows still take part in the tiling: an extra Terminal window
  left open made the Super+S stacking assertion pair a test window with it
  ("found 1"). Close stray windows before a run.
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
3. ~~Review backlog R1–R6~~ done except native full screen: **R11** (design
   needed: Space transitions, delayed re-checks) and **R12** (retry commands
   and observer registration after an unresponsive app recovers).
4. ~~Session installer under launchd~~ verified in the VM (install, `kill -9`
   restart + journal replay, uninstall).
5. Milestone 2: menu-bar status item (likely in the WM process, like the tab
   bar overlay); WM IPC event subscription if it lives out of process; launcher
   activation from the WM (tap Super), resident mode, bundle id; theming.
6. Minor backlog: SLS pump `mach_msg`, IPC line limit / UTF-8 error reply,
   windows rejected at creation never re-checked.
7. Commit the uncommitted work, then first ship: `gh repo create` (private),
   push, tag.
