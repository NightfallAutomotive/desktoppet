//! The product session placeholder until ADR-0090 stage L4.
//!
//! Linux has no model window yet, so every product entry point reports
//! [`UNSUPPORTED`] instead of pretending to run, and the session type is
//! uninhabited: no caller can hold a Linux session that draws nothing. The
//! methods exist only so the shared dispatch compiles; each one is an empty
//! match over a value that cannot exist.

use super::*;

/// The stable detail every Linux product overlay entry point reports.
pub(crate) const UNSUPPORTED: &str = "overlay_unsupported_on_linux";

/// How long a runtime handshake may take. Matches the Windows adapter, whose
/// cover capture runs the same activate-then-commit sequence.
pub(crate) const RUNTIME_TIMEOUT: Duration = Duration::from_secs(2);

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

pub(crate) fn run_model_switch_preview(
    _model_id: &str,
    _model_root: &Path,
    _switch_cycles: u32,
) -> Result<PreviewReport, OverlayError> {
    Err(OverlayError::new(UNSUPPORTED))
}

pub(crate) fn reject_model_commit(
    runtime_client: &RuntimeClient,
    render_consumer: &RenderConsumer,
    token: ModelCommitToken,
) -> Result<(), OverlayError> {
    report_model_commit(
        runtime_client,
        render_consumer,
        token,
        ModelCommitOutcome::Rejected(ModelCommitErrorCode::ResourcePreparationFailed),
    )
}

/// Tell the runtime whether the renderer accepted a model, and wait until the
/// runtime has finished the commit it was holding for that answer.
pub(crate) fn report_model_commit(
    runtime_client: &RuntimeClient,
    render_consumer: &RenderConsumer,
    token: ModelCommitToken,
    outcome: ModelCommitOutcome,
) -> Result<(), OverlayError> {
    render_consumer
        .report_model_commit(ModelCommitFeedback { token, outcome })
        .map_err(|error| OverlayError::new(error.to_string()))?;
    let completed = runtime_client
        .wait_for_command(token.command_sequence, RUNTIME_TIMEOUT)
        .ok_or_else(|| OverlayError::new("runtime did not finish the model commit"))?;
    let failure = completed
        .last_command_failure
        .filter(|failure| failure.sequence == token.command_sequence);
    match (outcome, failure) {
        (ModelCommitOutcome::Prepared, None)
        | (
            ModelCommitOutcome::Rejected(ModelCommitErrorCode::ResourcePreparationFailed),
            Some(bongocat_runtime::RuntimeCommandFailure {
                code: RuntimeRenderErrorCode::GpuPreparationFailed,
                ..
            }),
        ) => Ok(()),
        (ModelCommitOutcome::Prepared, Some(failure)) => Err(OverlayError::new(format!(
            "runtime rejected prepared model generation: {:?}",
            failure.code
        ))),
        (ModelCommitOutcome::Rejected(_), None) => Err(OverlayError::new(
            "runtime committed a renderer-rejected model generation",
        )),
        (ModelCommitOutcome::Rejected(_), Some(failure)) => Err(OverlayError::new(format!(
            "runtime reported the wrong model rejection: {:?}",
            failure.code
        ))),
    }
}
