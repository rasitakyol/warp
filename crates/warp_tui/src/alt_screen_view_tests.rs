use std::sync::Arc;

use parking_lot::FairMutex;
use warp::tui_export::{TermMode, TerminalModel, ToEscapeSequence as _};
use warpui::EntityIdMap;
use warpui_core::elements::tui::{
    TuiConstraint, TuiElement, TuiEvent, TuiLayoutContext, TuiPoint, TuiScreenPoint, TuiScreenRect,
    TuiSize, TuiZIndex,
};
use warpui_core::event::ModifiersState;
use warpui_core::App;

use super::{mouse_state_for_event, AltScreenElement};

const SGR_CLICK: TermMode = TermMode::SGR_MOUSE.union(TermMode::MOUSE_REPORT_CLICK);
const SGR_DRAG: TermMode = TermMode::SGR_MOUSE.union(TermMode::MOUSE_DRAG);
const SGR_MOTION: TermMode = TermMode::SGR_MOUSE.union(TermMode::MOUSE_MOTION);

/// Builds retained screen bounds anchored at `(x, y)`.
fn bounds(x: i32, y: i32, width: u16, height: u16) -> TuiScreenRect {
    TuiScreenRect::new(
        TuiScreenPoint::new(x, y, TuiZIndex::Normal(0)),
        TuiSize::new(width, height),
    )
}

/// Encodes `event` using the production TUI mouse-event adapter.
fn mouse_bytes(event: &TuiEvent, area: TuiScreenRect, modes: TermMode) -> Option<Vec<u8>> {
    let state = mouse_state_for_event(event, area, |mode| modes.contains(mode))?;
    state.to_escape_sequence(&TerminalModel::mock(None, None))
}

#[test]
fn layout_fills_the_allocated_pane() {
    App::test((), |app| async move {
        app.read(|app| {
            let model = Arc::new(FairMutex::new(TerminalModel::mock(None, None)));
            let mut element = AltScreenElement::new(model);
            let expected_size = TuiSize::new(42, 8);
            let mut rendered_views = EntityIdMap::default();
            let mut layout_ctx = TuiLayoutContext {
                rendered_views: &mut rendered_views,
            };

            let size = element.layout(TuiConstraint::loose(expected_size), &mut layout_ctx, app);
            assert_eq!(size, expected_size);
            // The laid-out size is retained for the size wrapper and mouse
            // hit-testing to read back.
            assert_eq!(element.size(), Some(expected_size));
        });
    });
}

#[test]
fn sgr_mouse_events_use_area_relative_coordinates() {
    let area = bounds(10, 5, 20, 10);
    let position = TuiPoint::new(12, 6);
    let modifiers = ModifiersState::default();
    let cases = [
        (
            TuiEvent::LeftMouseDown {
                position,
                modifiers,
                click_count: 1,
                is_first_mouse: false,
            },
            SGR_CLICK,
            b"\x1b[<0;3;2M".as_slice(),
        ),
        (
            TuiEvent::RightMouseDown {
                position,
                modifiers,
                click_count: 1,
            },
            SGR_CLICK,
            b"\x1b[<2;3;2M".as_slice(),
        ),
        (
            TuiEvent::LeftMouseUp {
                position,
                modifiers,
            },
            SGR_CLICK,
            b"\x1b[<0;3;2m".as_slice(),
        ),
        (
            TuiEvent::LeftMouseDragged {
                position,
                modifiers,
            },
            SGR_DRAG,
            b"\x1b[<32;3;2M".as_slice(),
        ),
        (
            TuiEvent::MouseMoved {
                position,
                modifiers,
                is_synthetic: false,
            },
            SGR_MOTION,
            b"\x1b[<35;3;2M".as_slice(),
        ),
        (
            TuiEvent::ScrollWheel {
                position,
                delta: (0, 1),
                precise: false,
                modifiers,
            },
            SGR_CLICK,
            b"\x1b[<64;3;2M".as_slice(),
        ),
        (
            TuiEvent::ScrollWheel {
                position,
                delta: (0, -1),
                precise: false,
                modifiers,
            },
            SGR_CLICK,
            b"\x1b[<65;3;2M".as_slice(),
        ),
    ];

    for (event, modes, expected) in cases {
        assert_eq!(mouse_bytes(&event, area, modes).as_deref(), Some(expected));
    }
}

#[test]
fn mouse_events_require_the_requested_reporting_mode() {
    let area = bounds(0, 0, 10, 10);
    let position = TuiPoint::new(2, 3);
    let modifiers = ModifiersState::default();
    let left_down = TuiEvent::LeftMouseDown {
        position,
        modifiers,
        click_count: 1,
        is_first_mouse: false,
    };
    let left_dragged = TuiEvent::LeftMouseDragged {
        position,
        modifiers,
    };
    let moved = TuiEvent::MouseMoved {
        position,
        modifiers,
        is_synthetic: false,
    };

    assert!(mouse_bytes(&left_down, area, TermMode::MOUSE_REPORT_CLICK).is_none());
    assert!(mouse_bytes(&left_down, area, TermMode::SGR_MOUSE).is_none());
    assert!(mouse_bytes(&left_down, area, SGR_DRAG).is_some());
    assert!(mouse_bytes(&left_dragged, area, SGR_CLICK).is_none());
    assert!(mouse_bytes(&left_dragged, area, SGR_DRAG).is_some());
    assert!(mouse_bytes(&moved, area, SGR_DRAG).is_none());
    assert!(mouse_bytes(&moved, area, SGR_MOTION).is_some());
}

#[test]
fn unsupported_or_intercepted_mouse_events_are_not_forwarded() {
    let area = bounds(5, 5, 10, 10);
    let modifiers = ModifiersState::default();

    let outside = TuiEvent::LeftMouseDown {
        position: TuiPoint::new(4, 5),
        modifiers,
        click_count: 1,
        is_first_mouse: false,
    };
    let shifted = TuiEvent::LeftMouseDown {
        position: TuiPoint::new(6, 6),
        modifiers: ModifiersState {
            shift: true,
            ..Default::default()
        },
        click_count: 1,
        is_first_mouse: false,
    };
    let middle = TuiEvent::MiddleMouseDown {
        position: TuiPoint::new(6, 6),
        modifiers,
        click_count: 1,
    };
    let synthetic_move = TuiEvent::MouseMoved {
        position: TuiPoint::new(6, 6),
        modifiers,
        is_synthetic: true,
    };
    let horizontal_scroll = TuiEvent::ScrollWheel {
        position: TuiPoint::new(6, 6),
        delta: (1, 0),
        precise: false,
        modifiers,
    };

    assert!(mouse_bytes(&outside, area, SGR_CLICK).is_none());
    assert!(mouse_bytes(&shifted, area, SGR_CLICK).is_none());
    assert!(mouse_bytes(&middle, area, SGR_CLICK).is_none());
    assert!(mouse_bytes(&synthetic_move, area, SGR_MOTION).is_none());
    assert!(mouse_bytes(&horizontal_scroll, area, SGR_CLICK).is_none());
}
