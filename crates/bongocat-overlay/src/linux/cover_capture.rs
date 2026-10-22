//! Rendering a model's own cover, off-screen.
//!
//! The capture reuses the product's renderer so the stored cover is the model
//! as the user will see it. Linux needs no hidden window for it: the renderer
//! draws into an off-screen target, so nothing can appear on screen.

use super::*;

/// One model cover capture, drawn a frame at a time.
///
/// The same sequence as the Windows and macOS captures: a private runtime, the
/// model activated into it and committed only after the renderer prepared it,
/// a first frame that must pass the frame smoke check, then up to
/// [`COVER_CAPTURE_FRAMES`] frames while the model settles. The caller waits
/// [`Self::frame_interval`] between steps so its own loop keeps running
/// (ADR-0055).
pub(crate) struct CoverCaptureSession {
    pub(crate) renderer: OffscreenRenderer,
    pub(crate) runtime: RuntimeOwner,
    pub(crate) render_consumer: RenderConsumer,
    pub(crate) captured: Option<CapturedFrame>,
    pub(crate) deadline: Instant,
    pub(crate) frames_drawn: u32,
    pub(crate) frame_interval: Duration,
}

impl CoverCaptureSession {
    pub(crate) fn start(model: Arc<CommittedModel>) -> Result<Self, OverlayError> {
        let (runtime, render_consumer) = RuntimeOwner::start_with_rendering(true, 64);
        let runtime_client = runtime.client();
        runtime_client
            .wait_for_revision(1, RUNTIME_TIMEOUT)
            .ok_or_else(|| OverlayError::new("cover capture runtime did not become ready"))?;
        let activation = runtime_client
            .send(RuntimeCommand::ActivateModel(model))
            .map_err(|error| OverlayError::new(error.to_string()))?;
        let prepared = runtime_client
            .wait_for_model_preparation(activation, RUNTIME_TIMEOUT)
            .ok_or_else(|| OverlayError::new("cover capture model activation was not prepared"))?;
        if let Some(failure) = prepared
            .last_command_failure
            .filter(|failure| failure.sequence == activation)
        {
            return Err(OverlayError::new(format!(
                "cover capture model activation failed: {:?}",
                failure.code
            )));
        }
        let initial_frame = render_consumer
            .take_latest()
            .ok_or_else(|| OverlayError::new("cover capture runtime published no render frame"))?;
        let initial_token = initial_frame
            .model_commit
            .filter(|token| token.command_sequence == activation)
            .ok_or_else(|| {
                OverlayError::new("cover capture frame has the wrong model commit token")
            })?;
        let options = OverlaySessionOptions {
            scale_percent: COVER_CAPTURE_SCALE_PERCENT,
            keep_inside_screen: false,
            ..OverlaySessionOptions::default()
        };
        let frame_interval = frame_interval_for_maximum_fps(options.maximum_fps)
            .expect("cover capture options carry a validated maximum FPS");
        let (width, height) =
            model_window_dimensions(initial_frame.snapshot.canvas, options.scale_percent);
        let mut renderer = match OffscreenRenderer::create(
            &initial_frame,
            width,
            height,
            options.corner_radius_percent,
        ) {
            Ok(renderer) => renderer,
            Err(error) => {
                reject_model_commit(&runtime_client, &render_consumer, initial_token)?;
                return Err(error);
            }
        };
        report_model_commit(
            &runtime_client,
            &render_consumer,
            initial_token,
            ModelCommitOutcome::Prepared,
        )?;
        // The first frame is verified like a committed model is: a capture that
        // renders nothing must fail before it replaces a cover with a blank
        // image. Later frames are not, because a settling animation may be
        // mostly transparent while the model moves.
        let captured = Some(renderer.draw_capturing(true)?);
        Ok(Self {
            renderer,
            runtime,
            render_consumer,
            captured,
            deadline: Instant::now() + COVER_CAPTURE_TIMEOUT,
            frames_drawn: 1,
            frame_interval,
        })
    }

    pub(crate) fn frame_interval(&self) -> Duration {
        self.frame_interval
    }

    /// Draw one more capture frame. `Ok(false)` means every frame is drawn or
    /// the deadline has passed, and `finish` is the next call.
    pub(crate) fn step(&mut self) -> Result<bool, OverlayError> {
        if self.frames_drawn >= COVER_CAPTURE_FRAMES || Instant::now() >= self.deadline {
            return Ok(false);
        }
        if let Some(frame) = self.render_consumer.take_latest() {
            self.renderer.sync_frame(&frame)?;
        }
        self.captured = Some(self.renderer.draw_capturing(false)?);
        self.frames_drawn += 1;
        Ok(true)
    }

    /// Stop the capture runtime and return the most recent captured frame.
    pub(crate) fn finish(self) -> Result<CapturedFrame, OverlayError> {
        self.runtime
            .shutdown(RUNTIME_TIMEOUT)
            .map_err(|error| OverlayError::new(error.to_string()))?;
        self.captured
            .ok_or_else(|| OverlayError::new("cover capture drew no frame"))
    }
}
