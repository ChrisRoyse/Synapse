//! `act_set_field_text` (#882): one primitive that REPLACES a field's text —
//! clear + type + verify — across the three target families Synapse can see,
//! so agents stop hand-rolling the click → Ctrl+A → type → observe dance per
//! web form field.
//!
//! Routing is deterministic, decided up front from the element id and target
//! metadata — a tier that fails returns its precise error; there is no
//! silent fallthrough to another tier:
//!
//! - **Web node** (`cdcd` element id): background CDP replace — click/focus
//!   the node, select-all on the exact resolved DOM node, `Input.insertText`
//!   (which replaces the selection — the Playwright `fill()` strategy), then a
//!   separate `cdp_node_value` readback must equal the requested text.
//! - **Chromium UIA editable** (no CDP id): the leased foreground tier — the
//!   target window must already be foreground (epic #771: Synapse never
//!   steals the human's foreground implicitly; call `act_focus_window`
//!   first). Click the element, verify focus landed on it, Ctrl+A, type (or
//!   Delete for empty text), then a separate UIA value readback must equal
//!   the requested text. UIA *reads* are safe on Chromium; only
//!   `ValuePattern.SetValue` mutation is refused (#686).
//! - **Native element**: delegates to the `act_set_value` background tiers
//!   (WM_SETTEXT / UIA `ValuePattern.SetValue`), which already implement
//!   replace + separate Source-of-Truth readback semantics.

use rmcp::{ErrorData, model::ErrorCode, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::{ElementId, UiaPattern, error_codes};

use crate::m1::mcp_error;
use crate::m2::set_value::ActSetValueParams;
use crate::m2::{
    default_auto_wait_timeout_ms,
    postcondition::{ActPostcondition, default_verify_timeout_ms, text_signature},
};

const TOOL: &str = "act_set_field_text";
pub(crate) const TIER_CDP: &str = "cdp";
pub(crate) const TIER_FOREGROUND_KEYS: &str = "foreground_keys";
const METHOD_CDP_REPLACE: &str = "cdp_select_all_insert_text";
const METHOD_CDP_CLEAR: &str = "cdp_select_all_delete";
pub(crate) const METHOD_FOREGROUND_REPLACE: &str = "foreground_click_select_all_type";
pub(crate) const METHOD_FOREGROUND_CLEAR: &str = "foreground_click_select_all_delete";
const SOURCE_CDP_NODE_VALUE: &str = "cdp_node.value";
pub(crate) const SOURCE_UIA_VALUE: &str = "uia_value_pattern.value";
pub(crate) const SOURCE_UIA_PASSWORD_LENGTH: &str = "uia_value_pattern.password_length";

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActSetFieldTextParams {
    #[serde(default)]
    pub element_id: Option<ElementId>,
    /// Durable action-time locator used to resolve a fresh UIA element in the
    /// target window immediately before filling. Use this for SPA/Electron
    /// controls whose UIA runtime ids can go stale between observe and act.
    #[serde(default)]
    pub locator: Option<ActSetFieldTextLocator>,
    /// Full replacement text. Empty clears the field.
    pub text: String,
    #[serde(default = "default_verify_timeout_ms")]
    #[schemars(default = "default_verify_timeout_ms", range(min = 50, max = 5000))]
    pub verify_timeout_ms: u32,
    #[serde(default)]
    #[schemars(
        default,
        description = "Opt in to pre-action CDP actionability polling for web element targets. When true, Synapse resolves the element/locator, scrolls the node into view, and waits until it is attached, visible, stable, enabled, editable, and receiving events before replacing text. Default false preserves existing fill semantics."
    )]
    pub auto_wait: bool,
    #[serde(default = "default_auto_wait_timeout_ms")]
    #[schemars(default = "default_auto_wait_timeout_ms", range(min = 50, max = 30000))]
    pub auto_wait_timeout_ms: u32,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActSetFieldTextLocator {
    /// Explicit top-level target window. If omitted, Synapse uses the active
    /// session window/CDP target, or the legacy element_id's HWND as a hint.
    #[serde(default)]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub window_hwnd: Option<i64>,
    #[serde(default)]
    pub role: Option<String>,
    /// Exact accessible name match, case-insensitive.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub name_substring: Option<String>,
    #[serde(default)]
    pub automation_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActSetFieldTextResponse {
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
    pub postcondition: ActPostcondition,
    pub elapsed_ms: u32,
}

/// Which delivery tier `act_set_field_text` resolved for the target, decided
/// before any mutation.
#[derive(Clone, Debug)]
pub(crate) enum SetFieldTextRoute {
    /// CDP web node (`cdcd` element id): background CDP replace.
    Web { backend_node_id: i64 },
    /// Chromium UIA editable: leased foreground click + select-all + type.
    ChromiumForeground {
        root_hwnd: i64,
        process_name: String,
        metadata: synapse_a11y::ElementMetadataReadback,
    },
    /// Anything else: `act_set_value` background tiers.
    NativeBackground,
}

pub(crate) fn validate_set_field_text_params(
    params: &ActSetFieldTextParams,
) -> Result<(), ErrorData> {
    if params.element_id.is_none() && params.locator.is_none() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{TOOL} requires element_id or locator"),
        ));
    }
    if let Some(locator) = &params.locator {
        validate_set_field_text_locator(locator)?;
    }
    if !(50..=5000).contains(&params.verify_timeout_ms) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{TOOL} verify_timeout_ms must be in 50..=5000, got {}",
                params.verify_timeout_ms
            ),
        ));
    }
    crate::m2::validate_auto_wait_timeout(TOOL, params.auto_wait, params.auto_wait_timeout_ms)?;
    if u32::try_from(params.text.chars().count()).is_err() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{TOOL} text has more than u32::MAX chars"),
        ));
    }
    Ok(())
}

pub(crate) fn validate_set_field_text_locator(
    locator: &ActSetFieldTextLocator,
) -> Result<(), ErrorData> {
    if locator
        .role
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
        && locator
            .name
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        && locator
            .name_substring
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
        && locator
            .automation_id
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{TOOL} locator requires at least one of role, name, name_substring, or automation_id"
            ),
        ));
    }
    if locator.name.is_some() && locator.name_substring.is_some() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{TOOL} locator accepts name or name_substring, not both"),
        ));
    }
    Ok(())
}

/// Resolves the delivery tier from the element id and live target metadata.
/// Fail-loud: a malformed id or unreadable target context is an error, not a
/// guess.
#[cfg(windows)]
pub(crate) fn set_field_text_route(element_id: &ElementId) -> Result<SetFieldTextRoute, ErrorData> {
    if let Some(backend_node_id) = synapse_a11y::cdp_backend_from_element_id(element_id) {
        return Ok(SetFieldTextRoute::Web { backend_node_id });
    }
    let root_hwnd = element_id
        .parts()
        .map_err(|err| {
            mcp_error(
                error_codes::ACTION_ELEMENT_NOT_RESOLVED,
                format!("{TOOL} element id is malformed: {err}"),
            )
        })?
        .hwnd;
    let context = synapse_a11y::foreground_context(root_hwnd).map_err(|error| {
        mcp_error(
            error.code(),
            format!("{TOOL} target window context readback failed for hwnd {root_hwnd}: {error}"),
        )
    })?;
    if !synapse_a11y::is_chromium_family(&context.process_name) {
        return Ok(SetFieldTextRoute::NativeBackground);
    }
    let metadata = synapse_a11y::element_metadata(element_id).map_err(|error| {
        mcp_error(
            error.code(),
            format!("{TOOL} target element metadata readback failed: {error}"),
        )
    })?;
    if chromium_editable_requires_foreground(&metadata) {
        return Ok(SetFieldTextRoute::ChromiumForeground {
            root_hwnd,
            process_name: context.process_name,
            metadata,
        });
    }
    Ok(SetFieldTextRoute::NativeBackground)
}

#[cfg(not(windows))]
pub(crate) fn set_field_text_route(
    _element_id: &ElementId,
) -> Result<SetFieldTextRoute, ErrorData> {
    Ok(SetFieldTextRoute::NativeBackground)
}

/// Same predicate `act_type` uses to refuse Chromium UIA `ValuePattern`
/// mutation (#686): an enabled, keyboard-focusable Value target with an
/// editable role or a Text pattern must take the foreground keyboard route.
pub(crate) fn chromium_editable_requires_foreground(
    metadata: &synapse_a11y::ElementMetadataReadback,
) -> bool {
    if !metadata.enabled {
        return false;
    }
    if !metadata.patterns.contains(&UiaPattern::Value) {
        return false;
    }
    let role = metadata.role.to_ascii_lowercase();
    let editable_role = role.contains("edit") || role.contains("document") || role.contains("text");
    let exposes_text_pattern = metadata.patterns.contains(&UiaPattern::Text);
    metadata.keyboard_focusable && (editable_role || exposes_text_pattern)
}

/// Background CDP replace tier. Requires a raw debug endpoint — the Chrome
/// debugger extension transport has no replace primitive, so its absence is a
/// precise refusal (use the foreground tier after `act_focus_window`), never
/// a degraded append.
#[cfg(windows)]
pub(crate) async fn act_set_field_text_web(
    params: &ActSetFieldTextParams,
    backend_node_id: i64,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActSetFieldTextResponse, ErrorData> {
    let started = std::time::Instant::now();
    let element_id = required_element_id(params)?;
    let hwnd = element_id
        .parts()
        .map_err(|err| {
            mcp_error(
                error_codes::ACTION_ELEMENT_NOT_RESOLVED,
                format!("{TOOL} web element id is malformed: {err}"),
            )
        })?
        .hwnd;
    let title_hint = synapse_a11y::foreground_context(hwnd)
        .map(|context| context.window_title)
        .unwrap_or_default();
    let target_id_hint = synapse_a11y::cdp_target_from_element_id(element_id);
    let Some(endpoint) = synapse_a11y::endpoint_for_window(hwnd) else {
        tracing::error!(
            code = error_codes::A11Y_CDP_UNREACHABLE,
            tool = TOOL,
            element_id = %element_id,
            hwnd,
            "act_set_field_text web tier requires a raw CDP endpoint; none reachable for the target window"
        );
        return Err(ErrorData::new(
            ErrorCode(-32099),
            format!(
                "{TOOL} web tier requires a raw CDP debug endpoint for element {element_id}, but none is reachable (browser closed, or not launched with --remote-debugging-port). Foreground alternative: act_focus_window the browser, then act_set_field_text the UIA element."
            ),
            Some(json!({
                "code": error_codes::A11Y_CDP_UNREACHABLE,
                "tool": TOOL,
                "element_id": element_id.to_string(),
                "root_hwnd": hwnd,
                "backend_node_id": backend_node_id,
                "reason": "raw_cdp_endpoint_unreachable",
            })),
        ));
    };

    let before = synapse_a11y::cdp_node_value(
        &endpoint,
        &title_hint,
        target_id_hint.as_deref(),
        backend_node_id,
    )
    .await
    .map_err(|error| cdp_tier_error(element_id, backend_node_id, "before_value_read", &error))?;

    boundary.ensure("immediately_before_cdp_set_node_text")?;
    let readback = synapse_a11y::cdp_set_node_text(
        &endpoint,
        &title_hint,
        target_id_hint.as_deref(),
        backend_node_id,
        &params.text,
    )
    .await
    .map_err(|error| cdp_tier_error(element_id, backend_node_id, "set_node_text", &error))?;

    tokio::time::sleep(std::time::Duration::from_millis(u64::from(
        params.verify_timeout_ms,
    )))
    .await;
    let after = synapse_a11y::cdp_node_value(
        &endpoint,
        &title_hint,
        target_id_hint.as_deref(),
        backend_node_id,
    )
    .await
    .map_err(|error| cdp_tier_error(element_id, backend_node_id, "after_value_read", &error))?;

    let method = if readback.cleared_with_delete {
        METHOD_CDP_CLEAR
    } else {
        METHOD_CDP_REPLACE
    };
    finish_replace_response(
        params,
        started,
        method,
        TIER_CDP,
        false,
        SOURCE_CDP_NODE_VALUE,
        &before,
        &after,
        json!({
            "element_id": element_id.to_string(),
            "backend_node_id": backend_node_id,
            "selection_mode": readback.selection_mode,
            "cleared_with_delete": readback.cleared_with_delete,
        }),
    )
}

/// Native (non-Chromium) tier: `act_set_value` background replace + readback,
/// re-shaped into the `act_set_field_text` wire response.
pub(crate) async fn act_set_field_text_native(
    params: &ActSetFieldTextParams,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActSetFieldTextResponse, ErrorData> {
    let started = std::time::Instant::now();
    let response = super::set_value::act_set_value_with_boundary(
        ActSetValueParams {
            element_id: required_element_id(params)?.clone(),
            text: params.text.clone(),
            verify_timeout_ms: params.verify_timeout_ms,
        },
        boundary,
    )
    .await?;
    let postcondition = ActPostcondition {
        detail: response
            .postcondition
            .detail
            .map(|detail| format!("{TOOL} native tier: {detail}")),
        ..response.postcondition
    };
    Ok(ActSetFieldTextResponse {
        ok: response.ok,
        method: response.method,
        backend_tier_used: response.backend_tier_used,
        required_foreground: response.required_foreground,
        source_of_truth: response.source_of_truth,
        requested_len: response.requested_len,
        before_len: response.before_len,
        after_len: response.after_len,
        requested_sha256: response.requested_sha256,
        before_sha256: response.before_sha256,
        after_sha256: response.after_sha256,
        changed: response.changed,
        postcondition,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    })
}

/// Builds the verified response for a replace tier from separate before/after
/// Source-of-Truth reads. Fails closed with `ACTION_POSTCONDITION_FAILED`
/// (and the full evidence trail) when the after-read does not equal the
/// requested text.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_replace_response(
    params: &ActSetFieldTextParams,
    started: std::time::Instant,
    method: &str,
    backend_tier_used: &str,
    required_foreground: bool,
    source_of_truth: &str,
    before: &str,
    after: &str,
    evidence: Value,
) -> Result<ActSetFieldTextResponse, ErrorData> {
    let element_id = required_element_id(params)?;
    let requested_len = u32::try_from(params.text.chars().count()).unwrap_or(u32::MAX);
    let before_len = u32::try_from(before.chars().count()).unwrap_or(u32::MAX);
    let after_len = u32::try_from(after.chars().count()).unwrap_or(u32::MAX);
    let requested_sha256 = text_signature(&params.text);
    let before_sha256 = text_signature(before);
    let after_sha256 = text_signature(after);
    let changed = before != after;

    if !replaced_text_matches(after, &params.text) {
        tracing::error!(
            code = error_codes::ACTION_POSTCONDITION_FAILED,
            tool = TOOL,
            element_id = %element_id,
            method,
            backend_tier_used,
            source_of_truth,
            before_len,
            after_len,
            requested_len,
            before_sha256,
            after_sha256,
            requested_sha256,
            "act_set_field_text separate target readback did not equal requested replacement text"
        );
        return Err(ErrorData::new(
            ErrorCode(-32099),
            format!(
                "{TOOL} Source-of-Truth postcondition failed: separate target readback does not equal the requested replacement text"
            ),
            Some(json!({
                "code": error_codes::ACTION_POSTCONDITION_FAILED,
                "tool": TOOL,
                "method": method,
                "backend_tier_used": backend_tier_used,
                "source_of_truth": source_of_truth,
                "verify_delta": {
                    "before_signature": before_sha256,
                    "after_signature": after_sha256,
                    "requested_signature": requested_sha256,
                    "before_len": before_len,
                    "after_len": after_len,
                    "requested_len": requested_len,
                },
                "evidence": evidence,
            })),
        ));
    }

    tracing::info!(
        code = "M2_ACT_SET_FIELD_TEXT_READBACK",
        element_id = %element_id,
        method,
        backend_tier_used,
        source_of_truth,
        before_len,
        after_len,
        requested_len,
        changed,
        "readback=act_set_field_text method={method} before_len={before_len} after_len={after_len} requested_len={requested_len} changed={changed}"
    );
    Ok(ActSetFieldTextResponse {
        ok: true,
        method: method.to_owned(),
        backend_tier_used: backend_tier_used.to_owned(),
        required_foreground,
        source_of_truth: source_of_truth.to_owned(),
        requested_len,
        before_len,
        after_len,
        requested_sha256,
        before_sha256: before_sha256.clone(),
        after_sha256: after_sha256.clone(),
        changed,
        postcondition: ActPostcondition {
            status: "verified_state".to_owned(),
            observed_delta: Some(changed),
            source_of_truth: Some(source_of_truth.to_owned()),
            before_signature: Some(before_sha256),
            after_signature: Some(after_sha256),
            detail: Some(if changed {
                format!("{TOOL} separate target readback equals requested replacement text")
            } else {
                format!(
                    "{TOOL} separate target readback already equaled requested replacement text"
                )
            }),
        },
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
    })
}

/// Replace verification: the after-read must equal the requested text.
/// Newlines are normalized on both sides (editable hosts surface `\r\n` /
/// trailing-newline variance that does not change field content).
#[must_use]
pub(crate) fn replaced_text_matches(after: &str, requested: &str) -> bool {
    let after = normalize_field_text(after);
    let requested = normalize_field_text(requested);
    after == requested
}

fn normalize_field_text(value: &str) -> String {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .strip_suffix('\n')
        .map_or_else(|| normalized.clone(), str::to_owned)
}

#[cfg(windows)]
fn cdp_tier_error(
    element_id: &ElementId,
    backend_node_id: i64,
    operation: &'static str,
    error: &synapse_a11y::A11yError,
) -> ErrorData {
    tracing::error!(
        code = error.code(),
        tool = TOOL,
        element_id = %element_id,
        backend_node_id,
        operation,
        detail = %error,
        "act_set_field_text CDP web tier operation failed"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!("{TOOL} CDP {operation} failed for element {element_id}: {error}"),
        Some(json!({
            "code": error.code(),
            "tool": TOOL,
            "operation": operation,
            "element_id": element_id.to_string(),
            "backend_node_id": backend_node_id,
            "source_of_truth": SOURCE_CDP_NODE_VALUE,
            "detail": error.to_string(),
        })),
    )
}

pub fn act_set_field_text_request_details(params: &ActSetFieldTextParams) -> Value {
    json!({
        "element_id": params.element_id.as_ref().map(ToString::to_string),
        "locator": params.locator.as_ref().map(locator_request_details),
        "requested_len": params.text.chars().count(),
        "requested_sha256": text_signature(&params.text),
        "verify_timeout_ms": params.verify_timeout_ms,
        "source_of_truth": [
            SOURCE_CDP_NODE_VALUE,
            SOURCE_UIA_VALUE,
            SOURCE_UIA_PASSWORD_LENGTH,
            "win32_window_text",
        ],
    })
}

pub(crate) fn required_element_id(params: &ActSetFieldTextParams) -> Result<&ElementId, ErrorData> {
    params.element_id.as_ref().ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("{TOOL} internal error: target element_id was not resolved before routing"),
        )
    })
}

pub(crate) fn params_with_resolved_element(
    params: &ActSetFieldTextParams,
    element_id: ElementId,
) -> ActSetFieldTextParams {
    let mut params = params.clone();
    params.element_id = Some(element_id);
    params
}

fn locator_request_details(locator: &ActSetFieldTextLocator) -> Value {
    json!({
        "window_hwnd": locator.window_hwnd,
        "role": locator.role.as_deref(),
        "name_sha256": locator.name.as_ref().map(|name| text_signature(name)),
        "name_len": locator.name.as_ref().map(|name| name.chars().count()),
        "name_substring_sha256": locator
            .name_substring
            .as_ref()
            .map(|name| text_signature(name)),
        "name_substring_len": locator
            .name_substring
            .as_ref()
            .map(|name| name.chars().count()),
        "automation_id_present": locator.automation_id.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use synapse_core::{Rect, UiaPattern};

    use super::{
        ActSetFieldTextLocator, ActSetFieldTextParams, chromium_editable_requires_foreground,
        normalize_field_text, replaced_text_matches, validate_set_field_text_params,
    };

    fn metadata(
        role: &str,
        patterns: Vec<UiaPattern>,
        enabled: bool,
        keyboard_focusable: bool,
    ) -> synapse_a11y::ElementMetadataReadback {
        synapse_a11y::ElementMetadataReadback {
            name: "synthetic".to_owned(),
            role: role.to_owned(),
            automation_id: None,
            bbox: Rect {
                x: 4,
                y: 8,
                w: 320,
                h: 28,
            },
            enabled,
            keyboard_focusable,
            patterns,
            value: Some("before".to_owned()),
        }
    }

    #[test]
    fn chromium_edit_routes_to_foreground_tier() {
        let metadata = metadata("edit", vec![UiaPattern::Value], true, true);
        println!(
            "readback=route edge=chromium_edit metadata_role={}",
            metadata.role
        );
        assert!(chromium_editable_requires_foreground(&metadata));
    }

    #[test]
    fn chromium_contenteditable_document_routes_to_foreground_tier() {
        let metadata = metadata(
            "document",
            vec![UiaPattern::Value, UiaPattern::Text],
            true,
            true,
        );
        assert!(chromium_editable_requires_foreground(&metadata));
    }

    #[test]
    fn chromium_button_stays_on_native_tier() {
        let metadata = metadata("button", vec![UiaPattern::Value], true, true);
        assert!(!chromium_editable_requires_foreground(&metadata));
    }

    #[test]
    fn disabled_chromium_edit_stays_on_native_tier() {
        let metadata = metadata("edit", vec![UiaPattern::Value], false, true);
        assert!(!chromium_editable_requires_foreground(&metadata));
    }

    #[test]
    fn replace_verification_is_newline_normalized() {
        println!(
            "readback=verify edge=newlines before=line-a\\r\\nline-b requested=line-a\\nline-b"
        );
        assert!(replaced_text_matches("line-a\r\nline-b", "line-a\nline-b"));
        assert!(replaced_text_matches("composer text\n", "composer text"));
        assert!(!replaced_text_matches("other", "requested"));
    }

    #[test]
    fn empty_replacement_matches_cleared_field() {
        println!("readback=verify edge=empty after=\"\" requested=\"\"");
        assert!(replaced_text_matches("", ""));
        assert!(!replaced_text_matches("leftover", ""));
    }

    #[test]
    fn normalize_strips_single_trailing_newline_only() {
        assert_eq!(normalize_field_text("a\n"), "a");
        assert_eq!(normalize_field_text("a\n\n"), "a\n");
        assert_eq!(normalize_field_text("a\r\nb"), "a\nb");
    }

    #[test]
    fn verify_timeout_out_of_range_fails_closed() {
        let params: ActSetFieldTextParams = serde_json::from_value(serde_json::json!({
            "element_id": "0x2a:0102",
            "text": "value",
            "verify_timeout_ms": 10
        }))
        .expect("params should deserialize");
        let error = validate_set_field_text_params(&params)
            .expect_err("verify_timeout_ms=10 must be rejected");
        println!("readback=params edge=low_timeout error={error}");
        assert!(error.message.contains("verify_timeout_ms"));
    }

    #[test]
    fn empty_text_is_a_valid_clear_request() {
        let params: ActSetFieldTextParams = serde_json::from_value(serde_json::json!({
            "element_id": "0x2a:0102",
            "text": ""
        }))
        .expect("params should deserialize");
        println!(
            "readback=params edge=empty_text verify_timeout_ms={}",
            params.verify_timeout_ms
        );
        assert!(validate_set_field_text_params(&params).is_ok());
        assert!(!params.auto_wait);
        assert_eq!(
            params.auto_wait_timeout_ms,
            crate::m2::default_auto_wait_timeout_ms()
        );
    }

    #[test]
    fn locator_only_request_is_valid_when_identity_is_specific() {
        let params: ActSetFieldTextParams = serde_json::from_value(serde_json::json!({
            "locator": {
                "window_hwnd": 0x2a,
                "role": "document",
                "name": "Message Body"
            },
            "text": "value"
        }))
        .expect("locator-only params should deserialize");
        assert!(params.element_id.is_none());
        assert!(validate_set_field_text_params(&params).is_ok());
    }

    #[test]
    fn locator_rejects_empty_identity() {
        let params = ActSetFieldTextParams {
            element_id: None,
            locator: Some(ActSetFieldTextLocator {
                window_hwnd: Some(0x2a),
                role: None,
                name: None,
                name_substring: None,
                automation_id: None,
            }),
            text: "value".to_owned(),
            verify_timeout_ms: crate::m2::default_verify_timeout_ms(),
            auto_wait: false,
            auto_wait_timeout_ms: crate::m2::default_auto_wait_timeout_ms(),
        };
        let error =
            validate_set_field_text_params(&params).expect_err("empty locator must fail closed");
        assert!(error.message.contains("locator requires"));
    }
}
