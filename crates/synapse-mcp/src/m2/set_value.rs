use std::time::{Duration, Instant};

use rmcp::{ErrorData, model::ErrorCode, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::{ElementId, error_codes};

use crate::{
    m1::mcp_error,
    m2::postcondition::{ActPostcondition, default_verify_timeout_ms, text_signature},
};

const TOOL: &str = "act_set_value";
const SOURCE_UIA_VALUE: &str = "uia_value_pattern.value";
const SOURCE_UIA_PASSWORD_LENGTH: &str = "uia_value_pattern.password_length";
const SOURCE_NATIVE_TEXT: &str = "win32_window_text";
const SOURCE_NATIVE_PASSWORD_LENGTH: &str = "win32_window_text.password_length";
const METHOD_NATIVE_TEXT_MESSAGE: &str = "uia_native_window_text_message";
const TEXT_INTEGRITY_UIA_VALUE_PATTERN: &str = "uia_value_pattern_readback";
const TEXT_INTEGRITY_UIA_PASSWORD_LENGTH: &str = "uia_value_pattern_password_length_readback";
const TEXT_INTEGRITY_NATIVE_TEXT_MESSAGE: &str = "win32_wm_settext_readback";
const TEXT_INTEGRITY_NATIVE_PASSWORD_LENGTH: &str = "win32_wm_settext_password_length_readback";
const BACKEND_ROUTER_NATIVE_EDIT_OR_UIA: &str = "native_edit_wm_settext_then_uia_value_pattern";

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActSetValueParams {
    pub element_id: ElementId,
    pub text: String,
    #[serde(default = "default_verify_timeout_ms")]
    #[schemars(default = "default_verify_timeout_ms", range(min = 50, max = 5000))]
    pub verify_timeout_ms: u32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActSetValueResponse {
    pub ok: bool,
    pub method: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub source_of_truth: String,
    pub requested_len: u32,
    pub before_len: u32,
    pub after_len: u32,
    pub requested_sha256: String,
    pub before_sha256: String,
    pub after_sha256: String,
    pub changed: bool,
    pub target_text_integrity: String,
    pub target_readback_required: bool,
    pub postcondition: ActPostcondition,
    pub elapsed_ms: u32,
}

pub(crate) async fn act_set_value_with_boundary(
    params: ActSetValueParams,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActSetValueResponse, ErrorData> {
    let started = Instant::now();
    validate_set_value_params(&params)?;
    let requested_len = char_count(&params.text)?;
    let requested_sha256 = text_signature(&params.text);

    let before = synapse_a11y::element_value(&params.element_id).map_err(|error| {
        a11y_error_to_set_value_mcp(&params.element_id, "before_value_read", error, None)
    })?;
    let before_sha256 = text_signature(&before.value);
    let before_len = char_count(&before.value)?;
    let before_readback = value_readback_json(&params.element_id, &before);

    boundary.ensure("immediately_before_set_element_value")?;
    let set_readback = match synapse_a11y::set_element_value(&params.element_id, &params.text) {
        Ok(readback) => readback,
        Err(error) => {
            tokio::time::sleep(Duration::from_millis(u64::from(params.verify_timeout_ms))).await;
            let after = synapse_a11y::element_value(&params.element_id).ok();
            return Err(set_failed_error(
                &params,
                &before,
                &requested_sha256,
                requested_len,
                error,
                after,
            ));
        }
    };

    tokio::time::sleep(Duration::from_millis(u64::from(params.verify_timeout_ms))).await;
    let after = synapse_a11y::element_value(&params.element_id).map_err(|error| {
        a11y_error_to_set_value_mcp(
            &params.element_id,
            "after_value_read",
            error,
            Some(before_readback.clone()),
        )
    })?;
    let after_sha256 = text_signature(&after.value);
    let after_len = char_count(&after.value)?;
    let changed = !value_readbacks_equivalent(&before, &after);
    let source_of_truth = set_source_of_truth(&set_readback);
    let target_text_integrity = set_text_integrity(&set_readback);

    if !after_matches_requested(&after, &set_readback, &params.text) {
        tracing::error!(
            code = error_codes::ACTION_POSTCONDITION_FAILED,
            tool = TOOL,
            element_id = %params.element_id,
            source_of_truth,
            method = %set_readback.method,
            before_sha256,
            after_sha256,
            requested_sha256,
            before_len,
            after_len,
            requested_len,
            expected_len = expected_set_value(&set_readback, &params.text).chars().count(),
            immediate_after_len = set_readback.after_value.chars().count(),
            "act_set_value background set tier returned, but separate target readback did not equal requested text"
        );
        return Err(postcondition_failed_set_value_error(
            &params,
            &before,
            &after,
            &set_readback,
            "separate target readback did not equal requested text after set_value",
        ));
    }

    tracing::info!(
        code = "M2_ACT_SET_VALUE_READBACK",
        element_id = %params.element_id,
        method = %set_readback.method,
        before_len,
        after_len,
        requested_len,
        changed,
        source_of_truth,
        "readback=act_set_value method={} before_len={} after_len={} requested_len={} changed={}",
        set_readback.method,
        before_len,
        after_len,
        requested_len,
        changed
    );

    Ok(ActSetValueResponse {
        ok: true,
        backend_tier_used: set_backend_tier(&set_readback).to_owned(),
        required_foreground: false,
        method: set_readback.method,
        source_of_truth: source_of_truth.to_owned(),
        requested_len,
        before_len,
        after_len,
        requested_sha256,
        before_sha256: before_sha256.clone(),
        after_sha256: after_sha256.clone(),
        changed,
        target_text_integrity: target_text_integrity.to_owned(),
        target_readback_required: false,
        postcondition: postcondition_verified_state(
            source_of_truth,
            before_sha256,
            after_sha256,
            changed,
            if changed {
                "separate target readback equals requested text after set_value"
            } else {
                "separate target readback already equaled requested text"
            },
        ),
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    })
}

pub fn act_set_value_request_details(params: &ActSetValueParams) -> Value {
    json!({
        "element_id": params.element_id.to_string(),
        "source_of_truth": [
            SOURCE_NATIVE_TEXT,
            SOURCE_UIA_VALUE,
            SOURCE_NATIVE_PASSWORD_LENGTH,
            SOURCE_UIA_PASSWORD_LENGTH
        ],
        "requested_len": params.text.chars().count(),
        "requested_sha256": text_signature(&params.text),
        "verify_timeout_ms": params.verify_timeout_ms,
        "required_foreground": false,
        "backend_router": BACKEND_ROUTER_NATIVE_EDIT_OR_UIA,
    })
}

fn set_backend_tier(readback: &synapse_a11y::ElementValueSetReadback) -> &'static str {
    if readback.method == METHOD_NATIVE_TEXT_MESSAGE {
        "wm_settext"
    } else {
        "uia"
    }
}

fn validate_set_value_params(params: &ActSetValueParams) -> Result<(), ErrorData> {
    if !(50..=5000).contains(&params.verify_timeout_ms) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "act_set_value verify_timeout_ms must be in 50..=5000, got {}",
                params.verify_timeout_ms
            ),
        ));
    }
    let _ = char_count(&params.text)?;
    Ok(())
}

fn char_count(text: &str) -> Result<u32, ErrorData> {
    u32::try_from(text.chars().count()).map_err(|_err| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "act_set_value text has more than u32::MAX chars",
        )
    })
}

fn after_matches_requested(
    after: &synapse_a11y::ElementValueReadback,
    set_readback: &synapse_a11y::ElementValueSetReadback,
    requested: &str,
) -> bool {
    if after.is_password || set_readback.is_password {
        return after.password_len == Some(expected_value_len(requested, true));
    }
    after.value == expected_set_value(set_readback, requested)
}

fn expected_set_value<'a>(
    readback: &'a synapse_a11y::ElementValueSetReadback,
    requested: &'a str,
) -> &'a str {
    readback
        .expected_after_value
        .as_deref()
        .unwrap_or(requested)
}

fn expected_value_len(value: &str, is_password: bool) -> usize {
    if is_password {
        value.encode_utf16().count()
    } else {
        value.chars().count()
    }
}

fn value_readbacks_equivalent(
    before: &synapse_a11y::ElementValueReadback,
    after: &synapse_a11y::ElementValueReadback,
) -> bool {
    if before.is_password || after.is_password {
        return before.password_len == after.password_len;
    }
    before.value == after.value
}

fn value_readback_matches_requested(
    readback: &synapse_a11y::ElementValueReadback,
    requested: &str,
) -> bool {
    if readback.is_password {
        return readback.password_len == Some(expected_value_len(requested, true));
    }
    readback.value == requested
}

fn value_readback_len(readback: &synapse_a11y::ElementValueReadback) -> usize {
    if readback.is_password {
        readback.password_len.unwrap_or(0)
    } else {
        readback.value.chars().count()
    }
}

fn value_readback_signature(readback: &synapse_a11y::ElementValueReadback) -> String {
    if readback.is_password {
        return readback
            .password_len
            .map(|len| format!("password_len:{len}"))
            .unwrap_or_else(|| "password_len:<missing>".to_owned());
    }
    text_signature(&readback.value)
}

fn value_set_after_len(readback: &synapse_a11y::ElementValueSetReadback) -> usize {
    if readback.is_password {
        readback.after_password_len.unwrap_or(0)
    } else {
        readback.after_value.chars().count()
    }
}

fn value_set_after_signature(readback: &synapse_a11y::ElementValueSetReadback) -> String {
    if readback.is_password {
        return readback
            .after_password_len
            .map(|len| format!("password_len:{len}"))
            .unwrap_or_else(|| "password_len:<missing>".to_owned());
    }
    text_signature(&readback.after_value)
}

fn set_text_integrity(readback: &synapse_a11y::ElementValueSetReadback) -> &'static str {
    match (readback.method.as_str(), readback.is_password) {
        (METHOD_NATIVE_TEXT_MESSAGE, true) => TEXT_INTEGRITY_NATIVE_PASSWORD_LENGTH,
        (METHOD_NATIVE_TEXT_MESSAGE, false) => TEXT_INTEGRITY_NATIVE_TEXT_MESSAGE,
        (_, true) => TEXT_INTEGRITY_UIA_PASSWORD_LENGTH,
        (_, false) => TEXT_INTEGRITY_UIA_VALUE_PATTERN,
    }
}

fn set_source_of_truth(readback: &synapse_a11y::ElementValueSetReadback) -> &'static str {
    match (readback.method.as_str(), readback.is_password) {
        (METHOD_NATIVE_TEXT_MESSAGE, true) => SOURCE_NATIVE_PASSWORD_LENGTH,
        (METHOD_NATIVE_TEXT_MESSAGE, false) => SOURCE_NATIVE_TEXT,
        (_, true) => SOURCE_UIA_PASSWORD_LENGTH,
        (_, false) => SOURCE_UIA_VALUE,
    }
}

fn value_readback_source_of_truth(readback: &synapse_a11y::ElementValueReadback) -> &'static str {
    match (readback.method.as_str(), readback.is_password) {
        (METHOD_NATIVE_TEXT_MESSAGE, true) => SOURCE_NATIVE_PASSWORD_LENGTH,
        (METHOD_NATIVE_TEXT_MESSAGE, false) => SOURCE_NATIVE_TEXT,
        (_, true) => SOURCE_UIA_PASSWORD_LENGTH,
        (_, false) => SOURCE_UIA_VALUE,
    }
}

fn postcondition_verified_state(
    source_of_truth: &'static str,
    before_signature: String,
    after_signature: String,
    changed: bool,
    detail: impl Into<String>,
) -> ActPostcondition {
    ActPostcondition {
        status: "verified_state".to_owned(),
        observed_delta: Some(changed),
        source_of_truth: Some(source_of_truth.to_owned()),
        before_signature: Some(before_signature),
        after_signature: Some(after_signature),
        detail: Some(format!("{TOOL} {}", detail.into())),
    }
}

fn a11y_error_to_set_value_mcp(
    element_id: &ElementId,
    operation: &'static str,
    error: synapse_a11y::A11yError,
    prior_readback: Option<Value>,
) -> ErrorData {
    let code = error.code();
    let root_hwnd = element_id.parts().ok().map(|parts| parts.hwnd);
    tracing::error!(
        code,
        tool = TOOL,
        element_id = %element_id,
        root_hwnd,
        operation,
        source_of_truth = "win32_window_text|uia_value_pattern.value",
        detail = %error,
        "act_set_value background value readback operation failed"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!("act_set_value {operation} failed for element {element_id}: {error}"),
        Some(json!({
            "code": code,
            "tool": TOOL,
            "operation": operation,
            "source_of_truth": [
                SOURCE_NATIVE_TEXT,
                SOURCE_UIA_VALUE,
                SOURCE_NATIVE_PASSWORD_LENGTH,
                SOURCE_UIA_PASSWORD_LENGTH
            ],
            "backend_router": BACKEND_ROUTER_NATIVE_EDIT_OR_UIA,
            "element_id": element_id.to_string(),
            "root_hwnd": root_hwnd,
            "detail": error.to_string(),
            "prior_readback": prior_readback,
        })),
    )
}

fn set_failed_error(
    params: &ActSetValueParams,
    before: &synapse_a11y::ElementValueReadback,
    requested_sha256: &str,
    requested_len: u32,
    error: synapse_a11y::A11yError,
    after: Option<synapse_a11y::ElementValueReadback>,
) -> ErrorData {
    let source_of_truth = after
        .as_ref()
        .map(value_readback_source_of_truth)
        .unwrap_or_else(|| value_readback_source_of_truth(before));
    let after_sha256 = after.as_ref().map(value_readback_signature);
    let after_len = after.as_ref().map(value_readback_len);
    let before_sha256 = value_readback_signature(before);
    let before_len = value_readback_len(before);
    let final_equals_requested = after
        .as_ref()
        .is_some_and(|readback| value_readback_matches_requested(readback, &params.text));
    let code = if final_equals_requested {
        error_codes::ACTION_POSTCONDITION_FAILED
    } else {
        error.code()
    };
    tracing::error!(
        code,
        tool = TOOL,
        element_id = %params.element_id,
        source_of_truth,
        before_sha256,
        after_sha256,
        requested_sha256,
        before_len,
        after_len = after_len.unwrap_or(0),
        requested_len,
        set_error = %error,
        final_equals_requested,
        "act_set_value background set operation failed; separate target value readback captured after failure"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "act_set_value background set operation failed for element {}: {error}",
            params.element_id
        ),
        Some(json!({
            "code": code,
            "tool": TOOL,
            "operation": "set_value",
            "source_of_truth": source_of_truth,
            "backend_router": BACKEND_ROUTER_NATIVE_EDIT_OR_UIA,
            "element_id": params.element_id.to_string(),
            "before_len": before_len,
            "after_len": after_len,
            "requested_len": requested_len,
            "before_sha256": before_sha256,
            "after_sha256": after_sha256,
            "requested_sha256": requested_sha256,
            "final_equals_requested": final_equals_requested,
            "set_error_code": error.code(),
            "set_error": error.to_string(),
            "after_read_succeeded": after.is_some(),
        })),
    )
}

fn postcondition_failed_set_value_error(
    params: &ActSetValueParams,
    before: &synapse_a11y::ElementValueReadback,
    after: &synapse_a11y::ElementValueReadback,
    set_readback: &synapse_a11y::ElementValueSetReadback,
    detail: &'static str,
) -> ErrorData {
    let source_of_truth = set_source_of_truth(set_readback);
    let before_sha256 = value_readback_signature(before);
    let after_sha256 = value_readback_signature(after);
    let requested_sha256 = text_signature(&params.text);
    ErrorData::new(
        ErrorCode(-32099),
        format!("{TOOL} verify_delta Source-of-Truth postcondition failed: {detail}"),
        Some(json!({
            "code": error_codes::ACTION_POSTCONDITION_FAILED,
            "tool": TOOL,
            "source_of_truth": source_of_truth,
            "detail": detail,
            "verify_delta": {
                "before_signature": before_sha256,
                "after_signature": after_sha256,
                "readback": {
                    "element_id": params.element_id.to_string(),
                    "method": set_readback.method,
                    "before_len": value_readback_len(before),
                    "immediate_after_len": value_set_after_len(set_readback),
                    "after_len": value_readback_len(after),
                    "requested_len": params.text.chars().count(),
                    "expected_len": expected_set_value(set_readback, &params.text).chars().count(),
                    "before_sha256": value_readback_signature(before),
                    "immediate_after_sha256": value_set_after_signature(set_readback),
                    "after_sha256": value_readback_signature(after),
                    "requested_sha256": requested_sha256,
                    "after_readonly": after.is_readonly,
                }
            }
        })),
    )
}

fn value_readback_json(
    element_id: &ElementId,
    readback: &synapse_a11y::ElementValueReadback,
) -> Value {
    json!({
        "element_id": element_id.to_string(),
        "method": readback.method,
        "source_of_truth": value_readback_source_of_truth(readback),
        "value_len": value_readback_len(readback),
        "value_signature": value_readback_signature(readback),
        "is_readonly": readback.is_readonly,
        "is_password": readback.is_password,
        "password_len": readback.password_len,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        METHOD_NATIVE_TEXT_MESSAGE, SOURCE_NATIVE_PASSWORD_LENGTH, SOURCE_NATIVE_TEXT,
        SOURCE_UIA_VALUE, TEXT_INTEGRITY_NATIVE_PASSWORD_LENGTH,
        TEXT_INTEGRITY_NATIVE_TEXT_MESSAGE, TEXT_INTEGRITY_UIA_VALUE_PATTERN,
        after_matches_requested, set_backend_tier, set_source_of_truth, set_text_integrity,
        value_readback_json,
    };
    use synapse_core::ElementId;

    fn value_set(
        method: &str,
        before: &str,
        after: &str,
        expected_after: Option<&str>,
        is_password: bool,
        before_password_len: Option<usize>,
        after_password_len: Option<usize>,
    ) -> synapse_a11y::ElementValueSetReadback {
        synapse_a11y::ElementValueSetReadback {
            method: method.to_owned(),
            before_value: before.to_owned(),
            after_value: after.to_owned(),
            expected_after_value: expected_after.map(str::to_owned),
            is_password,
            before_password_len,
            after_password_len,
        }
    }

    fn value_read(
        method: &str,
        value: &str,
        is_readonly: bool,
        is_password: bool,
        password_len: Option<usize>,
    ) -> synapse_a11y::ElementValueReadback {
        synapse_a11y::ElementValueReadback {
            method: method.to_owned(),
            value: value.to_owned(),
            is_readonly,
            is_password,
            password_len,
        }
    }

    #[test]
    fn value_pattern_tier_reports_uia_source() {
        let readback = value_set(
            "uia_value_pattern",
            "before",
            "after",
            None,
            false,
            None,
            None,
        );
        let after = value_read("uia_value_pattern", "after", false, false, None);

        assert_eq!(set_backend_tier(&readback), "uia");
        assert_eq!(set_source_of_truth(&readback), SOURCE_UIA_VALUE);
        assert_eq!(
            set_text_integrity(&readback),
            TEXT_INTEGRITY_UIA_VALUE_PATTERN
        );
        assert!(after_matches_requested(&after, &readback, "after"));
    }

    #[test]
    fn native_text_message_tier_reports_wm_settext_source() {
        let readback = value_set(
            METHOD_NATIVE_TEXT_MESSAGE,
            "before",
            "after-native",
            None,
            false,
            None,
            None,
        );
        let after = value_read(
            METHOD_NATIVE_TEXT_MESSAGE,
            "after-native",
            false,
            false,
            None,
        );

        assert_eq!(set_backend_tier(&readback), "wm_settext");
        assert_eq!(set_source_of_truth(&readback), SOURCE_NATIVE_TEXT);
        assert_eq!(
            set_text_integrity(&readback),
            TEXT_INTEGRITY_NATIVE_TEXT_MESSAGE
        );
        assert!(after_matches_requested(&after, &readback, "after-native"));
    }

    #[test]
    fn native_text_message_accepts_normalized_multiline_expected_value() {
        let readback = value_set(
            METHOD_NATIVE_TEXT_MESSAGE,
            "",
            "line-a\r\nline-b",
            Some("line-a\r\nline-b"),
            false,
            None,
            None,
        );
        let after = value_read(
            METHOD_NATIVE_TEXT_MESSAGE,
            "line-a\r\nline-b",
            false,
            false,
            None,
        );

        assert!(after_matches_requested(&after, &readback, "line-a\nline-b"));
        assert_eq!(set_backend_tier(&readback), "wm_settext");
        assert_eq!(set_source_of_truth(&readback), SOURCE_NATIVE_TEXT);
    }

    #[test]
    fn native_password_tier_uses_length_source() {
        let readback = value_set(
            METHOD_NATIVE_TEXT_MESSAGE,
            "",
            "",
            None,
            true,
            Some(0),
            Some(7),
        );
        let after = value_read(METHOD_NATIVE_TEXT_MESSAGE, "", false, true, Some(7));

        assert_eq!(set_backend_tier(&readback), "wm_settext");
        assert_eq!(
            set_source_of_truth(&readback),
            SOURCE_NATIVE_PASSWORD_LENGTH
        );
        assert_eq!(
            set_text_integrity(&readback),
            TEXT_INTEGRITY_NATIVE_PASSWORD_LENGTH
        );
        assert!(after_matches_requested(&after, &readback, "p@ss727"));
    }

    #[test]
    fn prior_readback_json_names_native_method_and_source() {
        let element_id =
            ElementId::parse("0x1000:0000002a00000001").expect("synthetic element id should parse");
        let readback = value_read(METHOD_NATIVE_TEXT_MESSAGE, "native", false, false, None);
        let json = value_readback_json(&element_id, &readback);

        assert_eq!(json["method"], METHOD_NATIVE_TEXT_MESSAGE);
        assert_eq!(json["source_of_truth"], SOURCE_NATIVE_TEXT);
        assert_eq!(json["value_len"], 6);
        assert_eq!(json["is_password"], false);
    }
}
