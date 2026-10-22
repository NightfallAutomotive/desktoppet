# Linux Renderer Spike

Date: 2026-10-22
Environment: Bazzite 44 (Fedora-based), KDE Plasma Wayland session, x86_64,
AMD Radeon 740M (RADV) integrated GPU and NVIDIA GeForce RTX 3050 6GB Laptop
GPU, run inside the VSCodium Flatpak sandbox.
Scope: ADR-0090 stage L3; Linux only, off-screen rendering only (no window).
Build: branch `feat/linux-renderer-spike` on top of `c030f7c0`, `cargo test`
(debug test profile), Rust 1.97.1, `wgpu` 29.0.4. Models: the three bundled
presets, first published frame, 700x405 (cover capture scale 200%).

## What Was Built

An off-screen `wgpu` renderer in `bongocat-overlay`'s Linux adapter, consuming
the same `RenderFrame` as the D3D11 and Metal backends:

- WGSL shader ported line for line from the Metal shader: multiply and screen
  colours, mask coverage with inversion, corner coverage, premultiplied output.
- ADR-0063 encoded-space contract: UNORM model textures, UNORM BGRA target, no
  sRGB decode or re-encode.
- The shared blend table and cull rule, one pipeline per blend and cull pair
  (`wgpu` bakes the cull mode into the pipeline).
- Mask targets per masked drawable, cleared and filled before the main pass;
  background, drawables in render order, key images on top.
- The Metal backend's immutability rules for a model generation (topology,
  indices, vertex counts) and the shared 17x17 frame smoke check.
- `wgpu` validation and out-of-memory errors captured by error scopes and an
  uncaptured-error handler, so a GPU failure is an `OverlayError` rather than a
  panic. The adapter is named in a failed frame check.
- The Linux model cover capture now runs this renderer for real, with the same
  activate, prepare, commit and first-frame verification sequence as Windows.

No `unsafe` code: `wgpu` owns every Vulkan and GLES handle.

## Results

`cargo test --locked -p bongocat-overlay --lib linux::`: all five tests pass.
`every_preset_renders_and_encodes_a_cover` renders `standard`, `keyboard` and
`gamepad` through the real cover capture on the product adapter request (Vulkan
or GLES, low-power preference), passes the first-frame smoke check, keeps
drawing while the model animates and encodes a cover PNG.

`backend_evidence` (ignored harness, `BONGOCAT_RENDER_EVIDENCE=<dir> cargo test
-p bongocat-overlay --lib backend_evidence -- --ignored --nocapture`) renders the
first frame of each preset at 700x405 on every adapter available and compares
straight-alpha RGBA with the hardware Vulkan frame:

| Preset | Adapter | Max channel delta | Mean delta | Pixels over 8 |
| --- | --- | --- | --- | --- |
| standard | RADV (reference) | — | — | — |
| standard | NVIDIA Vulkan | 255 | 0.0275 | 67 / 283500 |
| standard | llvmpipe (software Vulkan) | 255 | 0.1093 | 193 / 283500 |
| keyboard | NVIDIA Vulkan | 255 | 0.0255 | 62 / 283500 |
| keyboard | llvmpipe | 255 | 0.0966 | 190 / 283500 |
| gamepad | NVIDIA Vulkan | 255 | 0.0211 | 66 / 283500 |
| gamepad | llvmpipe | 255 | 0.0751 | 188 / 283500 |

Three independent Vulkan drivers agree on more than 99.93% of pixels. The
differing pixels are isolated triangle-edge pixels that one rasterizer covers
and another does not, which is why the maximum is a full 255 while the mean
stays near zero. Visual inspection of every image shows the correct models:
the paws over the desk line, the arms clipped behind it, and the key images of
the keyboard and gamepad presets.

GLES could not be measured here: inside the Flatpak sandbox, `wgpu`'s GLES
device creation failed with "Parent device is lost". GLES remains a fallback
in the product request but is unverified.

## What This Does Not Show

- There are no D3D11 or Metal reference images in the repository, so this is
  not a cross-platform pixel comparison. The Technical Design does not require
  pixel identity across platforms; the cross-driver agreement and the shared
  frame smoke check are the evidence.
- Nothing was presented on screen. The model window is stage L4.
- No frame-timing or idle-CPU measurement was taken; the off-screen path reads
  every frame back, which the windowed path will not.
