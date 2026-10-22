//! What the pointer does to the model window, without any Wayland in it.
//!
//! A left-button drag moves the window and a right-button drag resizes it
//! through the shared [`ResizeDrag`] state machine. The rules are kept apart
//! from the protocol so they can be tested as plain arithmetic.

use super::*;

/// What a pointer event asks of the window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PointerOutcome {
    /// The box the window should take now.
    pub(crate) bounds: Option<OverlayWindowBounds>,
    /// A finished resize drag: the scale to write back to configuration.
    pub(crate) resized_to: Option<u16>,
}

#[derive(Debug, Default)]
pub(crate) struct PointerInteraction {
    /// The last pointer position over the window, in surface coordinates.
    pointer: Option<(f64, f64)>,
    /// A left-button drag: where in the window it was pressed.
    move_drag: Option<(f64, f64)>,
    resize_drag: Option<ResizeDrag>,
}

impl PointerInteraction {
    pub(crate) fn moving(&self) -> bool {
        self.move_drag.is_some()
    }

    /// Apply one event.
    ///
    /// `bounds` is the window's current box and `committed_origin` the origin
    /// of the last frame the compositor has: surface coordinates are relative
    /// to that window, so a move is measured from it rather than from a move
    /// that has not been presented yet. `base` is the `100%` size of the model.
    pub(crate) fn handle(
        &mut self,
        event: PointerEvent,
        bounds: OverlayWindowBounds,
        committed_origin: (i32, i32),
        base: Option<ResizeBase>,
    ) -> PointerOutcome {
        match event {
            PointerEvent::Enter { x, y } | PointerEvent::Motion { x, y } => {
                self.pointer = Some((x, y));
                PointerOutcome {
                    bounds: self.moved(x, y, bounds, committed_origin),
                    resized_to: None,
                }
            }
            PointerEvent::Leave => {
                self.pointer = None;
                PointerOutcome::default()
            }
            PointerEvent::Button { button, pressed } => self.button(button, pressed, bounds, base),
        }
    }

    fn moved(
        &mut self,
        x: f64,
        y: f64,
        bounds: OverlayWindowBounds,
        committed_origin: (i32, i32),
    ) -> Option<OverlayWindowBounds> {
        let mut next = bounds;
        if let Some((pressed_x, pressed_y)) = self.move_drag {
            let origin_x = (f64::from(committed_origin.0) + x - pressed_x).round() as i32;
            let origin_y = (f64::from(committed_origin.1) + y - pressed_y).round() as i32;
            next = OverlayWindowBounds::new(origin_x, origin_y, next.width, next.height);
        }
        if let Some(drag) = &mut self.resize_drag {
            // Output-relative, so the position stays comparable while the
            // window's size changes under the pointer; the origin stays put.
            let position = (f64::from(next.x) + x, f64::from(next.y) + y);
            if let Some(outcome) = drag.observe(position) {
                next = OverlayWindowBounds::new(next.x, next.y, outcome.width, outcome.height);
            }
        }
        (next != bounds).then_some(next)
    }

    fn button(
        &mut self,
        button: u32,
        pressed: bool,
        bounds: OverlayWindowBounds,
        base: Option<ResizeBase>,
    ) -> PointerOutcome {
        match (button, pressed) {
            (BUTTON_LEFT, true) => self.move_drag = self.pointer,
            (BUTTON_LEFT, false) => self.move_drag = None,
            (BUTTON_RIGHT, true) => {
                self.resize_drag = self.pointer.zip(base).map(|(pointer, base)| {
                    ResizeDrag::begin(
                        (
                            f64::from(bounds.x) + pointer.0,
                            f64::from(bounds.y) + pointer.1,
                        ),
                        base,
                        base.scale_percent_for_width(bounds.width),
                    )
                });
            }
            (BUTTON_RIGHT, false) => {
                // A right-click that never became a drag would open the context
                // menu, which has no Linux presentation yet (ADR-0090 L6).
                let resized_to = self
                    .resize_drag
                    .take()
                    .filter(ResizeDrag::dragging)
                    .and_then(ResizeDrag::finish);
                return PointerOutcome {
                    bounds: None,
                    resized_to,
                };
            }
            _ => {}
        }
        PointerOutcome::default()
    }
}
