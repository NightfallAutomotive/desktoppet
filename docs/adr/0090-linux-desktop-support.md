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

- The renderer backend is chosen in Stage L3, by a spike measured against the
  same three preset models and readback checks that D3D11 and Metal pass. The
  leading candidate is OpenGL ES 3.0 over EGL, the API used by the official
  Cubism Native OpenGL renderer and available on both Wayland and X11; Vulkan
  and `wgpu` are the alternatives. Whichever backend wins consumes the
  existing immutable `RenderSnapshot` and adds no Linux-only render semantics.
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
| L3 | Renderer backend spike and decision | Offscreen readback of the three preset models matches the D3D11/Metal references within the existing tolerance |
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
