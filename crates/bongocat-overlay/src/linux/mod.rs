//! The Linux overlay adapter (ADR-0090).
//!
//! Stage L3 brings the off-screen `wgpu` renderer and the model cover capture
//! that drives it. The model window itself is stage L4, so the product session
//! still reports [`UNSUPPORTED`]. The layout follows the other two adapters:
//! `session` is what the product drives, `renderer` and `pipelines` own the
//! frame, and `cover_capture` is the one consumer that needs no window.
//!
//! Every call here is safe Rust: `wgpu` owns the Vulkan and GLES handles, so
//! this adapter has no `unsafe` block and no raw handle to keep alive.

mod cover_capture;
mod pipelines;
mod renderer;
mod session;
#[cfg(test)]
mod tests;

pub(crate) use cover_capture::*;
pub(crate) use pipelines::*;
pub(crate) use renderer::*;
pub(crate) use session::*;

use crate::{
    BlendFactor, DrawableCullMode, FRAME_SMOKE_GRID_DIMENSION, OverlayError,
    OverlayInteractionSinks, OverlaySessionOptions, OverlayTickOutcome, OverlayWindowBounds,
    PreviewReport, ProductOverlayReport, blend_factors, corner_radius_uniform,
    cover::{
        COVER_CAPTURE_FRAMES, COVER_CAPTURE_SCALE_PERCENT, COVER_CAPTURE_TIMEOUT, CapturedFrame,
    },
    drawable_cull_mode, model_window_dimensions, validate_frame_smoke,
    validate_model_generation_advance,
};
use bongocat_model::CommittedModel;
use bongocat_render::{
    BlendMode, DrawableId, KeyAssetId, KeyOverlay, ModelBounds, ModelCommitErrorCode,
    ModelCommitFeedback, ModelCommitOutcome, ModelCommitToken, RenderConsumer, RenderFrame,
    RenderResources, RenderSnapshot, TextureAsset, TextureId, legacy_key_overlay_bounds,
    quad_vertices, validate_render_snapshot,
};
use bongocat_runtime::{
    CursorProducer, GamepadAxisProducer, InputProducer, RuntimeClient, RuntimeCommand,
    RuntimeOwner, RuntimeRenderErrorCode, frame_interval_for_maximum_fps,
};
use image::ImageReader;
use raw_window_handle::{HandleError, WindowHandle};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
