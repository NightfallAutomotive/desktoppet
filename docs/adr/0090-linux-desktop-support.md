# ADR-0090: Linux desktop support

Date: 2026-10-22
Status: Accepted for this fork; staged, each stage gated. Partially supersedes
ADR-0006. No Linux support claim is made until Stage L8 passes.
Depends on: ADR-0006, ADR-0011, ADR-0030, ADR-0058, ADR-0081

## Context

ADR-0006 kept Linux out of the launch matrix and deferred it to a post-launch
evaluation. This fork has now decided to run on Linux. Windows and macOS have
shipped (v2.3.1), so Linux work no longer competes with the launch.

The shared crates (config, runtime, model, input contract, render snapshot,
storage, update and UI protocol) are already free of platform types. Every
operating-system surface has two implementations, Windows and macOS, and
neither can be reused on Linux:

- `bongocat-overlay`: a D3D11/DirectComposition or Metal model window
  (~13.5k lines including tests);
- `bongocat-platform`: Raw Input or CGEventTap input, the system menu, startup
  items, single instance, theme and display queries (~10.8k lines);
- `bongocat-live2d`: links only the Windows and macOS Cubism Core binaries;
- `bongocat-update`: `compile_error!` outside macOS and x86_64 Windows;
  `UpdateTargetTriple` has no Linux member;
- `bongocat-packaging`, `deny.toml` and `tools/tests/test_packaging_contract.py`
  declare only the Windows and macOS targets and artifacts.

The current Linux build state, from `cargo check --workspace` on
`x86_64-unknown-linux-gnu`, is that the first failure is `rfd` asking for a
`gtk3` or `xdg-portal` backend. Past that point, the update crate refuses
the target by design.

Linux also differs in kind, not just in API:

- **Wayland has no global input observation.** No protocol or portal gives an
  ordinary client listen-only access to other applications' keys. The
  `InputCapture` portal grabs input exclusively at a pointer barrier, which is
  the opposite of what BongoCat needs. The only route that works on both
  Wayland and X11 is reading `/dev/input/event*` through evdev, which needs
  membership in the `input` group. That access can read every key the user
  types.
- **Wayland has no global cursor position.** Only relative motion is
  available, through evdev.
- **Keeping the model window above other windows depends on the compositor.**
  `wlr-layer-shell` gives an overlay layer on KDE Plasma (KWin), wlroots
  compositors and others, but GNOME Mutter does not implement it. X11 has
  `_NET_WM_STATE_ABOVE` and the Shape extension.
- **The tray, global shortcuts and autostart** use freedesktop protocols
  (StatusNotifierItem, the `GlobalShortcuts` portal, XDG autostart), and
  desktops support them unevenly.

## Decision

### Scope

- One target: `x86_64-unknown-linux-gnu`. No aarch64 Linux, musl, or
  32-bit builds.
- Reference environment: KDE Plasma 6, on both Wayland and X11 sessions.
  GNOME Wayland is evaluated separately. It may end up as partial support, with
  a non-topmost model window and no in-app global shortcuts, provided the
  limits are reported through capability reporting.
- Product behavior, config v1 semantics and the input contract do not change.
  Linux adds platform adapters only. Linux uses the same Development and
  Production environment separation and storage roots, through the XDG base
  directories.

### Platform boundary

- Linux code lives only in the crates that already own a per-platform branch:
  the adapters (`bongocat-platform`, `bongocat-overlay`, `bongocat-live2d`
  bindings, `bongocat-update` target selection, `bongocat-packaging`) and the
  product crates that select between them (`bongocat-app`, `bongocat-ui`,
  `bongocat-config` storage roots, `bongocat-i18n` platform copy). It uses
  direct `#[cfg(target_os = "linux")]` items, the same shape as the existing
  `windows`/`macos` ones. The platform-neutral contract crates (input, render,
  runtime, model, storage, UI protocol and the rest) never see a Linux cfg.
- A capability that is not implemented yet returns a typed `Unsupported` error
  or capability status, which settings and diagnostics display. Linux never
  silently does nothing in a way that looks like success, and no `any()` cfg
  unions are added to force a Linux build.
- `tools/tests/test_supported_platform_cfg.py` changes from "no Linux cfg" to
  "Linux cfg only in the adapter paths above". It still forbids
  macOS/Windows `any()` unions everywhere.

### Rendering

- The renderer is `wgpu` (decided in Stage L3, see the L3 record below), on
  Vulkan with GLES as fallback. It consumes the existing immutable
  `RenderSnapshot` and adds no Linux-only render semantics.
- The overlay owns its window, GPU context and frame source with the same
  ownership and shutdown order as AGENTS.md §4.3. It is not embedded in GPUI.

### Input

- Keyboard and mouse use one evdev backend in listen-only mode, never
  `EVIOCGRAB`, on both Wayland and X11.
  - Devices are discovered at start and on hot-plug. Adding or removing a
    device emits a reset or reconcile event, never a silent edge.
  - The pressed set is reconciled with the `EVIOCGKEY` ioctl, the Linux
    equivalent of `GetAsyncKeyState`/`CGEventSourceKeyState`.
  - Sleep, session lock, VT switch and session change are observed through
    systemd-logind on D-Bus, and each one emits a reset.
- Missing `input` group membership is an explicit input permission state,
  equivalent to macOS TCC. The app shows the user the exact command to grant
  it, and never escalates privileges itself.
- Cursor following:
  - On X11, the absolute pointer position comes from XInput2 or
    `XQueryPointer`.
  - On Wayland, the position is accumulated from relative evdev motion with
    the existing `force_move` mechanism (ADR-0081), and diagnostics report
    the limit. The app never polls or guesses the absolute position.
- Gamepads use the `gilrs` evdev backend from the pinned `ayangweb/gilrs`
  fork, under the same §6.1 rules (idle CPU, reset ack and shutdown join).
- The issue #47 invariants apply unchanged: reliable ordered edge transport,
  zero lost edges in stress tests, and every pressed key cleared by release,
  reconcile or reset.

### System integration

| Capability | Linux route | Notes |
|---|---|---|
| Tray and menus | StatusNotifierItem | `tray-icon`/`muda` Linux backends need GTK; a D-Bus SNI crate is the alternative. Chosen in L6 |
| Global shortcuts | `GlobalShortcuts` portal on Wayland, X11 grab on X11 | `global-hotkey` covers X11 only |
| File dialogs | `rfd` with `xdg-portal` | Selections are still revalidated in Rust |
| Clipboard | `arboard` with Wayland data-control | |
| Startup item | XDG autostart `.desktop` via `auto-launch` | |
| Single instance | Unix socket in `$XDG_RUNTIME_DIR`, one per environment | |
| Theme | `org.freedesktop.appearance.color-scheme` portal setting | |
| Opening links and folders | `opener` / `xdg-open` | |

### Packaging and updates

- First artifact: an AppImage built by `bongocat-packaging` through
  `cargo-packager`, plus `x86_64-unknown-linux-gnu` in `UpdateTargetTriple`,
  `deny.toml`, the packaging contract test and the manifest fragment
  `linux-x86_64.json`. Updates stay HTTPS-only and signed (ADR-0033/0034).
- Flatpak is out of scope. Its sandbox needs `--device=all` for evdev input,
  which defeats the point of the sandbox. `.deb`/`.rpm` can follow later.
- The Linux Cubism Core binaries must come from the same pinned SDK baseline
  (`5-r.5`), with their version, source, hash and license recorded in the
  vendor provenance.

### Stages

Each stage is a minimal closed loop, built on `docs/linux-support-plan` or a
follow-up `feat/linux-*` branch, with its own evidence recorded in
`docs/benchmark/`. A failed spike is recorded here before any later stage
changes.

| Stage | Deliverable | Exit gate |
|---|---|---|
| L0 | This ADR, the TODO phase and the Technical Design update | Documents agree |
| L1 | Workspace builds and lints on Linux; capabilities without a Linux implementation return a typed unsupported state; Linux CI job | Linux clippy (`-D warnings`) and the tests of every crate that does not link the Cubism Core pass in CI; Windows/macOS `just check` unchanged |
| L2 | Linux Cubism Core vendored with provenance; `bongocat-live2d` links it | Live2D tests pass for all three preset models on Linux |
| L3 | Renderer backend spike and decision | The three preset models render off-screen and pass the shared frame smoke check; independent Linux drivers agree on the same frame |
| L4 | Overlay on X11 and KDE Wayland | Transparency, topmost, click-through, drag/resize and fractional scaling verified on hardware; repeated open/close and clean shutdown |
| L5 | evdev input, permission state, logind resets, gamepads | Zero lost edges in stress tests; hot-plug, lock, sleep and VT-switch reset matrix; `gilrs` idle-CPU retest |
| L6 | Tray, menus, shortcuts, dialogs, clipboard, autostart, single instance, theme | Settings UI and system-menu smoke tests on both KDE sessions; 800×600 and 100/125/150/200% layout |
| L7 | AppImage, signing, update target | Install, update, rollback and uninstall verified on hardware |
| L8 | Support decision | 8-hour soak with no resource growth; capability matrix published; a follow-up ADR states what is supported |

### L1 implementation record (2026-10-22)

- **Link boundary.** `cargo test` cannot link `bongocat-live2d` and its
  dependents (`runtime`, `overlay`, `ui`, `app`) until L2 vendors the Linux
  Core, so the L1 CI job excludes them from the test run. L2 removes the
  exclusions.
- **Raw bindings.** `x86_64-unknown-linux-gnu` was added to
  `tools/cubism-bindgen`; the real r.5 bindings were generated twice and are
  byte-identical to the reviewed macOS bindings (`docs/cubism/binding-generation.md`).
- **Real on Linux already** (not yet verified on hardware): clipboard
  (`arboard`, X11), model folder and cover pickers (`rfd` `xdg-portal`, scoped
  to the Linux target), XDG storage root, XDG autostart (`auto-launch`), the OS
  version (`os-release` and kernel release), the startup input check (an
  openable `/dev/input/event*` node) and its prompt copy in every locale.
- **Typed unsupported:** the overlay (`overlay_unsupported_on_linux`, so the
  product quits through its existing startup-failure path), native window
  show/hide/close, display queries (`None`), the overlay context menu and
  updates (`UpdateUnavailability::UnsupportedPlatform`, with locale copy).
  `global-hotkey` reaches X11 clients only.
- **Tray.** `tray-icon`'s `ksni` backend (pure-Rust StatusNotifierItem over
  D-Bus, no GTK) is enabled for the Linux target only. Its `muda/snapshot`
  feature swaps muda's internal `Rc<RefCell>` state for `Arc<Mutex>`, so it
  must never be unified into the Windows or macOS build.
- **Dependencies.** The 134 Linux-only packages in the graph are all covered by
  the `deny.toml` license allow list. `deny.toml` keeps listing only shipped
  targets (the packaging contract requires it) until L7.
- **Temporary lint exemptions.** `bongocat-overlay`, the `bongocat-app`
  binary, `system_menu_native` and one `DisplayBounds` method carry
  `cfg_attr(target_os = "linux", allow(dead_code, …))`. They cover shared
  code whose only callers are the macOS and Windows overlay sessions, and are
  removed when the L4 Linux overlay session lands.
- **Renderer input for L3.** GPUI already brings `wgpu` (Vulkan and GLES) into
  the Linux graph, so under ADR-0030 the L3 spike evaluates `wgpu` first,
  before a separate EGL/OpenGL ES stack.
- **Flatpak note.** Inside a Flatpak sandbox `os-release` describes the
  Flatpak runtime rather than the host distribution.

### L2 implementation record (2026-10-22)

- The maintainer accepted the Live2D agreements on the official download page
  and downloaded `CubismSdkForNative-5-r.5.zip`; its SHA-256 and the
  offline inspector both match the pinned baseline.
- Only `Core/lib/linux/x86_64/libLive2DCubismCore.a` was vendored, byte for
  byte, and is linked statically like the macOS Core. Hashes are in
  `docs/cubism/cubism-sdk-source-and-license.md`.
- `cargo test --locked --workspace` passes on Linux with nothing excluded
  (1156 tests; the three presets load through the Core). The Linux CI job
  runs the full suite.
- Running the Development product on KDE Wayland loads the Core (6.0.1),
  reports `overlay_unsupported_on_linux` and exits with code 1 through the
  existing startup-failure path, as L1 intended.

### L3 implementation record (2026-10-22)

- **Decision: `wgpu`.** GPUI already links `wgpu` on Linux, so under ADR-0030
  it is the first candidate, and it covers Vulkan and GLES behind one safe API.
  A separate EGL/OpenGL ES stack or raw Vulkan would add a second GPU stack and
  `unsafe` handle management for no gain. The Linux adapter has no `unsafe`.
- **Corrected gate.** The original L3 gate asked for a match against "D3D11/Metal
  references within the existing tolerance". No reference images exist: every
  backend is held to the shared 17x17 `validate_frame_smoke` check, and the
  Technical Design does not require pixel identity across platforms. The gate
  is now the frame smoke check on all three presets plus agreement between
  independent Linux drivers.
- **Evidence** (`docs/benchmark/linux-renderer-spike-2026-10-22.md`): all three
  presets render through the real cover capture; AMD RADV, NVIDIA and llvmpipe
  Vulkan agree on more than 99.93% of pixels, differing only at triangle edges.
  GLES was not measurable inside the Flatpak sandbox.
- **Version.** `wgpu` is pinned to `=29.0.4`, the version `gpui-kit 0.7.3`
  (through `gpui-pre-wgpu`) resolves, although 30.0.1 is the latest release.
  A direct 30.x dependency would link a second `wgpu`, `wgpu-core`, `wgpu-hal`
  and `naga` into the same process. Blocker owner: `longbridge/gpui-kit`.
  Lifted when the pinned `gpui-kit` moves to `wgpu` 30, at which point both
  move together. Review: MIT OR Apache-2.0, maintained by gfx-rs, no new crate
  in `Cargo.lock`, replacement boundary is `bongocat-overlay/src/linux/`.
- **Product effect.** The Linux model cover capture now renders for real. The
  model window, and with it everything the user sees on screen, is still L4.
- **CI.** The Linux job installs `mesa-vulkan-drivers` so the GPU test runs on
  lavapipe; a missing adapter fails the test instead of skipping it.

### L4 progress record (2026-10-22, in progress)

- **Wayland window.** The model window is a `wlr-layer-shell` surface on the
  adapter's own Wayland connection (not GPUI's). It is placed with margins from
  the output's top-left, so `OverlayWindowBounds::x`/`y` are output-relative on
  Wayland; the output size is learned from a configure of a fully anchored,
  zero-sized surface. Always-on-top maps to the `overlay` layer, otherwise
  `bottom`: a layer surface cannot join the normal window stack in between.
- **Presentation.** `wgpu` presents with premultiplied (or inherited) alpha and
  Mailbox/Immediate rather than FIFO, so the self-paced frame loop never waits
  on a frame callback of an unshown surface. Fractional scale uses
  `wp_fractional_scale_v1` with `wp_viewporter`; window opacity uses
  `wp_alpha_modifier_v1`, applied once to the whole surface. Click-through is
  an empty input region. Hiding destroys the layer surface; showing reopens it
  at the same box on the same device.
- **Session.** The Linux `ProductOverlaySession` follows the macOS protocol:
  the first frame gates the initial commit, a model that cannot be prepared is
  rejected while the current one keeps drawing, and shutdown releases the
  window only after input and the runtime have stopped. Platform input starts
  as `BackendUnavailable` until L5. The app's frame loop runs the shared
  macOS/Linux branch.
- **Evidence.** On KDE Plasma Wayland (AMD Radeon 740M, Vulkan) the ignored
  `product_window_smoke` test presents 301 frames in 5 s at 350x203, and the
  Development product runs and exits cleanly with `--run-seconds`. New direct
  dependencies (`wayland-client`, `wayland-backend` with `client_system`,
  `wayland-protocols`, `wayland-protocols-wlr`) were already in the graph
  through GPUI at their latest releases, MIT; no new crate enters the lockfile.
- **User confirmation.** The maintainer ran the Development product on KDE
  Plasma Wayland and confirmed the cat appears transparent and on top.
- **Pointer.** The window binds the seat's `wl_pointer` (and
  `wp_cursor_shape_v1` for a defined cursor). A left-button drag moves the
  window, measured from the last committed origin because surface coordinates
  are relative to the window the compositor already has; a right-button drag
  resizes it through the shared `ResizeDrag` (same threshold and mapping as the
  other platforms) and writes the scale back through the resize sink; the
  shared placement constraint keeps the window on its output. The drag rules
  live in a Wayland-free `PointerInteraction` with unit tests.
- **Not available, by design or for now.** Hover hide cannot exist on Wayland:
  a client only sees the pointer over a surface that accepts input, and a
  hidden pet passes input through, so it could never see the pointer leave. A
  right-click without a drag shows no menu, because `muda` has no GTK-free
  context menu; a Linux menu presentation is L6.
- **Hardware check (2026-10-23).** The maintainer confirmed on KDE Plasma
  Wayland that left-button drag follows the pointer, right-button drag grows
  and shrinks the window with its top-left corner fixed, and the position and
  size are restored on the next start.
- **X11 backend.** A borderless 32-bit ARGB window on the adapter's own XCB
  connection: `_MOTIF_WM_HINTS` without decorations, sticky, skipping taskbar
  and pager, `_NET_WM_STATE_ABOVE` (or `BELOW`) for always-on-top,
  `_NET_WM_WINDOW_OPACITY` for opacity and an empty input shape for
  click-through. Bounds are root coordinates and placement uses the RandR
  monitors. Pointer events arrive in root coordinates and are converted to
  window coordinates against the applied origin, so the shared drag rules hold.
  The window is mapped after its first frame so it never appears empty.
- **Backend choice.** Wayland with `wlr-layer-shell` first; a Wayland
  compositor without it (GNOME) and an X11 session use the X11 backend, through
  XWayland in the first case. `BONGOCAT_LINUX_WINDOW=wayland|x11` forces one
  for testing.
- **X11 evidence.** `product_window_smoke` forced to X11 and run on the host
  against XWayland (`DISPLAY=:0`) on KDE Plasma presents 301 frames in 5 s,
  centred on the primary RandR monitor. `x11rb` is pinned to `=0.13.2`, the
  version GPUI and `arboard` resolve, although 0.14.0 is the latest release:
  a direct 0.14 dependency would duplicate the X11 protocol stack. Blocker
  owner: `longbridge/gpui-kit` (through `gpui-pre-linux`) and `arboard`; lifted
  when both move to 0.14. MIT OR Apache-2.0; no new crate in `Cargo.lock`.
- **X11 hardware check (2026-10-23).** The maintainer ran the Development
  product forced to X11 against XWayland on KDE Plasma and confirmed
  transparency, staying on top, left-button drag and right-button resize.
- **Remaining for L4:** a real X11 session and GNOME (not available on the
  maintainer's Fedora 44 KDE machine), multiple outputs on Wayland,
  hover hide on X11 (it needs the L5 input service to be running), and
  removing the overlay crate's Linux `dead_code` exemption.

### L5 progress record (2026-10-23, in progress)

- **Service.** `LinuxInputService` (in `bongocat-platform`) observes every
  keyboard and mouse node under `/dev/input` from one worker thread, read-only
  and never grabbed. Startup reports `PermissionDenied` when nodes exist but
  none can be opened, `BackendUnavailable` when there is no keyboard or mouse,
  and follows the macOS/Windows start, stop and join contract.
- **Protocol.** Key and button edges go through the reliable runtime queue; a
  full queue is counted and recovered with a `QueueOverflow` reset. Every 250 ms,
  and at once after a `SYN_DROPPED`, the pressed set is re-read with
  `EVIOCGKEY` and published as a reconciliation, the same cadence as Windows.
  A removed device resets the input state (`DeviceRemoved`). New devices are
  picked up every second. evdev codes map to USB HID usages through the
  inverse of the kernel's HID table, so a key is the same `PhysicalKey` as on
  the other platforms.
- **Cursor.** There is no absolute pointer on Wayland, so the followed position
  is accumulated from relative motion inside the window's screen, seeded at its
  centre (ADR-0081). `force_move` therefore changes nothing on Linux.
- **Session.** The overlay starts the service and stops it in the shutdown
  order; idle hide works on both window backends. Hover hide stays unavailable
  on Wayland.
- **Privacy.** Keys leave the worker only as edges to the runtime; diagnostics
  are counts. The only `unsafe` is two read-only ioctls and `poll`, each with
  its invariant.
- **Evidence so far.** Decoder, key map, ioctl encoding and startup outcomes
  are unit-tested; without `input` group membership the product reports
  `PermissionDenied` and keeps running.
- **Hardware check (2026-10-23).** With the maintainer's account in the `input`
  group (Bazzite keeps the group in `/usr/lib/group`, so it was copied to
  `/etc/group` before `usermod`), the session smoke test started the service on
  the real devices, reconciled 16 times in 5 s and stopped cleanly, and the
  maintainer confirmed the paws react to typing and clicks and the cat follows
  the mouse. A session that predates the membership needs a fresh login (or
  `sg input`).
- **logind resets.** A watcher thread subscribes to `Manager.PrepareForSleep`
  and to the caller's session `Lock`, `Unlock` and `Active` changes, and each
  becomes a reset on the input worker (`Sleep` or `SessionLock`). The session
  is found through `session/auto` (a PID lookup fails inside a Flatpak), but
  signals come from the session's own path, so the watcher reads `Id` through
  the alias and resolves the real path with `Manager.GetSession`. The `Active`
  stream's initial value is not a change and is ignored. Without logind the
  watcher ends quietly and reconciliation remains the safety net. `zbus` is
  pinned to `=5.19.0`, the version GPUI, `ashpd` and `ksni` resolve, although
  6.0.0 is the latest; a direct 6.x dependency would duplicate the D-Bus stack.
  Blocker owners: `longbridge/gpui-kit` (through `accesskit_unix`), `ashpd`,
  `ksni`. MIT; no new crate in `Cargo.lock`.
- **Touchpads.** A device with a finger tool and `ABS_X`/`ABS_Y` is opened as
  a touchpad: while exactly one finger is down, the change between consecutive
  positions becomes motion, scaled to 8 px per mm from the axis resolution
  (`EVIOCGABS`). Multi-finger gestures move nothing, and touch and tool codes
  are touch state rather than unmapped keys. On the maintainer's laptop the
  service opens the keyboard, the ELAN touchpad's mouse interface (its click
  buttons) and the touchpad itself, and skips the power, lid, hotkey and audio
  nodes.
- **Remaining for L5:** touchpad hardware check (absolute motion),
  gamepads through `gilrs` with its idle-CPU retest, the stress test for zero
  lost edges, and the issue #47 lifecycle matrix (lock, sleep, VT switch,
  hot-plug).

## Consequences

- Linux has a clear scope and order. A compile pass, or one environment
  working, is never reported as Linux support (AGENTS.md §9.3).
- `input` group access lets the app read every key on the system. The
  existing logging rules (no key sequences or user content) are mandatory, and
  the permission prompt must say so plainly.
- Wayland users get reduced cursor following, and GNOME users may get a
  non-topmost model window. These limits are documented, not hidden.
- Each new dependency gets the §6 review when its stage starts. None is added
  by this ADR.
- The adapter-path cfg rule replaces the blanket Linux ban; ADR-0006 stays as
  history for the original launch decision.

## Verification

This document records the decision only. Each stage's gate above is the
verification for that stage, tracked in `docs/TODO.md` under Phase L.
