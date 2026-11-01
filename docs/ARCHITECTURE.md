# cosmicMac architecture

This document describes the design of the cosmicMac window manager as implemented in
milestone 1, with emphasis on the two platform-neutral crates that form the foundation:

* `crates/cosmic-mac-protocol` — serde types shared by the daemon, the CLI and the layout
  engine.
* `crates/cosmic-mac-layout` — a pure-Rust window-management state machine.

The platform layer (`crates/cosmic-mac-platform`), the daemon (`crates/cosmic-mac-wm`) and the
CLI (`crates/cosmic-mac-ctl`) consume these crates; they are described here only at their
interface boundary. `PLAN.md` remains the authoritative product plan.

## 1. Why a hybrid architecture

macOS has no Wayland compositor protocol, and WindowServer owns the display. cosmic-comp
cannot run as a display server, so cosmicMac splits the problem in two:

1. **A window-manager daemon** observes native applications through the Accessibility (AX) API
   and SkyLight notifications, and moves/resizes/raises/parks their windows.
2. **COSMIC shell components** run later as ordinary libcosmic windows (panel, dock,
   launcher). Milestone 1 only needs the window manager and its CLI.

The daemon is deliberately split into three layers:

```
                 ┌─────────────────────────────┐
                 │        cosmic-mac-wm        │
                 │  event loop, keybindings,   │
                 │  config, IPC, tab-bar       │
                 └───────────┬─────────────────┘
                             │
          ┌──────────────────┼───────────────────┐
          │                  │                   │
┌─────────▼─────────┐ ┌──────▼──────────┐ ┌──────▼──────────┐
│ cosmic-mac-layout │ │cosmic-mac-protocol│ │cosmic-mac-platform│
│ pure state machine│ │ serde IPC types  │ │ AX + SkyLight FFI │
│ no OS dependencies│ │ no OS dependencies│ │ macOS only        │
└───────────────────┘ └──────────────────┘ └───────────────────┘
```

`cosmic-mac-layout` is testable on any host, which is what makes the tiling, stack and
workspace semantics verifiable without a VM.

## 2. Coordinate model

* All geometry is in **global, top-left-origin logical pixels**, matching the Accessibility
  API. Conversion to AppKit's bottom-left coordinates happens in one place in
  `cosmic-mac-platform`.
* The layout engine receives a **work area** per event batch via
  `Layout::set_work_area(Rect)`. In milestone 1 the daemon derives it from
  `NSScreen.visibleFrame` (menu bar and Dock still visible); later the panel/dock reservations
  replace it.
* Frames are `f64`. Tiling boundaries are snapped to a micrometre (`1e-6`) during geometry
  computation so repeated fractional weight arithmetic does not leak floating-point noise
  (`899.9999999999999` becomes `900.0`).
* Gaps are applied inside the layout engine:
  * `outer` insets the work area before the tree is laid out;
  * `inner` is the space between adjacent children of every group.
  Floating windows ignore gaps. `active_hint` is carried in the configuration but is not part
  of frame math; the overlay renders it.

## 3. The layout engine

### 3.1 State machine

`Layout` is fed events and actions and returns commands:

```rust
pub fn window_added(&mut self, info: ToplevelInfo) -> Vec<Command>;
pub fn window_removed(&mut self, wid: WindowId) -> Vec<Command>;
pub fn focus_changed(&mut self, wid: WindowId) -> Vec<Command>;
pub fn update_window(&mut self, info: ToplevelInfo) -> Vec<Command>;
pub fn action(&mut self, action: Action) -> Vec<Command>;
pub fn set_work_area(&mut self, rect: Rect) -> Vec<Command>;
pub fn update_config(&mut self, config: Config) -> Vec<Command>;
```

It never calls the platform. After each mutation it diffs the desired visible state against
the last emitted state (`Layout::sync`) and emits only changes. This has three benefits:

* the daemon does not have to second-guess the layout;
* command sequences are deterministic and easy to assert in tests;
* repeated or no-op actions produce an empty command list, so the platform layer does no
  redundant AX work.

Command order is fixed:

1. `ActivateWorkspace` (if the active workspace changed),
2. `Hide` for windows that stopped being visible (sorted by window id),
3. `Show` for windows that became visible (sorted by window id),
4. `SetFrame` for changed frames (sorted by window id),
5. `Focus` + `Raise` (or `Unfocus`),
6. `StackChanged` (tab-bar update, `None` hides the bar).

### 3.2 Tiling tree

The tree is an arena of nodes (`crates/cosmic-mac-layout/src/tree.rs`):

* `Group { orientation, children, sizes }` — a split container. `sizes` are relative weights,
  not pixels. `Orientation::Horizontal` means children are placed left-to-right;
  `Orientation::Vertical` means top-to-bottom.
* `Window(wid)` — a leaf.
* `Stack { windows, active }` — a leaf holding an ordered tab list and the active index.

Weights normalise against the rectangle a group receives, so changing the work area, gaps or
workspace does not require rescaling stored sizes. Only the active stack member occupies the
stack's frame; the others are hidden.

**Insertion.** A new tiled window is inserted next to the focused leaf (or the last tiled
focus, or the first window). The split orientation is chosen from the reference frame's
aspect ratio (`Orientation::from_aspect`): wide references split into columns, tall
references into rows. If the reference's parent already has that orientation the new window
becomes a sibling and takes half of the reference's weight; otherwise a new nested group is
created in the reference's slot. This mirrors cosmic-comp's `map_to_tree`/`new_group`
behaviour.

**Removal and collapse.** Removing a child drops its weight; the remaining children expand
proportionally to fill the group. A group that drops to one child is replaced by that child;
an empty group is removed. `Tree::normalize` performs this bottom-up after structural
mutations. Because normalization can move a child into its parent's node id, callers that
hold a node id across a detach re-resolve it by window content (see `Neighbour` in
`lib.rs`).

**Focus.** `Focus(Direction)` walks up from the focused leaf. At each ancestor whose
orientation matches the direction's axis, it moves to the adjacent sibling and descends to the
nearest edge of that subtree. If no ancestor has a sibling, focus wraps to the opposite edge
of the root — but only when the root splits along the same axis. Perpendicular directions
have no neighbour and are no-ops, matching cosmic-comp. Focus on a floating window uses a
spatial search across both layers with wrap-around.

**Move.** `Move(Direction)` finds the nearest ancestor group with a matching orientation and
an adjacent sibling, then swaps the two children (weights move with them, so each window keeps
its size). If the moving leaf is a plain window and the sibling is a stack, the window is
removed from the tree and appended to the stack instead. A focused stack member moving towards
another stack changes stacks rather than moving the whole stack; otherwise the whole stack
node swaps with its neighbour.

**SwapWindow** exchanges the focused window with the previous entry in the workspace's focus
history. Plain/plain swaps node payloads; stack slots, floating windows and mixed layers
exchange their occupants. Stack active indices are updated so the focused window remains
visible.

**ToggleOrientation** flips the orientation of the focused leaf's parent group; weights are
preserved because they are ratios.

**Resize.** `Resize(Direction)` finds the nearest ancestor group matching the axis and
transfers `RESIZE_STEP` (32 px) of weight between the focused child and its neighbour:
* if a sibling exists in `direction`, the focused window grows by taking space from it;
* otherwise the focused window shrinks by giving space to the sibling on the opposite side;
* the donor is clamped so it never falls below `MIN_WINDOW_SIZE` (100 px).
Repeated resizes therefore grow, then clamp. `ResizeMode` toggles a flag that the daemon uses
to route arrow keys to `Resize`.

**Maximize** stores at most one maximized window per workspace. A maximized window's frame is
the full work area; other windows keep their tree frames behind it. A floating window's
restore frame is remembered so unmaximizing returns it to its floating position.

### 3.3 Stacks

A stack is a first-class leaf, not a group of hidden windows:

* `ToggleStacking` (`Super+S`) adds the focused window to the stack of its neighbour (next
  sibling first, then previous). If the neighbour is a plain window it becomes a two-tab
  stack; if it is already a stack the window is appended. Pressing it while focused inside a
  stack performs `Unstack` instead.
* `Unstack` removes the focused member into its own sibling frame, splitting the stack's
  weight in half. A stack that drops to one member is converted back to a plain window.
* `Move(Direction)` into a stack appends the moving window and activates it.
* Only the active member is visible. `Layout::sync` emits `Hide` for members that become
  inactive and `Show` + `SetFrame` for the newly active member. The daemon parks hidden
  members with the corner-park mechanism.
* `Layout::active_stack()` / `Layout::stacks()` expose tab titles, the active index and the
  frame so the daemon can draw the tab bar. `Command::StackChanged` is emitted when the
  focused stack changes; `None` hides the bar.
* Drag to stack: `Layout::drop_zone(wid, x, y)` names the tile whose middle third (both
  axes, cosmic-comp's stack region) holds the point; `wid`'s own tile is never a target and
  a stack is one target. `Layout::stack_onto(wid, target)` takes `wid` out of its tile or
  stack (a stack left with one member becomes a plain window) and adds it to `target`, which
  becomes a stack if it was a plain window; `wid` is the active, focused tab. Drops outside
  the middle third (cosmic-comp's split zones) are not implemented.

### 3.4 Floating layer

Each workspace has an ordered floating list. A window floats when:

* the workspace's `tiling_enabled` flag is off (new workspaces start off when `autotile` is
  false),
* the window is a dialog (`ToplevelInfo::is_dialog`),
* the window is not resizable, or
* it matches a `tiling_exceptions` rule (case-insensitive substring or `*` wildcard on
  `app_id` and optional `title`).

Placement is deterministic: the window keeps its initial size (clamped to the work area) or
uses `min(60% of work area, 960×720)`; it is centred, then offset by `32 px × cascade index`
(mod 8) and clamped inside the work area. Dialogs are centred without cascade.

`ToggleWindowFloating` (`Super+G`) moves the focused window between layers. A tiling window
keeps its current frame when it floats; a floating window is re-tiled next to the current
tiling focus. `ToggleTiling` (`Super+Y`) converts every tiling window to floating while
preserving frames, and re-tiles every floating window when switched back on (inactive stack
members get cascade placements so they do not overlap).

### 3.5 Workspaces

The engine models a single ordered pool; display assignment is the daemon's job. Indices are
**1-based** to match COSMIC's `Super+1` … `Super+9` bindings.

* `Workspace(n)` ensures workspaces up to `n` exist (creating empty ones) and switches.
* `MoveToWorkspace(n)` moves the focused window and stays on the current workspace.
* `NextWorkspace` / `PreviousWorkspace` wrap around the pool.
* `LastWorkspace` returns to the previously active workspace.
* `MoveToNextWorkspace` / `MoveToPreviousWorkspace` / `MoveToLastWorkspace` move the focused
  window without following it.
* Empty trailing workspaces are removed after a window is removed or moved away. If the active
  workspace was the trailing empty one, the previous workspace becomes active. At least one
  workspace always exists.
* Each workspace remembers its focus, MRU history and tiling flag.

`Layout::assign_display` is intentionally not implemented yet; `WorkspaceGroup` already wraps
the pool with an optional output name so a per-display model can be added without changing the
IPC shape.

### 3.6 Focus history

Each workspace keeps an MRU list (`focus_stack`) and a cycle cursor. `FocusNext` /
`FocusPrevious` visit every window once before repeating (unlike a naive MRU toggle).
Directional focus and external focus changes reset the cursor and move the window to the front.
Floating and tiling windows share one history, so cycling crosses layers.

### 3.7 Configuration

`Config` is the config-changed payload:

```rust
pub struct Config {
    pub gaps: Gaps { outer: u32, inner: u32 },
    pub active_hint: u32,
    pub autotile: bool,
    pub tiling_exceptions: Vec<TilingException>,
    pub passthrough: Vec<String>,
}
```

`TilingException { app_id: Option<String>, title: Option<String> }` matches case-insensitively;
patterns containing `*` are wildcards, other patterns are substrings. `passthrough` is
daemon-only (keybindings that must reach the application) but travels in the same payload.

## 4. Protocol crate

`cosmic-mac-protocol` mirrors the semantics of the upstream COSMIC protocols where that is
meaningful:

| cosmicMac | upstream analogue |
|---|---|
| `ToplevelInfo`, `ToplevelState`, `ToplevelCapability`, `ToplevelManagement` | `cosmic_toplevel_info_v1`, `cosmic_toplevel_management_v1` |
| `WorkspaceInfo`, `WorkspaceGroup`, `StackInfo` | `cosmic_workspace_v2` (simplified to one pool) |
| `Action` | cosmic-comp keybinding actions |
| `Command` | no upstream analogue (the compositor owns both sides there) |

### 4.1 Key types

```rust
pub type WindowId = u64;

pub struct Rect { pub x: f64, pub y: f64, pub width: f64, pub height: f64 }

pub enum Direction { Left, Right, Up, Down }
pub enum Axis { Horizontal, Vertical }
pub enum Orientation { Horizontal, Vertical } // child layout, not split axis

pub struct ToplevelInfo {
    pub wid: WindowId,
    pub pid: i32,
    pub app_id: String,        // bundle id
    pub title: String,
    pub role: Option<String>,  // AX role
    pub subrole: Option<String>,
    pub resizable: bool,
    pub is_dialog: bool,
    pub initial_frame: Option<Rect>,
}
```

`initial_frame` is an extension over cosmic-protocols: macOS hands us the frame when a window
is observed, and the layout engine needs a size to place floating windows.

`Action` covers the window-management subset of COSMIC's keybindings: focus/move/resize in
four directions, focus cycling, swap, orientation, resize mode, maximize, tiling/floating
toggles, stacking/unstack, workspace switching/moving, and close. It is adjacently tagged
(`{"action":"focus","args":{"direction":"left"}}`) so it round-trips through both JSON and
RON.

`Command` is the layout output:

```rust
pub enum Command {
    SetFrame { wid, rect },
    Raise { wid },
    Hide { wid },
    Show { wid },
    Focus { wid },
    Unfocus,
    Close { wid },
    ActivateWorkspace { index },
    StackChanged { stack: Option<StackInfo> },
}
```

`Unfocus` and `StackChanged` are additions beyond the minimum in `PLAN.md`: the former clears
keyboard focus when switching to an empty workspace, the latter lets the daemon drive the
stack tab-bar overlay without polling layout state.

### 4.2 IPC

The daemon listens on a Unix socket at
`~/Library/Application Support/cosmic/cosmic-mac-wm.sock` and exchanges one JSON document per
line:

```json
{"request":"list_windows"}
{"request":"action","data":{"action":"focus","args":{"direction":"left"}}}
{"request":"focus","data":{"wid":1234}}
{"request":"tree"}
{"request":"config"}
{"request":"restore"}
{"request":"subscribe"}
```

Responses are `windows`, `workspaces`, `tree`, `ok`, `config`, `error` or `state`
(adjacently tagged with `response`/`data`). `Response::Tree` carries the `TreeNode` snapshot
with per-node frames. `cosmic-mac-ctl` is a thin client over these payloads.

Every request gets one reply line, except `subscribe`: it turns the connection into a stream
of `state` lines (`ShellState`: the workspace pool, the active workspace's tiling flag and
whether its focused window floats), the current state at once and then one line per change,
until either side closes it. The daemon publishes the state after every message it handles
and only sends it when it changed; input on a subscribed connection is ignored. The
menu-bar status item (`crates/cosmic-mac-status`) and `cosmic-mac-ctl watch` use it.

## 5. Platform boundary (consumed, not implemented here)

The daemon translates each `Command` into platform operations:

* `SetFrame` — AX set-size / set-position / set-size workaround;
* `Raise` — raise without stealing focus where possible;
* `Hide` / `Show` — corner-park and unpark;
* `Focus` — focus-without-raise (`_SLPSSetFrontProcessWithOptions` +
  `SLPSPostEventRecordTo`), falling back to activation;
* `Close` — AX close button / terminate;
* `ActivateWorkspace` — update the display's visible workspace, panel state and journal;
* `StackChanged` — create/position/hide the tab-bar overlay above the active member.

The layout engine assumes the platform can move windows to arbitrary global frames and that
parked windows are not user-visible. It does not assume anything about z-order beyond
`Raise`.

## 6. Daemon behaviour that depends on the layout contract

* **Float-by-default rules** are evaluated in `Config::should_float` inside the layout crate,
  so they are unit-testable. The daemon only supplies `ToplevelInfo`.
* **Virtual workspaces** follow AeroSpace's model: windows of inactive workspaces are parked
  at the display's bottom-right corner. The layout emits `Hide` for every window that leaves
  the active workspace, which is exactly the park operation.
* **Crash safety**: the daemon journals original frames before touching a window and replays
  the journal on the next start if it is dirty. `Request::Restore` triggers the same path.
* **Drag to stack**: a listen-only mouse tap reports left-button down, dragged and up. While
  the button is held the daemon leaves alone a user move (or edge resize) of the tiled window
  whose tile the button went down on; on release a pure move whose release point is in a
  `Layout::drop_zone` stacks it with `Layout::stack_onto`, anything else snaps back. Moves
  that arrive just after a release (`DROP_GRACE`, 250 ms) end that drag. While the pointer
  is over a stack zone the tab-bar thread highlights the target tile.
* **Keybindings**: the daemon loads COSMIC's `keybindings.ron` format (defaults embedded with
  `include_str!`) and maps actions onto `cosmic_mac_protocol::Action`. The intended mapping
  for the default set is:

  | COSMIC binding | cosmicMac action |
  |---|---|
  | `Focus(Left/Right/Up/Down)` | `Focus { direction }` |
  | `Move(...)` | `Move { direction }` |
  | `Workspace(n)` / `LastWorkspace` | `Workspace { index }` / `LastWorkspace` |
  | `MoveToWorkspace(n)` / `MoveToLastWorkspace` | matching move actions |
  | `PreviousWorkspace` / `NextWorkspace` | matching actions |
  | `ToggleOrientation` (`Super+O`) | `ToggleOrientation` |
  | `ToggleStacking` (`Super+S`) | `ToggleStacking` |
  | `ToggleTiling` (`Super+Y`) | `ToggleTiling` |
  | `ToggleWindowFloating` (`Super+G`) | `ToggleWindowFloating` |
  | `SwapWindow` (`Super+X`) | `SwapWindow` |
  | `Maximize` (`Super+M`) | `Maximize` |
  | `Resizing(Outwards/Inwards)` (`Super+R`) | `ResizeMode` plus `Resize` on arrows |
  | `Close` (`Super+Q`) | `Close` |
  | `Fullscreen` (`F11`) | handled by the platform/daemon, not the layout |

  Actions for the launcher, app library, workspace overview and window switcher stay unarmed
  until milestone 2, so `Cmd+Tab` and `Cmd+Space` keep working.

## 7. Testing strategy

`cosmic-mac-layout` is tested through its public API only. The test suite uses a harness
(`tests/common/mod.rs`) that owns a `Layout` and a `FakePlatform` which applies commands; after
every event the harness asserts that the fake platform's frames, visibility, focus, workspace
and stack match the layout (`assert_synced`). This catches diffing bugs as well as geometry
bugs.

Test files:

| File | Focus |
|---|---|
| `tests/basic.rs` | insertion, collapse, events, snapshots, work-area/config changes |
| `tests/focus.rs` | directional focus, nesting, wrap, floating fallback, MRU cycling |
| `tests/move_resize.rs` | move/swap/orientation, resize growth and minimum clamping |
| `tests/stacks.rs` | stacking, unstacking, tab visibility, member removal |
| `tests/drag_stack.rs` | drop zones and `stack_onto`: onto windows, stacks, nested groups, no-ops |
| `tests/floating.rs` | float rules, placement, cascade, layer toggles |
| `tests/workspaces.rs` | switching, moving, wrap, last workspace, trailing removal |
| `tests/gaps.rs` | outer/inner gap arithmetic and clamping |
| `tests/property.rs` | seeded-LCG action sequences |

The property tests use a deterministic LCG rather than adding `proptest`. They assert, in
tiling mode with zero gaps, that visible frames never overlap, stay inside the work area and
exactly cover it; with gaps they assert no overlap and containment. A second sequence mixes
workspace operations and asserts that no window is lost or duplicated; a third mixes
tiling/floating/maximize and asserts frames stay in bounds.

Run:

```sh
cargo test -p cosmic-mac-protocol -p cosmic-mac-layout
cargo clippy -p cosmic-mac-protocol -p cosmic-mac-layout -- -D warnings
cargo fmt -p cosmic-mac-protocol -p cosmic-mac-layout -- --check
```

## 8. Invariants

The engine maintains these invariants (and tests them):

1. Every managed window is in exactly one workspace, in exactly one layer (tiling tree or
   floating list), and appears at most once in a stack.
2. `focus` is either `None` or a window in the active workspace; when it is set it is visible
   (the active member of its stack, or floating).
3. Every workspace with windows has a focus.
4. Groups have at least two children and matching `children`/`sizes` lengths after
   normalization; the root is never a single-child group.
5. Stack active indices are valid; a stack never has zero members; a stack with one member is
   converted to a plain window.
6. In tiling mode with zero gaps and no maximized window, visible frames exactly tile the work
   area.
7. Commands are idempotent: replaying a diff onto the platform yields the layout's desired
   state, and a no-op action returns no commands.
8. Workspace indices are contiguous from 1; empty trailing workspaces are removed after
   removal/move operations; at least one workspace exists.

## 9. Deviations from `PLAN.md` and upstream

These are deliberate and documented so future work can revisit them:

* **Single workspace pool.** `PLAN.md` says the daemon owns display assignment; the layout
  models one pool and does not implement `assign_display` yet. `WorkspaceGroup` keeps the IPC
  shape open for it.
* **Orientation naming.** `Orientation::Horizontal` means children are laid out horizontally
  (a vertical split line). Upstream cosmic-comp names the split axis, so its `Vertical` is our
  `Horizontal`. This is the only naming difference.
* **ToggleStacking semantics.** The task specifies adding the focused window to the
  neighbour's stack. Upstream cosmic-comp's `ToggleStacking` converts a window into a
  one-window stack (and back). cosmicMac follows the task because it is the useful behaviour
  on macOS, where z-order cannot be controlled and a one-window stack has no visual benefit.
* **Focus wrap.** Upstream cosmic-comp's tree focus returns no target at the edge. The task
  explicitly requires wrap-around, so cosmicMac wraps to the opposite edge when the root
  splits along the requested axis.
* **SwapWindow.** It swaps the focused window with the previous entry in the focus history.
  This is the only unambiguous "swap window" behaviour without a direction; mixed
  tiling/floating/stack locations exchange occupants.
* **Floating placement.** cosmic-comp lets the client choose the initial position. cosmicMac
  centres and cascades because AX only reports the frame after the app has already placed the
  window; the size is preserved. Dialogs are centred exactly.
* **`ToplevelInfo::initial_frame`.** An extension over cosmic-protocols, needed for floating
  size on macOS.
* **Maximize.** One maximized window per workspace, covering the full work area (gaps do not
  inset a maximized frame). This is the macOS expectation; cosmic-comp's per-window maximize
  state is otherwise mirrored.
* **Next/Previous workspace wrap** instead of creating a new trailing workspace. Empty
  trailing workspaces are removed, so the pool is finite; wrapping is predictable and keeps
  `Super+Ctrl+Arrow` useful.
* **Minimum size.** AX exposes per-window minimum sizes, but the layout uses a global
  `MIN_WINDOW_SIZE = 100 px` so the pure engine has no platform dependency. The daemon can
  clamp further when applying `SetFrame`.
* **Fullscreen** is not a layout action. macOS fullscreen is a native Space transition, so it
  belongs to the platform/daemon; the layout only knows maximize.
* **`Unfocus` / `StackChanged` commands** are additions for daemon ergonomics (empty
  workspace focus clearing and tab-bar updates).
* **Tests use a seeded LCG**, as permitted by the task, instead of adding `proptest`.

## 10. Licensing

cosmicMac is GPL-3.0-or-later (see `LICENSE`). cosmic-comp and the COSMIC shell applications
are GPL-3; libcosmic is MPL-2.0. The layout engine is a clean-room re-implementation of
cosmic-comp's documented tiling semantics; no cosmic-comp source is copied into this
repository.
