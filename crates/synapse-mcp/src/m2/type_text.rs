use std::{sync::Arc, time::Instant};

use rmcp::ErrorData;
use rmcp::model::ErrorCode;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use synapse_action::{
    ActionBackend, ActionError, ActionHandle, EmitState, RecordedInput, RecordingBackend,
};
use synapse_core::{
    Action, Backend, ElementId, KeystrokeDynamics, KeystrokeNaturalParams, UiaPattern, error_codes,
};

use crate::m1::mcp_error;
use crate::m2::default_auto_wait_timeout_ms;
use crate::m2::postcondition::{
    ActPostcondition, no_observed_delta_error, postcondition_failed_error,
    postcondition_not_requested, postcondition_observed_delta, text_signature,
};

const MIN_SAFE_LINEAR_MS_PER_CHAR: u32 = 20;
const TYPE_TIER_CDP: &str = "cdp";
const TYPE_TIER_UIA: &str = "uia";
const TYPE_TIER_WIN32_MESSAGE: &str = "win32_message";
const TYPE_TIER_FOREGROUND: &str = "foreground";
const MIN_VERIFY_TIMEOUT_MS: u32 = 50;
const MAX_VERIFY_TIMEOUT_MS: u32 = 5000;
const DEFAULT_ACT_TYPE_VERIFY_TIMEOUT_MS: u32 = 2000;
const TEXT_INTEGRITY_DISPATCH_ONLY: &str = "dispatch_only_requires_target_readback";
const TEXT_INTEGRITY_UIA_VALUE_PATTERN: &str = "uia_value_pattern_readback";
const TEXT_INTEGRITY_UIA_PASSWORD_LENGTH: &str = "uia_value_pattern_password_length_readback";
const TEXT_INTEGRITY_NATIVE_TEXT_MESSAGE: &str = "win32_text_message_readback";
const TEXT_INTEGRITY_NATIVE_PASSWORD_LENGTH: &str = "win32_text_message_password_length_readback";
const TEXT_INTEGRITY_UIA_VALUE_PATTERN_DISPATCH_ONLY: &str =
    "uia_value_pattern_dispatch_only_requires_target_readback";
const TEXT_INTEGRITY_CHROMIUM_UIA_VALUE_PATTERN_REFUSED: &str =
    "chromium_uia_value_pattern_refused_requires_cdp_or_foreground_typing";
const REASON_CHROMIUM_UIA_VALUE_PATTERN_REFUSED: &str = "chromium_uia_value_pattern_refused";
const SOURCE_UIA_VALUE: &str = "uia_value_pattern.value";
const SOURCE_UIA_PASSWORD_LENGTH: &str = "uia_value_pattern.password_length";
const SOURCE_NATIVE_TEXT: &str = "win32_window_text";
const SOURCE_NATIVE_PASSWORD_LENGTH: &str = "win32_window_text.password_length";
const METHOD_NATIVE_TEXT_MESSAGE: &str = "uia_native_window_text_message";
/// Text was inserted into a web input via CDP `Input.insertText` after focusing
/// the DOM node. Verify via `observe`/`find` (the node's `value`) — this path
/// dispatches into the renderer, so a follow-up readback is required (#686).
#[cfg(windows)]
const TEXT_INTEGRITY_CDP_INSERT_TEXT: &str =
    "cdp_insert_text_dispatch_only_requires_target_readback";

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActTypeParams {
    pub text: String,
    #[serde(default)]
    pub into_element: Option<ElementId>,
    #[serde(default = "default_type_dynamics")]
    #[schemars(default = "default_type_dynamics")]
    pub dynamics: TypeDynamics,
    #[serde(default = "default_linear_ms_per_char")]
    #[schemars(default = "default_linear_ms_per_char", range(min = 20))]
    pub linear_ms_per_char: u32,
    #[serde(default = "default_use_scancodes")]
    #[schemars(default = "default_use_scancodes")]
    pub use_scancodes: bool,
    #[serde(default = "default_press_enter_after")]
    #[schemars(default = "default_press_enter_after")]
    pub press_enter_after: bool,
    #[serde(default = "default_type_backend")]
    #[schemars(default = "default_type_backend")]
    pub backend: TypeBackend,
    #[serde(default = "default_verify_delta")]
    #[schemars(default = "default_verify_delta")]
    pub verify_delta: bool,
    #[serde(default)]
    #[schemars(
        default,
        range(min = 1, max = 4_294_967_295_u64),
        description = "Optional top-level HWND that foreground act_type visual verification must use as its physical Source of Truth when semantic text readback is unavailable. Requires verify_delta=true and no into_element or expected_browser_url_regex."
    )]
    pub verify_target_window_hwnd: Option<i64>,
    #[serde(default)]
    #[schemars(
        default,
        description = "When set with verify_delta=true, verify the after-read browser target URL against this regex. Intended for navigation where focus may move from an address field to the document; Synapse uses the session browser target readback (Chrome bridge chrome.tabs or raw CDP) and fails closed before input when no URL SoT is available."
    )]
    pub expected_browser_url_regex: Option<String>,
    #[serde(default = "default_act_type_verify_timeout_ms")]
    #[schemars(
        default = "default_act_type_verify_timeout_ms",
        range(min = 50, max = 5000)
    )]
    pub verify_timeout_ms: u32,
    #[serde(default)]
    #[schemars(
        default,
        description = "Opt in to pre-action CDP actionability polling for into_element web targets. When true, Synapse scrolls the node into view and waits until it is attached, visible, stable, enabled, editable, and receiving events before typing. Default false preserves existing typing semantics."
    )]
    pub auto_wait: bool,
    #[serde(default = "default_auto_wait_timeout_ms")]
    #[schemars(default = "default_auto_wait_timeout_ms", range(min = 50, max = 30000))]
    pub auto_wait_timeout_ms: u32,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TypeDynamics {
    Burst,
    Linear,
    Natural,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TypeBackend {
    Software,
    Hardware,
    Auto,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActTypeResponse {
    pub ok: bool,
    pub chars_typed: u32,
    pub elapsed_ms: u32,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub target_text_integrity: String,
    pub target_readback_required: bool,
    pub minimum_linear_ms_per_char: u32,
    pub postcondition: ActPostcondition,
}

/// Routes `act_type into_element=<web element id>` through CDP (#686): resolve
/// the browser endpoint from the element's window, focus the DOM node, and
/// insert the text. Fail-loud if the endpoint/node is gone.
#[cfg(windows)]
async fn cdp_type_into_element(
    element_id: &ElementId,
    backend_node_id: i64,
    emitted: &str,
    chars_typed: u32,
    started: Instant,
    verify_delta: bool,
    verify_timeout_ms: u32,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActTypeResponse, ErrorData> {
    use synapse_core::error_codes;

    let hwnd = element_id
        .parts()
        .map_err(|err| {
            mcp_error(
                error_codes::ACTION_ELEMENT_NOT_RESOLVED,
                format!("web element id is malformed: {err}"),
            )
        })?
        .hwnd;
    let title_hint = synapse_a11y::foreground_context(hwnd)
        .map(|context| context.window_title)
        .unwrap_or_default();
    let target_id_hint = synapse_a11y::cdp_target_from_element_id(element_id);
    let endpoint = synapse_a11y::endpoint_for_window(hwnd);
    let transport = if endpoint.is_some() {
        "raw_cdp"
    } else {
        "chrome_debugger_extension"
    };
    let before = if verify_delta {
        Some(
            cdp_or_extension_node_value(
                endpoint.as_deref(),
                hwnd,
                &title_hint,
                target_id_hint.as_deref(),
                backend_node_id,
            )
            .await
            .map_err(|err| cdp_transport_error("node value before readback", err))?,
        )
    } else {
        None
    };
    boundary.ensure("immediately_before_cdp_type_node")?;
    cdp_or_extension_type_node(
        endpoint.as_deref(),
        hwnd,
        &title_hint,
        target_id_hint.as_deref(),
        backend_node_id,
        emitted,
    )
    .await
    .map_err(|err| cdp_transport_error("insert text", err))?;
    let postcondition = if let Some(before) = before {
        tokio::time::sleep(std::time::Duration::from_millis(u64::from(
            verify_timeout_ms,
        )))
        .await;
        let after = cdp_or_extension_node_value(
            endpoint.as_deref(),
            hwnd,
            &title_hint,
            target_id_hint.as_deref(),
            backend_node_id,
        )
        .await
        .map_err(|err| cdp_transport_error("node value after readback", err))?;
        verify_cdp_type_delta(verify_timeout_ms, emitted, before, after)?
    } else {
        postcondition_not_requested("act_type", "cdp_node.value")
    };
    tracing::info!(
        code = "M2_ACT_TYPE_CDP_INSERT_TEXT",
        element_id = %element_id,
        chars_typed,
        transport,
        "readback=act_type_into_element method=cdp_insert_text chars_typed={chars_typed}"
    );
    Ok(ActTypeResponse {
        ok: true,
        chars_typed,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        backend_tier_used: TYPE_TIER_CDP.to_owned(),
        required_foreground: false,
        target_text_integrity: TEXT_INTEGRITY_CDP_INSERT_TEXT.to_owned(),
        target_readback_required: !verify_delta,
        minimum_linear_ms_per_char: MIN_SAFE_LINEAR_MS_PER_CHAR,
        postcondition,
    })
}

#[cfg(windows)]
enum CdpTypeTransportError {
    Raw(synapse_a11y::A11yError),
    Extension(crate::chrome_debugger_bridge::ChromeDebuggerBridgeError),
}

#[cfg(windows)]
impl CdpTypeTransportError {
    fn code(&self) -> &'static str {
        match self {
            Self::Raw(error) => error.code(),
            Self::Extension(error) => error.code(),
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::Raw(error) => error.to_string(),
            Self::Extension(error) => {
                format!(
                    "Chrome debugger extension type transport failed: {}",
                    error.detail()
                )
            }
        }
    }
}

#[cfg(windows)]
fn cdp_transport_error(operation: &str, error: CdpTypeTransportError) -> ErrorData {
    mcp_error(
        error.code(),
        format!("act_type CDP {operation} failed: {}", error.detail()),
    )
}

#[cfg(windows)]
async fn cdp_or_extension_node_value(
    endpoint: Option<&str>,
    hwnd: i64,
    title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
) -> Result<String, CdpTypeTransportError> {
    if let Some(endpoint) = endpoint {
        return synapse_a11y::cdp_node_value(endpoint, title_hint, target_id_hint, backend_node_id)
            .await
            .map_err(CdpTypeTransportError::Raw);
    }
    crate::chrome_debugger_bridge::node_value(hwnd, title_hint, target_id_hint, backend_node_id)
        .await
        .map(|readback| readback.value)
        .map_err(CdpTypeTransportError::Extension)
}

#[cfg(windows)]
async fn cdp_or_extension_type_node(
    endpoint: Option<&str>,
    hwnd: i64,
    title_hint: &str,
    target_id_hint: Option<&str>,
    backend_node_id: i64,
    emitted: &str,
) -> Result<(), CdpTypeTransportError> {
    if let Some(endpoint) = endpoint {
        return synapse_a11y::cdp_type_node(
            endpoint,
            title_hint,
            target_id_hint,
            backend_node_id,
            emitted,
        )
        .await
        .map_err(CdpTypeTransportError::Raw);
    }
    crate::chrome_debugger_bridge::type_node(
        hwnd,
        title_hint,
        target_id_hint,
        backend_node_id,
        emitted,
    )
    .await
    .map(|_result| ())
    .map_err(CdpTypeTransportError::Extension)
}

#[cfg(test)]
pub async fn act_type_with_handle(
    handle: ActionHandle,
    recording: Option<Arc<RecordingBackend>>,
    params: ActTypeParams,
) -> Result<ActTypeResponse, ErrorData> {
    let boundary = super::OperatorPanicActionBoundary::arm("act_type", "direct_call_entry")?;
    act_type_with_handle_and_boundary(handle, recording, params, boundary).await
}

pub(crate) async fn act_type_with_handle_and_boundary(
    handle: ActionHandle,
    recording: Option<Arc<RecordingBackend>>,
    params: ActTypeParams,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActTypeResponse, ErrorData> {
    let started = Instant::now();
    validate_type_params(&params)?;
    let emitted = emitted_text(&params);
    let chars_typed = char_count(&emitted)?;

    if let Some(element_id) = &params.into_element {
        // #686: a web element id (cdcd sentinel) routes through CDP focus+insert.
        #[cfg(windows)]
        if let Some(backend) = synapse_a11y::cdp_backend_from_element_id(element_id) {
            return cdp_type_into_element(
                element_id,
                backend,
                &emitted,
                chars_typed,
                started,
                params.verify_delta,
                params.verify_timeout_ms,
                boundary,
            )
            .await;
        }
        ensure_value_pattern_target_safe_for_act_type(element_id)?;
        let readback = if params.verify_delta {
            verified_set_element_value(element_id, &emitted, params.verify_timeout_ms, boundary)
                .await?
        } else {
            boundary.ensure("immediately_before_uia_set_element_value")?;
            synapse_a11y::set_element_value(element_id, &emitted).map_err(a11y_error_to_mcp)?
        };
        let readback_matches = uia_readback_matches_emitted(&readback, &emitted);
        if !readback_matches {
            tracing::warn!(
                code = "M2_ACT_TYPE_ELEMENT_VALUE_PATTERN_READBACK_MISMATCH",
                element_id = %element_id,
                method = %readback.method,
                is_password = readback.is_password,
                before_len = value_set_before_len(&readback),
                after_len = value_set_after_len(&readback),
                expected_len = expected_set_value(&readback, &emitted).chars().count(),
                chars_typed,
                "act_type into_element ValuePattern SetValue returned success but immediate UIA value readback did not match; target SoT readback is required"
            );
        }
        tracing::info!(
            code = "M2_ACT_TYPE_ELEMENT_VALUE_PATTERN_READBACK",
            element_id = %element_id,
            method = %readback.method,
            is_password = readback.is_password,
            before_len = value_set_before_len(&readback),
            after_len = value_set_after_len(&readback),
            chars_typed,
            "readback=act_type_into_element before_len={} after_len={} chars_typed={} method={} is_password={}",
            value_set_before_len(&readback),
            value_set_after_len(&readback),
            chars_typed,
            readback.method,
            readback.is_password
        );
        return Ok(ActTypeResponse {
            ok: true,
            chars_typed,
            elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
            backend_tier_used: set_backend_tier(&readback).to_owned(),
            required_foreground: false,
            target_text_integrity: if readback_matches {
                set_text_integrity(&readback)
            } else {
                TEXT_INTEGRITY_UIA_VALUE_PATTERN_DISPATCH_ONLY
            }
            .to_owned(),
            target_readback_required: !params.verify_delta || !readback_matches,
            minimum_linear_ms_per_char: MIN_SAFE_LINEAR_MS_PER_CHAR,
            postcondition: if params.verify_delta {
                verify_uia_type_delta(params.verify_timeout_ms, &emitted, &readback)?
            } else {
                postcondition_not_requested("act_type", set_source_of_truth(&readback))
            },
        });
    }

    let action = action_from_type_params(&params)?;

    boundary.ensure("immediately_before_foreground_type_dispatch")?;
    if let Some(recording) = recording {
        execute_recording(&recording, &action)?;
    } else {
        handle
            .execute(action)
            .await
            .map_err(|error| action_error_to_mcp(&error))?;
    }

    Ok(ActTypeResponse {
        ok: true,
        chars_typed,
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        backend_tier_used: TYPE_TIER_FOREGROUND.to_owned(),
        required_foreground: true,
        target_text_integrity: TEXT_INTEGRITY_DISPATCH_ONLY.to_owned(),
        target_readback_required: true,
        minimum_linear_ms_per_char: MIN_SAFE_LINEAR_MS_PER_CHAR,
        postcondition: postcondition_not_requested("act_type", "foreground_focused_ui_or_pixels"),
    })
}

pub fn action_from_type_params(params: &ActTypeParams) -> Result<Action, ErrorData> {
    validate_type_params(params)?;
    if let Some(element_id) = &params.into_element {
        return Err(action_error_to_mcp(&ActionError::BackendUnavailable {
            detail: format!(
                "act_type into_element target {element_id} requires live UIA ValuePattern dispatch, not action-only conversion"
            ),
        }));
    }
    Ok(Action::TypeText {
        text: emitted_text(params),
        dynamics: params
            .dynamics
            .to_keystroke_dynamics(params.linear_ms_per_char),
        backend: params.backend.to_backend(),
    })
}

async fn verified_set_element_value(
    element_id: &ElementId,
    emitted: &str,
    verify_timeout_ms: u32,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<synapse_a11y::ElementValueSetReadback, ErrorData> {
    let before = synapse_a11y::element_value(element_id).map_err(a11y_error_to_mcp)?;
    boundary.ensure("immediately_before_verified_uia_set_element_value")?;
    match synapse_a11y::set_element_value(element_id, emitted) {
        Ok(dispatch_readback) => {
            tokio::time::sleep(std::time::Duration::from_millis(u64::from(
                verify_timeout_ms,
            )))
            .await;
            let after = synapse_a11y::element_value(element_id).map_err(|error| {
                postcondition_failed_error(
                    "act_type",
                    set_source_of_truth(&dispatch_readback),
                    format!(
                        "ValuePattern SetValue returned success but separate target value readback failed: {error}"
                    ),
                    value_readback_signature(&before),
                    value_set_after_signature(&dispatch_readback),
                    json!({
                        "element_id": element_id.to_string(),
                        "method": dispatch_readback.method.clone(),
                        "requested_len": emitted.chars().count(),
                        "before_len": value_readback_len(&before),
                        "immediate_after_len": value_set_after_len(&dispatch_readback),
                        "readback_error_code": error.code(),
                        "readback_error": error.to_string(),
                    }),
                )
            })?;
            let verified_readback =
                set_readback_from_separate_reads(&dispatch_readback, &before, &after);
            if !uia_set_readbacks_equivalent(&dispatch_readback, &verified_readback) {
                tracing::warn!(
                    code = "M2_ACT_TYPE_ELEMENT_IMMEDIATE_READBACK_STALE",
                    element_id = %element_id,
                    method = %dispatch_readback.method,
                    requested_len = emitted.chars().count(),
                    immediate_before_len = value_set_before_len(&dispatch_readback),
                    immediate_after_len = value_set_after_len(&dispatch_readback),
                    verified_before_len = value_set_before_len(&verified_readback),
                    verified_after_len = value_set_after_len(&verified_readback),
                    "act_type into_element immediate UIA/native readback differed from separate post-dispatch Source-of-Truth readback"
                );
            }
            Ok(verified_readback)
        }
        Err(error) => {
            tokio::time::sleep(std::time::Duration::from_millis(u64::from(
                verify_timeout_ms,
            )))
            .await;
            let after = synapse_a11y::element_value(element_id).map_err(a11y_error_to_mcp)?;
            let before_is_password = before.is_password;
            let after_is_password = after.is_password;
            let source_of_truth =
                value_readback_source_of_truth(before_is_password || after_is_password);
            let before_signature = value_readback_signature(&before);
            let after_signature = value_readback_signature(&after);
            if uia_readbacks_equivalent(&before, &after) {
                return Err(no_observed_delta_error(
                    "act_type",
                    source_of_truth,
                    verify_timeout_ms,
                    before_signature,
                    after_signature,
                    json!({
                        "element_id": element_id.to_string(),
                        "before_len": value_readback_len(&before),
                        "after_len": value_readback_len(&after),
                        "before_readonly": before.is_readonly,
                        "after_readonly": after.is_readonly,
                        "before_is_password": before_is_password,
                        "after_is_password": after_is_password,
                        "before_password_len": before.password_len,
                        "after_password_len": after.password_len,
                        "set_error": error.to_string(),
                    }),
                ));
            }
            Err(postcondition_failed_error(
                "act_type",
                source_of_truth,
                format!("ValuePattern SetValue failed but value changed: {error}"),
                before_signature,
                after_signature,
                json!({
                    "element_id": element_id.to_string(),
                    "expected_len": expected_value_len(emitted, before_is_password || after_is_password),
                    "before_len": value_readback_len(&before),
                    "after_len": value_readback_len(&after),
                    "before_is_password": before_is_password,
                    "after_is_password": after_is_password,
                    "before_password_len": before.password_len,
                    "after_password_len": after.password_len,
                    "set_error": error.to_string(),
                }),
            ))
        }
    }
}

#[cfg(windows)]
fn ensure_value_pattern_target_safe_for_act_type(element_id: &ElementId) -> Result<(), ErrorData> {
    let hwnd = element_id
        .parts()
        .map_err(|err| {
            mcp_error(
                error_codes::ACTION_ELEMENT_NOT_RESOLVED,
                format!("act_type into_element id is malformed: {err}"),
            )
        })?
        .hwnd;
    let context = synapse_a11y::foreground_context(hwnd).map_err(a11y_error_to_mcp)?;
    if !synapse_a11y::is_chromium_family(&context.process_name) {
        return Ok(());
    }
    let metadata = synapse_a11y::element_metadata(element_id).map_err(a11y_error_to_mcp)?;
    if chromium_uia_value_pattern_should_be_refused(&context.process_name, &metadata) {
        return Err(chromium_uia_value_pattern_refused_error(
            element_id,
            &context.process_name,
            &metadata,
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn ensure_value_pattern_target_safe_for_act_type(_element_id: &ElementId) -> Result<(), ErrorData> {
    Ok(())
}

fn chromium_uia_value_pattern_should_be_refused(
    process_name: &str,
    metadata: &synapse_a11y::ElementMetadataReadback,
) -> bool {
    if !synapse_a11y::is_chromium_family(process_name) || !metadata.enabled {
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

fn chromium_uia_value_pattern_refused_error(
    element_id: &ElementId,
    process_name: &str,
    metadata: &synapse_a11y::ElementMetadataReadback,
) -> ErrorData {
    let value_len = metadata.value.as_ref().map(|value| value.chars().count());
    tracing::warn!(
        code = "M2_ACT_TYPE_CHROMIUM_UIA_VALUE_PATTERN_REFUSED",
        element_id = %element_id,
        process_name,
        role = %metadata.role,
        enabled = metadata.enabled,
        keyboard_focusable = metadata.keyboard_focusable,
        ?value_len,
        "act_type refused Chromium UIA ValuePattern.SetValue before mutation"
    );
    ErrorData::new(
        ErrorCode(-32099),
        "act_type refused UIA ValuePattern.SetValue for a Chromium editable UIA target before mutation; use a CDP-backed web element or the leased foreground typing route",
        Some(json!({
            "code": error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED,
            "reason": REASON_CHROMIUM_UIA_VALUE_PATTERN_REFUSED,
            "element_id": element_id.to_string(),
            "process_name": process_name,
            "role": metadata.role,
            "enabled": metadata.enabled,
            "keyboard_focusable": metadata.keyboard_focusable,
            "patterns": metadata.patterns,
            "automation_id_present": metadata.automation_id.is_some(),
            "name_len": metadata.name.chars().count(),
            "value_len": value_len,
            "unsafe_backend_tier_refused": TYPE_TIER_UIA,
            "required_foreground": true,
            "target_text_integrity": TEXT_INTEGRITY_CHROMIUM_UIA_VALUE_PATTERN_REFUSED,
            "target_readback_required": true,
        })),
    )
}

fn set_readback_from_separate_reads(
    dispatch_readback: &synapse_a11y::ElementValueSetReadback,
    before: &synapse_a11y::ElementValueReadback,
    after: &synapse_a11y::ElementValueReadback,
) -> synapse_a11y::ElementValueSetReadback {
    let is_password = dispatch_readback.is_password || before.is_password || after.is_password;
    synapse_a11y::ElementValueSetReadback {
        method: dispatch_readback.method.clone(),
        before_value: if is_password {
            String::new()
        } else {
            before.value.clone()
        },
        after_value: if is_password {
            String::new()
        } else {
            after.value.clone()
        },
        expected_after_value: dispatch_readback.expected_after_value.clone(),
        is_password,
        before_password_len: if is_password {
            before
                .password_len
                .or(dispatch_readback.before_password_len)
        } else {
            None
        },
        after_password_len: if is_password {
            after.password_len.or(dispatch_readback.after_password_len)
        } else {
            None
        },
    }
}

fn verify_uia_type_delta(
    verify_timeout_ms: u32,
    emitted: &str,
    readback: &synapse_a11y::ElementValueSetReadback,
) -> Result<ActPostcondition, ErrorData> {
    let before_signature = value_set_before_signature(readback);
    let after_signature = value_set_after_signature(readback);
    if readback.is_password {
        return verify_uia_password_length_delta(
            verify_timeout_ms,
            emitted,
            readback,
            before_signature,
            after_signature,
        );
    }
    if readback.before_value == readback.after_value {
        let expected_value = expected_set_value(readback, emitted);
        if readback.after_value == expected_value {
            return Ok(ActPostcondition {
                status: "verified_state".to_owned(),
                observed_delta: Some(false),
                source_of_truth: Some(set_source_of_truth(readback).to_owned()),
                before_signature: Some(before_signature),
                after_signature: Some(after_signature),
                detail: Some(format!(
                    "act_type verify_delta verified target value equals requested text after delivery; no Source-of-Truth delta was needed within {verify_timeout_ms} ms"
                )),
            });
        }
        return Err(no_observed_delta_error(
            "act_type",
            set_source_of_truth(readback),
            verify_timeout_ms,
            before_signature,
            after_signature,
            json!({
                "method": readback.method,
                "before_len": readback.before_value.chars().count(),
                "after_len": readback.after_value.chars().count(),
                "requested_len": emitted.chars().count(),
                "expected_len": expected_value.chars().count(),
                "normalized": expected_value != emitted,
            }),
        ));
    }
    let expected_value = expected_set_value(readback, emitted);
    if readback.after_value != expected_value {
        return Err(postcondition_failed_error(
            "act_type",
            set_source_of_truth(readback),
            "UIA ValuePattern value changed but does not equal expected text",
            before_signature,
            after_signature,
            json!({
                "method": readback.method,
                "before_len": readback.before_value.chars().count(),
                "after_len": readback.after_value.chars().count(),
                "requested_len": emitted.chars().count(),
                "expected_len": expected_value.chars().count(),
                "normalized": expected_value != emitted,
            }),
        ));
    }
    Ok(postcondition_observed_delta(
        "act_type",
        set_source_of_truth(readback),
        before_signature,
        after_signature,
        "observed target value equal requested text",
    ))
}

fn verify_uia_password_length_delta(
    verify_timeout_ms: u32,
    emitted: &str,
    readback: &synapse_a11y::ElementValueSetReadback,
    before_signature: String,
    after_signature: String,
) -> Result<ActPostcondition, ErrorData> {
    let Some(before_len) = readback.before_password_len else {
        return Err(postcondition_failed_error(
            "act_type",
            set_source_of_truth(readback),
            "UIA password ValuePattern readback did not include a before length Source of Truth",
            before_signature,
            after_signature,
            json!({
                "method": readback.method,
                "expected_len": expected_value_len(emitted, true),
                "is_password": true,
            }),
        ));
    };
    let Some(after_len) = readback.after_password_len else {
        return Err(postcondition_failed_error(
            "act_type",
            set_source_of_truth(readback),
            "UIA password ValuePattern readback did not include an after length Source of Truth",
            before_signature,
            after_signature,
            json!({
                "method": readback.method,
                "before_len": before_len,
                "expected_len": expected_value_len(emitted, true),
                "is_password": true,
            }),
        ));
    };
    let expected_len = expected_value_len(emitted, true);
    if after_len != expected_len {
        return Err(postcondition_failed_error(
            "act_type",
            set_source_of_truth(readback),
            "UIA password ValuePattern length changed but does not equal requested text length",
            before_signature,
            after_signature,
            json!({
                "method": readback.method,
                "before_len": before_len,
                "after_len": after_len,
                "expected_len": expected_len,
                "is_password": true,
            }),
        ));
    }
    if before_len == after_len {
        return Ok(ActPostcondition {
            status: "verified_state".to_owned(),
            observed_delta: Some(false),
            source_of_truth: Some(set_source_of_truth(readback).to_owned()),
            before_signature: Some(before_signature),
            after_signature: Some(after_signature),
            detail: Some(format!(
                "act_type verify_delta verified password target length equals requested length after delivery; value content intentionally not read or compared; timeout_ms={verify_timeout_ms}"
            )),
        });
    }
    Ok(postcondition_observed_delta(
        "act_type",
        set_source_of_truth(readback),
        before_signature,
        after_signature,
        "observed password target length equal requested text length; value content intentionally not read or compared",
    ))
}

fn verify_cdp_type_delta(
    verify_timeout_ms: u32,
    emitted: &str,
    before: String,
    after: String,
) -> Result<ActPostcondition, ErrorData> {
    let before_signature = text_signature(&before);
    let after_signature = text_signature(&after);
    if before == after {
        return Err(no_observed_delta_error(
            "act_type",
            "cdp_node.value",
            verify_timeout_ms,
            before_signature,
            after_signature,
            json!({
                "before_len": before.chars().count(),
                "after_len": after.chars().count(),
                "expected_insert_len": emitted.chars().count(),
            }),
        ));
    }
    if !after.contains(emitted) {
        return Err(postcondition_failed_error(
            "act_type",
            "cdp_node.value",
            "CDP node value changed but does not contain requested inserted text",
            before_signature,
            after_signature,
            json!({
                "before_len": before.chars().count(),
                "after_len": after.chars().count(),
                "expected_insert_len": emitted.chars().count(),
            }),
        ));
    }
    Ok(postcondition_observed_delta(
        "act_type",
        "cdp_node.value",
        before_signature,
        after_signature,
        "observed target value containing requested inserted text",
    ))
}

fn uia_readback_matches_emitted(
    readback: &synapse_a11y::ElementValueSetReadback,
    emitted: &str,
) -> bool {
    if readback.is_password {
        return readback.after_password_len == Some(expected_value_len(emitted, true));
    }
    readback.after_value == expected_set_value(readback, emitted)
}

fn expected_set_value<'a>(
    readback: &'a synapse_a11y::ElementValueSetReadback,
    emitted: &'a str,
) -> &'a str {
    readback.expected_after_value.as_deref().unwrap_or(emitted)
}

fn uia_readbacks_equivalent(
    before: &synapse_a11y::ElementValueReadback,
    after: &synapse_a11y::ElementValueReadback,
) -> bool {
    if before.is_password || after.is_password {
        return before.password_len == after.password_len;
    }
    before.value == after.value
}

fn uia_set_readbacks_equivalent(
    left: &synapse_a11y::ElementValueSetReadback,
    right: &synapse_a11y::ElementValueSetReadback,
) -> bool {
    if left.method != right.method
        || left.expected_after_value != right.expected_after_value
        || left.is_password != right.is_password
    {
        return false;
    }
    if left.is_password || right.is_password {
        return left.before_password_len == right.before_password_len
            && left.after_password_len == right.after_password_len;
    }
    left.before_value == right.before_value && left.after_value == right.after_value
}

fn value_set_before_len(readback: &synapse_a11y::ElementValueSetReadback) -> usize {
    value_set_len(
        readback.before_value.as_str(),
        readback.before_password_len,
        readback.is_password,
    )
}

fn value_set_after_len(readback: &synapse_a11y::ElementValueSetReadback) -> usize {
    value_set_len(
        readback.after_value.as_str(),
        readback.after_password_len,
        readback.is_password,
    )
}

fn value_set_len(value: &str, password_len: Option<usize>, is_password: bool) -> usize {
    if is_password {
        password_len.unwrap_or(0)
    } else {
        value.chars().count()
    }
}

fn value_readback_len(readback: &synapse_a11y::ElementValueReadback) -> usize {
    if readback.is_password {
        readback.password_len.unwrap_or(0)
    } else {
        readback.value.chars().count()
    }
}

fn value_set_before_signature(readback: &synapse_a11y::ElementValueSetReadback) -> String {
    value_set_signature(
        readback.before_value.as_str(),
        readback.before_password_len,
        readback.is_password,
    )
}

fn value_set_after_signature(readback: &synapse_a11y::ElementValueSetReadback) -> String {
    value_set_signature(
        readback.after_value.as_str(),
        readback.after_password_len,
        readback.is_password,
    )
}

fn value_set_signature(value: &str, password_len: Option<usize>, is_password: bool) -> String {
    if is_password {
        return password_len
            .map(|len| format!("password_len:{len}"))
            .unwrap_or_else(|| "password_len:<missing>".to_owned());
    }
    text_signature(value)
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

fn expected_value_len(value: &str, is_password: bool) -> usize {
    if is_password {
        value.encode_utf16().count()
    } else {
        value.chars().count()
    }
}

fn set_backend_tier(readback: &synapse_a11y::ElementValueSetReadback) -> &'static str {
    if readback.method == METHOD_NATIVE_TEXT_MESSAGE {
        TYPE_TIER_WIN32_MESSAGE
    } else {
        TYPE_TIER_UIA
    }
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

const fn value_readback_source_of_truth(is_password: bool) -> &'static str {
    if is_password {
        SOURCE_UIA_PASSWORD_LENGTH
    } else {
        SOURCE_UIA_VALUE
    }
}

impl TypeDynamics {
    const fn to_keystroke_dynamics(self, linear_ms_per_char: u32) -> KeystrokeDynamics {
        match self {
            Self::Burst => KeystrokeDynamics::Burst,
            Self::Linear => KeystrokeDynamics::Linear {
                ms_per_char: linear_ms_per_char,
            },
            Self::Natural => KeystrokeDynamics::Natural {
                params: KeystrokeNaturalParams::FAST,
            },
        }
    }
}

impl TypeBackend {
    const fn to_backend(self) -> Backend {
        match self {
            Self::Software => Backend::Software,
            Self::Hardware => Backend::Hardware,
            Self::Auto => Backend::Auto,
        }
    }
}

fn validate_type_params(params: &ActTypeParams) -> Result<(), ErrorData> {
    if params.text.is_empty() && !params.press_enter_after {
        return Err(empty_text_params_error());
    }
    if params.use_scancodes {
        return Err(action_error_to_mcp(&ActionError::BackendUnavailable {
            detail: "act_type use_scancodes=true is not wired for the M2 unicode typing path"
                .to_owned(),
        }));
    }
    if !(MIN_VERIFY_TIMEOUT_MS..=MAX_VERIFY_TIMEOUT_MS).contains(&params.verify_timeout_ms) {
        return Err(verify_timeout_params_error(params.verify_timeout_ms));
    }
    if params.dynamics == TypeDynamics::Linear
        && params.linear_ms_per_char < MIN_SAFE_LINEAR_MS_PER_CHAR
    {
        return Err(type_params_error(
            params.linear_ms_per_char,
            format!(
                "act_type linear_ms_per_char {} is below the text-integrity minimum {}; use slower pacing and verify target text via UI/file readback",
                params.linear_ms_per_char, MIN_SAFE_LINEAR_MS_PER_CHAR
            ),
        ));
    }
    Ok(())
}

pub(crate) fn emitted_text(params: &ActTypeParams) -> String {
    if params.press_enter_after {
        let mut text = params.text.clone();
        text.push('\n');
        text
    } else {
        params.text.clone()
    }
}

fn char_count(text: &str) -> Result<u32, ErrorData> {
    u32::try_from(text.chars().count()).map_err(|_err| {
        mcp_error(
            synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "act_type text has more than u32::MAX chars",
        )
    })
}

fn execute_recording(recording: &RecordingBackend, action: &Action) -> Result<(), ErrorData> {
    let before_events = recording.events();
    let before_event_count = before_events.len();
    let mut emit_state = EmitState::new();
    recording
        .execute(action, &mut emit_state)
        .map_err(|error| action_error_to_mcp(&error))?;
    let after_events = recording.events();
    let new_events = &after_events[before_event_count..];
    let recorded_ikis = recorded_ikis(new_events);
    tracing::info!(
        code = "M2_ACT_TYPE_RECORDING_READBACK",
        kind = "act_type",
        before_event_count,
        after_event_count = after_events.len(),
        new_event_count = new_events.len(),
        ?recorded_ikis,
        ?new_events,
        "readback=recording_backend tool=act_type after_events_readback"
    );
    Ok(())
}

fn recorded_ikis(events: &[RecordedInput]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|event| match event {
            RecordedInput::DelayMs { ms } => Some(*ms),
            _ => None,
        })
        .collect()
}

fn action_error_to_mcp(error: &ActionError) -> ErrorData {
    crate::m2::action_error_to_mcp(error)
}

fn a11y_error_to_mcp(error: synapse_a11y::A11yError) -> ErrorData {
    mcp_error(error.code(), error.to_string())
}

fn type_params_error(requested_linear_ms_per_char: u32, message: impl Into<String>) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message.into(),
        Some(json!({
            "code": synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "reason": "linear_ms_per_char_below_text_integrity_minimum",
            "requested_linear_ms_per_char": requested_linear_ms_per_char,
            "minimum_linear_ms_per_char": MIN_SAFE_LINEAR_MS_PER_CHAR,
            "target_text_integrity": TEXT_INTEGRITY_DISPATCH_ONLY,
            "target_readback_required": true,
            "backend_tier_used": TYPE_TIER_FOREGROUND,
            "required_foreground": true,
        })),
    )
}

fn empty_text_params_error() -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        "act_type text must be non-empty unless press_enter_after=true emits a newline",
        Some(json!({
            "code": synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "reason": "empty_text_without_emitted_input",
            "text_len": 0,
            "press_enter_after": false,
            "target_text_integrity": TEXT_INTEGRITY_DISPATCH_ONLY,
            "target_readback_required": true,
        })),
    )
}

fn verify_timeout_params_error(requested_verify_timeout_ms: u32) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "act_type verify_timeout_ms must be in {MIN_VERIFY_TIMEOUT_MS}..={MAX_VERIFY_TIMEOUT_MS}, got {requested_verify_timeout_ms}"
        ),
        Some(json!({
            "code": synapse_core::error_codes::TOOL_PARAMS_INVALID,
            "reason": "verify_timeout_ms_out_of_range",
            "requested_verify_timeout_ms": requested_verify_timeout_ms,
            "minimum_verify_timeout_ms": MIN_VERIFY_TIMEOUT_MS,
            "maximum_verify_timeout_ms": MAX_VERIFY_TIMEOUT_MS,
            "target_readback_required": true,
        })),
    )
}

const fn default_type_dynamics() -> TypeDynamics {
    TypeDynamics::Natural
}

const fn default_linear_ms_per_char() -> u32 {
    30
}

const fn default_type_backend() -> TypeBackend {
    TypeBackend::Auto
}

const fn default_verify_delta() -> bool {
    true
}

const fn default_act_type_verify_timeout_ms() -> u32 {
    DEFAULT_ACT_TYPE_VERIFY_TIMEOUT_MS
}

const fn default_use_scancodes() -> bool {
    false
}

const fn default_press_enter_after() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use synapse_action::{ActionEmitter, RecordedInput, sample_typing_schedule};
    use synapse_core::{ElementId, KeystrokeNaturalParams, Rect, UiaPattern};

    use crate::m2::default_verify_timeout_ms;

    use super::{
        ActTypeParams, DEFAULT_ACT_TYPE_VERIFY_TIMEOUT_MS, METHOD_NATIVE_TEXT_MESSAGE,
        MIN_SAFE_LINEAR_MS_PER_CHAR, SOURCE_NATIVE_PASSWORD_LENGTH, SOURCE_NATIVE_TEXT,
        SOURCE_UIA_PASSWORD_LENGTH, SOURCE_UIA_VALUE, TEXT_INTEGRITY_DISPATCH_ONLY,
        TEXT_INTEGRITY_NATIVE_PASSWORD_LENGTH, TEXT_INTEGRITY_NATIVE_TEXT_MESSAGE,
        TYPE_TIER_FOREGROUND, TYPE_TIER_WIN32_MESSAGE, TypeBackend, TypeDynamics,
        act_type_with_handle, action_from_type_params, chromium_uia_value_pattern_refused_error,
        chromium_uia_value_pattern_should_be_refused, default_act_type_verify_timeout_ms,
        default_linear_ms_per_char, default_press_enter_after, default_type_backend,
        default_type_dynamics, default_use_scancodes, default_verify_delta, recorded_ikis,
        set_backend_tier, set_readback_from_separate_reads, set_source_of_truth,
        set_text_integrity, uia_readback_matches_emitted, validate_type_params,
        verify_uia_type_delta,
    };

    fn metadata(role: &str, patterns: Vec<UiaPattern>) -> synapse_a11y::ElementMetadataReadback {
        synapse_a11y::ElementMetadataReadback {
            name: "synthetic".to_owned(),
            role: role.to_owned(),
            automation_id: None,
            bbox: Rect {
                x: 1,
                y: 2,
                w: 300,
                h: 40,
            },
            enabled: true,
            keyboard_focusable: true,
            patterns,
            value: Some("before".to_owned()),
        }
    }

    #[test]
    fn chromium_edit_value_pattern_is_refused_before_mutation() {
        let metadata = metadata("edit", vec![UiaPattern::Value]);

        assert!(chromium_uia_value_pattern_should_be_refused(
            "chrome.exe",
            &metadata
        ));
    }

    #[test]
    fn chromium_document_text_pattern_is_refused_before_mutation() {
        let metadata = metadata("document", vec![UiaPattern::Value, UiaPattern::Text]);

        assert!(chromium_uia_value_pattern_should_be_refused(
            "msedge.exe",
            &metadata
        ));
    }

    #[test]
    fn non_chromium_edit_value_pattern_remains_allowed() {
        let metadata = metadata("edit", vec![UiaPattern::Value]);

        assert!(!chromium_uia_value_pattern_should_be_refused(
            "notepad.exe",
            &metadata
        ));
    }

    #[test]
    fn chromium_non_edit_value_pattern_remains_allowed() {
        let metadata = metadata("button", vec![UiaPattern::Value]);

        assert!(!chromium_uia_value_pattern_should_be_refused(
            "chrome.exe",
            &metadata
        ));
    }

    #[test]
    fn chromium_text_role_without_value_pattern_remains_allowed() {
        let metadata = metadata("edit", vec![UiaPattern::Text]);

        assert!(!chromium_uia_value_pattern_should_be_refused(
            "chrome.exe",
            &metadata
        ));
    }

    #[test]
    fn chromium_refusal_error_is_structured_and_redacts_text() {
        let element_id = ElementId::parse("0x2a:0102").expect("synthetic element id should parse");
        let metadata = metadata("edit", vec![UiaPattern::Value]);

        let error = chromium_uia_value_pattern_refused_error(&element_id, "chrome.exe", &metadata);
        let data = error
            .data
            .expect("chromium refusal should include structured data");

        assert_eq!(
            data["code"],
            synapse_core::error_codes::ACTION_ELEMENT_PATTERN_UNSUPPORTED
        );
        assert_eq!(data["reason"], "chromium_uia_value_pattern_refused");
        assert_eq!(data["value_len"], 6);
        assert!(data.get("value").is_none());
    }

    #[tokio::test]
    async fn recording_backend_readback_uses_natural_fast_ikis() {
        let (handle, _snapshot_handle, _emitter) = ActionEmitter::channel();
        let recording = Arc::new(synapse_action::RecordingBackend::new());
        let text = "Hello world.";
        let params = ActTypeParams {
            text: text.to_owned(),
            into_element: None,
            dynamics: default_type_dynamics(),
            linear_ms_per_char: default_linear_ms_per_char(),
            use_scancodes: false,
            press_enter_after: false,
            backend: default_type_backend(),
            verify_delta: false,
            verify_target_window_hwnd: None,
            expected_browser_url_regex: None,
            verify_timeout_ms: default_verify_timeout_ms(),
            auto_wait: false,
            auto_wait_timeout_ms: crate::m2::default_auto_wait_timeout_ms(),
        };
        let before = recording.events();
        println!("readback=act_type_recording edge=natural_fast before={before:?}");

        let response = act_type_with_handle(handle, Some(Arc::clone(&recording)), params)
            .await
            .unwrap_or_else(|error| panic!("act_type recording should succeed: {error}"));
        let after = recording.events();
        let actual_ikis = recorded_ikis(&after);
        let expected_ikis: Vec<u32> = sample_typing_schedule(
            text,
            &TypeDynamics::Natural.to_keystroke_dynamics(default_linear_ms_per_char()),
            None,
        )
        .into_iter()
        .filter_map(|event| (event.iki_ms_before > 0).then_some(event.iki_ms_before))
        .collect();
        println!(
            "readback=act_type_recording edge=natural_fast after={after:?} expected_ikis={expected_ikis:?} actual_ikis={actual_ikis:?} chars_typed={}",
            response.chars_typed
        );

        assert!(response.ok);
        assert_eq!(response.chars_typed, 12);
        assert_eq!(response.backend_tier_used, TYPE_TIER_FOREGROUND);
        assert!(response.required_foreground);
        assert_eq!(response.target_text_integrity, TEXT_INTEGRITY_DISPATCH_ONLY);
        assert!(response.target_readback_required);
        assert_eq!(
            response.minimum_linear_ms_per_char,
            MIN_SAFE_LINEAR_MS_PER_CHAR
        );
        assert_eq!(actual_ikis, expected_ikis);
        assert_eq!(
            TypeDynamics::Natural.to_keystroke_dynamics(default_linear_ms_per_char()),
            synapse_core::KeystrokeDynamics::Natural {
                params: KeystrokeNaturalParams::FAST
            }
        );
    }

    #[test]
    fn defaults_are_issue_required_values() {
        assert_eq!(default_type_dynamics(), TypeDynamics::Natural);
        assert_eq!(default_linear_ms_per_char(), 30);
        assert_eq!(default_type_backend(), TypeBackend::Auto);
        assert!(default_verify_delta());
        assert_eq!(
            default_act_type_verify_timeout_ms(),
            DEFAULT_ACT_TYPE_VERIFY_TIMEOUT_MS
        );
        assert!(!default_use_scancodes());
        assert!(!default_press_enter_after());
    }

    #[test]
    fn act_type_params_default_verify_timeout_allows_web_field_settle_time() {
        let params: ActTypeParams = serde_json::from_value(serde_json::json!({
            "text": "issue880-default-timeout",
            "dynamics": "burst",
            "backend": "auto"
        }))
        .expect("minimal act_type params should deserialize");

        println!(
            "readback=act_type_defaults field=verify_timeout_ms actual={} expected={DEFAULT_ACT_TYPE_VERIFY_TIMEOUT_MS}",
            params.verify_timeout_ms
        );
        assert_eq!(params.verify_timeout_ms, DEFAULT_ACT_TYPE_VERIFY_TIMEOUT_MS);
        assert!(!params.auto_wait);
        assert_eq!(
            params.auto_wait_timeout_ms,
            crate::m2::default_auto_wait_timeout_ms()
        );
    }

    #[test]
    fn recorded_ikis_only_reads_delay_events() {
        let before = vec![
            RecordedInput::DelayMs { ms: 17 },
            RecordedInput::DelayMs { ms: 0 },
        ];
        let after = recorded_ikis(&before);
        println!("readback=act_type_recording edge=iki_readback before={before:?} after={after:?}");
        assert_eq!(after, [17, 0]);
    }

    #[test]
    fn linear_typing_below_safe_minimum_fails_closed() {
        let params = ActTypeParams {
            text: "unsafe".to_owned(),
            into_element: None,
            dynamics: TypeDynamics::Linear,
            linear_ms_per_char: MIN_SAFE_LINEAR_MS_PER_CHAR - 1,
            use_scancodes: false,
            press_enter_after: false,
            backend: TypeBackend::Software,
            verify_delta: false,
            verify_target_window_hwnd: None,
            expected_browser_url_regex: None,
            verify_timeout_ms: default_verify_timeout_ms(),
            auto_wait: false,
            auto_wait_timeout_ms: crate::m2::default_auto_wait_timeout_ms(),
        };

        let error = match action_from_type_params(&params) {
            Ok(action) => panic!("low linear pacing dispatched unexpectedly: {action:?}"),
            Err(error) => error,
        };
        let Some(data) = error.data else {
            panic!("low linear pacing error had no structured data");
        };

        assert_eq!(data["code"], synapse_core::error_codes::TOOL_PARAMS_INVALID);
        assert_eq!(
            data["reason"],
            "linear_ms_per_char_below_text_integrity_minimum"
        );
        assert_eq!(
            data["minimum_linear_ms_per_char"],
            MIN_SAFE_LINEAR_MS_PER_CHAR
        );
        assert_eq!(
            data["requested_linear_ms_per_char"],
            MIN_SAFE_LINEAR_MS_PER_CHAR - 1
        );
        assert_eq!(data["target_readback_required"], true);
        assert_eq!(data["target_text_integrity"], TEXT_INTEGRITY_DISPATCH_ONLY);
    }

    #[test]
    fn empty_text_without_enter_fails_closed_before_dispatch() {
        let params = ActTypeParams {
            text: String::new(),
            into_element: None,
            dynamics: TypeDynamics::Linear,
            linear_ms_per_char: MIN_SAFE_LINEAR_MS_PER_CHAR,
            use_scancodes: false,
            press_enter_after: false,
            backend: TypeBackend::Software,
            verify_delta: true,
            verify_target_window_hwnd: None,
            expected_browser_url_regex: None,
            verify_timeout_ms: default_verify_timeout_ms(),
            auto_wait: false,
            auto_wait_timeout_ms: crate::m2::default_auto_wait_timeout_ms(),
        };

        let error = match action_from_type_params(&params) {
            Ok(action) => panic!("empty no-op text dispatched unexpectedly: {action:?}"),
            Err(error) => error,
        };
        let Some(data) = error.data else {
            panic!("empty no-op text error had no structured data");
        };

        assert_eq!(data["code"], synapse_core::error_codes::TOOL_PARAMS_INVALID);
        assert_eq!(data["reason"], "empty_text_without_emitted_input");
        assert_eq!(data["text_len"], 0);
        assert_eq!(data["press_enter_after"], false);
        assert_eq!(data["target_readback_required"], true);
    }

    #[test]
    fn linear_typing_at_safe_minimum_is_allowed() {
        let params = ActTypeParams {
            text: "safe".to_owned(),
            into_element: None,
            dynamics: TypeDynamics::Linear,
            linear_ms_per_char: MIN_SAFE_LINEAR_MS_PER_CHAR,
            use_scancodes: false,
            press_enter_after: false,
            backend: TypeBackend::Software,
            verify_delta: false,
            verify_target_window_hwnd: None,
            expected_browser_url_regex: None,
            verify_timeout_ms: default_verify_timeout_ms(),
            auto_wait: false,
            auto_wait_timeout_ms: crate::m2::default_auto_wait_timeout_ms(),
        };

        let action = match action_from_type_params(&params) {
            Ok(action) => action,
            Err(error) => panic!("linear pacing at safe minimum failed unexpectedly: {error}"),
        };
        assert_eq!(
            action,
            synapse_core::Action::TypeText {
                text: "safe".to_owned(),
                dynamics: synapse_core::KeystrokeDynamics::Linear {
                    ms_per_char: MIN_SAFE_LINEAR_MS_PER_CHAR,
                },
                backend: synapse_core::Backend::Software,
            }
        );
    }

    #[test]
    fn verify_timeout_below_minimum_fails_before_dispatch() {
        let params = ActTypeParams {
            text: "should-not-dispatch".to_owned(),
            into_element: Some(
                ElementId::parse("0x1234:0000002a00000001")
                    .expect("synthetic element id should parse"),
            ),
            dynamics: default_type_dynamics(),
            linear_ms_per_char: default_linear_ms_per_char(),
            use_scancodes: false,
            press_enter_after: false,
            backend: default_type_backend(),
            verify_delta: true,
            verify_target_window_hwnd: None,
            expected_browser_url_regex: None,
            verify_timeout_ms: 1,
            auto_wait: false,
            auto_wait_timeout_ms: crate::m2::default_auto_wait_timeout_ms(),
        };

        let error = validate_type_params(&params)
            .expect_err("invalid verify timeout must fail validation before dispatch");
        let data = error.data.expect("validation error should include data");
        assert_eq!(data["code"], synapse_core::error_codes::TOOL_PARAMS_INVALID);
        assert_eq!(data["reason"], "verify_timeout_ms_out_of_range");
        assert_eq!(data["requested_verify_timeout_ms"], 1);
    }

    #[test]
    fn action_only_conversion_rejects_into_element() {
        let params = ActTypeParams {
            text: "targeted".to_owned(),
            into_element: Some(
                ElementId::parse("0x1000:0000002a00000001")
                    .expect("synthetic element id should parse"),
            ),
            dynamics: TypeDynamics::Linear,
            linear_ms_per_char: MIN_SAFE_LINEAR_MS_PER_CHAR,
            use_scancodes: false,
            press_enter_after: false,
            backend: TypeBackend::Software,
            verify_delta: false,
            verify_target_window_hwnd: None,
            expected_browser_url_regex: None,
            verify_timeout_ms: default_verify_timeout_ms(),
            auto_wait: false,
            auto_wait_timeout_ms: crate::m2::default_auto_wait_timeout_ms(),
        };

        let error = match action_from_type_params(&params) {
            Ok(action) => panic!("element-targeted act_type converted unexpectedly: {action:?}"),
            Err(error) => error,
        };
        assert!(
            error
                .message
                .contains("requires live UIA ValuePattern dispatch")
        );
    }

    #[test]
    fn password_value_pattern_uses_length_source_of_truth() {
        let readback = synapse_a11y::ElementValueSetReadback {
            method: "uia_value_pattern".to_owned(),
            before_value: String::new(),
            after_value: String::new(),
            expected_after_value: None,
            is_password: true,
            before_password_len: Some(0),
            after_password_len: Some(7),
        };

        assert!(uia_readback_matches_emitted(&readback, "p@ss727"));
        let postcondition =
            verify_uia_type_delta(default_verify_timeout_ms(), "p@ss727", &readback)
                .expect("password length readback should verify");

        assert_eq!(postcondition.status, "observed_delta");
        assert_eq!(postcondition.observed_delta, Some(true));
        assert_eq!(
            postcondition.source_of_truth.as_deref(),
            Some(SOURCE_UIA_PASSWORD_LENGTH)
        );
        assert_eq!(
            postcondition.before_signature.as_deref(),
            Some("password_len:0")
        );
        assert_eq!(
            postcondition.after_signature.as_deref(),
            Some("password_len:7")
        );
    }

    #[test]
    fn password_value_pattern_same_length_verifies_state() {
        let readback = synapse_a11y::ElementValueSetReadback {
            method: "uia_value_pattern".to_owned(),
            before_value: String::new(),
            after_value: String::new(),
            expected_after_value: None,
            is_password: true,
            before_password_len: Some(7),
            after_password_len: Some(7),
        };

        let postcondition =
            verify_uia_type_delta(default_verify_timeout_ms(), "p@ss727", &readback)
                .expect("password same-length state should verify");

        assert_eq!(postcondition.status, "verified_state");
        assert_eq!(postcondition.observed_delta, Some(false));
        assert_eq!(
            postcondition.source_of_truth.as_deref(),
            Some(SOURCE_UIA_PASSWORD_LENGTH)
        );
    }

    #[test]
    fn password_value_pattern_length_mismatch_fails_closed() {
        let readback = synapse_a11y::ElementValueSetReadback {
            method: "uia_value_pattern".to_owned(),
            before_value: String::new(),
            after_value: String::new(),
            expected_after_value: None,
            is_password: true,
            before_password_len: Some(0),
            after_password_len: Some(6),
        };

        let error = match verify_uia_type_delta(default_verify_timeout_ms(), "p@ss727", &readback) {
            Ok(postcondition) => {
                panic!("password mismatch verified unexpectedly: {postcondition:?}")
            }
            Err(error) => error,
        };

        assert!(
            error
                .message
                .contains("Source-of-Truth postcondition failed")
        );
        let data = error.data.expect("password mismatch should include data");
        assert_eq!(data["source_of_truth"], SOURCE_UIA_PASSWORD_LENGTH);
        assert_eq!(data["verify_delta"]["after_signature"], "password_len:6");
    }

    #[test]
    fn password_value_pattern_missing_length_fails_closed() {
        let readback = synapse_a11y::ElementValueSetReadback {
            method: "uia_value_pattern".to_owned(),
            before_value: String::new(),
            after_value: String::new(),
            expected_after_value: None,
            is_password: true,
            before_password_len: None,
            after_password_len: Some(7),
        };

        let error = match verify_uia_type_delta(default_verify_timeout_ms(), "p@ss727", &readback) {
            Ok(postcondition) => {
                panic!("missing password length verified unexpectedly: {postcondition:?}")
            }
            Err(error) => error,
        };

        assert!(error.message.contains("postcondition failed"));
        let data = error
            .data
            .expect("missing password length should include data");
        assert_eq!(data["source_of_truth"], SOURCE_UIA_PASSWORD_LENGTH);
        assert_eq!(data["verify_delta"]["after_signature"], "password_len:7");
    }

    #[test]
    fn native_text_message_reports_win32_source_and_tier() {
        let readback = synapse_a11y::ElementValueSetReadback {
            method: METHOD_NATIVE_TEXT_MESSAGE.to_owned(),
            before_value: String::new(),
            after_value: "WMSETTEXT-BG-727".to_owned(),
            expected_after_value: None,
            is_password: false,
            before_password_len: None,
            after_password_len: None,
        };

        assert_eq!(set_backend_tier(&readback), TYPE_TIER_WIN32_MESSAGE);
        assert_eq!(
            set_text_integrity(&readback),
            TEXT_INTEGRITY_NATIVE_TEXT_MESSAGE
        );
        assert_eq!(set_source_of_truth(&readback), SOURCE_NATIVE_TEXT);

        let postcondition =
            verify_uia_type_delta(default_verify_timeout_ms(), "WMSETTEXT-BG-727", &readback)
                .expect("native text message readback should verify");
        assert_eq!(postcondition.status, "observed_delta");
        assert_eq!(
            postcondition.source_of_truth.as_deref(),
            Some(SOURCE_NATIVE_TEXT)
        );
    }

    #[test]
    fn native_password_message_reports_length_source() {
        let readback = synapse_a11y::ElementValueSetReadback {
            method: METHOD_NATIVE_TEXT_MESSAGE.to_owned(),
            before_value: String::new(),
            after_value: String::new(),
            expected_after_value: None,
            is_password: true,
            before_password_len: Some(0),
            after_password_len: Some(7),
        };

        assert_eq!(set_backend_tier(&readback), TYPE_TIER_WIN32_MESSAGE);
        assert_eq!(
            set_text_integrity(&readback),
            TEXT_INTEGRITY_NATIVE_PASSWORD_LENGTH
        );
        assert_eq!(
            set_source_of_truth(&readback),
            SOURCE_NATIVE_PASSWORD_LENGTH
        );

        let postcondition =
            verify_uia_type_delta(default_verify_timeout_ms(), "p@ss727", &readback)
                .expect("native password length readback should verify");
        assert_eq!(postcondition.status, "observed_delta");
        assert_eq!(
            postcondition.source_of_truth.as_deref(),
            Some(SOURCE_NATIVE_PASSWORD_LENGTH)
        );
        assert_eq!(
            postcondition.after_signature.as_deref(),
            Some("password_len:7")
        );
    }

    #[test]
    fn native_text_message_can_verify_normalized_multiline_expected_value() {
        let readback = synapse_a11y::ElementValueSetReadback {
            method: METHOD_NATIVE_TEXT_MESSAGE.to_owned(),
            before_value: String::new(),
            after_value: "LineA-727\r\nLineB-727".to_owned(),
            expected_after_value: Some("LineA-727\r\nLineB-727".to_owned()),
            is_password: false,
            before_password_len: None,
            after_password_len: None,
        };

        assert!(uia_readback_matches_emitted(
            &readback,
            "LineA-727\nLineB-727"
        ));
        let postcondition = verify_uia_type_delta(
            default_verify_timeout_ms(),
            "LineA-727\nLineB-727",
            &readback,
        )
        .expect("normalized multiline readback should verify");

        assert_eq!(postcondition.status, "observed_delta");
        assert_eq!(
            postcondition.source_of_truth.as_deref(),
            Some(SOURCE_NATIVE_TEXT)
        );
    }

    #[test]
    fn stale_immediate_value_pattern_readback_uses_separate_after_sot() {
        let dispatch_readback = synapse_a11y::ElementValueSetReadback {
            method: "uia_value_pattern".to_owned(),
            before_value: String::new(),
            after_value: String::new(),
            expected_after_value: None,
            is_password: false,
            before_password_len: None,
            after_password_len: None,
        };
        let before = synapse_a11y::ElementValueReadback {
            method: "uia_value_pattern".to_owned(),
            value: String::new(),
            is_readonly: false,
            is_password: false,
            password_len: None,
        };
        let after = synapse_a11y::ElementValueReadback {
            method: "uia_value_pattern".to_owned(),
            value: "GH-CODE-825".to_owned(),
            is_readonly: false,
            is_password: false,
            password_len: None,
        };

        let verified = set_readback_from_separate_reads(&dispatch_readback, &before, &after);

        assert_eq!(verified.before_value, "");
        assert_eq!(verified.after_value, "GH-CODE-825");
        assert!(uia_readback_matches_emitted(&verified, "GH-CODE-825"));
        let postcondition =
            verify_uia_type_delta(default_verify_timeout_ms(), "GH-CODE-825", &verified)
                .expect("separate after SoT should verify the actual mutation");
        assert_eq!(postcondition.status, "observed_delta");
        assert_eq!(postcondition.observed_delta, Some(true));
        assert_eq!(
            postcondition.source_of_truth.as_deref(),
            Some(SOURCE_UIA_VALUE)
        );
    }

    #[test]
    fn value_pattern_already_matching_requested_text_verifies_state() {
        let readback = synapse_a11y::ElementValueSetReadback {
            method: "uia_value_pattern".to_owned(),
            before_value: "READY-825".to_owned(),
            after_value: "READY-825".to_owned(),
            expected_after_value: None,
            is_password: false,
            before_password_len: None,
            after_password_len: None,
        };

        let postcondition =
            verify_uia_type_delta(default_verify_timeout_ms(), "READY-825", &readback)
                .expect("matching final state should not be reported as no delta");

        assert_eq!(postcondition.status, "verified_state");
        assert_eq!(postcondition.observed_delta, Some(false));
        assert_eq!(
            postcondition.source_of_truth.as_deref(),
            Some(SOURCE_UIA_VALUE)
        );
    }
}
