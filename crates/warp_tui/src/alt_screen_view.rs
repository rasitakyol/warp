//! Full-screen alt-screen rendering + raw input forwarding for the TUI.
//!
//! When a PTY app switches to the alternate screen (vim, htop, less, …), the
//! terminal model flips [`TerminalModel::is_alt_screen_active`] and populates a
//! dedicated alt-screen grid. [`TuiTerminalSessionView`] then renders this
//! element full-area instead of the block/transcript UI, and forwards
//! input straight to the PTY as escape sequences — mirroring the GUI's
//! `AltScreenElement` (`app/src/terminal/alt_screen/alt_screen_element.rs`).
//!
//! Covers rendering, the cursor, and keyboard and SGR mouse forwarding. PTY
//! sizing is handled by the session view's `TuiTerminalSizeElement` wrapper,
//! which publishes this element's laid-out dimensions after every layout.
//!
//! [`TuiTerminalSessionView`]: crate::terminal_session_view::TuiTerminalSessionView
//! [`TerminalModel::is_alt_screen_active`]: warp::tui_export::TerminalModel

use std::ops::Deref as _;
use std::sync::Arc;

use parking_lot::FairMutex;
use warp::tui_export::{KeystrokeWithDetails, TermMode, TerminalModel, ToEscapeSequence as _};
use warp_terminal::model::escape_sequences::{alt_screen_scroll_to_pty_bytes, ModeProvider};
use warp_terminal::model::grid::Dimensions as _;
use warp_terminal::model::mouse::{MouseAction, MouseButton, MouseState};
use warp_terminal::model::Point;
use warpui_core::elements::tui::{
    TuiConstraint, TuiElement, TuiEvent, TuiEventContext, TuiLayoutContext, TuiPaintContext,
    TuiPaintSurface, TuiScreenPoint, TuiScreenPosition, TuiScreenRect, TuiSize,
};
use warpui_core::AppContext;

use crate::terminal_block::render_grid_handler;
use crate::terminal_session_view::TuiTerminalSessionAction;

/// Renders the terminal's alt-screen grid full-area and forwards input to the
/// PTY while a full-screen app is active.
pub(crate) struct AltScreenElement {
    model: Arc<FairMutex<TerminalModel>>,
    size: Option<TuiSize>,
    origin: Option<TuiScreenPoint>,
}

impl AltScreenElement {
    pub(crate) fn new(model: Arc<FairMutex<TerminalModel>>) -> Self {
        Self {
            model,
            size: None,
            origin: None,
        }
    }
}

/// Converts a supported pointer event into the terminal's SGR mouse model.
fn mouse_state_for_event(
    event: &TuiEvent,
    bounds: TuiScreenRect,
    is_mode_set: impl Fn(TermMode) -> bool,
) -> Option<MouseState> {
    if !is_mode_set(TermMode::SGR_MOUSE) {
        return None;
    }
    let reports_clicks = is_mode_set(TermMode::MOUSE_REPORT_CLICK);
    let reports_drag = is_mode_set(TermMode::MOUSE_DRAG);
    let reports_motion = is_mode_set(TermMode::MOUSE_MOTION);
    let reports_clicks = reports_clicks || reports_drag || reports_motion;
    let position = event.position()?;
    if !bounds.contains(position) {
        return None;
    }
    let point = Point::new(
        usize::try_from(i32::from(position.y) - bounds.origin.y).ok()?,
        usize::try_from(i32::from(position.x) - bounds.origin.x).ok()?,
    );

    let state = match event {
        TuiEvent::LeftMouseDown { modifiers, .. } if reports_clicks && !modifiers.shift => {
            MouseState::new(MouseButton::Left, MouseAction::Pressed, *modifiers)
        }
        TuiEvent::RightMouseDown { modifiers, .. } if reports_clicks && !modifiers.shift => {
            MouseState::new(MouseButton::Right, MouseAction::Pressed, *modifiers)
        }
        TuiEvent::LeftMouseUp { modifiers, .. } if reports_clicks && !modifiers.shift => {
            MouseState::new(MouseButton::Left, MouseAction::Released, *modifiers)
        }
        TuiEvent::LeftMouseDragged { modifiers, .. }
            if (reports_drag || reports_motion) && !modifiers.shift =>
        {
            MouseState::new(MouseButton::LeftDrag, MouseAction::Pressed, *modifiers)
        }
        TuiEvent::MouseMoved {
            modifiers,
            is_synthetic: false,
            ..
        } if reports_motion => MouseState::new(MouseButton::Move, MouseAction::Pressed, *modifiers),
        _ => return None,
    };
    Some(state.set_point(point))
}

/// Encodes a supported pointer event for the active alt-screen application.
fn mouse_event_to_pty_bytes<T: ModeProvider>(
    event: &TuiEvent,
    bounds: TuiScreenRect,
    is_mode_set: impl Fn(TermMode) -> bool,
    mode_provider: &T,
) -> Option<Vec<u8>> {
    if let TuiEvent::ScrollWheel {
        position,
        delta: (_, rows),
        ..
    } = event
    {
        if !bounds.contains(*position) {
            return None;
        }
        let point = Point::new(
            usize::try_from(i32::from(position.y) - bounds.origin.y).ok()?,
            usize::try_from(i32::from(position.x) - bounds.origin.x).ok()?,
        );
        return alt_screen_scroll_to_pty_bytes(
            i32::try_from(*rows).ok()?,
            point,
            is_mode_set(TermMode::SGR_MOUSE),
            mode_provider,
        );
    }

    mouse_state_for_event(event, bounds, is_mode_set)
        .and_then(|state| state.to_escape_sequence(mode_provider))
}

impl TuiElement for AltScreenElement {
    fn layout(
        &mut self,
        constraint: TuiConstraint,
        _ctx: &mut TuiLayoutContext,
        _app: &AppContext,
    ) -> TuiSize {
        // The alt-screen app owns the whole pane.
        let size = constraint.max;
        self.size = Some(size);
        size
    }

    fn render(
        &mut self,
        origin: TuiScreenPosition,
        surface: &mut TuiPaintSurface<'_>,
        ctx: &mut TuiPaintContext,
    ) {
        self.origin = Some(ctx.scene_point(origin));
        let Some(size) = self.size else {
            return;
        };
        let model = self.model.lock();
        let colors = model.colors();
        let alt = model.alt_screen();
        render_grid_handler(alt.grid_handler(), origin, size, surface, &colors);

        // Submit the hardware cursor if the alt-screen app is showing it. The
        // alt screen has no scrollback, but subtract history defensively so the
        // cursor maps to a visible (screen-relative) row.
        let cursor = if alt.is_mode_set(TermMode::SHOW_CURSOR) {
            let grid = alt.grid_handler();
            let point = grid.cursor_render_point();
            point.row.checked_sub(grid.history_size()).and_then(|row| {
                let col = u16::try_from(point.col).ok()?;
                let row = u16::try_from(row).ok()?;
                (col < size.width && row < size.height).then_some((col, row))
            })
        } else {
            None
        };
        drop(model);
        if let Some((col, row)) = cursor {
            let cursor_point = ctx.scene_point(origin.offset(i32::from(col), i32::from(row)));
            ctx.set_terminal_cursor(cursor_point);
        }
    }

    fn size(&self) -> Option<TuiSize> {
        self.size
    }

    fn origin(&self) -> Option<TuiScreenPoint> {
        self.origin
    }

    fn dispatch_event(
        &mut self,
        event: &TuiEvent,
        event_ctx: &mut TuiEventContext<'_>,
        _app: &AppContext,
    ) -> bool {
        // Forward the event to the app. Keys go through `to_pty_bytes`, which
        // layers the fallbacks a single-`KeyDown` frontend needs —
        // `Ctrl+<letter>` → C0, printable `chars`, and named control keys — on
        // top of the shared `to_escape_sequence` encoder in `warp_terminal`.
        // (ctrl-c never reaches here: the session view's interrupt handler
        // forwards it to the app.) Pointer events are translated to SGR mouse
        // reports when the app opted in.
        let bytes = {
            let model = self.model.lock();
            match event {
                TuiEvent::KeyDown {
                    keystroke,
                    chars,
                    details,
                    is_composing: false,
                } => KeystrokeWithDetails {
                    keystroke,
                    key_without_modifiers: details.key_without_modifiers.as_deref(),
                    chars: Some(chars.as_str()),
                }
                .to_pty_bytes(model.deref()),
                TuiEvent::KeyDown {
                    is_composing: true, ..
                } => None,
                _ => self.origin.zip(self.size).and_then(|(origin, size)| {
                    mouse_event_to_pty_bytes(
                        event,
                        TuiScreenRect::new(origin, size),
                        |mode| model.is_term_mode_set(mode),
                        model.deref(),
                    )
                }),
            }
        };
        let Some(bytes) = bytes else {
            return false;
        };
        event_ctx.dispatch_typed_action(TuiTerminalSessionAction::ForwardToPty(bytes));
        true
    }
}

#[cfg(test)]
#[path = "alt_screen_view_tests.rs"]
mod tests;
