//! The overlay session the product drives on Linux.
//!
//! The same protocol as the macOS session: the initial model is committed only
//! after its first frame is drawn and checked, a model switch keeps the current
//! model when the new one cannot be prepared, settings apply every tick, and
//! shutdown releases the window only after input and the runtime have stopped.
//!
//! The pointer is the window's own `wl_pointer`: a left-button drag moves the
//! window, a right-button drag resizes it through the shared [`ResizeDrag`]
//! state machine, and the shared placement constraint keeps it on its output.
//!
//! What Linux cannot do is reported rather than faked. There is no platform
//! input service until ADR-0090 stage L5, so input starts as
//! `BackendUnavailable`. A right-click without a drag shows no menu, because the
//! menu library has no GTK-free context menu (stage L6). Hover hide is not
//! available on Wayland at all: a client only sees the pointer over a surface
//! that accepts input, and a hidden pet passes input through, so it could never
//! notice the pointer leaving.

use super::*;

/// The stable detail of every Linux entry point that has no implementation yet.
pub(crate) const UNSUPPORTED: &str = "overlay_unsupported_on_linux";

/// How long a runtime handshake may take. Matches the Windows adapter, whose
/// cover capture runs the same activate-then-commit sequence.
pub(crate) const RUNTIME_TIMEOUT: Duration = Duration::from_secs(2);

/// Fields drop in declaration order: the window (and the GPU surface inside it)
/// first, then the renderer's device, and the Wayland connection last, because
/// the GPU surface holds raw pointers into it.
pub(crate) struct ProductOverlaySession {
    window: Option<ModelWindow>,
    renderer: Renderer,
    runtime_client: RuntimeClient,
    render_consumer: RenderConsumer,
    options: OverlaySessionOptions,
    /// The window box, kept while the window is unmapped so it reopens in place.
    bounds: OverlayWindowBounds,
    last_frame: RenderFrame,
    previous_snapshot: Arc<RenderSnapshot>,
    pending_initial_model_commit: Option<ModelCommitToken>,
    pending_model_frame: Option<RenderFrame>,
    retry_backoff: FrameRetryBackoff,
    frames_presented: u64,
    dynamic_snapshots: u64,
    model_commit_rejections: u64,
    input_start_error: Option<PlatformInputError>,
    input_stopped: bool,
    resize_sender: Option<SyncSender<OverlayResizeOutcome>>,
    pointer: PointerInteraction,
    /// The window origin of the last presented frame. Pointer coordinates are
    /// relative to the window the compositor has, which is the committed one,
    /// so a drag is measured from here rather than from a pending move.
    committed_origin: (i32, i32),
    placement: OverlayPlacementConstraint,
    session_started: Instant,
    display: WaylandDisplay,
}

impl ProductOverlaySession {
    pub(crate) fn start(
        runtime_client: RuntimeClient,
        input_producer: InputProducer,
        cursor_producer: CursorProducer,
        gamepad_axis_producer: GamepadAxisProducer,
        render_consumer: RenderConsumer,
        options: OverlaySessionOptions,
        interaction_sinks: OverlayInteractionSinks,
    ) -> Result<Self, OverlayError> {
        // Platform input is stage L5. The context menu has no GTK-free
        // presentation yet (stage L6), so only the resize outcome is wired.
        let _ = (input_producer, cursor_producer, gamepad_axis_producer);
        let OverlayInteractionSinks {
            context_menu_sender: _,
            resize_sender,
        } = interaction_sinks;
        let initial_frame = render_consumer
            .take_latest()
            .ok_or_else(|| OverlayError::new("runtime did not publish an initial render frame"))?;
        let token = initial_frame
            .model_commit
            .ok_or_else(|| OverlayError::new("initial render frame has no model commit token"))?;
        let opened = Self::open(&initial_frame, options);
        let (display, window, renderer) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                reject_model_commit(&runtime_client, &render_consumer, token)?;
                return Err(error);
            }
        };
        let bounds = window.window.bounds();
        let diagnostics_producer = runtime_client.platform_input_diagnostics_producer();
        let (_, input_start_error) =
            crate::start_platform_input::<()>(&diagnostics_producer, || {
                Err(PlatformInputError::BackendUnavailable)
            });
        let mut session = Self {
            window: Some(window),
            renderer,
            runtime_client,
            render_consumer,
            options,
            bounds,
            previous_snapshot: Arc::clone(&initial_frame.snapshot),
            last_frame: initial_frame,
            pending_initial_model_commit: None,
            pending_model_frame: None,
            retry_backoff: FrameRetryBackoff::default(),
            frames_presented: 0,
            dynamic_snapshots: 0,
            model_commit_rejections: 0,
            input_start_error,
            input_stopped: false,
            resize_sender,
            pointer: PointerInteraction::default(),
            committed_origin: (bounds.x, bounds.y),
            placement: OverlayPlacementConstraint::default(),
            session_started: Instant::now(),
            display,
        };
        session.apply_presentation();
        if session.runtime_client.snapshot().overlay_visible {
            match session.present(true) {
                Ok(()) => {}
                Err(error) if error.is_temporary_presentation_unavailable() => {
                    session.pending_initial_model_commit = Some(token);
                    let _ = session.retry_backoff.register_temporary_failure();
                }
                Err(error) => {
                    reject_model_commit(&session.runtime_client, &session.render_consumer, token)?;
                    return Err(error);
                }
            }
        } else {
            session.window = None;
        }
        if session.pending_initial_model_commit.is_none() {
            report_model_commit(
                &session.runtime_client,
                &session.render_consumer,
                token,
                ModelCommitOutcome::Prepared,
            )?;
        }
        Ok(session)
    }

    /// Connect, open the first window, then open a device compatible with it.
    fn open(
        frame: &RenderFrame,
        options: OverlaySessionOptions,
    ) -> Result<(WaylandDisplay, ModelWindow, Renderer), OverlayError> {
        let mut display = WaylandDisplay::connect()?;
        let default_size = model_window_dimensions(frame.snapshot.canvas, options.scale_percent);
        let instance = GpuContext::instance(AdapterRequest::PRODUCT);
        let (layer, surface) = open_window(
            &mut display,
            &instance,
            options.window_bounds,
            default_size,
            options,
        )?;
        let context = GpuContext::create(instance, AdapterRequest::PRODUCT, Some(&surface))?;
        let format = window_color_format(&surface, &context.adapter)?;
        let (width, height) = layer.buffer_size(&display);
        let renderer = Renderer::create(
            context,
            format,
            frame,
            width,
            height,
            options.corner_radius_percent,
        )?;
        let window = ModelWindow::new(layer, surface, &renderer, &display)?;
        Ok((display, window, renderer))
    }

    /// Re-open the window after it was hidden, at the box it had.
    fn reopen(&mut self) -> Result<(), OverlayError> {
        let (layer, surface) = open_window(
            &mut self.display,
            &self.renderer.context.instance,
            Some(self.bounds),
            (self.bounds.width, self.bounds.height),
            self.options,
        )?;
        let window = ModelWindow::new(layer, surface, &self.renderer, &self.display)?;
        self.window = Some(window);
        self.apply_presentation();
        Ok(())
    }

    fn apply_presentation(&mut self) {
        let alpha = f64::from(self.options.opacity_percent) / 100.0;
        let click_through = self.options.click_through;
        let always_on_top = self.options.always_on_top;
        if let Some(window) = &mut self.window {
            window.window.set_opacity(alpha);
            window
                .window
                .set_click_through(&self.display, click_through);
            window.window.set_always_on_top(always_on_top);
        }
    }

    fn present(&mut self, verify: bool) -> Result<(), OverlayError> {
        if self.window.is_none() {
            self.reopen()?;
        }
        let window = self
            .window
            .as_mut()
            .expect("the model window was just opened");
        window.present(&mut self.renderer, &self.display, verify)?;
        self.committed_origin = (self.bounds.x, self.bounds.y);
        self.retry_backoff.record_success();
        self.frames_presented = self.frames_presented.saturating_add(1);
        Ok(())
    }

    fn apply_bounds(&mut self) {
        if let Some(window) = &mut self.window {
            window.window.set_bounds(self.bounds);
        }
    }

    /// The window size a resize drag treats as `100%`: the size the window is
    /// created with for the current model, as on macOS.
    fn resize_base(&self) -> Option<ResizeBase> {
        let (width, height) = default_overlay_window_dimensions(self.last_frame.snapshot.canvas);
        ResizeBase::new(f64::from(width), f64::from(height))
    }

    fn bounds_match_scale(&self, scale_percent: u16) -> bool {
        self.resize_base()
            .is_some_and(|base| crate::bounds_match_scale(self.bounds, base, scale_percent))
    }

    /// Apply this tick's pointer events in the order they arrived.
    fn handle_pointer(&mut self) {
        for event in self.display.take_pointer_events() {
            let outcome = self.pointer.handle(
                event,
                self.bounds,
                self.committed_origin,
                self.resize_base(),
            );
            if let Some(bounds) = outcome.bounds {
                self.bounds = bounds;
                self.apply_bounds();
            }
            if let (Some(scale_percent), Some(sender)) = (outcome.resized_to, &self.resize_sender) {
                let _ = sender.try_send(OverlayResizeOutcome { scale_percent });
            }
        }
    }

    fn defer_drawable_unavailable(&mut self) -> OverlayTickOutcome {
        OverlayTickOutcome::Deferred(self.retry_backoff.register_temporary_failure())
    }

    pub(crate) fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let window = self.window.as_ref().ok_or(HandleError::Unavailable)?;
        let (_, raw) = self
            .display
            .raw_handles(&window.window)
            .map_err(|_| HandleError::Unavailable)?;
        // SAFETY: the `wl_surface` behind `raw` belongs to `window`, which the
        // returned borrow of `self` keeps alive.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }

    pub(crate) fn run_for(&mut self, duration: Duration) -> Result<(), OverlayError> {
        let started = Instant::now();
        let mut next_frame = started;
        while duration.is_zero() || started.elapsed() < duration {
            let outcome = self.tick()?;
            if outcome == OverlayTickOutcome::Hidden {
                break;
            }
            let interval = outcome.retry_after().unwrap_or_else(|| {
                frame_interval_for_maximum_fps(self.options.maximum_fps)
                    .expect("product overlay stores a validated maximum FPS")
            });
            if outcome.retry_after().is_some() {
                next_frame = Instant::now();
            }
            next_frame += interval;
            if let Some(delay) = next_frame.checked_duration_since(Instant::now()) {
                std::thread::sleep(delay);
            } else {
                next_frame = Instant::now();
            }
        }
        Ok(())
    }

    pub(crate) fn tick(&mut self) -> Result<OverlayTickOutcome, OverlayError> {
        self.display.dispatch()?;
        let runtime_snapshot = self.runtime_client.snapshot();
        if runtime_snapshot.state == RuntimeState::Stopped {
            return Err(OverlayError::new(
                "runtime stopped while the product overlay was active",
            ));
        }
        let next_options = self
            .options
            .with_runtime_settings(runtime_snapshot.overlay_settings);
        self.handle_pointer();
        if next_options != self.options {
            // A resize drag already sized the window before its scale reached
            // the configuration, so the scale it wrote back is not applied twice.
            if next_options.scale_percent != self.options.scale_percent
                && !self.bounds_match_scale(next_options.scale_percent)
            {
                self.bounds = self
                    .bounds
                    .rescale(self.options.scale_percent, next_options.scale_percent);
                self.apply_bounds();
            }
            self.renderer.corner_radius_percent = next_options.corner_radius_percent;
            self.options = next_options;
            self.apply_presentation();
        }
        if self.options.keep_inside_screen
            && !self.pointer.moving()
            && let Some(window) = &self.window
        {
            let output = window.window.output_bounds();
            let correction =
                self.placement
                    .observe(self.bounds, self.session_started.elapsed(), || {
                        vec![OverlayScreenBounds {
                            x: output.x,
                            y: output.y,
                            width: output.width,
                            height: output.height,
                        }]
                    });
            if let Some(correction) = correction {
                self.bounds = correction;
                self.apply_bounds();
            }
        }
        self.options.maximum_fps = runtime_snapshot.maximum_fps;
        let overlay_visible = runtime_snapshot.overlay_visible;
        if !overlay_visible {
            // Unmapping a layer surface means destroying it; it reopens at the
            // same box when the overlay is shown again.
            self.window = None;
        }

        if let Some(token) = self.pending_initial_model_commit {
            match self.present(true) {
                Ok(()) => {
                    report_model_commit(
                        &self.runtime_client,
                        &self.render_consumer,
                        token,
                        ModelCommitOutcome::Prepared,
                    )?;
                    self.pending_initial_model_commit = None;
                    return Ok(OverlayTickOutcome::Presented);
                }
                Err(error) if error.is_temporary_presentation_unavailable() => {
                    return Ok(self.defer_drawable_unavailable());
                }
                Err(error) => {
                    reject_model_commit(&self.runtime_client, &self.render_consumer, token)?;
                    return Err(error);
                }
            }
        }

        let next_frame = self.pending_model_frame.take().or_else(|| {
            if overlay_visible {
                self.render_consumer.take_latest()
            } else {
                self.render_consumer.take_model_commit()
            }
        });
        if let Some(frame) = next_frame {
            let model_changed = frame.model_generation != self.renderer.model_generation;
            let previous_bounds = self.bounds;
            match self.renderer.sync_frame(&frame) {
                Ok(_) => {
                    if model_changed {
                        // The window keeps its width and takes the new canvas's
                        // aspect ratio, exactly like a macOS model switch.
                        self.bounds =
                            model_switch_window_bounds(self.bounds, frame.snapshot.canvas);
                        if let Some(window) = &mut self.window {
                            window.window.set_bounds(self.bounds);
                        }
                        if overlay_visible {
                            match self.present(true) {
                                Ok(()) => {}
                                Err(error) if error.is_temporary_presentation_unavailable() => {
                                    self.pending_model_frame = Some(frame);
                                    return Ok(self.defer_drawable_unavailable());
                                }
                                Err(error) => {
                                    self.bounds = previous_bounds;
                                    if let Some(token) = frame.model_commit {
                                        reject_model_commit(
                                            &self.runtime_client,
                                            &self.render_consumer,
                                            token,
                                        )?;
                                        self.model_commit_rejections =
                                            self.model_commit_rejections.saturating_add(1);
                                    }
                                    return Err(error);
                                }
                            }
                        }
                    }
                    if let Some(token) = frame.model_commit {
                        report_model_commit(
                            &self.runtime_client,
                            &self.render_consumer,
                            token,
                            ModelCommitOutcome::Prepared,
                        )?;
                    }
                    if frame.snapshot.as_ref() != self.previous_snapshot.as_ref() {
                        self.dynamic_snapshots = self.dynamic_snapshots.saturating_add(1);
                    }
                    self.previous_snapshot = Arc::clone(&frame.snapshot);
                    self.last_frame = frame;
                    if model_changed {
                        return Ok(if overlay_visible {
                            OverlayTickOutcome::Presented
                        } else {
                            OverlayTickOutcome::Hidden
                        });
                    }
                }
                // A model that cannot be prepared is rejected and the current
                // one keeps drawing: `sync_frame` replaces nothing on failure.
                Err(error) if frame.model_commit.is_some() => {
                    reject_model_commit(
                        &self.runtime_client,
                        &self.render_consumer,
                        frame.model_commit.expect("checked model commit token"),
                    )?;
                    self.model_commit_rejections = self.model_commit_rejections.saturating_add(1);
                    let _ = error;
                }
                Err(error) => return Err(error),
            }
        }
        if !overlay_visible {
            return Ok(OverlayTickOutcome::Hidden);
        }
        match self.present(self.frames_presented == 0) {
            Ok(()) => Ok(OverlayTickOutcome::Presented),
            Err(error) if error.is_temporary_presentation_unavailable() => {
                Ok(self.defer_drawable_unavailable())
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn window_bounds(&self) -> Result<OverlayWindowBounds, OverlayError> {
        self.bounds.validate()
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.window.is_some() && self.frames_presented > 0
    }

    pub(crate) fn model_generation(&self) -> u64 {
        self.renderer.model_generation
    }

    pub(crate) fn stop_input(&mut self) -> Result<(), OverlayError> {
        self.input_stopped = true;
        Ok(())
    }

    pub(crate) fn finish_after_runtime_shutdown(
        mut self,
    ) -> Result<ProductOverlayReport, OverlayError> {
        if !self.input_stopped {
            return Err(OverlayError::new(
                "platform input must stop before the runtime",
            ));
        }
        if self.runtime_client.snapshot().state != RuntimeState::Stopped {
            return Err(OverlayError::new(
                "runtime must stop before releasing the product overlay",
            ));
        }
        while self.render_consumer.take_latest().is_some() {}
        let placement_fully_visible = !self.options.keep_inside_screen
            || self.window.as_ref().is_none_or(|window| {
                let output = window.window.output_bounds();
                bounds_inside_screens(
                    &[OverlayScreenBounds {
                        x: output.x,
                        y: output.y,
                        width: output.width,
                        height: output.height,
                    }],
                    self.bounds,
                )
            });
        let report = ProductOverlayReport {
            frames_presented: self.frames_presented,
            placement_fully_visible,
            dynamic_snapshots: self.dynamic_snapshots,
            model_commit_rejections: self.model_commit_rejections,
            input_start_error: self.input_start_error,
            input_diagnostics: None,
            render_diagnostics: self.render_consumer.diagnostics(),
            model_generation: self.renderer.model_generation,
            drawable_count: self.renderer.model.drawable_count(),
            masked_drawable_count: self.renderer.model.masked_drawable_count(),
            texture_count: self.renderer.model.texture_count(),
        };
        // Release the window before the device, explicitly, so the order does
        // not depend on the struct layout alone.
        self.window = None;
        Ok(report)
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
