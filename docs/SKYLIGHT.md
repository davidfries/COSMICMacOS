# SkyLight and Accessibility FFI notes

This document records every private API cosmicMac relies on, where the prototype
came from, and what actually happened when the spikes ran on macOS 15.6 with SIP
enabled. It is the reference for `crates/cosmic-mac-platform`.

Verified on:

| Environment | Version |
|---|---|
| Host (build machine) | macOS 15.7.9, aarch64, SIP on |
| Test VM | macOS 15.6, aarch64, SIP on, 2 CPU / 4 GB |
| Rust | 1.99.1, edition 2024 |
| SkyLight | `/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight` |

Everything here is used with SIP **on**. No scripting addition, no SIP disable,
no WindowServer injection.

---

## 1. Loading SkyLight

SkyLight is private but present in every macOS install. cosmicMac does **not**
link it at build time. `src/skylight.rs` does:

```rust
dlopen("/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight",
       RTLD_LAZY | RTLD_LOCAL)
```

and resolves each symbol with `dlsym` on first use. Reasons:

* the crate builds and its unit tests run even if Apple moves the framework;
* a missing symbol becomes `PlatformError::SkyLightUnavailable` with the symbol
  name, not a link error at build time;
* `otool -L` on the shipped binary shows no private framework dependency.

Function pointers are stored as `unsafe extern "C" fn` types and transmuted from
`dlsym` after a size assertion. Each wrapper is `unsafe`; the safe modules
(`window`, `overlay`, `observers`) are the intended entry points.

`SLSMainConnectionID()` returning 0 means there is no GUI session (for example
an SSH session before login); the platform layer reports that explicitly.

---

## 2. Function surface

Prototypes were cross-checked against:

* yabai `src/misc/extern.h` (MIT)
* JankyBorders `src/misc/extern.h` and `src/misc/window.h` (GPL-3.0)
* rift `src/sys/skylight.rs` (GPL-3.0)
* AeroSpace (MIT) for the AX-only behaviour, no SkyLight prototypes

All functions below are declared in `src/skylight.rs`.

### Connection and window metadata

```c
int      SLSMainConnectionID(void);
CGError  SLSNewConnection(int zero, int *cid);
CGError  SLSReleaseConnection(int cid);

CGError  SLSGetWindowBounds(int cid, uint32_t wid, CGRect *frame);
CGError  SLSGetWindowLevel(int cid, uint32_t wid, int32_t *level);
int32_t  SLSGetWindowSubLevel(int cid, uint32_t wid);
CGError  SLSGetWindowOwner(int cid, uint32_t wid, int *owner_cid);
CGError  SLSConnectionGetPID(int cid, pid_t *pid);
CGError  SLSWindowIsOrderedIn(int cid, uint32_t wid, uint8_t *ordered);
CGError  SLSGetWindowAlpha(int cid, uint32_t wid, float *alpha);
```

Notes:

* `SLSGetWindowLevel` writes an `int32_t`. JankyBorders declares `int64_t *`;
  yabai and rift use `int *`. Use `int32_t`.
* `SLSGetWindowSubLevel` returns the sub-level by value. Copy it when ordering
  an overlay above a target, otherwise the overlay can end up inside the
  target's stacking group.
* `SLSConnectionGetPID` is the only way to map a window to a PID without AX.

### Geometry and ordering

```c
CGError SLSMoveWindow(int cid, uint32_t wid, CGPoint *point);
CGError SLSOrderWindow(int cid, uint32_t wid, int order, uint32_t relative_to);
CGError SLSDisableUpdate(int cid);
CGError SLSReenableUpdate(int cid);
```

`order` is `1` (above), `0` (out) or `-1` (below). `relative_to = 0` means
"front/back of the level".

Ordering **another process's** window through SkyLight is rejected with
`kCGErrorFailure` (1000) unless the caller holds Accessibility. The AX
`AXRaise` action is the supported path; `window::raise` tries AX first and maps
the SLS failure to `AccessibilityDenied` when the grant is missing.

### Tags and window creation

```c
CGError SLSNewWindow(int cid, int store_type, float x, float y,
                     CFTypeRef region, uint32_t *wid);
CGError SLSNewWindowWithOpaqueShapeAndContext(
            int cid, int store_type, CFTypeRef region, CFTypeRef opaque_shape,
            int options, uint64_t *tags, float x, float y, int tag_size,
            uint32_t *wid, void *context);
CGError SLSReleaseWindow(int cid, uint32_t wid);
CGError SLSSetWindowTags(int cid, uint32_t wid, uint64_t *tags, int tag_size);
CGError SLSClearWindowTags(int cid, uint32_t wid, uint64_t *tags, int tag_size);
CGError SLSSetWindowShape(int cid, uint32_t wid, float x_off, float y_off,
                          CFTypeRef shape);
CGError SLSSetWindowResolution(int cid, uint32_t wid, double resolution);
CGError SLSSetWindowOpacity(int cid, uint32_t wid, bool is_opaque);
CGError SLSSetWindowAlpha(int cid, uint32_t wid, float alpha);
CGError SLSSetWindowLevel(int cid, uint32_t wid, int level);
CGError SLSSetWindowSubLevel(int cid, uint32_t wid, int level);
CGError SLSWindowSetShadowProperties(uint32_t wid, CFDictionaryRef props);
CGError CGSNewRegionWithRect(CGRect *rect, CFTypeRef *region);
CFTypeRef CGRegionCreateEmptyRegion(void);
```

* `store_type = 2` is `kCGBackingStoreBuffered`.
* Overlay tags are `(1 << 1) | (1 << 9)` = *floating* + *ignore for events*
  (JankyBorders). `tag_size` is 64 (bits), i.e. `0x40`.
* `SLSSetWindowOpacity(cid, wid, false)` means "not opaque", which is what an
  overlay with a transparent background needs. It is **not** an alpha of zero.
* `SLSNewWindowWithOpaqueShapeAndContext` is only needed for unmanaged windows
  (JankyBorders uses it for proxy windows with options `13 | (1 << 18)` and an
  empty opaque region). Milestone 1 does not need it; the declaration is kept
  for the tab-bar proxy path.
* `CGSNewRegionWithRect` returns a +1 CFType that must be `CFRelease`d.

### Transactions

```c
CFTypeRef SLSTransactionCreate(int cid);
CGError   SLSTransactionCommit(CFTypeRef txn, int synchronous);
CGError   SLSTransactionOrderWindow(CFTypeRef txn, uint32_t wid, int order,
                                    uint32_t relative_to);
CGError   SLSTransactionSetWindowLevel(CFTypeRef txn, uint32_t wid, int level);
CGError   SLSTransactionSetWindowSubLevel(CFTypeRef txn, uint32_t wid, int level);
CGError   SLSTransactionMoveWindowWithGroup(CFTypeRef txn, uint32_t wid,
                                            CGPoint point);
```

**Observed on 15.6:** for a plain `SLSNewWindow` window created by a CLI process,
`SLSTransactionMoveWindowWithGroup` and `SLSTransactionOrderWindow` can return
opaque `CGError` values (observed `34012224`, then different values). The
non-transactional `SLSMoveWindow` / `SLSSetWindowLevel` + `SLSOrderWindow` path
works. `OverlayWindow` therefore tries the transaction and falls back, logging at
debug level. JankyBorders uses transactions successfully; the difference is
likely its dedicated connection plus `SLSNewConnection` setup, but the fallback
is reliable and sufficient for a 60 Hz WM.

### Spaces

```c
CFArrayRef SLSCopySpacesForWindows(int cid, int selector, CFArrayRef windows);
void       SLSMoveWindowsToManagedSpace(int cid, CFArrayRef windows,
                                        uint64_t sid);
int        SLSSpaceGetType(int cid, uint64_t sid);
```

**Critical gotcha:** `SLSMoveWindowsToManagedSpace` returns **`void`**, not
`CGError`. yabai declares it `void`; JankyBorders' header declares `CGError` in
one place. Treating the absent return register as an error produced random
"CGError" values (`239075328`, `505413632`, ...) during Spike C. The wrapper now
returns `()` and success is checked by observing the window.

`selector = 0x7` returns the window's managed Space IDs. This is how the overlay
follows a target to its Space:

```text
SLSCopySpacesForWindows(cid, 0x7, [target_wid]) -> [sid]
SLSMoveWindowsToManagedSpace(overlay_cid, [overlay_wid], sid)
```

A raw `SLSNewWindow` window is not automatically on a managed Space. Without
this step it can be "on screen" in `CGWindowList` yet never composited.

### Notifications

```c
typedef void (*connection_callback)(uint32_t type, void *data, size_t len,
                                    void *context, int cid);
CGError SLSRegisterConnectionNotifyProc(int cid, connection_callback cb,
                                        uint32_t event, void *context);
CGError SLSRegisterNotifyProc(connection_callback cb, uint32_t event,
                              void *context);
CGError SLSRequestNotificationsForWindows(int cid, uint32_t *wids, int count);
```

The callback runs on the run loop that owns the SkyLight connection (the main
run loop for a normal process). There is no unregister API; register once per
process and dispatch through a global handler. `SLSRequestNotificationsForWindows`
is capped at 1024 windows per call; chunk larger lists.

**The event port must be drained.** Registering the callback is not enough:
`SLSRegisterNotifyProc` / `SLSRegisterConnectionNotifyProc` callbacks only fire
while the connection's event port is being pumped. JankyBorders does this with
`SLSGetEventPort` + a `CFMachPort` run-loop source; cosmicMac starts a dedicated
thread that loops:

```c
CGError SLSGetEventPort(int cid, mach_port_t *port);
CGEventRef SLEventCreateNextEvent(int cid);   // null when the queue is empty
```

and releases each returned event. Before this pump was added, the VM delivered
**zero** window events even though every registration returned 0. With the pump,
`spike-b watch` reports `WindowCreated`, `WindowMoved`, `WindowOrdered`,
`WindowLevelChanged`, `WindowHidden`, `WindowDestroyed` for the AppKit spike
window, with no Accessibility grant.

Event IDs used (from rift's `KnownCGSEvent`, itself derived from yabai and
`CGSInternal`):

| ID | Name | Payload |
|---|---|---|
| 804 | `WindowClosed` | `u32 wid` |
| 805 | `WindowDisplayChanged` | `u32 wid` |
| 806 | `WindowMoved` | `u32 wid` |
| 807 | `WindowResized` | `u32 wid` |
| 808 | `WindowReordered` | `u32 wid` |
| 811 | `WindowLevelChanged` | `u32 wid` |
| 815 | `WindowUnhidden` | `u32 wid` |
| 816 | `WindowHidden` | `u32 wid` |
| 1322 | `WindowTitleChanged` | `u32 wid` (Sequoia+) |
| 1325 | `SpaceWindowCreated` | `u64 sid`, `u32 wid` |
| 1326 | `SpaceWindowDestroyed` | `u64 sid`, `u32 wid` |
| 1508 | `FrontmostApplicationChanged` | best-effort `u32 pid` |

Do not assume the payload is longer than 4/12 bytes; always bounds-check. Unknown
events are forwarded as `PlatformEvent::UnhandledSls { event, wid, len }` so a
future macOS can be diagnosed without a rebuild.

### Focus without raise

```c
typedef struct { uint32_t high; uint32_t low; } ProcessSerialNumber;

CGError GetProcessForPID(pid_t pid, ProcessSerialNumber *psn);   // Carbon
CGError _SLPSSetFrontProcessWithOptions(const ProcessSerialNumber *psn,
                                        uint32_t wid, uint32_t mode);
CGError SLPSPostEventRecordTo(const ProcessSerialNumber *psn,
                              const uint8_t *bytes);
```

`GetProcessForPID` lives in `ApplicationServices` and needs no special
permission. The event record is the yabai/Hammerspoon technique (Hammerspoon
issue #370; also in rift `make_key_window`):

```text
bytes = [0u8; 0x100]
bytes[0x04] = 0xf8
bytes[0x08] = 0x01          // second record: 0x02
bytes[0x3a] = 0x10
bytes[0x3c..0x40] = wid.to_le_bytes()
bytes[0x20..0x30] = 0xff
_SLPSSetFrontProcessWithOptions(&psn, wid, 0x200 /* kCPSUserGenerated */)
SLPSPostEventRecordTo(&psn, bytes)   // 0x01
SLPSPostEventRecordTo(&psn, bytes)   // 0x02
```

**Observed on 15.6 without Accessibility:** `_SLPSSetFrontProcessWithOptions`
returns 0, then the first `SLPSPostEventRecordTo` returns `1002`
(`kCGErrorInvalidConnection`). The platform layer maps that to
`AccessibilityDenied` when the process is not trusted. With the grant this is the
yabai/rift/glide path and is expected to succeed; the spike prints a clear
"needs Accessibility" message until the one-time GUI grant exists.

---

## 3. Overlay windows (JankyBorders technique)

`src/overlay.rs` implements the milestone-1 tab-bar primitive.

Creation sequence (all verified in the VM):

1. `SLSNewConnection(0, &cid)` — a dedicated connection per overlay. JankyBorders
   does this per border window. Windows created on the main connection of a CLI
   process were not reliably composited.
2. `CGSNewRegionWithRect(frame)` + `SLSNewWindow(cid, 2, -9999, -9999, region, &wid)`.
3. `SLSSetWindowResolution(cid, wid, 2.0)` on Retina.
4. `SLSSetWindowTags(cid, wid, &((1<<1)|(1<<9)), 64)` — floating, ignore events.
5. `SLSSetWindowOpacity(cid, wid, false)` and remove the shadow via
   `SLSWindowSetShadowProperties(wid, {"com.apple.WindowShadowDensity": 0})`.
6. `SLWindowContextCreate(cid, wid, NULL)` — the Core Graphics context used for
   drawing.
7. Move to the target's managed Space (`SLSCopySpacesForWindows` + `SLSMoveWindowsToManagedSpace`).
8. Order above the target: copy its level and sub-level, then
   `SLSTransactionOrderWindow(txn, overlay_wid, 1, target_wid)` (falling back to
   `SLSSetWindowLevel`/`SubLevel` + `SLSOrderWindow`).
9. `SLSDisableUpdate` / `SLSReenableUpdate` around shape changes, and
   `SLSWindowFreezeWithOptions` / `SLSWindowThaw` around drawing. Without the
   freeze/thaw pair the surface can keep stale content.

Drawing uses `CGContextClearRect`, `CGContextSetRGBStrokeColor`,
`CGContextSetLineWidth`, `CGContextStrokeRect`, `CGContextFlush`, then
`SLSFlushWindowContentRegion`. The context's origin is the window's top-left;
the border is inset by half the line width.

### AppKit `windowNumber` test (Spike C)

An `NSWindow`'s `windowNumber` can be used as the relative window in the same
ordering calls. In the VM:

```text
AppKit windowNumber = 157
overlay: wid=158 frame=(316,329 568x396)
NSWindow moved to x=328, 336, 344, 352 ...
```

The overlay tracked the NSWindow and its own window capture showed the drawn
border. The only warning was `window 157 is not on a managed Space` while
querying its Space; ordering still worked. This answers the milestone-2
question: a libcosmic/winit `NSWindow` can be the tab-bar anchor.

### Evidence capture without Screen Recording

`screencapture` over SSH is TCC-filtered until Screen Recording is granted: the
full-screen image contains the wallpaper and menu bar but **not other apps'
windows**. `screencapture -l <wid>` also fails with "could not create image from
window".

`overlay::capture_window_png(wid, path)` uses
`CGWindowListCreateImage(CGRectNull, kCGWindowListOptionIncludingWindow, wid,
kCGWindowImageBoundsIgnoreFraming)` and ImageIO. Capturing **our own** window
does not need Screen Recording, so Spike C writes `*-overlay.png` and
`*-target.png` as evidence. The captured overlay image is transparent except for
the drawn border.

`CGWindowListCreateImage` is deprecated in favour of ScreenCaptureKit (macOS 14)
but still works on 15.6.

---

## 4. Accessibility (AX)

`src/ax.rs` declares the AX functions directly rather than depending on a
generated bindings crate. Link: `ApplicationServices.framework`.

```c
AXUIElementRef AXUIElementCreateApplication(pid_t pid);
AXUIElementRef AXUIElementCreateSystemWide(void);
AXError AXUIElementCopyAttributeValue(AXUIElementRef, CFStringRef, CFTypeRef *);
AXError AXUIElementSetAttributeValue(AXUIElementRef, CFStringRef, CFTypeRef);
AXError AXUIElementIsAttributeSettable(AXUIElementRef, CFStringRef, Boolean *);
AXError AXUIElementPerformAction(AXUIElementRef, CFStringRef);
AXError _AXUIElementGetWindow(AXUIElementRef, uint32_t *wid);   // private
Boolean AXIsProcessTrusted(void);
Boolean AXIsProcessTrustedWithOptions(CFDictionaryRef);
AXValueRef AXValueCreate(uint32_t type, const void *value);
Boolean AXValueGetValue(AXValueRef, uint32_t type, void *out);
```

Attributes used: `AXWindows`, `AXFocusedWindow`, `AXRole`, `AXSubrole`,
`AXTitle`, `AXPosition`, `AXSize`, `AXMinimized`, `AXCloseButton`,
`AXFullScreen`, `AXMain`, `AXFocused`, `AXHidden`. Actions: `AXPress`,
`AXRaise`.

* `_AXUIElementGetWindow` is private but exported by HIServices and is the only
  way to map an AX window element to a `CGWindowID`. It is used by yabai,
  AeroSpace and rift.
* `AXValue` types: `kAXValueCGPointType = 1`, `kAXValueCGSizeType = 2`,
  `kAXValueCGRectType = 3`.
* Error codes that matter: `0` success, `-25205` attribute unsupported,
  `-25212` no value, `-25211` API disabled (no Accessibility grant),
  `-25209` notification already registered (treat as success).
* AX calls are safe from any thread. Observers deliver on the run loop their
  source was added to.

### Window geometry workaround

`set_frame` applies **size → position → size**, then reads the frame back and
does one corrective position+size pass if AppKit clamped it. Windows with
resizing constraints (Terminal, TextEdit with fixed wrap, many Electron apps)
clamp a size applied before the move to the target display.

`set_position` and `set_size` are separate primitives for callers that only need
one. Parking uses `set_position` so the saved frame's size is untouched.

### Observers

One `AXObserver` per application. App-level notifications:
`AXWindowCreated`, `AXFocusedWindowChanged`, `AXApplicationActivated`,
`AXApplicationHidden`, `AXApplicationShown`. Per-window notifications:
`AXWindowMoved`, `AXWindowResized`, `AXTitleChanged`, `AXWindowMiniaturized`,
`AXWindowDeminiaturized`, `AXUIElementDestroyed`. When `AXWindowCreated` fires,
the callback registers the per-window set on the new element through the raw
observer pointer.

AX latency during a user drag can lag; the WM should prefer SLS move/resize
events for geometry and use AX for titles/focus.

---

## 5. CGEventTap

`src/eventtap.rs` creates an active tap at `kCGSessionEventTap` with
`kCGEventTapOptionDefault`, head-inserted, interested in key down/up and flags
changed.

* On `kCGEventTapDisabledByTimeout` (`4294967294`) and
  `kCGEventTapDisabledByUserInput` (`4294967295`) the callback calls
  `CGEventTapEnable(tap, true)` and logs. Keep the callback allocation-free.
* Returning `NULL` from the callback swallows the event. cosmicMac swallows only
  the configured keycode + exact modifier mask; everything else is returned
  unchanged (Cmd+C passes through).
* Accessibility is required to create the tap. `CGEventTapCreate` returning
  `NULL` is reported as a permission error with instructions.
* Secure Event Input (password fields, some terminals) disables the tap
  completely. The documented fallback is Carbon `RegisterEventHotKey` for
  critical bindings; Carbon hotkeys cannot swallow events, so it is a degraded
  mode. It is intentionally not wired up in the platform crate yet.

---

## 6. Coordinate spaces

Two global spaces exist:

* **AX / SkyLight / CGWindow / CGDisplay**: origin at the top-left of the main
  display, `y` down. `Rect` in this crate is always in this space.
* **NSScreen / NSWindow**: origin at the bottom-left of the main display, `y`
  up. `NSScreen.frame()` / `visibleFrame()` are here.

The flip anchor is the **main display's height**, not the height of the display
the rectangle belongs to:

```text
ax_y = main_height - (ns_y + height)
```

Displays above the main display therefore have negative `ax_y`, exactly as
CoreGraphics reports them. `coords::ns_to_ax` / `ax_to_ns` implement this and
have unit tests.

Work areas come from `NSScreen.visibleFrame` on the main thread. Off the main
thread the platform layer falls back to the full display frame and logs a
warning.

---

## 7. TCC and code signing

TCC keys the Accessibility and Screen Recording grants on the code signature's
**designated requirement**. Ad-hoc signing (`codesign -s -`) changes the
requirement on every rebuild, so the grant is lost. `scripts/sign.sh`:

* creates a self-signed `cosmicMac Dev` certificate with
  `extendedKeyUsage=codeSigning` (OpenSSL config in
  `~/Library/Application Support/cosmicMac/signing/`);
* exports a **legacy** PKCS#12 (`openssl pkcs12 -legacy`; macOS
  Security.framework cannot read OpenSSL 3+ AES-256 PKCS#12);
* imports it into a dedicated keychain
  `cosmicMac-signing.keychain-db` with a random password stored next to it
  (mode 600), sets the key partition list for `codesign`, and adds the keychain
  to the user search list. This avoids the GUI keychain prompt that a login
  keychain import triggers over SSH;
* signs with `--identifier com.cosmicmac.<binary>` so the requirement is
  `identifier "com.cosmicmac.ax-status" and certificate root = H"<sha1>"`.

`codesign --verify` succeeds even though the self-signed root is not trusted.
Trust (`security add-trusted-cert`) is a separate, confirm-gated step; it needs
one GUI/keychain authorization and is never automated.

Observed: with an untrusted identity, `security find-identity -v -p codesigning`
reports zero valid identities, but `security find-identity -p codesigning
<keychain>` finds it and `codesign` signs successfully. Do not require `-v` in
scripts.

The VM grant survives a rebuild as long as the same certificate is used. The
acceptance test is: grant Accessibility once, rebuild, redeploy, run `ax-status`
and check `accessibility_trusted: true`.

---

## 8. Gotchas found during the spikes

1. **`SLSMoveWindowsToManagedSpace` returns `void`.** Reading its return value
   as `CGError` yields random numbers. Fixed in the wrapper.
2. **Transaction APIs fail for plain `SLSNewWindow` windows on 15.6.** Fall back
   to `SLSMoveWindow` / `SLSOrderWindow`. The fallback is non-atomic but works.
3. **Raw SLS windows need a dedicated connection and a managed Space.** Create
   with `SLSNewConnection`, then move to the target's Space; otherwise the window
   can be in `CGWindowList` but never composited.
4. **`SLSSetWindowOpacity(cid, wid, false)` is not alpha.** It marks the window
   non-opaque so the transparent background composites. Use
   `SLSSetWindowAlpha` for actual alpha.
5. **Freeze/thaw around drawing.** `SLSWindowFreezeWithOptions` +
   `SLSWindowThaw` avoid stale surfaces when changing shape or content.
6. **`screencapture` is TCC-filtered.** Without Screen Recording, the full-screen
   capture omits other apps' windows and `screencapture -l` fails. Capture your
   own window with `CGWindowListCreateImage` for evidence.
7. **Notifications need the event port pump.** See section 2. A successful
   registration is not evidence that callbacks will fire.
8. **`SLSOrderWindow` on a foreign window fails without Accessibility**
   (`kCGErrorFailure`, 1000). Use AXRaise; map the failure to
   `AccessibilityDenied` when untrusted.
9. **`SLPSPostEventRecordTo` returns 1002 without Accessibility.** The CPS event
   record path is gated even though `_SLPSSetFrontProcessWithOptions` succeeds.
10. **CGWindow titles need Screen Recording or AX.** Without either,
   `kCGWindowName` is empty; `list_windows` still returns owner names and frames.
11. **AX observers are per app and must be added to a running run loop.** Creating
    them without running the loop produces no callbacks. The daemon should own
    the main run loop.
12. **`GetProcessForPID` is Carbon/ApplicationServices**, not SkyLight. It needs
    no permission and returns a PSN even when the app is not frontmost.
13. **`NSScreen` APIs need the main thread.** `MainThreadMarker::new()` returns
    `None` elsewhere; the platform layer falls back to CG bounds.

---

## 9. macOS 26 (Tahoe)

The plan notes that sub-level message IDs differ on macOS 26. What is known and
what cosmicMac does:

* JankyBorders only resolves `SLSWindowIteratorGetCornerRadii` on macOS 26
  (`__builtin_available(macOS 26.0, *)`), so new symbols can appear without
  breaking older ones.
* Window levels and sub-levels are still queried by value
  (`SLSGetWindowLevel` / `SLSGetWindowSubLevel`); the transaction setters are the
  same names. If a future macOS changes the sub-level message, the overlay can
  fall back to copying only the level.
* Notification IDs are not contractual. `PlatformObserver` forwards unknown IDs
  as `UnhandledSls` with the payload length and first `u32`, which is enough to
  identify a shifted ID from a VM run.
* `CGWindowListCreateImage` may disappear; the ScreenCaptureKit path is the
  milestone-3 replacement for thumbnails and evidence capture.

When testing on macOS 26, run `spike-b watch` and move/resize a window; the
`UnhandledSls` events will show any renumbering immediately.

---

## 10. Attribution and licensing

* **yabai** (MIT): extern prototypes, event IDs, focus-without-raise sequence,
  space/raise behaviour.
* **JankyBorders** (GPL-3.0): overlay window creation, tags, separate
  connection, freeze/thaw, transaction ordering, Space membership.
* **rift** (GPL-3.0): Rust SkyLight declarations, CPS event record bytes,
  notification payload layouts.
* **glide** (GPL-3.0): focus-without-raise pattern reference.
* **AeroSpace** (MIT): corner-parking model, AX observer-per-app design,
  "external activation wins" behaviour.
* **Hammerspoon** issue #370: the synthetic event record layout.

cosmicMac is GPL-3.0-or-later. The prototypes are redeclared from public
knowledge; no source code is copied from the projects above.

---

## 11. Verification commands

Host (never builds the whole workspace while other crates are mid-write):

```sh
cargo build -p cosmic-mac-platform --examples
cargo clippy -p cosmic-mac-platform -- -D warnings
cargo fmt -p cosmic-mac-platform -- --check
cargo test -p cosmic-mac-platform
```

VM (after `scripts/deploy.sh`):

```sh
~/cosmicMac/bin/ax-status
~/cosmicMac/bin/spike-b selftest
~/cosmicMac/bin/spike-b list
~/cosmicMac/bin/spike-b watch --seconds 5
~/cosmicMac/bin/spike-c --seconds 10 --capture /tmp/spike-c
~/cosmicMac/bin/spike-c --target <wid> --seconds 10 --capture /tmp/spike-c
~/cosmicMac/bin/spike-c --appkit --seconds 10 --capture /tmp/spike-c
```

Spike results on 15.6 are recorded in the commit message and in
`scratch/vm-spike-c.png` (full desktop; TCC-filtered), plus
`scratch/spike-c-overlay.png`, `scratch/spike-c-target.png`,
`scratch/sc-foreign-overlay.png`, and `scratch/sc-appkit-overlay.png` (window
captures that prove the drawing and ordering).
