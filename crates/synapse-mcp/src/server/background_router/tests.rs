//! Unit tests for background_router (split out of the module body per #1555).

use super::*;
use rmcp::schemars::schema_for;
use std::collections::BTreeSet;

fn read_action() -> TargetActParams {
    serde_json::from_value(json!({ "verb": "read" }))
        .expect("synthetic read action should deserialize")
}

fn act_error_field(error: &ErrorData, field: &str) -> Option<String> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get(field))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn act_error_u64(error: &ErrorData, field: &str) -> Option<u64> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get(field))
        .and_then(Value::as_u64)
}

fn synthetic_foreground_context() -> synapse_core::ForegroundContext {
    synapse_core::ForegroundContext {
        hwnd: 0x2000,
        pid: 42,
        process_name: "synthetic-editor.exe".to_owned(),
        process_path: "C:\\synthetic\\synthetic-editor.exe".to_owned(),
        window_title: "Synthetic Editor".to_owned(),
        window_bounds: Rect {
            x: 0,
            y: 0,
            w: 800,
            h: 600,
        },
        monitor_index: 0,
        dpi_scale: 1.0,
        profile_id: None,
        steam_appid: None,
        is_fullscreen: false,
        is_dwm_composed: true,
    }
}

fn sanitized_tool_input_schema(tool_name: &str) -> Value {
    let tools = crate::server::schema_sanitize::sanitize_tools(
        crate::server::SynapseService::tool_router().list_all(),
    );
    let tool = tools
        .iter()
        .find(|tool| tool.name.as_ref() == tool_name)
        .unwrap_or_else(|| panic!("{tool_name} tool missing"));
    Value::Object((*tool.input_schema).clone())
}

fn act_schema_variant<'a>(schema: &'a Value, operation: &str) -> &'a Value {
    schema["oneOf"]
        .as_array()
        .unwrap_or_else(|| panic!("act schema oneOf missing"))
        .iter()
        .find(|variant| variant["properties"]["operation"]["const"] == operation)
        .unwrap_or_else(|| panic!("act schema operation={operation} variant missing"))
}

fn schema_property_names(schema: &Value) -> BTreeSet<String> {
    schema["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("schema properties missing"))
        .keys()
        .cloned()
        .collect()
}

#[test]
fn act_facade_rejects_unknown_operation_enum() {
    let error = serde_json::from_value::<ActParams>(json!({
        "operation": "teleport",
        "action": { "verb": "read" }
    }))
    .expect_err("unknown act operation must fail schema deserialization");

    assert!(
        error.to_string().contains("unknown variant"),
        "unexpected act operation error: {error}"
    );
}

#[test]
fn act_facade_invoke_rejects_foreground_fields() {
    let params = ActParams {
        operation: ActOperation::Invoke,
        action: Some(read_action()),
        reason: Some("needs hardware foreground".to_owned()),
        ttl_ms: None,
    };

    let error =
        validate_act_invoke_params(&params).expect_err("invoke must reject foreground-only reason");

    assert_eq!(
        act_error_field(&error, "code").as_deref(),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        act_error_field(&error, "operation").as_deref(),
        Some("invoke")
    );
    assert_eq!(
        act_error_field(&error, "source_id").as_deref(),
        Some("reason")
    );
}

#[test]
fn act_facade_foreground_requires_non_empty_reason() {
    let params = ActParams {
        operation: ActOperation::Foreground,
        action: Some(read_action()),
        reason: Some("   ".to_owned()),
        ttl_ms: Some(30_000),
    };

    let error =
        validate_act_foreground_params(&params).expect_err("foreground must reject blank reason");

    assert_eq!(
        act_error_field(&error, "code").as_deref(),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        act_error_field(&error, "operation").as_deref(),
        Some("foreground")
    );
    assert_eq!(
        act_error_field(&error, "source_id").as_deref(),
        Some("reason")
    );
}

#[test]
fn act_facade_foreground_rejects_out_of_range_ttl() {
    let params = ActParams {
        operation: ActOperation::Foreground,
        action: Some(read_action()),
        reason: Some("needs audited hardware foreground".to_owned()),
        ttl_ms: Some(30_001),
    };

    let error = validate_act_foreground_params(&params)
        .expect_err("foreground must reject above-max ttl_ms");

    assert_eq!(
        act_error_field(&error, "code").as_deref(),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        act_error_field(&error, "detail_code").as_deref(),
        Some("LEASE_TTL_OUT_OF_RANGE")
    );
    assert_eq!(
        act_error_field(&error, "source_id").as_deref(),
        Some("ttl_ms")
    );
    assert_eq!(act_error_u64(&error, "ttl_ms"), Some(30_001));
    assert_eq!(
        act_error_u64(&error, "min_ttl_ms"),
        Some(synapse_action::MIN_LEASE_TTL_MS)
    );
    assert_eq!(
        act_error_u64(&error, "max_ttl_ms"),
        Some(synapse_action::MAX_LEASE_TTL_MS)
    );
}

#[test]
fn act_facade_lease_acquire_rejects_out_of_range_ttl() {
    let params = ActParams {
        operation: ActOperation::LeaseAcquire,
        action: None,
        reason: None,
        ttl_ms: Some(99),
    };

    let error = validate_act_lease_acquire_params(&params)
        .expect_err("lease_acquire must reject below-min ttl_ms");

    assert_eq!(
        act_error_field(&error, "code").as_deref(),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        act_error_field(&error, "detail_code").as_deref(),
        Some("LEASE_TTL_OUT_OF_RANGE")
    );
    assert_eq!(
        act_error_field(&error, "tool").as_deref(),
        Some("act operation=lease_acquire")
    );
    assert_eq!(act_error_u64(&error, "ttl_ms"), Some(99));
}

#[test]
fn act_facade_operation_schema_is_closed_enum() {
    let schema = sanitized_tool_input_schema("act");
    let operation_schema = schema
        .pointer("/properties/operation")
        .unwrap_or_else(|| panic!("act schema must include operation: {schema}"));
    let enum_schema = operation_schema
        .pointer("/enum")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("ActOperation enum schema missing: {operation_schema}"));
    let enum_values = enum_schema
        .iter()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    assert!(
        enum_values.contains("invoke")
            && enum_values.contains("foreground")
            && enum_values.contains("lease_acquire")
            && enum_values.contains("lease_status")
            && enum_values.contains("lease_release"),
        "act operation schema must enumerate invoke/foreground/lease operations: {operation_schema}"
    );
}

#[test]
fn act_facade_public_schema_is_operation_specific() {
    let schema = sanitized_tool_input_schema("act");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], Value::Bool(false));
    assert_eq!(
        schema["properties"]["ttl_ms"]["minimum"],
        synapse_action::MIN_LEASE_TTL_MS
    );
    assert_eq!(
        schema["properties"]["ttl_ms"]["maximum"],
        synapse_action::MAX_LEASE_TTL_MS
    );

    let variants = schema["oneOf"]
        .as_array()
        .expect("act schema oneOf present");
    assert_eq!(variants.len(), 5);
    for variant in variants {
        assert_eq!(variant["type"], "object");
        assert_eq!(variant["additionalProperties"], Value::Bool(false));
    }

    let invoke_fields = schema_property_names(act_schema_variant(&schema, "invoke"));
    assert_eq!(
        invoke_fields,
        BTreeSet::from(["action".to_owned(), "operation".to_owned()])
    );

    let foreground = act_schema_variant(&schema, "foreground");
    let foreground_fields = schema_property_names(foreground);
    assert_eq!(
        foreground_fields,
        BTreeSet::from([
            "action".to_owned(),
            "operation".to_owned(),
            "reason".to_owned(),
            "ttl_ms".to_owned()
        ])
    );
    assert_eq!(foreground["properties"]["reason"]["type"], "string");
    assert_eq!(foreground["properties"]["reason"]["minLength"], 1);

    let lease_acquire_fields = schema_property_names(act_schema_variant(&schema, "lease_acquire"));
    assert_eq!(
        lease_acquire_fields,
        BTreeSet::from(["operation".to_owned(), "ttl_ms".to_owned()])
    );
    assert!(!lease_acquire_fields.contains("action"));
    assert!(!lease_acquire_fields.contains("reason"));

    let lease_status_fields = schema_property_names(act_schema_variant(&schema, "lease_status"));
    assert_eq!(
        lease_status_fields,
        BTreeSet::from(["operation".to_owned()])
    );

    let lease_release_fields = schema_property_names(act_schema_variant(&schema, "lease_release"));
    assert_eq!(
        lease_release_fields,
        BTreeSet::from(["operation".to_owned()])
    );
}

#[test]
fn act_facade_invoke_requires_action() {
    let params = ActParams {
        operation: ActOperation::Invoke,
        action: None,
        reason: None,
        ttl_ms: None,
    };

    let error =
        validate_act_invoke_params(&params).expect_err("invoke must require action payload");

    assert_eq!(
        act_error_field(&error, "code").as_deref(),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        act_error_field(&error, "operation").as_deref(),
        Some("invoke")
    );
    assert_eq!(
        act_error_field(&error, "source_id").as_deref(),
        Some("action")
    );
}

#[test]
fn act_facade_lease_status_rejects_action_and_ttl() {
    let with_action = ActParams {
        operation: ActOperation::LeaseStatus,
        action: Some(read_action()),
        reason: None,
        ttl_ms: None,
    };
    let action_error = validate_act_lease_read_params(&with_action, ActOperation::LeaseStatus)
        .expect_err("lease_status must not accept target actions");
    assert_eq!(
        act_error_field(&action_error, "source_id").as_deref(),
        Some("action")
    );

    let with_ttl = ActParams {
        operation: ActOperation::LeaseStatus,
        action: None,
        reason: None,
        ttl_ms: Some(1_000),
    };
    let ttl_error = validate_act_lease_read_params(&with_ttl, ActOperation::LeaseStatus)
        .expect_err("lease_status must not accept ttl_ms");
    assert_eq!(
        act_error_field(&ttl_error, "source_id").as_deref(),
        Some("ttl_ms")
    );
}

#[test]
fn target_act_verb_click_deserializes() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "click",
        "element_id": "0x2a:0000000000000001",
        "clicks": 2
    }))
    .expect("click params should deserialize");

    assert_eq!(params.verb.as_str(), "click");
    assert_eq!(params.clicks, Some(2));
}

#[test]
fn target_act_set_field_accepts_selector() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "set_field",
        "selector": "input[name=\"q\"]",
        "text": "hello"
    }))
    .expect("set_field selector params should deserialize");

    assert_eq!(params.verb.as_str(), "set_field");
    assert_eq!(params.selector.as_deref(), Some("input[name=\"q\"]"));
    assert_eq!(params.text.as_deref(), Some("hello"));
    assert!(params.element_id.is_none());
}

#[test]
fn target_act_set_field_accepts_native_locator() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "set_field",
        "role": "document",
        "name": "Message Body",
        "automation_id": "compose-body",
        "text": "hello"
    }))
    .expect("set_field native locator params should deserialize");
    let locator = target_act_set_field_locator(&params).expect("role/name/automation_id locator");

    assert_eq!(params.verb.as_str(), "set_field");
    assert_eq!(locator.role.as_deref(), Some("document"));
    assert_eq!(locator.name.as_deref(), Some("Message Body"));
    assert_eq!(locator.automation_id.as_deref(), Some("compose-body"));
    assert!(locator.name_substring.is_none());
}

#[test]
fn target_act_set_field_bridge_element_id_routes_to_browser_bridge() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "set_field",
        "element_id": "chrome-tab:589708698:frame:0:path:0.1.1",
        "text": "hello"
    }))
    .expect("set_field bridge element_id params should deserialize");

    match target_act_set_field_target(&params).expect("bridge element_id routes") {
        TargetActSetFieldTarget::Browser {
            selector,
            element_id,
        } => {
            assert!(selector.is_none());
            assert_eq!(
                element_id.as_deref(),
                Some("chrome-tab:589708698:frame:0:path:0.1.1")
            );
        }
        TargetActSetFieldTarget::Native { .. } => {
            panic!("bridge element_id must not route to native/UIA")
        }
    }
}

#[test]
fn target_act_set_field_native_element_id_routes_to_native_text() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "set_field",
        "element_id": "0x2a:0000000000000001",
        "text": "hello"
    }))
    .expect("set_field native element_id params should deserialize");

    match target_act_set_field_target(&params).expect("native element_id routes") {
        TargetActSetFieldTarget::Native {
            element_id,
            locator,
        } => {
            assert_eq!(
                element_id.as_ref().map(ElementId::as_str),
                Some("0x2a:0000000000000001")
            );
            assert!(locator.is_none());
        }
        TargetActSetFieldTarget::Browser { .. } => {
            panic!("native element_id must not route to browser bridge")
        }
    }
}

#[test]
fn target_act_set_field_plain_dom_element_id_routes_to_selector() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "set_field",
        "element_id": "compose-body",
        "text": "hello"
    }))
    .expect("set_field plain DOM id params should deserialize");

    match target_act_set_field_target(&params).expect("plain DOM id routes") {
        TargetActSetFieldTarget::Browser {
            selector,
            element_id,
        } => {
            assert_eq!(selector.as_deref(), Some("[id=\"compose-body\"]"));
            assert!(element_id.is_none());
        }
        TargetActSetFieldTarget::Native { .. } => {
            panic!("plain DOM id must not route to native/UIA")
        }
    }
}

#[test]
fn target_act_set_field_rejects_mixed_locators() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "set_field",
        "selector": "textarea",
        "element_id": "chrome-tab:589708698:frame:0:path:0.1.1",
        "text": "hello"
    }))
    .expect("set_field mixed params should deserialize");

    let error = target_act_set_field_target(&params).expect_err("mixed locators must fail");
    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert!(error.message.contains("exactly one"));
}

#[test]
fn target_act_verb_schema_is_forward_compatible_string() {
    let schema = serde_json::to_value(schema_for!(TargetActParams))
        .unwrap_or_else(|error| panic!("target_act params schema should serialize: {error}"));
    let verb_schema = schema
        .pointer("/properties/verb")
        .unwrap_or_else(|| panic!("target_act schema must include verb: {schema}"));

    assert!(
        verb_schema
            .pointer("/type")
            .and_then(Value::as_str)
            .is_some_and(|value| value == "string"),
        "target_act verb schema must be an open string: {verb_schema}"
    );
    assert!(
        verb_schema.pointer("/enum").is_none(),
        "target_act verb schema must not be a closed enum: {verb_schema}"
    );
}

#[test]
fn target_act_unknown_verb_is_runtime_validation_error() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "future_dashboard_action"
    }))
    .expect("future target_act verb should deserialize so clients do not stale on schema");
    let error = target_act_unknown_verb_error(params.verb.as_str());

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert!(
        error.message.contains("future_dashboard_action"),
        "unknown verb error should name the rejected verb: {}",
        error.message
    );
}

#[test]
fn target_act_focus_window_is_forward_compatible_verb() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "focus_window"
    }))
    .expect("focus_window should use the existing open-string target_act schema");

    assert_eq!(params.verb.as_str(), "focus_window");
    assert!(params.path.is_none());
    assert!(params.element_id.is_none());
}

#[test]
fn target_act_save_accepts_file_source_of_truth_params() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "save",
        "path": "C:\\Temp\\issue1275-save.txt",
        "text": "Issue1275 expected persisted text",
        "wait_timeout_ms": 750
    }))
    .expect("save should use the existing open-string target_act schema");

    assert_eq!(params.verb.as_str(), "save");
    assert_eq!(params.path.as_deref(), Some("C:\\Temp\\issue1275-save.txt"));
    assert_eq!(
        params.text.as_deref(),
        Some("Issue1275 expected persisted text")
    );
    assert_eq!(params.wait_timeout_ms, Some(750));
    target_act_validate_save_params(&params).expect("save params should validate");
}

#[test]
fn target_act_save_rejects_locator_command_and_coordinate_mixes() {
    for params in [
        json!({
            "verb": "save",
            "path": "C:\\Temp\\issue1275-save.txt",
            "selector": "#document"
        }),
        json!({
            "verb": "save",
            "path": "C:\\Temp\\issue1275-save.txt",
            "command": "powershell.exe"
        }),
        json!({
            "verb": "save",
            "path": "C:\\Temp\\issue1275-save.txt",
            "x": 12,
            "y": 34
        }),
    ] {
        let params: TargetActParams =
            serde_json::from_value(params).expect("synthetic save params deserialize");
        let error = target_act_validate_save_params(&params)
            .expect_err("save must reject unrelated action parameters");

        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }
}

#[test]
fn target_act_save_timeout_is_bounded() {
    assert_eq!(
        target_act_save_verify_timeout(None).expect("default timeout should validate"),
        DEFAULT_TARGET_ACT_SAVE_TIMEOUT_MS
    );
    assert_eq!(
        target_act_save_verify_timeout(Some(50)).expect("lower bound should validate"),
        50
    );
    assert_eq!(
        target_act_save_verify_timeout(Some(10_000)).expect("upper bound should validate"),
        10_000
    );

    for value in [49, 10_001] {
        let error = target_act_save_verify_timeout(Some(value))
            .expect_err("out-of-range save timeout must fail closed");
        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }
}

#[test]
fn target_act_save_matches_notepad_title_to_file_source_of_truth() {
    let path = Path::new("C:\\Temp\\issue1275-save.txt");

    assert!(target_act_notepad_title_matches_path(
        "issue1275-save.txt - Notepad",
        path
    ));
    assert!(target_act_notepad_title_matches_path(
        "*issue1275-save.txt - Notepad",
        path
    ));
    assert!(!target_act_notepad_title_matches_path(
        "other.txt - Notepad",
        path
    ));
    assert!(!target_act_notepad_title_matches_path(
        "issue1275-save.txt - WordPad",
        path
    ));
}

#[test]
fn target_act_save_satisfied_requires_expected_bytes_or_file_delta() {
    let before = target_act_test_snapshot(b"old");
    let same = target_act_test_snapshot(b"old");
    let expected = target_act_test_snapshot(b"expected");
    let changed = target_act_test_snapshot(b"changed");

    assert!(
        target_act_save_satisfied(&before, &expected, Some("expected")),
        "expected text should accept exact file bytes even if delta semantics are separate"
    );
    assert!(
        !target_act_save_satisfied(&before, &changed, Some("expected")),
        "wrong file bytes must not satisfy an expected-text save"
    );
    assert!(
        !target_act_save_satisfied(&before, &same, None),
        "without expected text, unchanged file bytes are not enough"
    );
    assert!(
        target_act_save_satisfied(&before, &changed, None),
        "without expected text, any file-byte delta verifies the save side effect"
    );
}

#[test]
fn target_act_cleanup_notepad_tabs_accepts_existing_schema_fields() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "cleanup_notepad_tabs",
        "path": "C:\\Temp\\issue1276-keep.txt",
        "value": "discard_modified",
        "wait_timeout_ms": 750
    }))
    .expect("cleanup_notepad_tabs params should deserialize through the open target_act schema");

    assert_eq!(params.verb.as_str(), "cleanup_notepad_tabs");
    assert_eq!(params.path.as_deref(), Some("C:\\Temp\\issue1276-keep.txt"));
    assert_eq!(params.value.as_deref(), Some("discard_modified"));
    assert_eq!(params.wait_timeout_ms, Some(750));
    target_act_validate_cleanup_notepad_tabs_params(&params)
        .expect("cleanup_notepad_tabs should accept path/value/timeout only");
    assert_eq!(
        target_act_modified_stale_policy(params.value.as_deref())
            .expect("discard policy should validate"),
        TargetActModifiedStalePolicy::DiscardModified
    );
}

#[test]
fn target_act_cleanup_notepad_tabs_rejects_unrelated_action_params() {
    for params in [
        json!({
            "verb": "cleanup_notepad_tabs",
            "path": "C:\\Temp\\issue1276-keep.txt",
            "element_id": "0x2a:0000000000000001"
        }),
        json!({
            "verb": "cleanup_notepad_tabs",
            "path": "C:\\Temp\\issue1276-keep.txt",
            "command": "powershell.exe"
        }),
        json!({
            "verb": "cleanup_notepad_tabs",
            "path": "C:\\Temp\\issue1276-keep.txt",
            "x": 12,
            "y": 34
        }),
    ] {
        let params: TargetActParams = serde_json::from_value(params)
            .expect("synthetic cleanup_notepad_tabs params deserialize");
        let error = target_act_validate_cleanup_notepad_tabs_params(&params)
            .expect_err("cleanup_notepad_tabs must reject unrelated parameters");

        assert_eq!(
            target_act_error_code(&error),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
    }
}

#[test]
fn target_act_cleanup_notepad_tabs_policy_is_bounded() {
    assert_eq!(
        target_act_modified_stale_policy(None).expect("default policy should validate"),
        TargetActModifiedStalePolicy::DiscardModified
    );
    assert_eq!(
        target_act_modified_stale_policy(Some("refuse_modified"))
            .expect("refuse policy should validate"),
        TargetActModifiedStalePolicy::RefuseModified
    );
    let error = target_act_modified_stale_policy(Some("save_modified"))
        .expect_err("unknown policy must fail closed");
    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
}

#[test]
fn target_act_cleanup_notepad_tabs_parses_keep_and_stale_tabs() {
    let nodes = vec![
        target_act_test_accessible_node(
            1,
            "issue1276-keep.txt. Unmodified.",
            "tab item",
            &[UiaPattern::SelectionItem],
        ),
        target_act_test_accessible_node(
            2,
            "old-agent-tab.txt. Modified.",
            "TabItem",
            &[UiaPattern::SelectionItem],
        ),
        target_act_test_accessible_node(3, "Close Tab", "Button", &[UiaPattern::Invoke]),
    ];
    let tabs = target_act_notepad_tabs_from_nodes(&nodes, "issue1276-keep.txt");

    assert_eq!(tabs.len(), 2);
    assert!(tabs[0].keep);
    assert!(!tabs[0].modified);
    assert!(!tabs[1].keep);
    assert!(tabs[1].modified);
    assert_eq!(target_act_stale_notepad_tab_count(&tabs), 1);
    target_act_validate_notepad_keep_tab(&tabs, "issue1276-keep.txt")
        .expect("single keep tab should validate");
    assert!(
        target_act_notepad_close_tab_button(&nodes, &tabs[1]).is_some(),
        "Close Tab invoke button should be discoverable"
    );
}

#[test]
fn target_act_cleanup_notepad_tabs_chooses_nearest_close_tab_button() {
    let mut stale = target_act_test_accessible_node(
        10,
        "old-agent-tab.txt. Unmodified.",
        "TabItem",
        &[UiaPattern::SelectionItem],
    );
    stale.bbox = Rect {
        x: 100,
        y: 20,
        w: 120,
        h: 30,
    };
    let mut far_close =
        target_act_test_accessible_node(11, "Close Tab", "Button", &[UiaPattern::Invoke]);
    far_close.bbox = Rect {
        x: 900,
        y: 20,
        w: 28,
        h: 30,
    };
    let mut near_close =
        target_act_test_accessible_node(12, "Close Tab", "Button", &[UiaPattern::Invoke]);
    near_close.bbox = Rect {
        x: 205,
        y: 20,
        w: 28,
        h: 30,
    };
    let nodes = vec![far_close.clone(), stale, near_close.clone()];
    let tabs = target_act_notepad_tabs_from_nodes(&nodes, "issue1276-keep.txt");

    let close_button = target_act_notepad_close_tab_button(&nodes, &tabs[0])
        .expect("nearest close tab button should resolve");

    assert_eq!(close_button.element_id, near_close.element_id);
}

#[test]
fn target_act_cleanup_notepad_tabs_finds_close_glyph_inside_tab() {
    let mut stale = target_act_test_accessible_node(
        10,
        "old-agent-tab.txt. Modified.",
        "TabItem",
        &[UiaPattern::SelectionItem],
    );
    stale.bbox = Rect {
        x: 100,
        y: 20,
        w: 150,
        h: 48,
    };
    let mut title =
        target_act_test_accessible_node(11, "old-agent-tab.txt", "Text", &[UiaPattern::Text]);
    title.bbox = Rect {
        x: 112,
        y: 31,
        w: 90,
        h: 23,
    };
    let mut glyph = target_act_test_accessible_node(12, "x", "Text", &[UiaPattern::Text]);
    glyph.bbox = Rect {
        x: 214,
        y: 34,
        w: 18,
        h: 18,
    };
    let nodes = vec![stale.clone(), title, glyph.clone()];
    let tabs = target_act_notepad_tabs_from_nodes(&nodes, "issue1276-keep.txt");

    let close_glyph = target_act_notepad_close_tab_glyph(&nodes, &tabs[0])
        .expect("small right-side text glyph inside selected tab should be close target");

    assert_eq!(close_glyph.element_id, glyph.element_id);
}

#[test]
fn target_act_cleanup_notepad_tabs_finds_file_close_tab_menu_item() {
    let file_menu =
        target_act_test_accessible_node(10, "File", "MenuItem", &[UiaPattern::ExpandCollapse]);
    let close_window =
        target_act_test_accessible_node(11, "Close", "Button", &[UiaPattern::Invoke]);
    let close_tab =
        target_act_test_accessible_node(12, "Close tab Ctrl+W", "MenuItem", &[UiaPattern::Invoke]);
    let nodes = vec![close_window, file_menu.clone(), close_tab.clone()];

    let found_file = target_act_notepad_file_menu_item(&nodes)
        .expect("File menu item should resolve by name and ExpandCollapse");
    let found_close_tab = target_act_notepad_close_tab_menu_item(&nodes)
        .expect("Close tab menu item should resolve by name and InvokePattern");

    assert_eq!(found_file.element_id, file_menu.element_id);
    assert_eq!(found_close_tab.element_id, close_tab.element_id);
}

#[test]
fn target_act_cleanup_notepad_tabs_prefers_visible_stale_tab() {
    let mut offscreen = target_act_test_accessible_node(
        10,
        "offscreen-old.txt. Unmodified.",
        "TabItem",
        &[UiaPattern::SelectionItem],
    );
    offscreen.bbox = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };
    let mut visible = target_act_test_accessible_node(
        11,
        "visible-old.txt. Unmodified.",
        "TabItem",
        &[UiaPattern::SelectionItem],
    );
    visible.bbox = Rect {
        x: 200,
        y: 20,
        w: 120,
        h: 48,
    };
    let keep = target_act_test_accessible_node(
        12,
        "issue1276-keep.txt. Unmodified.",
        "TabItem",
        &[UiaPattern::SelectionItem],
    );
    let tabs =
        target_act_notepad_tabs_from_nodes(&[offscreen, keep, visible], "issue1276-keep.txt");

    let next = target_act_next_stale_notepad_tab(&tabs)
        .expect("visible stale tab should be selected first");

    assert_eq!(next.document_name, "visible-old.txt");
    assert!(target_act_rect_has_area(next.bbox));
}

#[test]
fn target_act_cleanup_notepad_tabs_refuse_policy_detects_any_modified_stale_tab() {
    let keep = target_act_test_accessible_node(
        10,
        "issue1276-keep.txt. Unmodified.",
        "TabItem",
        &[UiaPattern::SelectionItem],
    );
    let mut visible_unmodified = target_act_test_accessible_node(
        11,
        "visible-old.txt. Unmodified.",
        "TabItem",
        &[UiaPattern::SelectionItem],
    );
    visible_unmodified.bbox = Rect {
        x: 200,
        y: 20,
        w: 120,
        h: 48,
    };
    let mut offscreen_modified = target_act_test_accessible_node(
        12,
        "offscreen-old.txt. Modified.",
        "TabItem",
        &[UiaPattern::SelectionItem],
    );
    offscreen_modified.bbox = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };
    let tabs = target_act_notepad_tabs_from_nodes(
        &[keep, visible_unmodified, offscreen_modified],
        "issue1276-keep.txt",
    );

    let next = target_act_next_stale_notepad_tab(&tabs)
        .expect("visible unmodified tab should be selected for discard policy");
    let modified = target_act_first_modified_stale_notepad_tab(&tabs)
        .expect("refuse policy must detect modified stale tab before closing any stale tab");

    assert_eq!(next.document_name, "visible-old.txt");
    assert_eq!(modified.document_name, "offscreen-old.txt");
    assert!(modified.modified);
}

#[test]
fn target_act_cleanup_notepad_tabs_matches_refreshed_tab_by_document() {
    let mut original = target_act_test_accessible_node(
        10,
        "old-agent-tab.txt. Modified.",
        "TabItem",
        &[UiaPattern::SelectionItem],
    );
    original.bbox = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };
    let original_tabs = target_act_notepad_tabs_from_nodes(&[original], "issue1276-keep.txt");
    let mut refreshed = target_act_test_accessible_node(
        44,
        "old-agent-tab.txt. Modified.",
        "TabItem",
        &[UiaPattern::SelectionItem],
    );
    refreshed.bbox = Rect {
        x: 350,
        y: 242,
        w: 126,
        h: 48,
    };
    let refreshed_tabs = target_act_notepad_tabs_from_nodes(&[refreshed], "issue1276-keep.txt");

    let matched = target_act_matching_notepad_tab(&refreshed_tabs, &original_tabs[0])
        .expect("refreshed tab should match by document name and modified state");

    assert_eq!(matched.document_name, "old-agent-tab.txt");
    assert!(target_act_rect_has_area(matched.bbox));
}

#[test]
fn target_act_cleanup_notepad_tabs_requires_one_keep_tab_when_tabs_exist() {
    let nodes = vec![
        target_act_test_accessible_node(
            1,
            "one.txt. Unmodified.",
            "TabItem",
            &[UiaPattern::SelectionItem],
        ),
        target_act_test_accessible_node(
            2,
            "two.txt. Unmodified.",
            "TabItem",
            &[UiaPattern::SelectionItem],
        ),
    ];
    let tabs = target_act_notepad_tabs_from_nodes(&nodes, "missing.txt");
    let error = target_act_validate_notepad_keep_tab(&tabs, "missing.txt")
        .expect_err("missing keep tab must fail closed");

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::ACTION_TARGET_INVALID)
    );
}

#[test]
fn target_act_focus_window_uses_window_session_target() {
    let params = target_act_focus_window_params(Some(&SessionTarget::Window { hwnd: 0x250a08 }))
        .expect("window target should produce focus params");

    assert_eq!(params.hwnd, Some(0x250a08));
    assert!(params.title_regex.is_none());
    assert!(params.pid.is_none());
    assert_eq!(params.stable_ms, DEFAULT_TARGET_ACT_FOCUS_STABLE_MS);
}

#[test]
fn target_act_focus_window_uses_cdp_parent_window() {
    let params = target_act_focus_window_params(Some(&SessionTarget::Cdp {
        window_hwnd: 0x250a08,
        cdp_target_id: "chrome-tab:42".to_owned(),
    }))
    .expect("cdp target should focus its containing browser HWND");

    assert_eq!(params.hwnd, Some(0x250a08));
    assert!(params.title_regex.is_none());
    assert!(params.pid.is_none());
}

#[test]
fn target_act_focus_window_requires_session_target() {
    let error = target_act_focus_window_params(None).expect_err("missing target should refuse");

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TARGET_NOT_SET)
    );
    assert_eq!(target_act_error_status(&error), TARGET_ACT_STATUS_REFUSED);
}

#[test]
fn target_act_read_routes_cdp_targets_to_target_info() {
    let target = SessionTarget::Cdp {
        window_hwnd: 0x1234,
        cdp_target_id: "chrome-tab:42".to_owned(),
    };

    assert_eq!(
        target_act_read_delegated_tool(Some(&target)).expect("cdp target routes to target info"),
        "cdp_target_info"
    );
}

#[test]
fn target_act_read_routes_window_targets_to_observe() {
    let target = SessionTarget::Window { hwnd: 0x1234 };

    assert_eq!(
        target_act_read_delegated_tool(Some(&target)).expect("window target routes to observe"),
        "observe"
    );
}

#[test]
fn target_act_read_without_target_refuses_foreground_fallback() {
    let error =
        target_act_read_delegated_tool(None).expect_err("missing target should fail closed");

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TARGET_NOT_SET)
    );
    assert_eq!(target_act_error_status(&error), TARGET_ACT_STATUS_REFUSED);
}

#[test]
fn target_act_click_count_rejects_out_of_range() {
    let error =
        target_act_click_count_for_action("click", Some(4)).expect_err("clicks=4 should fail");

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
}

#[test]
fn target_act_dblclick_defaults_and_rejects_wrong_count() {
    assert_eq!(
        target_act_click_count_for_action("dblclick", None).expect("default dblclick"),
        2
    );
    assert_eq!(
        target_act_click_count_for_action("dblclick", Some(2)).expect("explicit dblclick"),
        2
    );
    let error = target_act_click_count_for_action("dblclick", Some(1))
        .expect_err("dblclick clickCount=1 should fail");
    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
}

#[test]
fn target_act_coordinate_click_deserializes() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "click",
        "x": 42,
        "y": 77,
        "coordinate_space": "viewport",
        "clicks": 3
    }))
    .expect("coordinate click params should deserialize");
    let coordinate = target_act_coordinate(&params)
        .expect("coordinate should validate")
        .expect("coordinate should be present");

    assert_eq!(params.verb.as_str(), "click");
    assert_eq!(coordinate.x, 42);
    assert_eq!(coordinate.y, 77);
    assert_eq!(coordinate.space, TargetActCoordinateSpace::Viewport);
    assert_eq!(coordinate.space.as_bridge_str(), "viewport");
    assert_eq!(
        target_act_click_count_for_action("click", params.clicks).unwrap(),
        3
    );
}

#[test]
fn target_act_coordinate_defaults_to_screen_space() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "click",
        "x": 12,
        "y": 34
    }))
    .expect("coordinate click params should deserialize");
    let coordinate = target_act_coordinate(&params)
        .expect("coordinate should validate")
        .expect("coordinate should be present");

    assert_eq!(coordinate.space, TargetActCoordinateSpace::Screen);
    assert_eq!(coordinate.space.as_bridge_str(), "screen");
}

#[test]
fn target_act_coordinate_space_accepts_nested_position() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "click",
        "coordinate_space": "window",
        "position": {
            "x": 42,
            "y": 77
        }
    }))
    .expect("nested coordinate position params should deserialize");
    let coordinate = target_act_coordinate(&params)
        .expect("nested coordinate position should validate")
        .expect("coordinate should be present");

    assert_eq!(coordinate.x, 42);
    assert_eq!(coordinate.y, 77);
    assert_eq!(coordinate.space, TargetActCoordinateSpace::Window);
    assert!(target_act_coordinate_uses_nested_position(&params));
    assert_eq!(
        target_act_click_position(&params).expect("DOM click position remains readable"),
        Some((42, 77))
    );
}

#[test]
fn target_act_coordinate_space_rejects_ambiguous_position_sources() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "click",
        "coordinate_space": "window",
        "x": 42,
        "y": 77,
        "position": {
            "x": 42,
            "y": 77
        }
    }))
    .expect("ambiguous coordinate params should deserialize");
    let error =
        target_act_coordinate(&params).expect_err("mixed x/y and nested position must fail closed");

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
}

#[test]
fn target_act_coordinate_requires_x_y_pair() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "click",
        "x": 42
    }))
    .expect("partial coordinate params should deserialize");
    let error = target_act_coordinate(&params).expect_err("missing y must fail closed");

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
}

#[test]
fn target_act_coordinate_space_requires_coordinates() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "click",
        "coordinate_space": "viewport"
    }))
    .expect("coordinate-space-only params should deserialize");
    let error = target_act_coordinate(&params)
        .expect_err("coordinate_space without coordinates must fail closed");

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
}

#[test]
fn target_act_coordinate_rejects_locator_mix_before_routing() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "click",
        "selector": "#submit",
        "x": 42,
        "y": 77
    }))
    .expect("mixed coordinate and selector params should deserialize");

    assert!(
        target_act_coordinate(&params)
            .expect("coordinate pair should validate")
            .is_some()
    );
    assert!(
        target_act_has_any_locator(&params),
        "mixed selector/coordinate input must be detected before routing"
    );
}

#[test]
fn target_act_tap_viewport_coordinate_deserializes() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "tap",
        "x": 52,
        "y": 191,
        "coordinate_space": "viewport"
    }))
    .expect("tap coordinate params should deserialize");
    let coordinate = target_act_coordinate(&params)
        .expect("coordinate should validate")
        .expect("coordinate should be present");

    assert_eq!(params.verb.as_str(), "tap");
    assert_eq!(coordinate.x, 52);
    assert_eq!(coordinate.y, 191);
    assert_eq!(coordinate.space, TargetActCoordinateSpace::Viewport);
}

#[test]
fn target_act_bridge_cdp_input_accepts_bridge_element_id() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "hover",
        "element_id": "chrome-tab:589708699:frame:0:path:0.1.1",
        "auto_wait": true
    }))
    .expect("bridge cdp input params should deserialize");

    target_act_validate_bridge_cdp_input("hover", None, &params)
        .expect("chrome-tab bridge element id should be valid for cdpInput");
}

#[test]
fn target_act_bridge_cdp_input_rejects_raw_cdp_element_id() {
    let raw_cdp_id = synapse_a11y::cdp_element_id(0x2a, 42).to_string();
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "hover",
        "element_id": raw_cdp_id
    }))
    .expect("raw cdp params should deserialize");

    let error = target_act_validate_bridge_cdp_input("hover", None, &params)
        .expect_err("raw cdp element id should require a raw endpoint");

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::ACTION_TARGET_INVALID)
    );
}

#[test]
fn target_act_tap_dom_id_selector_escapes_css_string() {
    let selector = target_act_dom_id_selector(r#"apply"now\button"#)
        .expect("visible DOM id should become a selector");

    assert_eq!(selector, r#"[id="apply\"now\\button"]"#);
}

#[test]
fn target_act_hover_params_deserialize() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "hover",
        "role": "button",
        "name": "Account menu"
    }))
    .expect("hover params should deserialize");

    assert_eq!(params.verb.as_str(), "hover");
    assert_eq!(params.role.as_deref(), Some("button"));
    assert_eq!(params.name.as_deref(), Some("Account menu"));
    assert!(
        target_act_coordinate(&params)
            .expect("hover should not contain coordinates")
            .is_none()
    );
}

#[test]
fn target_act_dom_verbs_deserialize_and_validate() {
    let click_with_options: TargetActParams = serde_json::from_value(json!({
        "verb": "click",
        "selector": "#canvas",
        "clickCount": 2,
        "button": "right",
        "modifiers": ["Shift", "control", "meta"],
        "position": { "x": 12, "y": 8 }
    }))
    .expect("click options params should deserialize");
    assert_eq!(click_with_options.verb.as_str(), "click");
    assert_eq!(click_with_options.clicks, Some(2));
    assert_eq!(click_with_options.button, Some(TargetActMouseButton::Right));
    assert_eq!(
        click_with_options.modifiers,
        vec![
            TargetActClickModifier::Shift,
            TargetActClickModifier::Ctrl,
            TargetActClickModifier::Meta
        ]
    );
    assert_eq!(
        target_act_click_position(&click_with_options).expect("click position"),
        Some((12, 8))
    );
    target_act_validate_dom_locator("click", &click_with_options)
        .expect("click options locator should validate");

    let dblclick: TargetActParams = serde_json::from_value(json!({
        "verb": "dblclick",
        "selector": "#apply",
        "offsetX": 3,
        "offsetY": 4
    }))
    .expect("dblclick params should deserialize");
    assert_eq!(dblclick.verb.as_str(), "dblclick");
    assert_eq!(
        target_act_dom_click_count("dblclick", dblclick.clicks).expect("dblclick count"),
        Some(2)
    );
    assert_eq!(
        target_act_click_position(&dblclick).expect("dblclick position"),
        Some((3, 4))
    );
    target_act_validate_dom_locator("dblclick", &dblclick)
        .expect("dblclick locator should validate");

    let press: TargetActParams = serde_json::from_value(json!({
        "verb": "press",
        "role": "button",
        "name": "Create token"
    }))
    .expect("press params should deserialize");
    assert_eq!(press.verb.as_str(), "press");
    target_act_validate_dom_locator("press", &press).expect("press locator should validate");

    let select: TargetActParams = serde_json::from_value(json!({
        "verb": "select",
        "selector": "#scope",
        "option": "Workers KV Storage"
    }))
    .expect("select params should deserialize");
    assert_eq!(select.verb.as_str(), "select");
    target_act_validate_dom_locator("select", &select).expect("select locator should validate");

    let select_by_label: TargetActParams = serde_json::from_value(json!({
        "verb": "select",
        "selector": "#scope",
        "option_label": "Workers KV Storage"
    }))
    .expect("select by label params should deserialize");
    assert_eq!(
        select_by_label.option_label.as_deref(),
        Some("Workers KV Storage")
    );
    target_act_validate_dom_locator("select", &select_by_label)
        .expect("select label should validate");

    let select_by_index: TargetActParams = serde_json::from_value(json!({
        "verb": "select",
        "selector": "#scope",
        "option_index": 2
    }))
    .expect("select by index params should deserialize");
    assert_eq!(select_by_index.option_index, Some(2));
    target_act_validate_dom_locator("select", &select_by_index)
        .expect("select index should validate");

    let select_many: TargetActParams = serde_json::from_value(json!({
        "verb": "select",
        "selector": "#scope",
        "options": [
            { "value": "read" },
            { "label": "Write" },
            { "index": 3 }
        ]
    }))
    .expect("multi-select params should deserialize");
    assert_eq!(select_many.options.len(), 3);
    target_act_validate_dom_locator("select", &select_many)
        .expect("multi-select options should validate");

    let bad_select_option: TargetActParams = serde_json::from_value(json!({
        "verb": "select",
        "selector": "#scope",
        "options": [
            { "value": "read", "label": "Read" }
        ]
    }))
    .expect("bad select option shape should deserialize");
    let error = target_act_validate_dom_locator("select", &bad_select_option)
        .expect_err("ambiguous select option spec should be rejected");
    assert!(
        error
            .message
            .contains("exactly one of value, label, or index"),
        "select validation should reject ambiguous option specs: {error:?}"
    );

    let submit: TargetActParams = serde_json::from_value(json!({
        "verb": "submit",
        "selector": "form#token"
    }))
    .expect("submit params should deserialize");
    assert_eq!(submit.verb.as_str(), "submit");
    target_act_validate_dom_locator("submit", &submit).expect("submit locator should validate");

    let dispatch_event: TargetActParams = serde_json::from_value(json!({
        "verb": "dispatch_event",
        "selector": "#token",
        "event_type": "synapse-ready",
        "event_init": {
            "bubbles": true,
            "cancelable": true,
            "detail": {
                "ok": true
            }
        }
    }))
    .expect("dispatch_event params should deserialize");
    assert_eq!(dispatch_event.verb.as_str(), "dispatch_event");
    assert_eq!(dispatch_event.event_type.as_deref(), Some("synapse-ready"));
    assert_eq!(
        dispatch_event
            .event_init
            .as_ref()
            .and_then(|value| value.get("detail"))
            .and_then(|value| value.get("ok"))
            .and_then(Value::as_bool),
        Some(true)
    );
    target_act_validate_dom_locator("dispatch_event", &dispatch_event)
        .expect("dispatch_event locator should validate");

    for verb in ["check", "uncheck"] {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": verb,
            "role": "checkbox",
            "name": "Accept terms"
        }))
        .expect("check state params should deserialize");
        assert_eq!(params.verb.as_str(), verb);
        target_act_validate_dom_locator(verb, &params)
            .expect("check state locator should validate");
    }

    for verb in ["clear", "focus", "blur", "select_text", "selectText"] {
        let params: TargetActParams = serde_json::from_value(json!({
            "verb": verb,
            "selector": "#token"
        }))
        .expect("primitive params should deserialize");
        let expected = if verb == "selectText" {
            "selecttext"
        } else {
            verb
        };
        assert_eq!(params.verb.as_str(), expected);
        let action = if verb == "selectText" {
            "select_text"
        } else {
            verb
        };
        target_act_validate_dom_primitive_params(action, &params)
            .expect("primitive locator should validate");
    }

    let clear_with_text: TargetActParams = serde_json::from_value(json!({
        "verb": "clear",
        "selector": "#token",
        "text": "ignored"
    }))
    .expect("clear params should deserialize");
    let error = target_act_validate_dom_primitive_params("clear", &clear_with_text)
        .expect_err("clear should reject unused text");
    assert!(
        error.message.contains("does not accept text"),
        "clear validation should reject ignored text: {error:?}"
    );
}

#[test]
fn target_act_key_chord_deserializes_and_constructs_press_request() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "key",
        "key": "Ctrl+End",
        "wait_timeout_ms": 750
    }))
    .expect("key params should deserialize");

    let keys = target_act_key_chord_keys(&params, "key").expect("key chord should parse");
    assert_eq!(keys, vec!["Ctrl", "End"]);
    let press = target_act_press_params(keys, params.wait_timeout_ms, "key").expect("press params");
    assert_eq!(press.keys, vec!["Ctrl", "End"]);
    assert!(press.verify_delta);
    assert_eq!(press.verify_timeout_ms, 750);
}

#[test]
fn target_act_key_chord_rejects_key_and_keys_together() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "key",
        "key": "Ctrl+Z",
        "keys": ["ctrl", "z"]
    }))
    .expect("key params should deserialize");

    let error = target_act_key_chord_keys(&params, "key")
        .expect_err("key and keys together should fail closed");
    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
}

#[test]
fn target_act_key_rejects_printable_text_sequence() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "key",
        "keys": ["V", "F", "X"]
    }))
    .expect("key params should deserialize");

    let keys = target_act_key_chord_keys(&params, "key").expect("keys should parse");
    let error = target_act_press_params(keys, params.wait_timeout_ms, "key")
        .expect_err("printable text sequence must fail before act_press dispatch");

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert!(
        error
            .message
            .contains("simultaneous chord route, not ordered text input"),
        "error should explain the root cause and route guidance: {error:?}"
    );
}

#[test]
fn target_act_key_allows_modifier_plus_single_printable_key() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "key",
        "key": "Ctrl+F"
    }))
    .expect("key params should deserialize");

    let keys = target_act_key_chord_keys(&params, "key").expect("key chord should parse");
    let press = target_act_press_params(keys, params.wait_timeout_ms, "key").expect("press params");

    assert_eq!(press.keys, vec!["Ctrl", "F"]);
    assert!(press.verify_delta);
}

#[test]
fn target_act_insert_and_append_params_deserialize() {
    let insert: TargetActParams = serde_json::from_value(json!({
        "verb": "insert_text",
        "element_id": "0x2a:0000000000000001",
        "text": "insert me"
    }))
    .expect("insert_text params should deserialize");
    assert_eq!(insert.verb.as_str(), "insert_text");
    assert_eq!(insert.text.as_deref(), Some("insert me"));

    let append: TargetActParams = serde_json::from_value(json!({
        "verb": "append_text",
        "text": "append me"
    }))
    .expect("append_text current-focus params should deserialize");
    assert_eq!(append.verb.as_str(), "append_text");
    assert!(!target_act_has_any_locator(&append));
}

#[test]
fn target_act_insert_native_element_id_routes_to_native_text() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "insert_text",
        "element_id": "0x2a:0000000000000001",
        "text": "insert me"
    }))
    .expect("insert_text params should deserialize");

    let element_id = target_act_native_text_element_id(&params, "insert_text")
        .expect("native element routing should validate")
        .expect("native element id should use native text route");

    assert_eq!(element_id.as_str(), "0x2a:0000000000000001");
}

#[test]
fn target_act_insert_cdp_element_id_uses_existing_focus_type_path() {
    let cdp_id = synapse_a11y::cdp_element_id(0x2a, 42);
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "insert_text",
        "element_id": cdp_id.to_string(),
        "text": "insert me"
    }))
    .expect("insert_text params should deserialize");

    assert!(
        target_act_native_text_element_id(&params, "insert_text")
            .expect("cdp element routing should validate")
            .is_none()
    );
}

#[test]
fn target_act_insert_native_element_id_rejects_dom_locator_mix() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "insert_text",
        "element_id": "0x2a:0000000000000001",
        "selector": "#editor",
        "text": "insert me"
    }))
    .expect("insert_text params should deserialize");

    let error = target_act_native_text_element_id(&params, "insert_text")
        .expect_err("native element id plus selector must fail closed");
    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
}

#[test]
fn target_act_set_selection_params_deserialize_with_aliases() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "set_selection",
        "element_id": "0x2a:0000000000000001",
        "start": 3,
        "end": 8
    }))
    .expect("set_selection params should deserialize");

    assert_eq!(params.verb.as_str(), "set_selection");
    assert_eq!(target_act_selection_range(&params).expect("range"), (3, 8));
}

#[test]
fn target_act_set_selection_rejects_reversed_range() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "set_selection",
        "element_id": "0x2a:0000000000000001",
        "selection_start": 9,
        "selection_end": 2
    }))
    .expect("set_selection params should deserialize");

    let error = target_act_selection_range(&params)
        .expect_err("set_selection must reject end before start");
    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
}

#[test]
fn target_act_select_requires_option_or_value() {
    let params: TargetActParams = serde_json::from_value(json!({
        "verb": "select",
        "selector": "#scope"
    }))
    .expect("synthetic select params should deserialize");
    let error = target_act_validate_dom_locator("select", &params)
        .expect_err("select must require an option/value");

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
}

#[test]
fn target_act_type_params_constructs_act_type_request() {
    let params = target_act_type_params("issue-1267".to_owned(), Some(750), Some(0x1234))
        .expect("target_act type params should construct act_type params");

    assert_eq!(params.text, "issue-1267");
    assert_eq!(params.verify_timeout_ms, 750);
    assert!(params.verify_delta);
    assert_eq!(params.verify_target_window_hwnd, Some(0x1234));
    assert!(params.into_element.is_none());
}

#[test]
fn target_act_type_wait_timeout_is_bounded() {
    let error = target_act_type_params("issue-1267".to_owned(), Some(30_000), None)
        .expect_err("type wait timeout must be bounded");

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
}

#[test]
fn target_act_rect_contains_point_uses_exclusive_bottom_right() {
    let rect = Rect {
        x: 10,
        y: 20,
        w: 30,
        h: 40,
    };

    assert!(target_act_rect_contains_point(rect, Point { x: 10, y: 20 }));
    assert!(target_act_rect_contains_point(rect, Point { x: 39, y: 59 }));
    assert!(!target_act_rect_contains_point(
        rect,
        Point { x: 40, y: 59 }
    ));
    assert!(!target_act_rect_contains_point(
        rect,
        Point { x: 39, y: 60 }
    ));
}

#[test]
fn target_act_click_plain_element_id_routes_to_dom() {
    let routed = target_act_legacy_click_element_id("create-token-button")
        .expect("plain page id should be accepted as DOM id");

    assert!(
        routed.is_none(),
        "plain page element ids should route through the browser DOM bridge"
    );
}

#[test]
fn target_act_click_bridge_element_id_routes_to_dom() {
    let routed = target_act_legacy_click_element_id("chrome-tab:589708698:frame:4970:path:0.1.1")
        .expect("normal bridge element id should be accepted as a DOM id");

    assert!(
        routed.is_none(),
        "normal bridge element ids must route through chrome_debugger_bridge.domAction"
    );
}

#[test]
fn target_act_click_native_shaped_element_id_stays_legacy() {
    let routed = target_act_legacy_click_element_id("0x2a:0000000000000001")
        .expect("valid native/UIA id should parse");

    assert_eq!(
        routed
            .expect("native/UIA id should stay on legacy click path")
            .as_str(),
        "0x2a:0000000000000001"
    );
}

#[test]
fn target_act_click_malformed_native_id_fails_closed() {
    let error = target_act_legacy_click_element_id("0xnotvalid:bad")
        .expect_err("malformed native-looking id should not fall back to DOM");

    assert_eq!(
        target_act_error_code(&error),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
}

#[test]
fn target_act_cdp_target_match_accepts_owned_root_case_insensitive() {
    assert!(target_act_cdp_target_matches_session_or_frame(
        "ABC123",
        "abc123",
        &[]
    ));
}

#[test]
fn target_act_cdp_target_match_accepts_owned_oopif_child() {
    let frames = vec![
        target_act_test_frame_entry("main-frame", None, "root-target", 0, false),
        target_act_test_frame_entry("child-frame", Some("main-frame"), "iframe-target", 1, true),
    ];

    assert!(target_act_cdp_target_matches_session_or_frame(
        "root-target",
        "IFRAME-TARGET",
        &frames
    ));
}

#[test]
fn target_act_cdp_target_match_rejects_unrelated_or_stale_child() {
    let frames = vec![
        target_act_test_frame_entry("main-frame", None, "root-target", 0, false),
        target_act_test_frame_entry("child-frame", Some("main-frame"), "iframe-target", 1, true),
    ];

    assert!(!target_act_cdp_target_matches_session_or_frame(
        "root-target",
        "stale-frame-target",
        &frames
    ));
}

#[test]
fn target_act_dom_error_codes_classify() {
    for code in [
        error_codes::CHROME_DOM_ACTION_UNSUPPORTED,
        error_codes::CHROME_DOM_ELEMENT_AMBIGUOUS,
        error_codes::CHROME_DOM_ELEMENT_NOT_ACTIONABLE,
        error_codes::CHROME_DOM_ELEMENT_NOT_FOUND,
        error_codes::CHROME_DOM_SELECTOR_INVALID,
    ] {
        let error = mcp_error(code, "synthetic DOM refusal");
        assert_eq!(target_act_error_status(&error), TARGET_ACT_STATUS_REFUSED);
    }

    let postcondition = mcp_error(
        error_codes::CHROME_DOM_ACTION_POSTCONDITION_FAILED,
        "synthetic DOM readback mismatch",
    );
    assert_eq!(
        target_act_error_status(&postcondition),
        TARGET_ACT_STATUS_VERIFY_NEEDED
    );
}

#[test]
fn target_act_errors_classify_verify_needed() {
    for code in [
        error_codes::ACTION_NO_OBSERVED_DELTA,
        error_codes::ACTION_FOREGROUND_LOST,
        error_codes::ACTION_POSTCONDITION_FAILED,
        error_codes::ACTION_VERIFY_SURFACE_UNAVAILABLE,
    ] {
        let error = mcp_error(code, "synthetic delegated postcondition failure");
        assert_eq!(
            target_act_error_status(&error),
            TARGET_ACT_STATUS_VERIFY_NEEDED
        );
    }
}

#[test]
fn target_act_errors_classify_refusal() {
    for code in [
        error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED,
        error_codes::ACTION_ELEMENT_VALUE_READ_ONLY,
        error_codes::ACTION_FOREGROUND_LEASE_BUSY,
        error_codes::ACTION_FOREGROUND_LEASE_NOT_HELD,
        error_codes::FOREGROUND_ACTIVATION_REFUSED,
        error_codes::TARGET_CLAIM_NOT_FOUND,
        error_codes::TARGET_NOT_SET,
    ] {
        let error = mcp_error(code, "synthetic delegated refusal");
        assert_eq!(target_act_error_status(&error), TARGET_ACT_STATUS_REFUSED);
    }
}

#[test]
fn target_act_delivery_state_distinguishes_verification_failures() {
    assert_eq!(
        target_act_delivery_state(true, TARGET_ACT_STATUS_OK),
        "delivered_and_verified"
    );
    assert_eq!(
        target_act_delivery_state(false, TARGET_ACT_STATUS_VERIFY_NEEDED),
        "delivered_unverified"
    );
    assert_eq!(
        target_act_delivery_state(false, TARGET_ACT_STATUS_REFUSED),
        "refused_before_delivery"
    );
    assert_eq!(
        target_act_delivery_state(false, TARGET_ACT_STATUS_ERROR),
        "failed_before_delivery"
    );
    assert_eq!(
        target_act_delivery_state_from_result(
            true,
            TARGET_ACT_STATUS_OK,
            &json!({
                "postcondition": {
                    "source_of_truth": "target_window_ui_or_pixels",
                    "observed_delta": true,
                }
            }),
        ),
        "delivered_and_pixel_verified"
    );
}

#[test]
fn target_act_foreground_lost_guidance_uses_public_act_facade() {
    let foreground = synthetic_foreground_context();
    let error = target_act_foreground_lost_error(0x1000, 0x2000, &foreground, false, None);
    let message = error.message.to_string();

    assert!(message.contains("act operation=foreground"));
    assert!(message.contains("same action payload"));
    assert!(!message.contains("control_lease_acquire"));
    assert!(!message.contains("verb=focus_window immediately"));
    assert_eq!(
        act_error_field(&error, "recommended_public_tool").as_deref(),
        Some("act")
    );
    assert_eq!(
        act_error_field(&error, "recommended_public_operation").as_deref(),
        Some("foreground")
    );
    assert_eq!(
        act_error_field(&error, "recommended_public_route").as_deref(),
        Some(TARGET_ACT_FOREGROUND_ROUTE_REMEDIATION)
    );
}

#[test]
fn target_act_error_result_preserves_delegated_data() {
    let error = mcp_error(error_codes::ACTION_POSTCONDITION_FAILED, "mismatch");
    let result = target_act_error_result("act_set_field_text", error);

    assert_eq!(
        result.pointer("/error/code").and_then(Value::as_str),
        Some(error_codes::ACTION_POSTCONDITION_FAILED)
    );
    assert_eq!(
        result
            .pointer("/error/delegated_tool")
            .and_then(Value::as_str),
        Some("act_set_field_text")
    );
    assert_eq!(
        result.pointer("/error/data/code").and_then(Value::as_str),
        Some(error_codes::ACTION_POSTCONDITION_FAILED)
    );
}

#[test]
fn target_act_secret_safe_result_redacts_page_text_and_values() {
    let result = target_act_secret_safe_result(
        "chrome_debugger_bridge.domAction",
        json!({
            "target_id": "target-1470",
            "tab_id": 1470,
            "action": "click",
            "before_page": {
                "url": "https://example.test/secrets",
                "title": "API keys",
                "ready_state": "complete"
            },
            "after_page": {
                "url": "https://example.test/secrets",
                "title": "API keys",
                "ready_state": "complete"
            },
            "before_page_text": {"text": "existing-key-secret-1470"},
            "after_page_text": {"text": "new-one-time-secret-1470"},
            "action_readback": {
                "selected_text": "copied-secret-1470",
                "after_value": "input-secret-1470",
                "value_len": 17,
                "checked": true
            }
        }),
    )
    .expect("secret-safe result should sanitize");

    let encoded = serde_json::to_string(&result).expect("serialize sanitized result");
    for raw in [
        "existing-key-secret-1470",
        "new-one-time-secret-1470",
        "copied-secret-1470",
        "input-secret-1470",
    ] {
        assert!(
            !encoded.contains(raw),
            "secret-safe result leaked raw field {raw}"
        );
    }
    assert_eq!(
        result.get("secret_safe").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        result.pointer("/before_page/url").and_then(Value::as_str),
        Some("https://example.test/secrets")
    );
    assert_eq!(
        result
            .pointer("/after_page_text/redaction_policy")
            .and_then(Value::as_str),
        Some(TARGET_ACT_SECRET_SAFE_REDACTION_POLICY)
    );
    assert_eq!(
        result
            .pointer("/action_readback/value_len")
            .and_then(Value::as_u64),
        Some(17)
    );
}

#[test]
fn target_act_secret_safe_error_redacts_message_and_data() {
    let error = ErrorData::new(
        ErrorCode(-32099),
        "domAction frame_results contained new-one-time-secret-1470".to_owned(),
        Some(json!({
            "code": error_codes::CHROME_DOM_ELEMENT_NOT_FOUND,
            "error_detail": "button near existing-key-secret-1470 was not found",
            "frame_results": [{
                "result": {
                    "in_page_before_text": "existing-key-secret-1470"
                }
            }]
        })),
    );

    let result =
        target_act_secret_safe_error_result_ref("chrome_debugger_bridge.domAction", &error);
    let encoded = serde_json::to_string(&result).expect("serialize sanitized error");
    for raw in [
        "new-one-time-secret-1470",
        "existing-key-secret-1470",
        "button near existing-key-secret-1470 was not found",
    ] {
        assert!(
            !encoded.contains(raw),
            "secret-safe error leaked raw field {raw}"
        );
    }
    assert_eq!(
        result.pointer("/error/code").and_then(Value::as_str),
        Some(error_codes::CHROME_DOM_ELEMENT_NOT_FOUND)
    );
    assert_eq!(
        result
            .pointer("/error/message_redacted")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        result
            .pointer("/error/data/error_detail/redaction_policy")
            .and_then(Value::as_str),
        Some(TARGET_ACT_SECRET_SAFE_REDACTION_POLICY)
    );
}

fn target_act_test_snapshot(bytes: &[u8]) -> TargetActFileSnapshot {
    TargetActFileSnapshot {
        len: u64::try_from(bytes.len()).expect("synthetic bytes length should fit u64"),
        sha256: crate::m2::postcondition::hex_encode(&Sha256::digest(bytes)),
        bytes: bytes.to_vec(),
    }
}

fn target_act_test_accessible_node(
    sequence: u32,
    name: &str,
    role: &str,
    patterns: &[UiaPattern],
) -> AccessibleNode {
    AccessibleNode {
        element_id: synapse_core::element_id(0x2a, &format!("0000002a{sequence:08x}")),
        parent: None,
        name: name.to_owned(),
        role: role.to_owned(),
        automation_id: None,
        value: None,
        bbox: Rect {
            x: i32::try_from(sequence).unwrap_or(0) * 10,
            y: 20,
            w: 100,
            h: 30,
        },
        enabled: true,
        focused: false,
        patterns: patterns.to_vec(),
        children_count: 0,
        depth: 1,
    }
}

fn target_act_test_frame_entry(
    frame_id: &str,
    parent_frame_id: Option<&str>,
    cdp_target_id: &str,
    depth: u32,
    is_out_of_process: bool,
) -> synapse_a11y::CdpFrameTreeEntry {
    synapse_a11y::CdpFrameTreeEntry {
        frame_id: frame_id.to_owned(),
        parent_frame_id: parent_frame_id.map(ToOwned::to_owned),
        cdp_target_id: cdp_target_id.to_owned(),
        target_type: if is_out_of_process { "iframe" } else { "page" }.to_owned(),
        target_attached: Some(true),
        url: format!("https://example.test/{frame_id}"),
        name: None,
        origin: "https://example.test".to_owned(),
        security_origin: Some("https://example.test".to_owned()),
        loader_id: Some(format!("loader-{frame_id}")),
        depth,
        sibling_index: 0,
        child_count: 0,
        is_out_of_process,
        frame_element_id: None,
        frame_element_backend_node_id: None,
        frame_element_cdp_target_id: None,
        frame_element_source: if parent_frame_id.is_some() {
            "DOM.Node.frameId".to_owned()
        } else {
            "main_frame".to_owned()
        },
    }
}
