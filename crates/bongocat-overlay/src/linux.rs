//! The Linux overlay placeholder until ADR-0090 stage L4.
//!
//! Linux has no model window yet: the renderer backend is decided by the L3
//! spike and the X11/`wlr-layer-shell` window is L4. Until then every entry
//! point reports [`UNSUPPORTED`] instead of pretending to run, and the session
//! types are uninhabited, so no caller can hold a Linux session that draws
//! nothing. The methods exist only so the shared dispatch compiles; each one is
//! an empty match over a value that cannot exist.

use super::*;
use crate::cover::CapturedFrame;

/// The stable detail every Linux overlay entry point reports.
pub(crate) const UNSUPPORTED: &str = "overlay_unsupported_on_linux";

pub(crate) enum ProductOverlaySession {}

impl ProductOverlaySession {
    pub(crate) fn start(
        _runtime_client: RuntimeClient,
        _input_producer: InputProducer,
        _cursor_producer: CursorProducer,
        _gamepad_axis_producer: GamepadAxisProducer,
        _render_consumer: RenderConsumer,
        _options: OverlaySessionOptions,
        _interaction_sinks: OverlayInteractionSinks,
    ) -> Result<Self, OverlayError> {
        Err(OverlayError::new(UNSUPPORTED))
    }

    pub(crate) fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        match *self {}
    }

    pub(crate) fn run_for(&mut self, _duration: Duration) -> Result<(), OverlayError> {
        match *self {}
    }

    pub(crate) fn tick(&mut self) -> Result<OverlayTickOutcome, OverlayError> {
        match *self {}
    }

    pub(crate) fn window_bounds(&self) -> Result<OverlayWindowBounds, OverlayError> {
        match *self {}
    }

    pub(crate) fn is_visible(&self) -> bool {
        match *self {}
    }

    pub(crate) fn model_generation(&self) -> u64 {
        match *self {}
    }

    pub(crate) fn stop_input(&mut self) -> Result<(), OverlayError> {
        match *self {}
    }

    pub(crate) fn finish_after_runtime_shutdown(
        self,
    ) -> Result<ProductOverlayReport, OverlayError> {
        match self {}
    }
}

pub(crate) enum CoverCaptureSession {}

impl CoverCaptureSession {
    pub(crate) fn start(_model: Arc<bongocat_model::CommittedModel>) -> Result<Self, OverlayError> {
        Err(OverlayError::new(UNSUPPORTED))
    }

    pub(crate) fn frame_interval(&self) -> Duration {
        match *self {}
    }

    pub(crate) fn step(&mut self) -> Result<bool, OverlayError> {
        match *self {}
    }

    pub(crate) fn finish(self) -> Result<CapturedFrame, OverlayError> {
        match self {}
    }
}

pub(crate) fn run_model_switch_preview(
    _model_id: &str,
    _model_root: &Path,
    _switch_cycles: u32,
) -> Result<PreviewReport, OverlayError> {
    Err(OverlayError::new(UNSUPPORTED))
}
