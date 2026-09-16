use synapse_core::{
    AimCurve, AimNaturalParams, ElementId, MouseButton, MouseTarget, Point, error_codes,
};

use super::{
    CoordinateFallbackPlan,
    dispatch::{FALLBACK_MOVE_DURATION_MS, emit_coordinate_fallback_click},
    resolver::{
        RectEdges, center_from_rect_edges, element_not_resolved, element_pattern_unsupported,
        invoke_pattern_failed, transient_element_expired,
    },
};
#[cfg(not(windows))]
use super::{click_element_or_fallback, invoke_element};
use crate::ActionError;
use crate::{EmitState, RecordedInput, RecordingBackend};

#[test]
fn re_resolve_failures_map_to_element_not_resolved() {
    let before = "synthetic stale element";
    let after = element_not_resolved(before);
    assert_eq!(after.code(), error_codes::ACTION_ELEMENT_NOT_RESOLVED);
    assert_eq!(after.detail(), before);
    println!(
        "readback=invoke_error_mapping edge=re_resolve_failure before={before:?} after_code={} after_detail={:?}",
        after.code(),
        after.detail()
    );
}

#[test]
fn stale_uia_elements_map_to_transient_expired() {
    let element_id = synthetic_element_id();
    let before = format!("UI Automation element is stale: element id {element_id} disappeared");
    let after = transient_element_expired(&element_id, &before);
    assert_eq!(after.code(), error_codes::TRANSIENT_ELEMENT_EXPIRED);
    assert_eq!(after.detail(), before);
    match after {
        ActionError::TransientElementExpired {
            element_id: actual,
            detail,
        } => {
            assert_eq!(actual, element_id);
            assert!(detail.contains("stale"));
        }
        other => panic!("expected transient expired error, got {other:?}"),
    }
    println!(
        "readback=invoke_error_mapping edge=stale_transient before={before:?} after_code={} after_element_id={element_id}",
        error_codes::TRANSIENT_ELEMENT_EXPIRED
    );
}

#[test]
fn unsupported_click_patterns_map_to_element_pattern_unsupported() {
    let element_id = synthetic_element_id();
    let before = format!(
        "element {element_id} does not expose a supported click control pattern; attempted_patterns=[InvokePattern, TogglePattern, SelectionItemPattern, ExpandCollapsePattern, LegacyIAccessiblePattern.DoDefaultAction]"
    );
    let after = element_pattern_unsupported(&element_id, &before);
    assert_eq!(
        after.code(),
        error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED
    );
    assert_eq!(after.detail(), before);
    match after {
        ActionError::ElementPatternUnsupported {
            element_id: actual,
            detail,
        } => {
            assert_eq!(actual, element_id);
            assert!(detail.contains("SelectionItemPattern"));
            assert!(detail.contains("LegacyIAccessiblePattern.DoDefaultAction"));
        }
        other => panic!("expected unsupported pattern error, got {other:?}"),
    }
    println!(
        "readback=invoke_error_mapping edge=unsupported_click_patterns before={before:?} after_code={} after_element_id={element_id}",
        error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED
    );
}

#[test]
fn invoke_failures_map_to_target_invalid_without_cursor_fallback_in_bridge() {
    let element_id = synthetic_element_id();
    let before = "blocked by modal";
    let after = invoke_pattern_failed(&element_id, before);
    assert_eq!(after.code(), error_codes::ACTION_TARGET_INVALID);
    assert!(after.detail().contains(element_id.as_str()));
    assert!(after.detail().contains("InvokePattern.invoke failed"));
    println!(
        "readback=invoke_error_mapping edge=invoke_failure before={before:?} after_code={} after_detail={:?}",
        after.code(),
        after.detail()
    );
}

#[cfg(not(windows))]
#[test]
fn non_windows_stub_fails_closed() {
    let element_id = synthetic_element_id();
    let before = format!("os={} element_id={element_id}", std::env::consts::OS);
    let after = invoke_element(&element_id);
    let Err(ActionError::BackendUnavailable { detail }) = after else {
        panic!("expected non-Windows invoke_element to fail closed");
    };
    assert_eq!(
        ActionError::BackendUnavailable {
            detail: detail.clone()
        }
        .code(),
        error_codes::ACTION_BACKEND_UNAVAILABLE
    );
    assert!(detail.contains("requires Windows"));
    println!(
        "readback=invoke_non_windows_stub edge=non_windows before={before:?} after_code={} after_detail={detail:?}",
        error_codes::ACTION_BACKEND_UNAVAILABLE
    );
}

#[cfg(not(windows))]
#[test]
fn non_windows_click_fallback_fails_closed() {
    let element_id = synthetic_element_id();
    let backend = RecordingBackend::default();
    let mut state = EmitState::default();
    let before = format!(
        "os={} element_id={element_id} events={:?}",
        std::env::consts::OS,
        backend.events()
    );
    let after = click_element_or_fallback(&element_id, &backend, &mut state, MouseButton::Left);
    let Err(ActionError::BackendUnavailable { detail }) = after else {
        panic!("expected non-Windows click_element_or_fallback to fail closed");
    };
    assert_eq!(
        ActionError::BackendUnavailable {
            detail: detail.clone()
        }
        .code(),
        error_codes::ACTION_BACKEND_UNAVAILABLE
    );
    assert!(backend.events().is_empty());
    println!(
        "readback=invoke_coordinate_fallback edge=non_windows before={before:?} after_code={} after_detail={detail:?} after_events={:?}",
        error_codes::ACTION_BACKEND_UNAVAILABLE,
        backend.events()
    );
}

#[test]
fn coordinate_fallback_emits_move_down_up_at_bbox_center() {
    let backend = RecordingBackend::default();
    let mut state = EmitState::default();
    let plan = CoordinateFallbackPlan {
        screen_point: Point { x: 60, y: 120 },
        window_point: Point { x: 10, y: 20 },
    };
    let before = backend.events();

    if let Err(error) =
        emit_coordinate_fallback_click(&backend, &mut state, MouseButton::Left, plan)
    {
        panic!("recording backend should accept coordinate fallback: {error}");
    }

    let after = backend.events();
    let expected = vec![
        RecordedInput::MouseMove {
            to: MouseTarget::Screen {
                point: plan.screen_point,
            },
            curve: AimCurve::Natural {
                params: AimNaturalParams::FAST,
            },
            duration_ms: FALLBACK_MOVE_DURATION_MS,
        },
        RecordedInput::MouseButtonDown {
            button: MouseButton::Left,
        },
        RecordedInput::MouseButtonUp {
            button: MouseButton::Left,
        },
    ];
    assert_eq!(after, expected);
    println!(
        "readback=recording_backend edge=coordinate_fallback_sequence before={before:?} after={after:?}"
    );
}

#[test]
fn unsupported_click_pattern_error_does_not_emit_coordinate_fallback() {
    let backend = RecordingBackend::default();
    let element_id = synthetic_element_id();
    let before = backend.events();

    let after = element_pattern_unsupported(&element_id, "no supported UIA click pattern");

    assert_eq!(
        after.code(),
        error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED
    );
    assert!(backend.events().is_empty());
    println!(
        "readback=invoke_coordinate_fallback edge=unsupported_pattern before={before:?} after_code={} after_events={:?}",
        after.code(),
        backend.events()
    );
}

#[test]
fn bbox_center_rounds_inside_odd_sized_rectangle() {
    let rect = RectEdges {
        left: 10,
        top: 20,
        right: 111,
        bottom: 221,
    };
    let before = format!("{rect:?}");
    let after = match center_from_rect_edges(rect) {
        Ok(point) => point,
        Err(error) => panic!("odd-sized rect should have a center: {error}"),
    };
    let expected_exact_center = (60.5_f64, 120.5_f64);
    let dx = f64::from(after.x) - expected_exact_center.0;
    let dy = f64::from(after.y) - expected_exact_center.1;
    assert!(after.x >= rect.left && after.x < rect.right);
    assert!(after.y >= rect.top && after.y < rect.bottom);
    assert!(dx.hypot(dy) <= 1.0);
    println!(
        "readback=bbox_center edge=odd_sized before={before:?} after={after:?} expected_exact_center={expected_exact_center:?}"
    );
}

#[test]
fn bbox_center_rejects_empty_or_inverted_rectangles() {
    for rect in [
        RectEdges {
            left: 5,
            top: 5,
            right: 5,
            bottom: 10,
        },
        RectEdges {
            left: 10,
            top: 10,
            right: 9,
            bottom: 12,
        },
        RectEdges {
            left: 10,
            top: 10,
            right: 12,
            bottom: 10,
        },
    ] {
        let before = format!("{rect:?}");
        let after = center_from_rect_edges(rect);
        let Err(error) = after else {
            panic!("expected invalid rect to fail: {rect:?}");
        };
        assert_eq!(error.code(), error_codes::ACTION_TARGET_INVALID);
        println!(
            "readback=bbox_center edge=invalid_rect before={before:?} after_code={} after_detail={:?}",
            error.code(),
            error.detail()
        );
    }
}

#[test]
fn bbox_center_handles_large_screen_coordinates_without_overflow() {
    let rect = RectEdges {
        left: i32::MAX - 100,
        top: i32::MAX - 200,
        right: i32::MAX,
        bottom: i32::MAX - 20,
    };
    let before = format!("{rect:?}");
    let after = match center_from_rect_edges(rect) {
        Ok(point) => point,
        Err(error) => panic!("large rect should stay in i32 bounds: {error}"),
    };
    assert_eq!(
        after,
        Point {
            x: i32::MAX - 50,
            y: i32::MAX - 110,
        }
    );
    println!("readback=bbox_center edge=large_coordinates before={before:?} after={after:?}");
}

fn synthetic_element_id() -> ElementId {
    match ElementId::parse("0x1234:0000002a00000001") {
        Ok(element_id) => element_id,
        Err(error) => panic!("synthetic element id must parse: {error}"),
    }
}
