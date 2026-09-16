use std::sync::Arc;

use serde_json::json;
use synapse_action::{ActionBackend, ActionEmitter, ActionError, RecordedInput, RecordingBackend};
use synapse_core::{Backend, ElementId, MouseTarget, Point, error_codes};
use tokio_util::sync::CancellationToken;

use super::{
    ForegroundClickPolicy, act_click_with_handle, act_click_with_handle_and_lease,
    click_target_root_hwnd,
    schema::{
        ActClickParams, ActClickPointTarget, ActClickTarget, ClickVelocityProfile,
        default_click_backend, default_click_button, default_click_count,
        default_click_duration_ms, default_click_velocity_profile,
        default_coordinate_fallback_on_unsupported, default_use_invoke_pattern,
    },
};

#[test]
fn click_visual_verification_rejects_noncanonical_hwnd_before_window_lookup() {
    for invalid in [-1, 0, i64::from(u32::MAX) + 1, i64::MAX] {
        let params: ActClickParams = serde_json::from_value(json!({
            "target": {"x": 12, "y": 34},
            "verify_delta": true,
            "verify_target_window_hwnd": invalid,
        }))
        .expect("synthetic click params deserialize");
        let error = click_target_root_hwnd(&params)
            .expect_err("noncanonical HWND must fail before native liveness lookup");
        let data = error.data.as_ref().expect("structured HWND error data");
        assert_eq!(
            data.get("field").and_then(serde_json::Value::as_str),
            Some("verify_target_window_hwnd")
        );
        assert_eq!(
            data.get("actual_value").and_then(serde_json::Value::as_i64),
            Some(invalid)
        );
    }
}

#[tokio::test]
async fn coordinate_click_leaves_actor_held_state_empty() {
    let _serial = crate::test_support::lease_serial("act_click_coordinate_test_reset");
    let cancel = CancellationToken::new();
    let backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
    let (handle, snapshot_handle, join) =
        ActionEmitter::spawn_with_backend(cancel.clone(), backend);
    let before = match snapshot_handle.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(err) => panic!("before snapshot failed: {err}"),
    };
    println!(
        "readback=act_click_actor edge=coordinate before=held_buttons:{:?} held_keys:{:?}",
        before.held_buttons, before.held_keys
    );
    let response = match act_click_with_handle_and_lease(
        handle,
        None,
        ActClickParams {
            target: ActClickTarget::Point(ActClickPointTarget { x: 12, y: 34 }),
            button: default_click_button(),
            clicks: default_click_count(),
            modifiers: Vec::new(),
            velocity_profile: default_click_velocity_profile(),
            duration_ms: default_click_duration_ms(),
            hold_ms: super::schema::default_click_hold_ms(),
            backend: default_click_backend(),
            use_invoke_pattern: default_use_invoke_pattern(),
            coordinate_fallback_on_unsupported: default_coordinate_fallback_on_unsupported(),
            verify_delta: false,
            verify_target_window_hwnd: None,
            verify_timeout_ms: super::schema::default_verify_timeout_ms(),
            auto_wait: false,
            auto_wait_timeout_ms: crate::m2::default_auto_wait_timeout_ms(),
            deprecated_curve_alias_used: false,
        },
        ForegroundClickPolicy::allowed(Some("test-click-coordinate-session".to_owned())),
        crate::m2::OperatorPanicActionBoundary::arm("act_click", "test_direct_call")
            .expect("operator panic admission should be open for coordinate click test"),
    )
    .await
    {
        Ok(response) => response,
        Err(err) => panic!("act_click failed: {err}"),
    };
    let after = match snapshot_handle.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(err) => panic!("after snapshot failed: {err}"),
    };
    println!(
        "readback=act_click_actor edge=coordinate after=ok:{} backend_used:{} held_buttons:{:?} held_keys:{:?}",
        response.ok, response.backend_used, after.held_buttons, after.held_keys
    );
    assert!(response.ok);
    assert!(!response.used_invoke_pattern);
    assert_eq!(response.backend_used, "software");
    assert_eq!(response.press_hold_ms, 120);
    assert!(after.held_buttons.is_empty());
    assert!(after.held_keys.is_empty());
    cancel.cancel();
    let _final_snapshot = match join.await {
        Ok(snapshot) => snapshot,
        Err(err) => panic!("join failed: {err}"),
    };
}

#[tokio::test]
async fn element_click_rejects_non_mouse_element_transports_before_delivery() {
    for backend in [Backend::Hardware, Backend::Vigem] {
        let cancel = CancellationToken::new();
        let action_backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
        let (handle, _snapshot_handle, join) =
            ActionEmitter::spawn_with_backend(cancel.clone(), action_backend);
        let error = match act_click_with_handle(
            handle,
            None,
            ActClickParams {
                target: ActClickTarget::Element(super::schema::ActClickElementTarget {
                    element_id: synapse_core::ElementId::parse("0x1000:0000002a00000001")
                        .expect("synthetic element id must be valid"),
                }),
                button: default_click_button(),
                clicks: default_click_count(),
                modifiers: Vec::new(),
                velocity_profile: default_click_velocity_profile(),
                duration_ms: default_click_duration_ms(),
                hold_ms: super::schema::default_click_hold_ms(),
                backend,
                use_invoke_pattern: default_use_invoke_pattern(),
                coordinate_fallback_on_unsupported: default_coordinate_fallback_on_unsupported(),
                verify_delta: false,
                verify_target_window_hwnd: None,
                verify_timeout_ms: super::schema::default_verify_timeout_ms(),
                auto_wait: false,
                auto_wait_timeout_ms: crate::m2::default_auto_wait_timeout_ms(),
                deprecated_curve_alias_used: false,
            },
        )
        .await
        {
            Ok(response) => panic!("element click should reject backend {backend:?}: {response:?}"),
            Err(error) => error,
        };
        let code = error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(serde_json::Value::as_str);
        println!(
            "readback=act_click_element_backend edge=reject_explicit_backend backend={backend:?} code={code:?} message={}",
            error.message
        );
        assert_eq!(code, Some(error_codes::ACTION_BACKEND_UNAVAILABLE));
        cancel.cancel();
        let _ = join.await;
    }
}

#[test]
fn transient_element_expired_error_carries_reobserve_guidance() {
    let element_id =
        ElementId::parse("0x1000:0000002a00000001").expect("synthetic element id must be valid");
    let detail = format!(
        "UI Automation element is stale: element id {element_id} was not found under hwnd 0x1000"
    );
    let before = ActionError::TransientElementExpired {
        element_id: element_id.clone(),
        detail,
    };

    let after = super::action_error_to_mcp(&before);
    let data = after
        .data
        .as_ref()
        .expect("transient element error should carry structured data");
    println!(
        "readback=act_click_transient_expired edge=stale_toast before_code={} after_data={data}",
        before.code()
    );

    assert_eq!(
        data.get("code").and_then(serde_json::Value::as_str),
        Some(error_codes::TRANSIENT_ELEMENT_EXPIRED)
    );
    assert_eq!(
        data.get("detail_code").and_then(serde_json::Value::as_str),
        Some("UIA_ELEMENT_STALE_AFTER_OBSERVE")
    );
    assert_eq!(
        data.get("fallback_attempted")
            .and_then(serde_json::Value::as_bool),
        Some(false)
    );
    assert_eq!(
        data.get("element_id").and_then(serde_json::Value::as_str),
        Some(element_id.as_str())
    );
    assert_eq!(
        data.get("root_hwnd").and_then(serde_json::Value::as_i64),
        Some(0x1000)
    );
    assert!(
        data.get("recommended_pattern")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value.contains("observe") && value.contains("fresh element_id"))
    );
}

#[test]
fn element_pattern_unsupported_error_carries_attempted_patterns_without_fallback() {
    let element_id =
        ElementId::parse("0x1000:0000002a00000001").expect("synthetic element id must be valid");
    let detail = format!(
        "element {element_id} does not expose a supported click control pattern; attempted_patterns=[InvokePattern, TogglePattern, SelectionItemPattern, ExpandCollapsePattern, LegacyIAccessiblePattern.DoDefaultAction]"
    );
    let before = ActionError::ElementPatternUnsupported {
        element_id: element_id.clone(),
        detail,
    };

    let after = super::action_error_to_mcp(&before);
    let data = after
        .data
        .as_ref()
        .expect("unsupported pattern error should carry structured data");
    println!(
        "readback=act_click_pattern_unsupported edge=no_pattern before_code={} after_data={data}",
        before.code()
    );

    assert_eq!(
        data.get("code").and_then(serde_json::Value::as_str),
        Some(error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED)
    );
    assert_eq!(
        data.get("detail_code").and_then(serde_json::Value::as_str),
        Some("UIA_CONTROL_PATTERN_UNSUPPORTED")
    );
    assert_eq!(
        data.get("fallback_attempted")
            .and_then(serde_json::Value::as_bool),
        Some(false)
    );
    assert_eq!(
        data.get("element_id").and_then(serde_json::Value::as_str),
        Some(element_id.as_str())
    );
    assert_eq!(
        data.get("root_hwnd").and_then(serde_json::Value::as_i64),
        Some(0x1000)
    );
    let attempted_patterns = data
        .get("attempted_patterns")
        .and_then(serde_json::Value::as_array)
        .expect("attempted_patterns should be an array");
    assert_eq!(attempted_patterns.len(), 5);
    assert!(attempted_patterns.iter().any(|value| {
        value
            .as_str()
            .is_some_and(|pattern| pattern == "SelectionItemPattern")
    }));
    assert!(attempted_patterns.iter().any(|value| {
        value
            .as_str()
            .is_some_and(|pattern| pattern == "LegacyIAccessiblePattern.DoDefaultAction")
    }));
    assert_eq!(
        data.get("router_escalation_required")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
}

#[tokio::test]
async fn browser_ocr_element_click_uses_coordinate_route_even_when_invoke_default_is_enabled() {
    let cancel = CancellationToken::new();
    let action_backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
    let (handle, _snapshot_handle, join) =
        ActionEmitter::spawn_with_backend(cancel.clone(), action_backend);
    let recording = Arc::new(RecordingBackend::new());
    let before = recording.events();
    let element_id =
        synapse_core::ElementId::parse("0x1000:0c0c010000000c00000022000000500000001400000007")
            .expect("synthetic OCR element id must be valid");
    println!(
        "readback=act_click_ocr_element_route edge=before event_count:{} element_id={element_id}",
        before.len()
    );

    let response = match act_click_with_handle(
        handle,
        Some(Arc::clone(&recording)),
        ActClickParams {
            target: ActClickTarget::Element(super::schema::ActClickElementTarget { element_id }),
            button: default_click_button(),
            clicks: default_click_count(),
            modifiers: Vec::new(),
            velocity_profile: default_click_velocity_profile(),
            duration_ms: default_click_duration_ms(),
            hold_ms: super::schema::default_click_hold_ms(),
            backend: default_click_backend(),
            use_invoke_pattern: default_use_invoke_pattern(),
            coordinate_fallback_on_unsupported: default_coordinate_fallback_on_unsupported(),
            verify_delta: false,
            verify_target_window_hwnd: None,
            verify_timeout_ms: super::schema::default_verify_timeout_ms(),
            auto_wait: false,
            auto_wait_timeout_ms: crate::m2::default_auto_wait_timeout_ms(),
            deprecated_curve_alias_used: false,
        },
    )
    .await
    {
        Ok(response) => response,
        Err(error) => panic!("OCR element click should use coordinate route: {error}"),
    };

    let after = recording.events();
    println!(
        "readback=act_click_ocr_element_route edge=after ok:{} used_invoke:{} backend_used:{} events={after:?}",
        response.ok, response.used_invoke_pattern, response.backend_used
    );
    assert!(response.ok);
    assert!(!response.used_invoke_pattern);
    assert_eq!(response.backend_used, "software");
    assert!(matches!(
        after.first(),
        Some(RecordedInput::MouseMove {
            to: MouseTarget::Screen {
                point: Point { x: 52, y: 44 }
            },
            ..
        })
    ));
    cancel.cancel();
    let _ = join.await;
}

#[tokio::test]
async fn browser_ocr_element_click_rejects_empty_ocr_bbox_before_uia_resolution() {
    let cancel = CancellationToken::new();
    let action_backend: Arc<dyn ActionBackend> = Arc::new(RecordingBackend::new());
    let (handle, _snapshot_handle, join) =
        ActionEmitter::spawn_with_backend(cancel.clone(), action_backend);
    let element_id =
        synapse_core::ElementId::parse("0x1000:0c0c010000000c00000022000000000000001400000007")
            .expect("synthetic OCR element id must be valid");
    println!("readback=act_click_ocr_element_route edge=empty_bbox_before element_id={element_id}");

    let error = match act_click_with_handle(
        handle,
        Some(Arc::new(RecordingBackend::new())),
        ActClickParams {
            target: ActClickTarget::Element(super::schema::ActClickElementTarget { element_id }),
            button: default_click_button(),
            clicks: default_click_count(),
            modifiers: Vec::new(),
            velocity_profile: default_click_velocity_profile(),
            duration_ms: default_click_duration_ms(),
            hold_ms: super::schema::default_click_hold_ms(),
            backend: default_click_backend(),
            use_invoke_pattern: default_use_invoke_pattern(),
            coordinate_fallback_on_unsupported: default_coordinate_fallback_on_unsupported(),
            verify_delta: false,
            verify_target_window_hwnd: None,
            verify_timeout_ms: super::schema::default_verify_timeout_ms(),
            auto_wait: false,
            auto_wait_timeout_ms: crate::m2::default_auto_wait_timeout_ms(),
            deprecated_curve_alias_used: false,
        },
    )
    .await
    {
        Ok(response) => panic!("empty OCR bbox should fail closed: {response:?}"),
        Err(error) => error,
    };
    let code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(serde_json::Value::as_str);
    println!(
        "readback=act_click_ocr_element_route edge=empty_bbox_after code={code:?} message={}",
        error.message
    );
    assert_eq!(code, Some(error_codes::ACTION_TARGET_INVALID));
    assert!(error.message.contains("browser OCR element"));
    cancel.cancel();
    let _ = join.await;
}

#[test]
fn click_velocity_profile_accepts_hidden_legacy_curve_alias() {
    let new_name: ActClickParams = serde_json::from_value(json!({
        "target": {"x": 10, "y": 20},
        "velocity_profile": "linear"
    }))
    .expect("velocity_profile should parse");
    assert_eq!(new_name.velocity_profile, ClickVelocityProfile::Linear);
    assert!(!new_name.auto_wait);
    assert_eq!(
        new_name.auto_wait_timeout_ms,
        crate::m2::default_auto_wait_timeout_ms()
    );
    assert!(!new_name.deprecated_curve_alias_used);

    let old_alias: ActClickParams = serde_json::from_value(json!({
        "target": {"x": 10, "y": 20},
        "curve": "ease_in_out"
    }))
    .expect("legacy curve alias should parse");
    assert_eq!(old_alias.velocity_profile, ClickVelocityProfile::EaseInOut);
    assert!(old_alias.deprecated_curve_alias_used);

    let conflict = serde_json::from_value::<ActClickParams>(json!({
        "target": {"x": 10, "y": 20},
        "velocity_profile": "linear",
        "curve": "natural"
    }))
    .expect_err("velocity_profile and curve together must fail closed");
    assert!(
        conflict
            .to_string()
            .contains("velocity_profile or deprecated curve"),
        "{conflict}"
    );
}
