//! Unit tests for the m2 input tools (split out of the module body per #1555).

use synapse_core::{ElementId, Rect};

use super::*;

#[test]
fn key_hold_lease_ttl_matches_bounded_hold_window() {
    assert_eq!(
        lease_ttl_for_hold_ms(1),
        synapse_action::DEFAULT_LEASE_TTL_MS
    );
    assert_eq!(lease_ttl_for_hold_ms(6_000), 8_500);
    assert_eq!(
        lease_ttl_for_hold_ms(u32::MAX),
        synapse_action::MAX_LEASE_TTL_MS
    );
}

#[test]
fn hidden_desktop_foreground_refusal_carries_physical_route_context() {
    let hidden_desktop = crate::server::session_lifecycle::SessionHiddenDesktopReadback {
        session_id: "session-743".to_owned(),
        desktop_names: vec!["SynapseAgent_abc123".to_owned()],
        launch_pids: vec![4242],
        resource_count: 1,
    };

    let error = hidden_desktop_foreground_refusal("act_press", &hidden_desktop);
    let data = error.data.as_ref().expect("structured error data");
    println!(
        "readback=hidden_desktop_foreground_refusal before=session:{} desktop:{:?} after=data:{}",
        hidden_desktop.session_id, hidden_desktop.desktop_names, data
    );

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::FOREGROUND_ACTIVATION_REFUSED)
    );
    assert_eq!(
        data.get("reason").and_then(Value::as_str),
        Some("hidden_desktop_foreground_tier_refused")
    );
    assert_eq!(data.get("tool").and_then(Value::as_str), Some("act_press"));
    assert_eq!(
        data.get("foreground_tier_allowed").and_then(Value::as_bool),
        Some(false)
    );
    assert_eq!(
        data.get("desktop_names")
            .and_then(Value::as_array)
            .and_then(|names| names.first())
            .and_then(Value::as_str),
        Some("SynapseAgent_abc123")
    );
}

#[test]
fn stroke_foreground_lost_error_carries_specific_code_and_readbacks() {
    let expected = foreground_proof(100, 10, "notepad.exe", "before");
    let actual = foreground_context(200, 20, "calc.exe", "after");

    let error = act_stroke_foreground_lost_error(&expected, Some(&actual), None);
    let data = match error.data.as_ref() {
        Some(data) => data,
        None => panic!("foreground lost error should carry structured data"),
    };

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::ACTION_FOREGROUND_LOST)
    );
    assert_eq!(
        data.pointer("/foreground_expected/hwnd")
            .and_then(Value::as_i64),
        Some(100)
    );
    assert_eq!(
        data.pointer("/foreground_actual/hwnd")
            .and_then(Value::as_i64),
        Some(200)
    );
    assert_eq!(
        data.pointer("/queue_rate_state/kind")
            .and_then(Value::as_str),
        Some("not_rate_or_queue")
    );
}

#[test]
fn act_stroke_foreground_monitor_only_runs_for_live_leased_strokes() {
    assert!(
        should_monitor_act_stroke_foreground(false, true),
        "live real-cursor strokes require foreground-loss monitoring"
    );
    assert!(
        should_acquire_act_stroke_input_lease(false, true),
        "live real-cursor strokes require the foreground input lease"
    );
    assert!(
        !should_monitor_act_stroke_foreground(false, false),
        "background CDP strokes must not be aborted by the global foreground monitor"
    );
    assert!(
        !should_acquire_act_stroke_input_lease(false, false),
        "background CDP strokes must not acquire the foreground input lease"
    );
    assert!(
        !should_monitor_act_stroke_foreground(true, true),
        "recording strokes do not touch live foreground input"
    );
    assert!(
        !should_acquire_act_stroke_input_lease(true, true),
        "recording strokes do not need the foreground input lease"
    );
    assert!(
        !should_monitor_act_stroke_foreground(true, false),
        "recording background strokes also skip live foreground monitoring"
    );
    assert!(
        !should_acquire_act_stroke_input_lease(true, false),
        "recording background strokes also skip foreground lease acquisition"
    );
}

#[test]
fn act_set_value_background_guard_rejects_target_activation() {
    let before = foreground_context(100, 10, "chrome.exe", "before");
    let after = foreground_context(200, 20, "wpf-test.exe", "after");
    let target = BackgroundTargetForegroundGuard {
        element_hwnd: 150,
        root_hwnd: 200,
    };

    let error = verify_background_target_not_activated(
        "act_set_value",
        "uia_value_pattern.value",
        target,
        &before,
        &after,
    )
    .expect_err("background set_value must fail if it activates the target root");
    let data = error.data.as_ref().expect("structured error data");

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::ACTION_FOREGROUND_LOST)
    );
    assert_eq!(
        data.get("reason").and_then(Value::as_str),
        Some("background_action_changed_foreground")
    );
    assert_eq!(
        data.get("target_root_hwnd").and_then(Value::as_i64),
        Some(200)
    );
    assert_eq!(
        data.get("target_element_hwnd").and_then(Value::as_i64),
        Some(150)
    );
    assert_eq!(
        data.pointer("/foreground_before/hwnd")
            .and_then(Value::as_i64),
        Some(100)
    );
    assert_eq!(
        data.pointer("/foreground_after/hwnd")
            .and_then(Value::as_i64),
        Some(200)
    );
}

#[test]
fn act_scroll_background_guard_rejects_target_activation() {
    let before = foreground_context(100, 10, "Code.exe", "before");
    let after = foreground_context(200, 20, "notepad.exe", "after");
    let target = BackgroundTargetForegroundGuard {
        element_hwnd: 150,
        root_hwnd: 200,
    };

    let error = verify_background_target_not_activated(
        "act_scroll",
        "uia_scroll_pattern.scroll_state",
        target,
        &before,
        &after,
    )
    .expect_err("background scroll must fail if a UIA provider activates the target");
    let data = error.data.as_ref().expect("structured error data");

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::ACTION_FOREGROUND_LOST)
    );
    assert_eq!(data.get("tool").and_then(Value::as_str), Some("act_scroll"));
    assert_eq!(
        data.get("action_source_of_truth").and_then(Value::as_str),
        Some("uia_scroll_pattern.scroll_state")
    );
}

#[test]
fn act_click_background_guard_rejects_target_activation() {
    let before = foreground_context(100, 10, "Code.exe", "before");
    let after = foreground_context(200, 20, "notepad.exe", "after");
    let target = BackgroundTargetForegroundGuard {
        element_hwnd: 150,
        root_hwnd: 200,
    };

    let error = verify_background_target_not_activated(
        "act_click",
        "target_window_ui_or_pixels",
        target,
        &before,
        &after,
    )
    .expect_err("background click must fail if a UIA provider activates the target");
    let data = error.data.as_ref().expect("structured error data");

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::ACTION_FOREGROUND_LOST)
    );
    assert_eq!(data.get("tool").and_then(Value::as_str), Some("act_click"));
    assert_eq!(
        data.get("action_source_of_truth").and_then(Value::as_str),
        Some("target_window_ui_or_pixels")
    );
    assert_eq!(
        data.pointer("/foreground_restore/status")
            .and_then(Value::as_str),
        Some("skipped")
    );
}

#[test]
fn act_set_value_background_guard_rejects_target_child_activation() {
    let before = foreground_context(100, 10, "chrome.exe", "before");
    let after = foreground_context(150, 20, "winforms-test.exe", "after child");
    let target = BackgroundTargetForegroundGuard {
        element_hwnd: 150,
        root_hwnd: 200,
    };

    let error = verify_background_target_not_activated(
        "act_set_value",
        "uia_value_pattern.value",
        target,
        &before,
        &after,
    )
    .expect_err("background set_value must fail if it activates the target child hwnd");
    let data = error.data.as_ref().expect("structured error data");

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::ACTION_FOREGROUND_LOST)
    );
    assert_eq!(
        data.get("target_element_hwnd").and_then(Value::as_i64),
        Some(150)
    );
    assert_eq!(
        data.get("target_root_hwnd").and_then(Value::as_i64),
        Some(200)
    );
    assert_eq!(
        data.pointer("/foreground_after/hwnd")
            .and_then(Value::as_i64),
        Some(150)
    );
}

#[test]
fn act_set_value_background_guard_allows_non_target_foreground_change() {
    let before = foreground_context(100, 10, "chrome.exe", "before");
    let after = foreground_context(300, 30, "code.exe", "human moved");
    let target = BackgroundTargetForegroundGuard {
        element_hwnd: 150,
        root_hwnd: 200,
    };

    verify_background_target_not_activated(
        "act_set_value",
        "win32_window_text",
        target,
        &before,
        &after,
    )
    .expect("non-target foreground changes should not be treated as target activation");
}

#[test]
fn act_set_value_background_guard_allows_already_target_foreground() {
    let before = foreground_context(150, 20, "winforms-test.exe", "already target");
    let after = foreground_context(200, 20, "winforms-test.exe", "root after");
    let target = BackgroundTargetForegroundGuard {
        element_hwnd: 150,
        root_hwnd: 200,
    };

    verify_background_target_not_activated(
        "act_set_value",
        "uia_value_pattern.value",
        target,
        &before,
        &after,
    )
    .expect("background guard should not fail when the target was already foreground");
}

#[test]
fn act_press_verify_delta_rejects_foreground_change_by_default() {
    let before = click_signature(100, 10, "WindowsTerminal.exe", "Terminal", 1);
    let after = click_signature(
        200,
        20,
        "chrome.exe",
        "Device Activation - Google Chrome",
        1,
    );

    let error = verify_captured_action_delta(
        "act_press",
        "foreground_focused_ui_or_pixels",
        250,
        before,
        after,
        None,
        ForegroundChangePolicy::reject(),
    )
    .expect_err("unexpected foreground changes must remain fail-closed");
    let data = error.data.as_ref().expect("structured error data");

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::ACTION_FOREGROUND_LOST)
    );
    assert_eq!(
        data.get("reason").and_then(Value::as_str),
        Some("unexpected_foreground_change")
    );
}

#[test]
fn act_press_verify_delta_accepts_declared_foreground_transition() {
    let before = click_signature(100, 10, "WindowsTerminal.exe", "Terminal", 1);
    let after = click_signature(
        200,
        20,
        "chrome.exe",
        "Device Activation - Google Chrome",
        1,
    );
    let policy = ForegroundChangePolicy {
        allow: true,
        expected_process_regex: Some(regex::Regex::new("^chrome\\.exe$").unwrap()),
        expected_process_pattern: Some("^chrome\\.exe$".to_owned()),
        expected_title_regex: Some(regex::Regex::new("Device Activation").unwrap()),
        expected_title_pattern: Some("Device Activation".to_owned()),
    };

    let postcondition = verify_captured_action_delta(
        "act_press",
        "foreground_focused_ui_or_pixels",
        250,
        before,
        after,
        None,
        policy,
    )
    .expect("declared foreground transition should satisfy verify_delta");

    assert_eq!(postcondition.status, "observed_delta");
    assert_eq!(postcondition.observed_delta, Some(true));
    assert!(
        postcondition
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("expected foreground transition"))
    );
}

#[test]
fn act_press_verify_delta_rejects_declared_transition_to_wrong_title() {
    let before = click_signature(100, 10, "WindowsTerminal.exe", "Terminal", 1);
    let after = click_signature(200, 20, "chrome.exe", "New Tab - Google Chrome", 1);
    let policy = ForegroundChangePolicy {
        allow: true,
        expected_process_regex: Some(regex::Regex::new("^chrome\\.exe$").unwrap()),
        expected_process_pattern: Some("^chrome\\.exe$".to_owned()),
        expected_title_regex: Some(regex::Regex::new("Device Activation").unwrap()),
        expected_title_pattern: Some("Device Activation".to_owned()),
    };

    let error = verify_captured_action_delta(
        "act_press",
        "foreground_focused_ui_or_pixels",
        250,
        before,
        after,
        None,
        policy,
    )
    .expect_err("wrong foreground title must fail closed");
    let data = error.data.as_ref().expect("structured error data");

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::ACTION_FOREGROUND_LOST)
    );
    assert_eq!(
        data.get("reason").and_then(Value::as_str),
        Some("foreground_change_policy_mismatch")
    );
    assert_eq!(
        data.pointer("/matches/process").and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        data.pointer("/matches/title").and_then(Value::as_bool),
        Some(false)
    );
}

#[test]
fn act_press_foreground_policy_requires_verify_delta_before_input() {
    let params = act_press_params(false, true, None, None);

    let error = act_press_foreground_change_policy(&params)
        .expect_err("foreground-change policy without verify_delta must fail before input");
    let data = error.data.as_ref().expect("structured error data");

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        data.get("reason").and_then(Value::as_str),
        Some("verify_delta_required")
    );
}

#[test]
fn act_press_foreground_policy_rejects_invalid_regex_before_input() {
    let params = act_press_params(true, true, None, Some("["));

    let error = act_press_foreground_change_policy(&params)
        .expect_err("invalid foreground regex must fail before input");
    let data = error.data.as_ref().expect("structured error data");

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        data.get("reason").and_then(Value::as_str),
        Some("invalid_expected_foreground_regex")
    );
    assert_eq!(
        data.get("field").and_then(Value::as_str),
        Some("expected_foreground_title_regex")
    );
}

#[test]
fn act_press_background_target_candidate_is_strict() {
    let mut params = act_press_params(false, false, None, None);
    params.backend = PressBackend::Auto;
    assert!(press_background_target_candidate(&params, false));

    params.backend = PressBackend::Software;
    assert!(press_background_target_candidate(&params, false));

    params.backend = PressBackend::Hardware;
    assert!(!press_background_target_candidate(&params, false));

    params.backend = PressBackend::Auto;
    assert!(!press_background_target_candidate(&params, true));

    params.verify_delta = true;
    params.allow_foreground_change = true;
    assert!(!press_background_target_candidate(&params, false));

    params.allow_foreground_change = false;
    params.expected_foreground_title_regex = Some("Chrome".to_owned());
    assert!(!press_background_target_candidate(&params, false));
}

#[test]
fn hwnd_keyboard_ctrl_a_requires_full_selection_without_text_mutation() {
    let before = hwnd_keyboard_signature("alpha beta gamma", 16, 16);
    let after_inserted_a = hwnd_keyboard_signature("alpha beta gammaa", 17, 17);

    let error = verify_hwnd_keyboard_delta_signature(
        "act_press",
        "target_hwnd_text_or_selection",
        250,
        before.clone(),
        after_inserted_a,
        HwndKeyboardExpectedEffect::SelectAll,
        "observed target HWND text/selection change after PostMessage keyboard delivery",
    )
    .expect_err("Ctrl+A must not pass when it inserts a literal a");
    let data = error.data.as_ref().expect("structured error data");

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::ACTION_POSTCONDITION_FAILED)
    );
    assert_eq!(
        data.get("detail").and_then(Value::as_str),
        Some("Ctrl+A select-all changed target text instead of preserving it")
    );

    let selected = hwnd_keyboard_signature("alpha beta gamma", 0, 16);
    let postcondition = verify_hwnd_keyboard_delta_signature(
        "act_press",
        "target_hwnd_text_or_selection",
        250,
        before,
        selected,
        HwndKeyboardExpectedEffect::SelectAll,
        "observed target HWND text/selection change after PostMessage keyboard delivery",
    )
    .expect("Ctrl+A should pass only when readback shows full selection");
    assert_eq!(postcondition.status, "observed_delta");
}

#[test]
fn hwnd_keyboard_clipboard_chord_verified_by_sequence_change() {
    // Ctrl+C/Ctrl+X leave the target text+selection unchanged; their real
    // effect is a clipboard sequence-number bump (#1331). A copy that bumps
    // the clipboard must pass; a no-op copy (no selection -> seq unchanged)
    // must fail loud as ACTION_NO_OBSERVED_DELTA, not a false success.
    let before = hwnd_keyboard_signature("alpha beta gamma", 0, 16);
    let mut after_copied = hwnd_keyboard_signature("alpha beta gamma", 0, 16);
    after_copied.clipboard_sequence = before.clipboard_sequence + 1;

    let postcondition = verify_hwnd_keyboard_delta_signature(
        "act_press",
        "target_hwnd_text_or_selection",
        250,
        before.clone(),
        after_copied,
        HwndKeyboardExpectedEffect::Clipboard,
        "observed target HWND text/selection change after PostMessage keyboard delivery",
    )
    .expect("Ctrl+C must pass when the clipboard sequence number changes");
    assert_eq!(postcondition.status, "observed_delta");

    // No-op copy: text/selection AND clipboard sequence all unchanged.
    let no_op_after = hwnd_keyboard_signature("alpha beta gamma", 0, 16);
    let error = verify_hwnd_keyboard_delta_signature(
        "act_press",
        "target_hwnd_text_or_selection",
        250,
        before,
        no_op_after,
        HwndKeyboardExpectedEffect::Clipboard,
        "observed target HWND text/selection change after PostMessage keyboard delivery",
    )
    .expect_err("a no-op copy must not report success");
    let data = error.data.as_ref().expect("structured error data");
    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::ACTION_NO_OBSERVED_DELTA)
    );
}

#[test]
fn hwnd_keyboard_printable_after_full_selection_requires_exact_replacement() {
    let before = hwnd_keyboard_signature("alpha beta gamma", 0, 16);
    let wrong_after = hwnd_keyboard_signature("alpha beta gammaz", 17, 17);

    let error = verify_hwnd_keyboard_delta_signature(
        "act_press",
        "target_hwnd_text_or_selection",
        250,
        before.clone(),
        wrong_after,
        HwndKeyboardExpectedEffect::PrintableText {
            text: "z".to_owned(),
        },
        "observed target HWND text/selection change after PostMessage keyboard delivery",
    )
    .expect_err("full-selection replacement must match the emitted character");
    let data = error.data.as_ref().expect("structured error data");

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::ACTION_POSTCONDITION_FAILED)
    );

    let replaced = hwnd_keyboard_signature("z", 1, 1);
    let postcondition = verify_hwnd_keyboard_delta_signature(
        "act_press",
        "target_hwnd_text_or_selection",
        250,
        before,
        replaced,
        HwndKeyboardExpectedEffect::PrintableText {
            text: "z".to_owned(),
        },
        "observed target HWND text/selection change after PostMessage keyboard delivery",
    )
    .expect("single printable key should pass when it replaces full selection exactly");
    assert_eq!(postcondition.status, "observed_delta");
}

#[test]
fn act_type_browser_url_policy_requires_verify_delta_before_input() {
    let params = act_type_params(false, Some("^file:///synapse-810\\.html$"));

    let error = act_type_browser_url_policy(&params)
        .expect_err("browser URL policy without verify_delta must fail before input");
    let data = error.data.as_ref().expect("structured error data");

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        data.get("reason").and_then(Value::as_str),
        Some("verify_delta_required")
    );
}

#[test]
fn act_type_browser_url_policy_rejects_invalid_regex_before_input() {
    let params = act_type_params(true, Some("["));

    let error = act_type_browser_url_policy(&params)
        .expect_err("invalid browser URL regex must fail before input");
    let data = error.data.as_ref().expect("structured error data");

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        data.get("reason").and_then(Value::as_str),
        Some("invalid_expected_browser_url_regex")
    );
    assert_eq!(
        data.get("field").and_then(Value::as_str),
        Some("expected_browser_url_regex")
    );
}

#[test]
fn act_type_text_signature_capture_respects_verify_delta_opt_out() {
    let params = act_type_params(false, None);

    assert!(
        !act_type_should_capture_text_signature(&params),
        "verify_delta=false must not collect foreground text signatures or run postconditions"
    );
}

#[test]
fn act_type_text_signature_capture_only_for_foreground_verify_delta() {
    let params = act_type_params(true, None);

    assert!(
        act_type_should_capture_text_signature(&params),
        "foreground act_type with verify_delta=true must keep fail-closed SoT verification"
    );
}

#[test]
fn act_type_text_signature_capture_skips_into_element_route() {
    let mut params = act_type_params(true, None);
    params.into_element = Some(
        ElementId::parse("0x1000:0000002a00000001").expect("synthetic element id must be valid"),
    );

    assert!(
        !act_type_should_capture_text_signature(&params),
        "into_element routes own background readback and must not use foreground text signatures"
    );
}

#[test]
fn act_type_background_route_recognizes_chrome_bridge_session_target() {
    let bridge = SessionTarget::Cdp {
        window_hwnd: 0x1109ee,
        cdp_target_id: "chrome-tab:600749997".to_owned(),
    };
    let raw = SessionTarget::Cdp {
        window_hwnd: 0x1109ee,
        cdp_target_id: "F295449AD3B4C764645A641045F6C68B".to_owned(),
    };
    let window = SessionTarget::Window { hwnd: 0x1109ee };

    assert_eq!(
        chrome_bridge_session_target_parts(Some(&bridge)),
        Some((0x1109ee, "chrome-tab:600749997"))
    );
    assert_eq!(chrome_bridge_session_target_parts(Some(&raw)), None);
    assert_eq!(chrome_bridge_session_target_parts(Some(&window)), None);
    assert_eq!(chrome_bridge_session_target_parts(None), None);
}

#[test]
fn act_type_chromium_fallback_requires_foreground_route_for_refused_target() {
    let mut params = act_type_params(true, None);
    params.into_element = Some(
        ElementId::parse("0x1000:0000002a00000001").expect("synthetic element id must be valid"),
    );
    let target = act_type_foreground_fallback_target(
        0x1000,
        "edit",
        Rect {
            x: 100,
            y: 200,
            w: 300,
            h: 40,
        },
    );

    println!(
        "readback=act_type_foreground_fallback_route before=into_element after=requires_foreground:{}",
        act_type_requires_foreground_route(&params, Some(&target))
    );

    assert!(act_type_requires_foreground_route(&params, Some(&target)));
    assert!(
        !act_type_requires_foreground_route(&params, None),
        "ordinary into_element routes stay target-capable unless the Chromium fallback target is detected"
    );
    params.into_element = None;
    assert!(act_type_requires_foreground_route(&params, None));
}

#[test]
fn chromium_foreground_fallback_eligibility_matches_unsafe_value_pattern_shape() {
    let metadata = act_type_element_metadata("edit", true, true, vec![UiaPattern::Value]);

    assert!(chromium_editable_value_pattern_requires_foreground_fallback("chrome.exe", &metadata));
    assert!(
        !chromium_editable_value_pattern_requires_foreground_fallback("notepad.exe", &metadata)
    );
    assert!(
        !chromium_editable_value_pattern_requires_foreground_fallback(
            "chrome.exe",
            &act_type_element_metadata("button", true, true, vec![UiaPattern::Value])
        )
    );
    assert!(
        !chromium_editable_value_pattern_requires_foreground_fallback(
            "chrome.exe",
            &act_type_element_metadata("edit", true, false, vec![UiaPattern::Value])
        )
    );
    assert!(
        !chromium_editable_value_pattern_requires_foreground_fallback(
            "chrome.exe",
            &act_type_element_metadata("edit", true, true, vec![UiaPattern::Text])
        )
    );
}

#[test]
fn act_type_foreground_fallback_focus_accepts_matching_edit_bbox() {
    let target = act_type_foreground_fallback_target(
        0x1000,
        "edit",
        Rect {
            x: 100,
            y: 200,
            w: 300,
            h: 40,
        },
    );
    let readback = act_type_signature_for_fallback(
        0x1000,
        Some("edit"),
        Some(Rect {
            x: 120,
            y: 205,
            w: 120,
            h: 30,
        }),
    );

    act_type_foreground_fallback_focus_matches_target(&target, &readback)
        .expect("intersecting focused edit bbox should identify the clicked target");
}

#[test]
fn act_type_foreground_fallback_focus_rejects_wrong_target() {
    let target = act_type_foreground_fallback_target(
        0x1000,
        "edit",
        Rect {
            x: 100,
            y: 200,
            w: 300,
            h: 40,
        },
    );
    let wrong_role = act_type_signature_for_fallback(
        0x1000,
        Some("button"),
        Some(Rect {
            x: 120,
            y: 205,
            w: 120,
            h: 30,
        }),
    );
    let wrong_bbox = act_type_signature_for_fallback(
        0x1000,
        Some("edit"),
        Some(Rect {
            x: 800,
            y: 900,
            w: 120,
            h: 30,
        }),
    );

    let role_error = act_type_foreground_fallback_focus_matches_target(&target, &wrong_role)
        .expect_err("non-edit focused role must fail closed before typing");
    let bbox_error = act_type_foreground_fallback_focus_matches_target(&target, &wrong_bbox)
        .expect_err("focused edit outside target bbox must fail closed before typing");

    assert_eq!(
        role_error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(Value::as_str),
        Some("focused_role_is_not_text_editable")
    );
    assert_eq!(
        bbox_error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(Value::as_str),
        Some("focused_element_did_not_match_target_or_bbox")
    );
}

#[test]
fn act_type_foreground_fallback_rejects_empty_target_bbox() {
    let target = act_type_foreground_fallback_target(
        0x1000,
        "edit",
        Rect {
            x: 100,
            y: 200,
            w: 0,
            h: 40,
        },
    );

    let error = act_type_target_center_point(&target)
        .expect_err("empty target bbox must fail closed before foreground input");

    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str),
        Some(error_codes::ACTION_TARGET_INVALID)
    );
}

#[test]
fn act_type_browser_url_policy_accepts_navigation_focus_change_when_url_matches() {
    let policy = act_type_browser_url_policy(&act_type_params(
        true,
        Some("^file:///C:/synapse-810-after\\.html$"),
    ))
    .expect("valid browser URL policy")
    .expect("policy should be present");
    let before = act_type_readback(
        Some("file:///C:/synapse-810-before.html"),
        Some("address-bar"),
        Some("file:///C:/synapse-810-before.html"),
    );
    let after = act_type_readback(
        Some("file:///C:/synapse-810-after.html"),
        Some("document-body"),
        None,
    );
    let response = ActTypeResponse {
        ok: true,
        chars_typed: 36,
        elapsed_ms: 10,
        backend_tier_used: "foreground".to_owned(),
        required_foreground: true,
        target_text_integrity: "dispatch_only_requires_target_readback".to_owned(),
        target_readback_required: true,
        minimum_linear_ms_per_char: 20,
        postcondition: crate::m2::postcondition::postcondition_not_requested(
            "act_type",
            "foreground_focused_ui_or_pixels",
        ),
    };

    let verified = verify_act_type_browser_url_response(
        response,
        before,
        after,
        "before-hash".to_owned(),
        "after-hash".to_owned(),
        250,
        &policy,
    )
    .expect("matching browser URL should verify despite focus moving to the document");

    assert_eq!(verified.postcondition.status, "observed_delta");
    assert_eq!(verified.postcondition.observed_delta, Some(true));
    assert_eq!(
        verified.postcondition.source_of_truth.as_deref(),
        Some(ACT_TYPE_BROWSER_URL_SOURCE_OF_TRUTH)
    );
    assert_eq!(
        verified.target_text_integrity,
        ACT_TYPE_BROWSER_URL_TEXT_INTEGRITY
    );
    assert!(!verified.target_readback_required);
}

#[test]
fn act_type_verify_delta_accepts_cdp_active_element_text_surface() {
    let before = act_type_text_readback_with_source(
        None,
        Some("document"),
        Some("draft"),
        Some(ACT_TYPE_TEXT_SOURCE_CDP_ACTIVE),
    );
    let after = act_type_text_readback_with_source(
        None,
        Some("document"),
        Some("draft issue786-cdp-text"),
        Some(ACT_TYPE_TEXT_SOURCE_CDP_ACTIVE),
    );
    let response = act_type_response_for_verify_delta();

    let verified = verify_act_type_text_response(
        response,
        before,
        after,
        "before-cdp-hash".to_owned(),
        "after-cdp-hash".to_owned(),
        250,
        "issue786-cdp-text",
    )
    .expect("CDP active-element text readback should satisfy act_type verify_delta");

    assert_eq!(verified.postcondition.status, "observed_delta");
    assert_eq!(verified.postcondition.observed_delta, Some(true));
    assert_eq!(
        verified.postcondition.source_of_truth.as_deref(),
        Some("foreground_text_readback:cdp_active_element_value")
    );
    assert_eq!(
        verified.target_text_integrity,
        "verify_delta_text_readback:cdp_active_element_value"
    );
    assert!(!verified.target_readback_required);
}

#[test]
fn act_type_verify_delta_keeps_no_delta_distinct_from_no_surface() {
    let before = act_type_text_readback_with_source(
        None,
        Some("document"),
        Some("unchanged"),
        Some(ACT_TYPE_TEXT_SOURCE_CDP_ACTIVE),
    );
    let after = before.clone();
    let response = act_type_response_for_verify_delta();

    let error = verify_act_type_text_response(
        response,
        before,
        after,
        "before-same-hash".to_owned(),
        "after-same-hash".to_owned(),
        250,
        "issue786",
    )
    .expect_err("same CDP active-element text must be verified no-delta, not no-surface");
    let data = error.data.as_ref().expect("structured error data");

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::ACTION_NO_OBSERVED_DELTA)
    );
}

#[test]
fn act_type_verify_polling_keeps_target_switch_terminal() {
    let before = act_type_text_readback_with_source(
        None,
        Some("title-field"),
        Some("draft"),
        Some(ACT_TYPE_TEXT_SOURCE_UIA_VALUE),
    );
    let after_same_target = act_type_text_readback_with_source(
        None,
        Some("title-field"),
        Some("draft issue880"),
        Some(ACT_TYPE_TEXT_SOURCE_UIA_VALUE),
    );
    let after_switched_target = act_type_text_readback_with_source(
        None,
        Some("description-field"),
        Some("draft issue880"),
        Some(ACT_TYPE_TEXT_SOURCE_UIA_VALUE),
    );

    println!(
        "readback=act_type_verify_polling same_target_terminal={} switched_target_terminal={}",
        act_type_text_terminal_failure(&before, &after_same_target),
        act_type_text_terminal_failure(&before, &after_switched_target)
    );
    assert!(!act_type_text_terminal_failure(&before, &after_same_target));
    assert!(act_type_text_terminal_failure(
        &before,
        &after_switched_target
    ));
}

#[test]
fn act_type_verify_delta_reports_distinct_surface_unavailable_code() {
    let no_surface = act_type_text_readback_with_source(None, Some("document"), None, None);

    let error = act_type_verify_surface_unavailable_error(
        "synthetic no-surface regression",
        "no-surface-hash".to_owned(),
        no_surface.signature,
    );
    let data = error.data.as_ref().expect("structured error data");

    assert_eq!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::ACTION_VERIFY_SURFACE_UNAVAILABLE)
    );
    assert_ne!(
        data.get("code").and_then(Value::as_str),
        Some(error_codes::ACTION_NO_OBSERVED_DELTA)
    );
    assert!(act_type_error_allows_visual_delta(&error));
}

#[test]
fn act_type_visual_delta_only_reconciles_missing_text_surface() {
    let no_surface_before = act_type_text_readback_with_source(None, Some("document"), None, None);
    let no_surface_after = no_surface_before.clone();
    let response = act_type_response_for_verify_delta();

    let no_surface_error = verify_act_type_text_response(
        response.clone(),
        no_surface_before,
        no_surface_after,
        "before-no-surface".to_owned(),
        "after-no-surface".to_owned(),
        250,
        "issue1368",
    )
    .expect_err("missing text surface should not be semantically verified");
    assert!(
        act_type_error_allows_visual_delta(&no_surface_error),
        "missing semantic surface can be reconciled by a separate visual SoT"
    );

    let unchanged_text_before = act_type_text_readback_with_source(
        None,
        Some("document"),
        Some("unchanged"),
        Some(ACT_TYPE_TEXT_SOURCE_UIA_VALUE),
    );
    let unchanged_text_after = unchanged_text_before.clone();
    let unchanged_text_error = verify_act_type_text_response(
        response,
        unchanged_text_before,
        unchanged_text_after,
        "before-text".to_owned(),
        "after-text".to_owned(),
        250,
        "issue1368",
    )
    .expect_err("unchanged real text surface should remain fail-closed");
    assert!(
        !act_type_error_allows_visual_delta(&unchanged_text_error),
        "visual delta must not cover up a real semantic text no-op"
    );
}

#[test]
fn act_type_visual_delta_target_window_params_fail_closed() {
    for invalid in [-1, 0, i64::from(u32::MAX) + 1, i64::MAX] {
        let mut params = act_type_params(true, None);
        params.verify_target_window_hwnd = Some(invalid);
        let error = act_type_visual_delta_target_window(&params, None)
            .expect_err("noncanonical visual verification HWND must fail before capture");
        let data = error.data.as_ref().expect("structured HWND error data");
        assert_eq!(
            data.get("field").and_then(Value::as_str),
            Some("verify_target_window_hwnd")
        );
        assert_eq!(
            data.get("actual_value").and_then(Value::as_i64),
            Some(invalid)
        );
    }

    let mut canonical_max = act_type_params(true, None);
    canonical_max.verify_target_window_hwnd = Some(i64::from(u32::MAX));
    assert_eq!(
        act_type_visual_delta_target_window(&canonical_max, None)
            .expect("u32::MAX is a canonical HWND wire value"),
        Some(i64::from(u32::MAX))
    );

    let mut params = act_type_params(false, None);
    params.verify_target_window_hwnd = Some(0x1234);
    let error = act_type_visual_delta_target_window(&params, None)
        .expect_err("target HWND visual verification requires verify_delta");
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(Value::as_str),
        Some("verify_delta_required")
    );

    let mut params = act_type_params(true, Some("^https://example\\.test/$"));
    params.verify_target_window_hwnd = Some(0x1234);
    let policy = act_type_browser_url_policy(&params)
        .expect("valid URL policy should compile before conflict check");
    let error = act_type_visual_delta_target_window(&params, policy.as_ref())
        .expect_err("visual target HWND cannot replace URL verification");
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(Value::as_str),
        Some("conflicting_postconditions")
    );
}

#[test]
fn act_type_visual_delta_postcondition_uses_target_window_pixels() {
    let before = click_signature(100, 10, "egui-test.exe", "Synthetic Editor", 1);
    let after = click_signature(100, 10, "egui-test.exe", "Synthetic Editor", 2);
    let postcondition = verify_captured_action_delta(
        "act_type",
        "target_window_ui_or_pixels",
        250,
        before,
        after,
        None,
        ForegroundChangePolicy::reject(),
    )
    .expect("target-window visual delta should verify");

    assert_eq!(
        postcondition.source_of_truth.as_deref(),
        Some("target_window_ui_or_pixels")
    );
    assert_eq!(postcondition.observed_delta, Some(true));
}

#[test]
fn act_type_text_readback_prefers_editable_cdp_when_uia_is_browser_shell_url() {
    let focused = act_type_focused_candidate("document", Some("data:text/html,issue786"));
    let cdp = cdp_active_text_readback_for_test(Some("alpha issue786"), true, "DIV");
    let ocr = ocr_text_readback_for_test(Some("visible page words"));

    let (value, source) = choose_act_type_text_readback(
        Some(&focused),
        Some("data:text/html,issue786".to_owned()),
        Some(ACT_TYPE_TEXT_SOURCE_UIA_VALUE),
        &cdp,
        &ocr,
    );

    assert_eq!(value.as_deref(), Some("alpha issue786"));
    assert_eq!(source.as_deref(), Some(ACT_TYPE_TEXT_SOURCE_CDP_ACTIVE));
}

#[test]
fn act_type_text_readback_rejects_browser_shell_url_without_editable_cdp() {
    let focused = act_type_focused_candidate("document", Some("data:text/html,issue786"));
    let cdp = cdp_active_text_readback_for_test(None, false, "BODY");
    let ocr = ocr_text_readback_for_test(Some("visible page words"));

    let (value, source) = choose_act_type_text_readback(
        Some(&focused),
        Some("data:text/html,issue786".to_owned()),
        Some(ACT_TYPE_TEXT_SOURCE_UIA_VALUE),
        &cdp,
        &ocr,
    );

    assert_eq!(value, None);
    assert_eq!(source, None);
}

#[test]
fn act_type_text_readback_prefers_editable_cdp_over_empty_uia_text_placeholder() {
    let focused = act_type_focused_candidate("group", None);
    let cdp = cdp_active_text_readback_for_test(Some("alpha issue786"), true, "DIV");
    let ocr = ocr_text_readback_for_test(None);

    let (value, source) = choose_act_type_text_readback(
        Some(&focused),
        Some(String::new()),
        Some(ACT_TYPE_TEXT_SOURCE_UIA_EMPTY),
        &cdp,
        &ocr,
    );

    assert_eq!(value.as_deref(), Some("alpha issue786"));
    assert_eq!(source.as_deref(), Some(ACT_TYPE_TEXT_SOURCE_CDP_ACTIVE));
}

#[test]
fn act_type_text_readback_keeps_real_uia_edit_control_authoritative() {
    let focused = act_type_focused_candidate("Edit", Some("https://example.test/search"));
    let cdp = cdp_active_text_readback_for_test(Some("dom editor text"), true, "DIV");
    let ocr = ocr_text_readback_for_test(Some("visible words"));

    let (value, source) = choose_act_type_text_readback(
        Some(&focused),
        Some("https://example.test/search".to_owned()),
        Some(ACT_TYPE_TEXT_SOURCE_UIA_VALUE),
        &cdp,
        &ocr,
    );

    assert_eq!(value.as_deref(), Some("https://example.test/search"));
    assert_eq!(source.as_deref(), Some(ACT_TYPE_TEXT_SOURCE_UIA_VALUE));
}

#[test]
fn click_router_respects_coordinate_fallback_disabled() {
    let mut params = act_click_element_params();
    params.use_invoke_pattern = true;
    params.coordinate_fallback_on_unsupported = false;

    let can_route = can_route_click_element_background_first(&params, None);

    assert!(!can_route);
}

#[test]
fn click_router_keeps_direct_coordinate_element_route() {
    let mut params = act_click_element_params();
    params.use_invoke_pattern = false;
    params.coordinate_fallback_on_unsupported = false;

    let can_route = can_route_click_element_background_first(&params, None);

    assert!(can_route);
}

#[test]
fn click_router_advances_without_replaying_attempted_tiers() {
    let uia_failed = click_attempt(
        "uia",
        "failed",
        Some(error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED),
    );
    assert!(should_try_click_postmessage_tier(std::slice::from_ref(
        &uia_failed
    )));
    assert!(!should_try_click_foreground_tier(std::slice::from_ref(
        &uia_failed
    )));

    let postmessage_no_delta = click_attempt(
        CLICK_TIER_POSTMESSAGE,
        "failed",
        Some(error_codes::ACTION_NO_OBSERVED_DELTA),
    );
    let after_postmessage = vec![uia_failed, postmessage_no_delta];
    assert!(!should_try_click_postmessage_tier(&after_postmessage));
    assert!(should_try_click_foreground_tier(&after_postmessage));

    let foreground_no_delta = click_attempt(
        CLICK_TIER_FOREGROUND,
        "failed",
        Some(error_codes::ACTION_NO_OBSERVED_DELTA),
    );
    let exhausted = vec![
        click_attempt(
            "uia",
            "failed",
            Some(error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED),
        ),
        click_attempt(
            CLICK_TIER_POSTMESSAGE,
            "failed",
            Some(error_codes::ACTION_NO_OBSERVED_DELTA),
        ),
        foreground_no_delta,
    ];
    assert!(!should_try_click_postmessage_tier(&exhausted));
    assert!(!should_try_click_foreground_tier(&exhausted));
}

#[test]
fn click_router_treats_toggle_readback_failure_as_postdispatch_retry_eligible() {
    let error = postdispatch_click_error(
        "accessibility backend failed: TogglePattern.toggle returned for element 0x1:0000002a00000001, but ToggleState stayed Off",
    );

    println!(
        "readback=act_click_postdispatch edge=toggle detail={:?} retry={}",
        error.message,
        should_try_next_click_tier(&error)
    );
    assert!(click_postdispatch_readback_failed(&error));
    assert!(should_try_next_click_tier(&error));
}

#[test]
fn click_router_recognizes_toggle_readback_failure_when_background_route_disabled() {
    let mut params = act_click_element_params();
    params.use_invoke_pattern = true;
    params.coordinate_fallback_on_unsupported = false;
    let error = postdispatch_click_error(
        "accessibility backend failed: TogglePattern.toggle returned for element 0x1:0000002a00000001, but ToggleState stayed Off",
    );

    println!(
        "readback=act_click_postdispatch edge=toggle_background_route_disabled can_route={} reconcile={}",
        can_route_click_element_background_first(&params, None),
        click_postdispatch_readback_failed(&error)
    );
    assert!(!can_route_click_element_background_first(&params, None));
    assert!(click_postdispatch_readback_failed(&error));
}

#[test]
fn click_router_keeps_generic_target_invalid_fail_closed() {
    let error = postdispatch_click_error("element bbox is empty or inverted");

    println!(
        "readback=act_click_postdispatch edge=generic_target_invalid detail={:?} retry={}",
        error.message,
        should_try_next_click_tier(&error)
    );
    assert!(!click_postdispatch_readback_failed(&error));
    assert!(!should_try_next_click_tier(&error));
}

#[test]
fn auto_wait_failure_filter_matches_action_requirement() {
    let predicates = [
        "attached",
        "visible",
        "stable",
        "enabled",
        "receives_events",
    ];
    for predicate in predicates {
        assert!(actionability_failure_is_relevant(
            predicate,
            ActionabilityAutoWaitRequirement::Action
        ));
    }
    assert!(!actionability_failure_is_relevant(
        "editable",
        ActionabilityAutoWaitRequirement::Action
    ));
}

#[test]
fn auto_wait_failure_filter_includes_editable_for_text_requirement() {
    assert!(actionability_failure_is_relevant(
        "editable",
        ActionabilityAutoWaitRequirement::Editable
    ));
    assert!(actionability_failure_is_relevant(
        "receives_events",
        ActionabilityAutoWaitRequirement::Editable
    ));
}

fn foreground_proof(
    hwnd: i64,
    pid: u32,
    process_name: &str,
    window_title: &str,
) -> ForegroundProof {
    ForegroundProof {
        hwnd,
        pid,
        process_name: process_name.to_owned(),
        process_path: format!(r"C:\test\{process_name}"),
        window_title: window_title.to_owned(),
        is_minimized: Some(false),
        minimized_readback_error: None,
        observed_profile_id: None,
    }
}

fn foreground_context(
    hwnd: i64,
    pid: u32,
    process_name: &str,
    window_title: &str,
) -> ForegroundContext {
    ForegroundContext {
        hwnd,
        pid,
        process_name: process_name.to_owned(),
        process_path: format!(r"C:\test\{process_name}"),
        window_title: window_title.to_owned(),
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

fn act_press_params(
    verify_delta: bool,
    allow_foreground_change: bool,
    expected_process_regex: Option<&str>,
    expected_title_regex: Option<&str>,
) -> ActPressParams {
    ActPressParams {
        keys: vec!["enter".to_owned()],
        hold_ms: 33,
        backend: crate::m2::PressBackend::Auto,
        verify_delta,
        allow_foreground_change,
        expected_foreground_process_regex: expected_process_regex.map(str::to_owned),
        expected_foreground_title_regex: expected_title_regex.map(str::to_owned),
        verify_timeout_ms: crate::m2::default_verify_timeout_ms(),
        window_hwnd: None,
        cdp_target_id: None,
        auto_wait: false,
        auto_wait_timeout_ms: crate::m2::default_auto_wait_timeout_ms(),
        auto_wait_element_id: None,
    }
}

fn act_type_params(verify_delta: bool, expected_browser_url_regex: Option<&str>) -> ActTypeParams {
    serde_json::from_value(json!({
        "text": "file:///C:/synapse-810-after.html",
        "dynamics": "burst",
        "press_enter_after": true,
        "backend": "auto",
        "verify_delta": verify_delta,
        "expected_browser_url_regex": expected_browser_url_regex,
        "verify_timeout_ms": crate::m2::default_verify_timeout_ms(),
    }))
    .expect("synthetic act_type params must deserialize through the public tool schema")
}

fn act_type_readback(
    browser_url: Option<&str>,
    focused_element_id: Option<&str>,
    focused_value: Option<&str>,
) -> ActTypeTextReadback {
    act_type_text_readback_with_source(
        browser_url,
        focused_element_id,
        focused_value,
        focused_value.map(|_| "focused.value"),
    )
}

fn act_type_text_readback_with_source(
    browser_url: Option<&str>,
    focused_element_id: Option<&str>,
    focused_value: Option<&str>,
    readback_source: Option<&str>,
) -> ActTypeTextReadback {
    let focused_value = focused_value.map(str::to_owned);
    let browser_url_owned = browser_url.map(str::to_owned);
    ActTypeTextReadback {
        signature: ActTypeTextSignature {
            foreground_hwnd: 100,
            foreground_pid: 20,
            foreground_process: "chrome.exe".to_owned(),
            foreground_title_sha256: non_empty_sha256("Synthetic - Google Chrome"),
            focused_element_id: focused_element_id.map(str::to_owned),
            focused_role: focused_element_id.map(|_| "Edit".to_owned()),
            focused_name_sha256: focused_element_id.and_then(non_empty_sha256),
            focused_value_len: focused_value.as_ref().map(|value| value.chars().count()),
            focused_value_sha256: focused_value.as_deref().and_then(non_empty_sha256),
            focused_selected_text_sha256: None,
            focused_bbox: Some(Rect {
                x: 10,
                y: 10,
                w: 400,
                h: 32,
            }),
            readback_source: readback_source.map(str::to_owned),
            has_text_readback: focused_value.is_some(),
            text_readback_attempts: vec![
                readback_source
                    .map(|source| format!("{source}:available"))
                    .unwrap_or_else(|| "all_text_surfaces:unavailable".to_owned()),
            ],
            cdp_status: Some("ok".to_owned()),
            cdp_endpoint_present: true,
            cdp_selected_target_id: Some("TARGET810".to_owned()),
            cdp_active_has_element: Some(true),
            cdp_active_is_editable: Some(true),
            cdp_active_tag_name: Some("DIV".to_owned()),
            cdp_active_id_sha256: non_empty_sha256("issue786-editor"),
            cdp_active_name_sha256: None,
            cdp_active_value_len: focused_value.as_ref().map(|value| value.chars().count()),
            cdp_active_value_sha256: focused_value.as_deref().map(text_sha256),
            cdp_active_error_code: None,
            cdp_active_error_detail_sha256: None,
            ocr_word_count: 0,
            ocr_text_len: None,
            ocr_text_sha256: None,
            web_path: None,
            browser_url_len: browser_url_owned
                .as_ref()
                .map(|value| value.chars().count()),
            browser_url_sha256: browser_url_owned.as_deref().and_then(non_empty_sha256),
            browser_cdp_target_id: Some("TARGET810".to_owned()),
            browser_url_readback_source: Some("Target.getTargets".to_owned()),
            browser_title_sha256: non_empty_sha256("Synthetic - Google Chrome"),
            browser_ready_state: Some("complete".to_owned()),
            browser_tab_active: Some(false),
        },
        value: focused_value,
        browser_url: browser_url_owned,
    }
}

fn act_type_element_metadata(
    role: &str,
    enabled: bool,
    keyboard_focusable: bool,
    patterns: Vec<UiaPattern>,
) -> synapse_a11y::ElementMetadataReadback {
    synapse_a11y::ElementMetadataReadback {
        name: "synthetic chrome edit".to_owned(),
        role: role.to_owned(),
        automation_id: Some("synthetic-input".to_owned()),
        bbox: Rect {
            x: 100,
            y: 200,
            w: 300,
            h: 40,
        },
        enabled,
        keyboard_focusable,
        patterns,
        value: Some("before".to_owned()),
    }
}

fn act_type_foreground_fallback_target(
    root_hwnd: i64,
    role: &str,
    bbox: Rect,
) -> ActTypeForegroundFallbackTarget {
    ActTypeForegroundFallbackTarget {
        element_id: format!("0x{root_hwnd:x}:0000002a00000001"),
        root_hwnd,
        process_name: "chrome.exe".to_owned(),
        role: role.to_owned(),
        automation_id_present: true,
        bbox,
        enabled: true,
        keyboard_focusable: true,
        patterns: vec![UiaPattern::Value, UiaPattern::Text],
        name_len: "synthetic chrome edit".chars().count(),
        value_len: Some("before".chars().count()),
    }
}

fn act_type_signature_for_fallback(
    foreground_hwnd: i64,
    focused_role: Option<&str>,
    focused_bbox: Option<Rect>,
) -> ActTypeTextSignature {
    ActTypeTextSignature {
        foreground_hwnd,
        foreground_pid: 20,
        foreground_process: "chrome.exe".to_owned(),
        foreground_title_sha256: non_empty_sha256("Synthetic - Google Chrome"),
        focused_element_id: focused_role.map(|_| format!("0x{foreground_hwnd:x}:0000002a00000002")),
        focused_role: focused_role.map(str::to_owned),
        focused_name_sha256: focused_role.and_then(non_empty_sha256),
        focused_value_len: Some("before".chars().count()),
        focused_value_sha256: Some(text_sha256("before")),
        focused_selected_text_sha256: None,
        focused_bbox,
        readback_source: Some(ACT_TYPE_TEXT_SOURCE_UIA_VALUE.to_owned()),
        has_text_readback: true,
        text_readback_attempts: vec![format!("{ACT_TYPE_TEXT_SOURCE_UIA_VALUE}:available")],
        cdp_status: None,
        cdp_endpoint_present: false,
        cdp_selected_target_id: None,
        cdp_active_has_element: None,
        cdp_active_is_editable: None,
        cdp_active_tag_name: None,
        cdp_active_id_sha256: None,
        cdp_active_name_sha256: None,
        cdp_active_value_len: None,
        cdp_active_value_sha256: None,
        cdp_active_error_code: None,
        cdp_active_error_detail_sha256: None,
        ocr_word_count: 0,
        ocr_text_len: None,
        ocr_text_sha256: None,
        web_path: Some("uia_only".to_owned()),
        browser_url_len: None,
        browser_url_sha256: None,
        browser_cdp_target_id: None,
        browser_url_readback_source: None,
        browser_title_sha256: None,
        browser_ready_state: None,
        browser_tab_active: None,
    }
}

fn act_type_focused_candidate(role: &str, value: Option<&str>) -> ActTypeFocusedTextCandidate {
    ActTypeFocusedTextCandidate {
        element_id: "issue786-focused".to_owned(),
        role: role.to_owned(),
        name: String::new(),
        selected_text: None,
        bbox: Rect {
            x: 10,
            y: 10,
            w: 400,
            h: 40,
        },
        value: value.map(str::to_owned),
        patterns: Vec::new(),
    }
}

fn cdp_active_text_readback_for_test(
    value: Option<&str>,
    is_editable: bool,
    tag_name: &str,
) -> CdpActiveTextReadback {
    CdpActiveTextReadback {
        value: value.map(str::to_owned),
        target_id: Some("TARGET810".to_owned()),
        has_active_element: Some(true),
        is_editable: Some(is_editable),
        tag_name: Some(tag_name.to_owned()),
        id_sha256: non_empty_sha256("issue786-editor"),
        name_sha256: None,
        value_len: value.map(|value| value.chars().count()),
        value_sha256: value.map(text_sha256),
        error_code: None,
        error_detail_sha256: None,
        attempt: if value.is_some() {
            "cdp_active_element_value:available".to_owned()
        } else {
            "cdp_active_element_value:unavailable:active_element_not_editable".to_owned()
        },
    }
}

fn ocr_text_readback_for_test(value: Option<&str>) -> OcrTextReadback {
    OcrTextReadback {
        value: value.map(str::to_owned),
        word_count: value
            .map(|value| value.split_whitespace().count())
            .unwrap_or(0),
        value_len: value.map(|value| value.chars().count()),
        value_sha256: value.map(text_sha256),
        attempt: if value.is_some() {
            "ocr_focused_rect_text:available".to_owned()
        } else {
            "ocr_focused_rect_text:unavailable:no_ocr_words_in_focused_bbox".to_owned()
        },
    }
}

fn act_type_response_for_verify_delta() -> ActTypeResponse {
    ActTypeResponse {
        ok: true,
        chars_typed: 16,
        elapsed_ms: 10,
        backend_tier_used: "foreground".to_owned(),
        required_foreground: true,
        target_text_integrity: "dispatch_only_requires_target_readback".to_owned(),
        target_readback_required: true,
        minimum_linear_ms_per_char: 20,
        postcondition: crate::m2::postcondition::postcondition_not_requested(
            "act_type",
            "foreground_focused_ui_or_pixels",
        ),
    }
}

fn act_click_element_params() -> ActClickParams {
    serde_json::from_value(json!({
        "target": {
            "element_id": "0x1000:0000002a00000001"
        },
        "verify_delta": true
    }))
    .expect("synthetic act_click params must deserialize through the public tool schema")
}

fn postdispatch_click_error(detail: &str) -> ErrorData {
    let tier_attempts = vec![ActClickTierAttempt {
        tier: "uia".to_owned(),
        status: "failed".to_owned(),
        reason_code: Some("target_invalid".to_owned()),
        error_code: Some(error_codes::ACTION_TARGET_INVALID.to_owned()),
        detail: Some(detail.to_owned()),
        required_foreground: false,
    }];
    ErrorData::new(
        ErrorCode(-32099),
        format!("action target invalid: {detail}"),
        Some(json!({
            "code": error_codes::ACTION_TARGET_INVALID,
            "tier_attempts": tier_attempts,
        })),
    )
}

fn click_attempt(tier: &str, status: &str, error_code: Option<&str>) -> ActClickTierAttempt {
    ActClickTierAttempt {
        tier: tier.to_owned(),
        status: status.to_owned(),
        reason_code: error_code.map(str::to_owned),
        error_code: error_code.map(str::to_owned),
        detail: Some("synthetic regression attempt".to_owned()),
        required_foreground: tier == CLICK_TIER_FOREGROUND,
    }
}

fn hwnd_keyboard_signature(
    text: &str,
    selection_start: u32,
    selection_end: u32,
) -> HwndKeyboardDeltaSignature {
    HwndKeyboardDeltaSignature {
        target: HwndKeyboardTargetState {
            root_hwnd: 0x1000,
            hwnd: 0x2000,
            class_name: "WindowsForms10.EDIT.synthetic".to_owned(),
            text_len: Some(text.chars().count()),
            text_sha256: Some(text_sha256(text)),
            selection_start: Some(selection_start),
            selection_end: Some(selection_end),
        },
        clipboard_sequence: 0,
    }
}

fn click_signature(
    hwnd: i64,
    pid: u32,
    process_name: &str,
    window_title: &str,
    element_count: usize,
) -> ClickDeltaSignature {
    ClickDeltaSignature {
        foreground_hwnd: hwnd,
        foreground_pid: pid,
        foreground_process: process_name.to_owned(),
        foreground_title: window_title.to_owned(),
        foreground_title_sha256: non_empty_sha256(window_title),
        focused_element_id: Some("focused.synthetic".to_owned()),
        focused_role: Some("Edit".to_owned()),
        focused_name_sha256: non_empty_sha256("synthetic focus"),
        focused_value_sha256: non_empty_sha256("synthetic value"),
        focused_bbox: Some(Rect {
            x: 1,
            y: 2,
            w: 300,
            h: 40,
        }),
        element_count,
        elements_sha256: format!("elements-{element_count}"),
        cdp_status: Some("unavailable".to_owned()),
        cdp_endpoint_present: false,
        web_path: None,
        cursor_position: None,
        pixel: ClickPixelSignature {
            status: "synthetic".to_owned(),
            region: Rect {
                x: 0,
                y: 0,
                w: 800,
                h: 600,
            },
            bitmap_sha256: Some("pixel-signature".to_owned()),
            detail: Some("synthetic pixel signature".to_owned()),
        },
        point_pixel: None,
    }
}
