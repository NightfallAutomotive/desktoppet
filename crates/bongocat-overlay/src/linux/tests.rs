//! The Linux renderer: its encodings, its draw order and real GPU frames.
//!
//! The GPU tests render the three shipped presets through the same cover
//! capture the product uses. They need a Vulkan or GLES adapter; CI provides
//! Mesa's software Vulkan driver (lavapipe), and a missing adapter fails the
//! test rather than skipping it, because a skipped GPU test would look green.

use super::*;
use bongocat_model::{ModelId, ModelPackageLimits, PresetModelCatalog};
use bongocat_render::CanvasInfo;
use std::path::PathBuf;

#[test]
fn model_transform_preserves_aspect_ratio() {
    let canvas = CanvasInfo {
        width: 2048.0,
        height: 2048.0,
        origin_x: 1024.0,
        origin_y: 1024.0,
        pixels_per_unit: 1024.0,
    };
    assert_eq!(
        model_transform(ModelBounds::from_canvas(canvas), 800.0, 800.0, false),
        [1.0, 1.0, -0.0, -0.0]
    );
    assert_eq!(
        model_transform(ModelBounds::from_canvas(canvas), 1600.0, 800.0, false),
        [0.5, 1.0, -0.0, -0.0]
    );
    assert_eq!(
        model_transform(ModelBounds::from_canvas(canvas), 800.0, 800.0, true),
        [-1.0, 1.0, 0.0, -0.0]
    );
}

#[test]
fn uniforms_encode_six_vec4_in_shader_order() {
    let uniforms = Uniforms {
        scale_offset: [1.0, 2.0, 3.0, 4.0],
        multiply_color: [5.0, 6.0, 7.0, 8.0],
        screen_color: [9.0, 10.0, 11.0, 12.0],
        mask_settings: [13.0, 14.0, 15.0, 16.0],
        corner_radius: [17.0, 18.0, 19.0, 20.0],
        opacity: 21.0,
    };
    let bytes = uniforms.to_bytes();
    let floats = bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("four bytes")))
        .collect::<Vec<_>>();
    let mut expected = (1..=21).map(|value| value as f32).collect::<Vec<_>>();
    expected.extend([0.0, 0.0, 0.0]);
    assert_eq!(floats, expected);
}

#[test]
fn vertices_encode_position_then_uv() {
    let bytes = vertex_bytes(&[bongocat_render::Vertex {
        position: [1.5, -2.0],
        uv: [0.25, 0.75],
    }]);
    assert_eq!(bytes.len() as u64, VERTEX_STRIDE);
    let floats = bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("four bytes")))
        .collect::<Vec<_>>();
    assert_eq!(floats, [1.5, -2.0, 0.25, 0.75]);
}

/// Same table as the other backends: the mapping is the contract, not the API.
#[test]
fn blend_and_cull_tables_map_to_wgpu() {
    assert_eq!(wgpu_blend_factor(BlendFactor::One), wgpu::BlendFactor::One);
    assert_eq!(
        wgpu_blend_factor(BlendFactor::OneMinusSourceAlpha),
        wgpu::BlendFactor::OneMinusSrcAlpha
    );
    assert_eq!(
        wgpu_blend_factor(BlendFactor::DestinationColor),
        wgpu::BlendFactor::Dst
    );
    assert_eq!(wgpu_cull_mode(DrawableCullMode::None), None);
    assert_eq!(
        wgpu_cull_mode(DrawableCullMode::Back),
        Some(wgpu::Face::Back)
    );
}

fn preset(id: &str) -> Arc<CommittedModel> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../resources/models");
    Arc::new(
        PresetModelCatalog::open(root, ModelPackageLimits::default())
            .expect("preset catalog")
            .load(&ModelId::parse(id).expect("model id"))
            .expect("preset model"),
    )
}

/// Every shipped preset renders through the real cover capture: the first
/// frame passes the shared frame smoke check, the capture keeps drawing while
/// the model animates, and the result encodes as a cover.
#[test]
fn every_preset_renders_and_encodes_a_cover() {
    for id in ["standard", "keyboard", "gamepad"] {
        let mut session = CoverCaptureSession::start(preset(id))
            .unwrap_or_else(|error| panic!("{id}: cover capture did not start: {error}"));
        eprintln!("{id}: rendered on {:?}", session.renderer.adapter());
        while session
            .step()
            .unwrap_or_else(|error| panic!("{id}: capture step failed: {error}"))
        {}
        assert!(session.frames_drawn > 1, "{id}: only the first frame drew");
        let frame = session
            .finish()
            .unwrap_or_else(|error| panic!("{id}: capture did not finish: {error}"));
        let cover = crate::cover::encode_cover(frame)
            .unwrap_or_else(|error| panic!("{id}: cover did not encode: {error}"));
        assert!(!cover.png().is_empty(), "{id}: empty cover");
    }
}

/// The first published frame of a preset, with the runtime that produced it.
/// The pending commit is rejected again by [`release_first_frame`].
fn first_frame(id: &str) -> (RuntimeOwner, RenderConsumer, RenderFrame) {
    let (runtime, consumer) = RuntimeOwner::start_with_rendering(true, 64);
    let client = runtime.client();
    client
        .wait_for_revision(1, RUNTIME_TIMEOUT)
        .expect("runtime ready");
    let activation = client
        .send(RuntimeCommand::ActivateModel(preset(id)))
        .expect("activate");
    client
        .wait_for_model_preparation(activation, RUNTIME_TIMEOUT)
        .expect("prepared");
    let frame = consumer.take_latest().expect("first frame");
    (runtime, consumer, frame)
}

fn release_first_frame(runtime: RuntimeOwner, consumer: &RenderConsumer, frame: &RenderFrame) {
    let token = frame.model_commit.expect("commit token");
    reject_model_commit(&runtime.client(), consumer, token).expect("reject commit");
    runtime.shutdown(RUNTIME_TIMEOUT).expect("runtime shutdown");
}

/// ADR-0090 L3 evidence: the same frame on every adapter this machine offers.
///
/// Run with
/// `BONGOCAT_RENDER_EVIDENCE=<dir> cargo test -p bongocat-overlay --lib backend_evidence -- --ignored --nocapture`.
/// Each adapter's frame is written as `<preset>-<adapter>.png`, and every frame
/// is compared channel by channel with the hardware Vulkan frame. An adapter
/// the machine does not have is reported and skipped.
#[test]
#[ignore = "evidence harness; writes images and needs every adapter it names"]
fn backend_evidence() {
    let output = std::env::var_os("BONGOCAT_RENDER_EVIDENCE").map(PathBuf::from);
    let adapters = [
        (
            "vulkan-low-power",
            AdapterRequest::PRODUCT.backends & wgpu::Backends::VULKAN,
            wgpu::PowerPreference::LowPower,
            false,
        ),
        (
            "vulkan-high-performance",
            wgpu::Backends::VULKAN,
            wgpu::PowerPreference::HighPerformance,
            false,
        ),
        (
            "gles",
            wgpu::Backends::GL,
            wgpu::PowerPreference::LowPower,
            false,
        ),
        (
            "vulkan-software",
            wgpu::Backends::VULKAN,
            wgpu::PowerPreference::LowPower,
            true,
        ),
    ];
    for id in ["standard", "keyboard", "gamepad"] {
        let (runtime, consumer, frame) = first_frame(id);
        let (width, height) =
            model_window_dimensions(frame.snapshot.canvas, COVER_CAPTURE_SCALE_PERCENT);
        let mut reference: Option<Vec<u8>> = None;
        for (name, backends, power_preference, force_fallback) in adapters {
            let request = AdapterRequest {
                backends,
                power_preference,
                force_fallback,
            };
            let captured = OffscreenRenderer::create_on(request, &frame, width, height, 0)
                .and_then(|mut renderer| {
                    let adapter = renderer.adapter().clone();
                    renderer.draw_capturing(true).map(|frame| (adapter, frame))
                });
            let (adapter, captured) = match captured {
                Ok(result) => result,
                Err(error) => {
                    eprintln!("{id} {name}: unavailable: {error}");
                    continue;
                }
            };
            let pixels = captured.pixels().to_vec();
            if let Some(directory) = &output {
                image::save_buffer(
                    directory.join(format!("{id}-{name}.png")),
                    &pixels,
                    width,
                    height,
                    image::ColorType::Rgba8,
                )
                .expect("write evidence image");
            }
            let comparison = reference.as_deref().map(|reference| {
                let mut maximum = 0_u8;
                let mut total = 0_u64;
                let mut differing = 0_usize;
                for (left, right) in reference.chunks_exact(4).zip(pixels.chunks_exact(4)) {
                    let delta = left
                        .iter()
                        .zip(right)
                        .map(|(left, right)| left.abs_diff(*right))
                        .max()
                        .unwrap_or(0);
                    maximum = maximum.max(delta);
                    total += u64::from(delta);
                    differing += usize::from(delta > 8);
                }
                let count = pixels.len() / 4;
                format!(
                    "max channel delta {maximum}, mean {:.4}, pixels over 8: {differing}/{count}",
                    total as f64 / count as f64
                )
            });
            eprintln!(
                "{id} {name}: {width}x{height} on {} {} ({}); {}",
                adapter.backend,
                adapter.name,
                adapter.driver,
                comparison.as_deref().unwrap_or("reference")
            );
            reference.get_or_insert(pixels);
        }
        release_first_frame(runtime, &consumer, &frame);
    }
}

/// The real model window on the running Wayland session, for a few seconds.
///
/// Run with `cargo test -p bongocat-overlay --lib product_window_smoke -- --ignored --nocapture`
/// on a compositor with `wlr-layer-shell` (KDE Plasma, wlroots). The cat is
/// shown on screen, so this never runs in CI.
#[test]
#[ignore = "shows a window on the running Wayland session"]
fn product_window_smoke() {
    let (runtime, consumer) = RuntimeOwner::start_with_rendering(true, 64);
    let client = runtime.client();
    client
        .wait_for_revision(1, RUNTIME_TIMEOUT)
        .expect("runtime ready");
    let activation = client
        .send(RuntimeCommand::ActivateModel(preset("standard")))
        .expect("activate");
    client
        .wait_for_model_preparation(activation, RUNTIME_TIMEOUT)
        .expect("prepared");
    eprintln!("model prepared; starting the session");
    let mut session = ProductOverlaySession::start(
        client.clone(),
        client.input_producer(),
        client.cursor_producer(),
        client.gamepad_axis_producer(),
        consumer,
        OverlaySessionOptions::default(),
        OverlayInteractionSinks {
            context_menu_sender: None,
            resize_sender: None,
        },
    )
    .expect("session start");
    eprintln!("session started at {:?}; running", session.window_bounds());
    session.run_for(Duration::from_secs(5)).expect("run");
    eprintln!("ran; visible = {}", session.is_visible());
    session.stop_input().expect("stop input");
    runtime.shutdown(RUNTIME_TIMEOUT).expect("runtime shutdown");
    let report = session.finish_after_runtime_shutdown().expect("finish");
    eprintln!("report: {report:?}");
    assert!(report.frames_presented > 1, "the window presented frames");
}

mod pointer_interaction {
    use super::*;

    const BOUNDS: OverlayWindowBounds = OverlayWindowBounds {
        x: 100,
        y: 50,
        width: 350,
        height: 203,
    };

    fn base() -> Option<ResizeBase> {
        ResizeBase::new(350.0, 203.0)
    }

    fn feed(
        interaction: &mut PointerInteraction,
        bounds: &mut OverlayWindowBounds,
        committed: (i32, i32),
        events: &[PointerEvent],
    ) -> Vec<PointerOutcome> {
        events
            .iter()
            .map(|event| {
                let outcome = interaction.handle(*event, *bounds, committed, base());
                if let Some(next) = outcome.bounds {
                    *bounds = next;
                }
                outcome
            })
            .collect()
    }

    #[test]
    fn a_left_drag_moves_the_window_from_the_committed_origin() {
        let mut interaction = PointerInteraction::default();
        let mut bounds = BOUNDS;
        feed(
            &mut interaction,
            &mut bounds,
            (100, 50),
            &[
                PointerEvent::Enter { x: 20.0, y: 30.0 },
                PointerEvent::Button {
                    button: BUTTON_LEFT,
                    pressed: true,
                },
                // Two motions before the compositor has the moved window: both
                // are relative to the committed one, so the second replaces the
                // first instead of adding to it.
                PointerEvent::Motion { x: 30.0, y: 35.0 },
                PointerEvent::Motion { x: 45.0, y: 40.0 },
            ],
        );
        assert_eq!((bounds.x, bounds.y), (125, 60));
        assert!(interaction.moving());
        feed(
            &mut interaction,
            &mut bounds,
            (125, 60),
            &[
                PointerEvent::Button {
                    button: BUTTON_LEFT,
                    pressed: false,
                },
                PointerEvent::Motion { x: 0.0, y: 0.0 },
            ],
        );
        assert_eq!((bounds.x, bounds.y), (125, 60));
        assert!(!interaction.moving());
        assert_eq!((bounds.width, bounds.height), (350, 203));
    }

    #[test]
    fn a_right_drag_resizes_and_reports_the_scale_once_it_ends() {
        let mut interaction = PointerInteraction::default();
        let mut bounds = BOUNDS;
        let outcomes = feed(
            &mut interaction,
            &mut bounds,
            (100, 50),
            &[
                PointerEvent::Enter { x: 10.0, y: 10.0 },
                PointerEvent::Button {
                    button: BUTTON_RIGHT,
                    pressed: true,
                },
                // +40 px on both axes is +40% at 0.5% per pixel per axis.
                PointerEvent::Motion { x: 50.0, y: 50.0 },
                PointerEvent::Button {
                    button: BUTTON_RIGHT,
                    pressed: false,
                },
            ],
        );
        assert_eq!((bounds.x, bounds.y), (100, 50), "the origin stays put");
        assert!(bounds.width > 350 && bounds.height > 203);
        assert_eq!(outcomes.last().and_then(|o| o.resized_to), Some(140));
    }

    #[test]
    fn a_right_click_without_a_drag_changes_nothing() {
        let mut interaction = PointerInteraction::default();
        let mut bounds = BOUNDS;
        let outcomes = feed(
            &mut interaction,
            &mut bounds,
            (100, 50),
            &[
                PointerEvent::Enter { x: 10.0, y: 10.0 },
                PointerEvent::Button {
                    button: BUTTON_RIGHT,
                    pressed: true,
                },
                PointerEvent::Motion { x: 11.0, y: 11.0 },
                PointerEvent::Button {
                    button: BUTTON_RIGHT,
                    pressed: false,
                },
            ],
        );
        assert_eq!(bounds, BOUNDS);
        assert!(
            outcomes
                .iter()
                .all(|outcome| *outcome == PointerOutcome::default())
        );
    }

    #[test]
    fn a_press_without_a_known_pointer_starts_no_drag() {
        let mut interaction = PointerInteraction::default();
        let mut bounds = BOUNDS;
        feed(
            &mut interaction,
            &mut bounds,
            (100, 50),
            &[
                PointerEvent::Button {
                    button: BUTTON_LEFT,
                    pressed: true,
                },
                PointerEvent::Motion { x: 60.0, y: 60.0 },
            ],
        );
        assert!(!interaction.moving());
        assert_eq!(bounds, BOUNDS);
    }
}
