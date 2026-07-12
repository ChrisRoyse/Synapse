//! `browser_set_value` (#1000/#994/#1005/#717): background-safe field REPLACE in
//! the user's *normal, authenticated* Chrome via the safe extension bridge — no
//! debugger attach, no OS foreground, no UIA dependency.
//!
//! Root cause this closes: when a Chromium web form is perceived UIA-only (the
//! user's real Chrome has no `--remote-debugging-port`, by design), the legacy
//! `act_set_field_text` Chromium path was the *foreground* UIA-keys tier — so an
//! agent had to steal the operator's foreground to type into a tab. The bridge's
//! `chrome.scripting` path runs in the renderer regardless of paint/foreground
//! state (proven live: it reads/acts on occluded, inactive tabs UIA cannot see),
//! so this tool replaces a field's value entirely in-page by a strict selector,
//! Chrome bridge element id, or active element, and verifies it with TWO
//! independent reads: the in-page post-set readback, and a separate
//! `chrome.tabs` active-element readback. Fail-loud on every divergence; never
//! an optimistic success and never a foreground fallback.

use super::browser_facades::merge_top_level_target;
use super::{ErrorData, Json, Parameters, SessionTarget, SynapseService, tool, tool_router};
use crate::m1::mcp_error;
use crate::m2::postcondition::text_signature;
use rmcp::schemars::JsonSchema;
use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::error_codes;

const TOOL: &str = "browser_set_value";
const FORM_TOOL: &str = "browser_form";
const CHROME_TAB_PREFIX: &str = "chrome-tab:";
const SOURCE_OF_TRUTH: &str =
    "chrome_bridge_in_page_value + separate chrome.tabs active-element readback";
const FORM_SOURCE_OF_TRUTH: &str =
    "target-scoped DOM form mutation through browser_set_value/browser_fill_form";
const FORM_READBACK_SOURCE_OF_TRUTH: &str =
    "browser_set_value dual readback or browser_fill_form per-field DOM readback";

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserFormOperation {
    SetValue,
    Fill,
}

impl BrowserFormOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SetValue => "set_value",
            Self::Fill => "fill",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserFormParams {
    /// Form operation to run. Supply exactly the matching nested spec object.
    pub operation: BrowserFormOperation,
    /// Optional top-level target alias (#1551). When set, this populates the
    /// selected operation spec's `cdp_target_id`; a conflicting nested value
    /// fails closed. Defaults to the nested spec / session target.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Optional top-level target-window alias (#1551). When set, this populates
    /// the selected operation spec's `window_hwnd`; a conflicting nested value
    /// fails closed. Defaults to the nested spec / session target window.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// `operation=set_value`: replace one field value with dual readback.
    #[serde(default)]
    pub set_value: Option<BrowserSetValueParams>,
    /// `operation=fill`: apply ordered multi-field form changes.
    #[serde(default)]
    pub fill: Option<BrowserFillFormParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserFormResponse {
    pub operation: BrowserFormOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_value: Option<BrowserSetValueResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<BrowserFillFormResponse>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserSetValueParams {
    /// Full replacement text. Empty string clears the field.
    pub text: String,
    /// Strict CSS selector for the target field. Exactly one editable+visible
    /// match is required; 0 or >1 fails loud. Mutually exclusive with
    /// `element_id` and `active_element`.
    #[serde(default)]
    pub selector: Option<String>,
    /// Normal Chrome bridge element id
    /// (`chrome-tab:<tabId>:frame:<frameId>:path:<domPath>`) returned by
    /// browser locate/read tools. Mutually exclusive with `selector` and
    /// `active_element`.
    #[serde(default)]
    pub element_id: Option<String>,
    /// Target the tab's current `document.activeElement` instead of a selector.
    /// Mutually exclusive with `selector` and `element_id`.
    #[serde(default)]
    pub active_element: bool,
    /// Chrome bridge tab target id (`chrome-tab:<id>`). Defaults to this
    /// session's active CDP target. Must be owned by this session; the human
    /// foreground tab is never an implicit fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND owning the target. Defaults to the session target's window.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserSetValueResponse {
    pub ok: bool,
    /// Always false — this tool never drives or depends on the OS foreground.
    pub required_foreground: bool,
    pub transport: String,
    pub window_hwnd: i64,
    pub cdp_target_id: String,
    /// `selector`, `element_id`, or `active_element`.
    pub resolved_by: String,
    /// Editable+visible nodes the locator matched (1 on success).
    pub match_count: u32,
    pub tag_name: String,
    pub source_of_truth: String,
    pub requested_len: u32,
    pub before_len: u32,
    pub after_len: u32,
    pub requested_sha256: String,
    pub before_sha256: String,
    pub after_sha256: String,
    /// Whether the value actually changed (before != after).
    pub changed: bool,
    /// Length of the SEPARATE chrome.tabs active-element readback.
    pub independent_readback_len: u32,
    pub independent_readback_sha256: String,
    pub status: String,
    pub elapsed_ms: u32,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserFillFormParams {
    /// Ordered field operations. Applied in order; by default Synapse stops on
    /// the first failed field so later fields are not corrupted by a bad spec.
    pub fields: Vec<BrowserFillFormField>,
    /// Continue after per-field failures and report every outcome. When false,
    /// Synapse stops on the first failed field.
    #[serde(default)]
    pub continue_on_error: bool,
    /// Chrome bridge tab target id (`chrome-tab:<id>`). Defaults to this
    /// session's active CDP target. Must be owned by this session.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND owning the target. Defaults to the session target's window.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Per-action DOM wait budget in milliseconds. Defaults to 5000.
    #[serde(default)]
    pub wait_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserFillFormField {
    /// Optional caller label echoed in the per-field outcome.
    #[serde(default)]
    pub name: Option<String>,
    /// Strict CSS selector for the target field/control.
    pub selector: String,
    /// Field operation kind.
    #[serde(rename = "type")]
    pub field_type: BrowserFillFormFieldType,
    /// Text replacement value for `text`; value/option alias for `select`.
    #[serde(default)]
    pub value: Option<String>,
    /// Text replacement value for `text`. Takes precedence over `value`.
    #[serde(default)]
    pub text: Option<String>,
    /// Desired checkbox/radio state. Checkbox supports true/false; radio
    /// supports true only because native radio uncheck is not meaningful.
    #[serde(default)]
    pub checked: Option<bool>,
    /// Select option value.
    #[serde(default)]
    pub option: Option<String>,
    /// Select option label.
    #[serde(default)]
    pub option_label: Option<String>,
    /// Select zero-based option index.
    #[serde(default)]
    pub option_index: Option<i32>,
    /// Multi-select option specs.
    #[serde(default)]
    pub options: Vec<BrowserFillFormSelectOption>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserFillFormFieldType {
    Text,
    Checkbox,
    Radio,
    Select,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserFillFormSelectOption {
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub index: Option<i32>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserFillFormResponse {
    pub ok: bool,
    pub required_foreground: bool,
    pub transport: String,
    pub window_hwnd: i64,
    pub cdp_target_id: String,
    pub source_of_truth: String,
    pub continue_on_error: bool,
    pub status: String,
    pub total_fields: u32,
    pub attempted_fields: u32,
    pub succeeded_fields: u32,
    pub failed_fields: u32,
    pub skipped_fields: u32,
    pub fields: Vec<BrowserFillFormFieldOutcome>,
    pub elapsed_ms: u32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserFillFormFieldOutcome {
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub selector: String,
    #[serde(rename = "type")]
    pub field_type: BrowserFillFormFieldType,
    pub ok: bool,
    pub status: String,
    pub delegated_tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

#[tool_router(router = browser_field_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Public form facade for the calling session's owned browser tab. operation=set_value delegates to the dual-readback field replacement path; operation=fill delegates to ordered multi-field form fill. The operation requires exactly its matching nested spec object and rejects extra operation specs before mutation. Target addressing (cdp_target_id/window_hwnd) may be supplied at the envelope top level as an alias for the selected nested spec's target; a conflicting nested value fails closed. Target-scoped and background-safe: never activates Chrome, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_form(
        &self,
        params: Parameters<BrowserFormParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserFormResponse>, ErrorData> {
        let mut params = params.0;
        let operation = params.operation;
        // #1551: fold top-level cdp_target_id/window_hwnd aliases into the nested
        // operation spec(s) before source/validation so the effective target is
        // resolved identically to the equivalent nested-spec form.
        let top_cdp_target_id = params.cdp_target_id.clone();
        let top_window_hwnd = params.window_hwnd;
        if let Some(spec) = params.set_value.as_mut() {
            merge_top_level_target(
                FORM_TOOL,
                "set_value",
                top_cdp_target_id.as_deref(),
                top_window_hwnd,
                &mut spec.cdp_target_id,
                &mut spec.window_hwnd,
            )?;
        }
        if let Some(spec) = params.fill.as_mut() {
            merge_top_level_target(
                FORM_TOOL,
                "fill",
                top_cdp_target_id.as_deref(),
                top_window_hwnd,
                &mut spec.cdp_target_id,
                &mut spec.window_hwnd,
            )?;
        }
        let source_id = browser_form_source_id(&params);
        validate_browser_form_params(&params)?;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = FORM_TOOL,
            operation = operation.as_str(),
            source_id = %source_id,
            "tool.invocation kind=browser_form"
        );
        match operation {
            BrowserFormOperation::SetValue => {
                let spec = params.set_value.ok_or_else(|| {
                    browser_form_facade_error(
                        operation,
                        source_id.clone(),
                        "browser_form operation=set_value reached dispatch without its validated set_value spec",
                        "send exactly one nested spec whose field name matches operation",
                    )
                })?;
                let response = self
                    .browser_set_value(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        browser_form_delegate_error(
                            operation,
                            source_id.clone(),
                            error,
                            "pass exactly one locator for an editable field in the owned target tab",
                        )
                    })?;
                Ok(Json(browser_form_response(
                    operation,
                    Some(response.0),
                    None,
                )))
            }
            BrowserFormOperation::Fill => {
                let spec = params.fill.ok_or_else(|| {
                    browser_form_facade_error(
                        operation,
                        source_id.clone(),
                        "browser_form operation=fill reached dispatch without its validated fill spec",
                        "send exactly one nested spec whose field name matches operation",
                    )
                })?;
                let response = self
                    .browser_fill_form(Parameters(spec), request_context)
                    .await
                    .map_err(|error| {
                        browser_form_delegate_error(
                            operation,
                            source_id.clone(),
                            error,
                            "pass at least one valid field spec for controls in the owned target tab",
                        )
                    })?;
                Ok(Json(browser_form_response(
                    operation,
                    None,
                    Some(response.0),
                )))
            }
        }
    }

    #[tool(
        description = "Background-safe REPLACE of a web form field's text in the user's normal authenticated Chrome via the safe extension bridge (#1000/#717). No debugger attach, no OS foreground, no UIA: runs entirely in-page through chrome.scripting, so it works on inactive/occluded tabs and never steals the operator's foreground. Target by strict CSS `selector` (exactly one editable+visible match; 0 or >1 fails loud), normal Chrome bridge `element_id` (`chrome-tab:<tabId>:frame:<frameId>:path:<domPath>`), or `active_element=true`. Replaces the value with the native prototype setter (React/Vue/Angular-safe) and verifies with TWO independent reads (in-page post-set + a separate chrome.tabs active-element readback); any divergence is ACTION_POSTCONDITION_FAILED, never an optimistic success. Defaults to this session's active CDP tab target (bind one with set_target/cdp_open_tab); the human foreground tab is never a fallback. Use this instead of foregrounding Chrome to type into a dashboard/form."
    )]
    pub async fn browser_set_value(
        &self,
        params: Parameters<BrowserSetValueParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserSetValueResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_set_value"
        );
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("{TOOL} requires an MCP session id (run the daemon in HTTP mode)"),
                )
            })?;

        let locator = validate_locator(&params)?;
        let (window_hwnd, cdp_target_id) = self.resolve_bridge_target(&session_id, &params)?;

        if synapse_a11y::endpoint_for_window(window_hwnd).is_some() {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{TOOL} targets the normal Chrome extension bridge, but window {window_hwnd} exposes a raw CDP debug endpoint; use the raw-CDP browser_* tools for a Synapse automation profile"
                ),
            ));
        }
        if !cdp_target_id.starts_with(CHROME_TAB_PREFIX) {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{TOOL} requires a normal Chrome bridge tab target ({CHROME_TAB_PREFIX}<id>); got {cdp_target_id:?}"
                ),
            ));
        }

        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "resolved_by": locator.label(),
            "requested_len": char_len(&params.text),
            "requested_sha256": text_signature(&params.text),
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_set_value_run(
                &session_id,
                window_hwnd,
                &cdp_target_id,
                &locator,
                &params.text,
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Fill multiple fields in the calling session's owned normal Chrome bridge tab in one ordered call (#1148). Each item targets a strict CSS selector and type=text|checkbox|radio|select. Text fields reuse browser_set_value's dual Source-of-Truth replacement readback; checkbox/radio/select reuse target_act-equivalent DOM actions over the debugger-free Chrome bridge. By default stops on first failed field so later fields are not changed by a bad spec; set continue_on_error=true to report every per-field success/failure. Never activates Chrome, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_fill_form(
        &self,
        params: Parameters<BrowserFillFormParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserFillFormResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "browser_fill_form",
            "tool.invocation kind=browser_fill_form"
        );
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_fill_form requires an MCP session id (run the daemon in HTTP mode)",
                )
            })?;
        validate_fill_form_params(&params)?;
        let (window_hwnd, cdp_target_id) = self.resolve_bridge_target(
            &session_id,
            &BrowserSetValueParams {
                text: String::new(),
                selector: Some("#__synapse_unused__".to_owned()),
                element_id: None,
                active_element: false,
                cdp_target_id: params.cdp_target_id.clone(),
                window_hwnd: params.window_hwnd,
            },
        )?;
        if synapse_a11y::endpoint_for_window(window_hwnd).is_some() {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_fill_form targets the normal Chrome extension bridge, but window {window_hwnd} exposes a raw CDP debug endpoint; use raw-CDP primitives for a Synapse automation profile"
                ),
            ));
        }
        if !cdp_target_id.starts_with(CHROME_TAB_PREFIX) {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_fill_form requires a normal Chrome bridge tab target ({CHROME_TAB_PREFIX}<id>); got {cdp_target_id:?}"
                ),
            ));
        }

        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "field_count": params.fields.len(),
            "continue_on_error": params.continue_on_error,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            "browser_fill_form",
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_fill_form_run(&session_id, window_hwnd, &cdp_target_id, &params)
            .await;
        self.audit_action_result_for_session("browser_fill_form", &result, &session_id)?;
        result.map(Json)
    }

    async fn browser_set_value_run(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        locator: &Locator,
        text: &str,
    ) -> Result<BrowserSetValueResponse, ErrorData> {
        let started = std::time::Instant::now();
        let (selector_arg, element_id_arg, active_arg) = match locator {
            Locator::Selector(selector) => (Some(selector.as_str()), None, false),
            Locator::ElementId(element_id) => (None, Some(element_id.as_str()), false),
            Locator::ActiveElement => (None, None, true),
        };
        let owner = self.cdp_target_owner_for_readback(TOOL, session_id, cdp_target_id)?;
        if let Some(owner) = owner.as_ref()
            && owner.window_hwnd != window_hwnd
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{TOOL} refused target {cdp_target_id:?}: owner window {:#x} does not match requested window {:#x}",
                    owner.window_hwnd, window_hwnd
                ),
            ));
        }
        let expected_chrome_window_id = owner.as_ref().and_then(|owner| owner.chrome_window_id);

        let result = crate::chrome_debugger_bridge::set_field_value(
            window_hwnd,
            cdp_target_id,
            expected_chrome_window_id,
            selector_arg,
            element_id_arg,
            active_arg,
            text,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "{TOOL} bridge setFieldValue failed for target {cdp_target_id:?}: {}",
                    error.detail()
                ),
            )
        })?;

        let before_value = result.before_value.clone().unwrap_or_default();
        let after_value = result.after_value.clone().ok_or_else(|| {
            mcp_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!("{TOOL} bridge returned no after_value for target {cdp_target_id:?}"),
            )
        })?;
        if let Some(expected_window_id) = expected_chrome_window_id
            && result.chrome_window_id != Some(expected_window_id)
        {
            return Err(mcp_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!(
                    "{TOOL} bridge returned Chrome window {:?} for target {cdp_target_id:?}, expected Chrome window {expected_window_id}",
                    result.chrome_window_id
                ),
            ));
        }

        // SoT #1: the in-page post-set value must equal the requested text.
        if !value_matches(&after_value, text) {
            return Err(postcondition_error(
                "in_page_post_set",
                cdp_target_id,
                text,
                &after_value,
            ));
        }

        // SoT #2: a SEPARATE chrome.tabs active-element readback (the field is
        // focused by setFieldValue) must independently equal the requested text.
        let expected_context = synapse_a11y::foreground_context(window_hwnd).ok();
        let info = crate::chrome_debugger_bridge::target_info(
            window_hwnd,
            cdp_target_id,
            expected_chrome_window_id,
            expected_context.as_ref().map(|context| context.window_bounds),
            expected_context
                .as_ref()
                .map(|context| context.window_title.as_str()),
        )
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "{TOOL} separate active-element readback failed for target {cdp_target_id:?}: {}",
                        error.detail()
                    ),
                )
            })?;
        if let Some(expected_window_id) = expected_chrome_window_id
            && info.chrome_window_id != Some(expected_window_id)
        {
            return Err(mcp_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!(
                    "{TOOL} separate active-element readback returned Chrome window {:?} for target {cdp_target_id:?}, expected Chrome window {expected_window_id}",
                    info.chrome_window_id
                ),
            ));
        }
        let independent = info
            .active_element
            .as_ref()
            .filter(|active| active.has_active_element == Some(true))
            .and_then(|active| active.value.clone())
            .unwrap_or_default();
        if !value_matches(&independent, text) {
            return Err(postcondition_error(
                "separate_active_element_readback",
                cdp_target_id,
                text,
                &independent,
            ));
        }

        let requested_len = char_len(text);
        let before_len = char_len(&before_value);
        let after_len = char_len(&after_value);
        let changed = before_value != after_value;
        let trusted_text_backend = result.readback_backend.contains("chrome.debugger.Input");
        let source_of_truth = if trusted_text_backend {
            "chrome.debugger.Input text dispatch + chrome.scripting editable readback + separate chrome.tabs active-element readback"
        } else {
            SOURCE_OF_TRUTH
        };

        tracing::info!(
            code = "BROWSER_SET_VALUE_READBACK",
            session_id = %session_id,
            hwnd = window_hwnd,
            cdp_target_id = %cdp_target_id,
            resolved_by = %result.resolved_by,
            match_count = result.match_count,
            tag_name = %result.tag_name,
            readback_backend = %result.readback_backend,
            trusted_text_backend,
            before_len,
            after_len,
            requested_len,
            changed,
            "readback=browser_set_value dual_verified=true"
        );

        Ok(BrowserSetValueResponse {
            ok: true,
            required_foreground: false,
            transport: "chrome_tabs_extension".to_owned(),
            window_hwnd,
            cdp_target_id: cdp_target_id.to_owned(),
            resolved_by: result.resolved_by,
            match_count: result.match_count,
            tag_name: result.tag_name,
            source_of_truth: source_of_truth.to_owned(),
            requested_len,
            before_len,
            after_len,
            requested_sha256: text_signature(text),
            before_sha256: text_signature(&before_value),
            after_sha256: text_signature(&after_value),
            changed,
            independent_readback_len: char_len(&independent),
            independent_readback_sha256: text_signature(&independent),
            status: "verified_state".to_owned(),
            elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        })
    }

    async fn browser_fill_form_run(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &BrowserFillFormParams,
    ) -> Result<BrowserFillFormResponse, ErrorData> {
        let started = std::time::Instant::now();
        let mut outcomes = Vec::with_capacity(params.fields.len());
        for (index, field) in params.fields.iter().enumerate() {
            let outcome = self
                .browser_fill_form_apply_field(
                    session_id,
                    window_hwnd,
                    cdp_target_id,
                    index,
                    field,
                    params.wait_timeout_ms.unwrap_or(5000),
                )
                .await;
            let ok = outcome.ok;
            outcomes.push(outcome);
            if !ok && !params.continue_on_error {
                break;
            }
        }

        let total_fields = u32::try_from(params.fields.len()).unwrap_or(u32::MAX);
        let attempted_fields = u32::try_from(outcomes.len()).unwrap_or(u32::MAX);
        let succeeded_fields =
            u32::try_from(outcomes.iter().filter(|outcome| outcome.ok).count()).unwrap_or(u32::MAX);
        let failed_fields = attempted_fields.saturating_sub(succeeded_fields);
        let skipped_fields = total_fields.saturating_sub(attempted_fields);
        let ok = failed_fields == 0 && skipped_fields == 0;
        let status = if ok {
            "verified_state"
        } else if params.continue_on_error {
            "completed_with_field_errors"
        } else {
            "stopped_on_field_error"
        };

        tracing::info!(
            code = "BROWSER_FILL_FORM_READBACK",
            session_id = %session_id,
            hwnd = window_hwnd,
            cdp_target_id = %cdp_target_id,
            total_fields,
            attempted_fields,
            succeeded_fields,
            failed_fields,
            skipped_fields,
            status,
            "readback=browser_fill_form ordered per-field Source-of-Truth"
        );

        Ok(BrowserFillFormResponse {
            ok,
            required_foreground: false,
            transport: "chrome_tabs_extension".to_owned(),
            window_hwnd,
            cdp_target_id: cdp_target_id.to_owned(),
            source_of_truth:
                "per-field browser_set_value/domAction readback plus caller page readback"
                    .to_owned(),
            continue_on_error: params.continue_on_error,
            status: status.to_owned(),
            total_fields,
            attempted_fields,
            succeeded_fields,
            failed_fields,
            skipped_fields,
            fields: outcomes,
            elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        })
    }

    async fn browser_fill_form_apply_field(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        index: usize,
        field: &BrowserFillFormField,
        wait_timeout_ms: u64,
    ) -> BrowserFillFormFieldOutcome {
        let index_u32 = u32::try_from(index).unwrap_or(u32::MAX);
        let selector = field.selector.trim();
        if selector.is_empty() {
            return invalid_fill_form_field(index_u32, field, "selector must be non-empty");
        }

        match field.field_type {
            BrowserFillFormFieldType::Text => {
                let Some(text) = field.text.as_deref().or(field.value.as_deref()) else {
                    return invalid_fill_form_field(
                        index_u32,
                        field,
                        "text fields require text or value",
                    );
                };
                match self
                    .browser_set_value_run(
                        session_id,
                        window_hwnd,
                        cdp_target_id,
                        &Locator::Selector(selector.to_owned()),
                        text,
                    )
                    .await
                {
                    Ok(result) => {
                        fill_form_success(index_u32, field, "browser_set_value", json!(result))
                    }
                    Err(error) => {
                        fill_form_error_data(index_u32, field, "browser_set_value", error)
                    }
                }
            }
            BrowserFillFormFieldType::Checkbox => {
                let Some(checked) = field.checked else {
                    return invalid_fill_form_field(
                        index_u32,
                        field,
                        "checkbox fields require checked=true|false",
                    );
                };
                let action = if checked { "check" } else { "uncheck" };
                self.browser_fill_form_dom_action(
                    index_u32,
                    field,
                    window_hwnd,
                    cdp_target_id,
                    action,
                    None,
                    wait_timeout_ms,
                )
                .await
            }
            BrowserFillFormFieldType::Radio => {
                if matches!(field.checked, Some(false)) {
                    return invalid_fill_form_field(
                        index_u32,
                        field,
                        "radio fields support checked=true only; select another radio to change the group",
                    );
                }
                self.browser_fill_form_dom_action(
                    index_u32,
                    field,
                    window_hwnd,
                    cdp_target_id,
                    "check",
                    None,
                    wait_timeout_ms,
                )
                .await
            }
            BrowserFillFormFieldType::Select => {
                let options_value = if field.options.is_empty() {
                    None
                } else {
                    match serde_json::to_value(&field.options) {
                        Ok(value) => Some(value),
                        Err(error) => {
                            return invalid_fill_form_field(
                                index_u32,
                                field,
                                &format!("select options encode failed: {error}"),
                            );
                        }
                    }
                };
                if field.option.is_none()
                    && field.option_label.is_none()
                    && field.option_index.is_none()
                    && field.options.is_empty()
                    && field.value.is_none()
                {
                    return invalid_fill_form_field(
                        index_u32,
                        field,
                        "select fields require option, value, option_label, option_index, or options[]",
                    );
                }
                self.browser_fill_form_dom_action(
                    index_u32,
                    field,
                    window_hwnd,
                    cdp_target_id,
                    "select",
                    options_value.as_ref(),
                    wait_timeout_ms,
                )
                .await
            }
        }
    }

    async fn browser_fill_form_dom_action(
        &self,
        index: u32,
        field: &BrowserFillFormField,
        window_hwnd: i64,
        cdp_target_id: &str,
        action: &str,
        options: Option<&Value>,
        wait_timeout_ms: u64,
    ) -> BrowserFillFormFieldOutcome {
        let option = field.option.as_deref().or(field.value.as_deref());
        match crate::chrome_debugger_bridge::dom_action(
            crate::chrome_debugger_bridge::ChromeDebuggerDomActionRequest {
                hwnd: window_hwnd,
                target_id: cdp_target_id,
                action,
                selector: Some(field.selector.trim()),
                element_id: None,
                role: None,
                name: None,
                value: None,
                option,
                option_label: field.option_label.as_deref(),
                option_index: field.option_index,
                options,
                event_type: None,
                event_init: None,
                clicks: None,
                button: None,
                modifiers: None,
                position_x: None,
                position_y: None,
                wait_timeout_ms,
                auto_wait: false,
                auto_wait_timeout_ms: 0,
                suppress_page_text: false,
            },
        )
        .await
        {
            Ok(result) => {
                fill_form_success(index, field, "chrome_debugger_bridge.domAction", result)
            }
            Err(error) => {
                fill_form_bridge_error(index, field, "chrome_debugger_bridge.domAction", error)
            }
        }
    }

    fn resolve_bridge_target(
        &self,
        session_id: &str,
        params: &BrowserSetValueParams,
    ) -> Result<(i64, String), ErrorData> {
        let active_target = self.session_target(Some(session_id))?;
        let cdp_target_id = match (params.cdp_target_id.as_ref(), active_target.as_ref()) {
            (Some(target_id), _) => target_id.clone(),
            (None, Some(SessionTarget::Cdp { cdp_target_id, .. })) => cdp_target_id.clone(),
            (None, _) => {
                return Err(mcp_error(
                    error_codes::TARGET_NOT_SET,
                    format!(
                        "{TOOL} requires an active CDP session target or an explicit cdp_target_id owned by this session; refusing the human foreground tab"
                    ),
                ));
            }
        };
        let window_hwnd = params
            .window_hwnd
            .or_else(|| match active_target.as_ref() {
                Some(SessionTarget::Cdp { window_hwnd, .. }) => Some(*window_hwnd),
                Some(SessionTarget::Window { hwnd }) => Some(*hwnd),
                None => None,
            })
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TARGET_NOT_SET,
                    format!("{TOOL} requires window_hwnd when no active session target is set"),
                )
            })?;
        // Ownership: an explicit target must match this session's active CDP
        // target. This keeps the tool from acting on another session's tab.
        if let Some(explicit) = params.cdp_target_id.as_ref() {
            let owned = matches!(
                active_target.as_ref(),
                Some(SessionTarget::Cdp { cdp_target_id, .. })
                    if cdp_target_id.eq_ignore_ascii_case(explicit)
            );
            if !owned {
                return Err(mcp_error(
                    error_codes::ACTION_TARGET_INVALID,
                    format!(
                        "{TOOL} refused target {explicit:?}: it is not this session's active CDP target"
                    ),
                ));
            }
        }
        Ok((window_hwnd, cdp_target_id))
    }
}

fn validate_browser_form_params(params: &BrowserFormParams) -> Result<(), ErrorData> {
    let fields = [
        ("set_value", params.set_value.is_some()),
        ("fill", params.fill.is_some()),
    ];
    let supplied = fields
        .iter()
        .filter_map(|(field, present)| present.then_some(*field))
        .collect::<Vec<_>>();
    let expected = params.operation.as_str();
    if supplied.len() != 1 || supplied[0] != expected {
        return Err(browser_form_facade_error(
            params.operation,
            browser_form_source_id(params),
            format!(
                "{FORM_TOOL} operation={} requires exactly `{expected}` spec and no other operation specs; supplied={supplied:?}",
                params.operation.as_str()
            ),
            "send exactly one nested spec whose field name matches operation",
        ));
    }
    Ok(())
}

fn browser_form_response(
    operation: BrowserFormOperation,
    set_value: Option<BrowserSetValueResponse>,
    fill: Option<BrowserFillFormResponse>,
) -> BrowserFormResponse {
    BrowserFormResponse {
        operation,
        source_of_truth: FORM_SOURCE_OF_TRUTH.to_owned(),
        readback_source_of_truth: FORM_READBACK_SOURCE_OF_TRUTH.to_owned(),
        set_value,
        fill,
    }
}

fn browser_form_source_id(params: &BrowserFormParams) -> String {
    match params.operation {
        BrowserFormOperation::SetValue => params
            .set_value
            .as_ref()
            .map(|spec| {
                format!(
                    "{};locator={}",
                    browser_form_target_source(spec.window_hwnd, spec.cdp_target_id.as_deref()),
                    browser_set_value_locator_source(spec)
                )
            })
            .unwrap_or_else(|| "missing_set_value_spec".to_owned()),
        BrowserFormOperation::Fill => params
            .fill
            .as_ref()
            .map(|spec| {
                format!(
                    "{};field_count={}",
                    browser_form_target_source(spec.window_hwnd, spec.cdp_target_id.as_deref()),
                    spec.fields.len()
                )
            })
            .unwrap_or_else(|| "missing_fill_spec".to_owned()),
    }
}

fn browser_form_target_source(window_hwnd: Option<i64>, cdp_target_id: Option<&str>) -> String {
    match (window_hwnd, cdp_target_id) {
        (Some(hwnd), Some(target)) => format!("window_hwnd={hwnd:#x};cdp_target_id={target}"),
        (Some(hwnd), None) => format!("window_hwnd={hwnd:#x}"),
        (None, Some(target)) => format!("cdp_target_id={target}"),
        (None, None) => "active_session_target".to_owned(),
    }
}

fn browser_set_value_locator_source(params: &BrowserSetValueParams) -> &'static str {
    match (
        params.selector.as_ref(),
        params.element_id.as_ref(),
        params.active_element,
    ) {
        (Some(_), None, false) => "selector",
        (None, Some(_), false) => "element_id",
        (None, None, true) => "active_element",
        _ => "invalid_locator_set",
    }
}

fn browser_form_facade_error(
    operation: BrowserFormOperation,
    source_id: impl Into<String>,
    message: impl Into<String>,
    remediation: &'static str,
) -> ErrorData {
    let message = message.into();
    ErrorData::new(
        ErrorCode(-32099),
        message,
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "operation": operation.as_str(),
            "source_of_truth": FORM_SOURCE_OF_TRUTH,
            "source_id": source_id.into(),
            "readback_source_of_truth": FORM_READBACK_SOURCE_OF_TRUTH,
            "remediation": remediation,
        })),
    )
}

fn browser_form_delegate_error(
    operation: BrowserFormOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    let cause_code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned();
    let cause = error.data.clone().unwrap_or(Value::Null);
    ErrorData::new(
        error.code,
        error.message.to_string(),
        Some(json!({
            "code": cause_code,
            "operation": operation.as_str(),
            "source_of_truth": FORM_SOURCE_OF_TRUTH,
            "source_id": source_id.into(),
            "readback_source_of_truth": FORM_READBACK_SOURCE_OF_TRUTH,
            "remediation": remediation,
            "cause": cause,
        })),
    )
}

fn validate_fill_form_params(params: &BrowserFillFormParams) -> Result<(), ErrorData> {
    if params.fields.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_fill_form requires at least one field",
        ));
    }
    if params.fields.len() > 200 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_fill_form supports at most 200 fields per call, got {}",
                params.fields.len()
            ),
        ));
    }
    if let Some(timeout) = params.wait_timeout_ms
        && !(50..=30_000).contains(&timeout)
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("browser_fill_form wait_timeout_ms must be in 50..=30000, got {timeout}"),
        ));
    }
    Ok(())
}

fn fill_form_success(
    index: u32,
    field: &BrowserFillFormField,
    delegated_tool: &str,
    result: Value,
) -> BrowserFillFormFieldOutcome {
    BrowserFillFormFieldOutcome {
        index,
        name: field.name.clone(),
        selector: field.selector.clone(),
        field_type: field.field_type,
        ok: true,
        status: "ok".to_owned(),
        delegated_tool: delegated_tool.to_owned(),
        error_code: None,
        error_message: None,
        result: Some(result),
    }
}

fn invalid_fill_form_field(
    index: u32,
    field: &BrowserFillFormField,
    message: &str,
) -> BrowserFillFormFieldOutcome {
    BrowserFillFormFieldOutcome {
        index,
        name: field.name.clone(),
        selector: field.selector.clone(),
        field_type: field.field_type,
        ok: false,
        status: "invalid_params".to_owned(),
        delegated_tool: "browser_fill_form".to_owned(),
        error_code: Some(error_codes::TOOL_PARAMS_INVALID.to_owned()),
        error_message: Some(message.to_owned()),
        result: None,
    }
}

fn fill_form_error_data(
    index: u32,
    field: &BrowserFillFormField,
    delegated_tool: &str,
    error: ErrorData,
) -> BrowserFillFormFieldOutcome {
    let error_code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("TOOL_ERROR")
        .to_owned();
    BrowserFillFormFieldOutcome {
        index,
        name: field.name.clone(),
        selector: field.selector.clone(),
        field_type: field.field_type,
        ok: false,
        status: "error".to_owned(),
        delegated_tool: delegated_tool.to_owned(),
        error_code: Some(error_code),
        error_message: Some(error.message.to_string()),
        result: None,
    }
}

fn fill_form_bridge_error(
    index: u32,
    field: &BrowserFillFormField,
    delegated_tool: &str,
    error: crate::chrome_debugger_bridge::ChromeDebuggerBridgeError,
) -> BrowserFillFormFieldOutcome {
    BrowserFillFormFieldOutcome {
        index,
        name: field.name.clone(),
        selector: field.selector.clone(),
        field_type: field.field_type,
        ok: false,
        status: "error".to_owned(),
        delegated_tool: delegated_tool.to_owned(),
        error_code: Some(error.code().to_owned()),
        error_message: Some(error.detail().to_owned()),
        result: None,
    }
}

#[derive(Debug)]
enum Locator {
    Selector(String),
    ElementId(String),
    ActiveElement,
}

impl Locator {
    fn label(&self) -> &'static str {
        match self {
            Self::Selector(_) => "selector",
            Self::ElementId(_) => "element_id",
            Self::ActiveElement => "active_element",
        }
    }
}

fn validate_locator(params: &BrowserSetValueParams) -> Result<Locator, ErrorData> {
    let selector = params
        .selector
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let element_id = params
        .element_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let locator_count = usize::from(selector.is_some())
        + usize::from(element_id.is_some())
        + usize::from(params.active_element);
    if locator_count != 1 {
        let message = if locator_count == 0 {
            format!("{TOOL} requires a `selector`, `element_id`, or `active_element=true`")
        } else {
            format!(
                "{TOOL} requires exactly one of `selector`, `element_id`, or `active_element`, not multiple locators"
            )
        };
        return Err(mcp_error(error_codes::TOOL_PARAMS_INVALID, message));
    }
    if let Some(selector) = selector {
        return Ok(Locator::Selector(selector.to_owned()));
    }
    if let Some(element_id) = element_id {
        if !element_id.starts_with(CHROME_TAB_PREFIX) {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "{TOOL} element_id must be a normal Chrome bridge id starting with {CHROME_TAB_PREFIX:?}; got {element_id:?}"
                ),
            ));
        }
        return Ok(Locator::ElementId(element_id.to_owned()));
    }
    Ok(Locator::ActiveElement)
}

/// Replace verification: newline-normalized exact equality (editable hosts emit
/// `\r\n`/trailing-newline variance that does not change field content).
fn value_matches(observed: &str, requested: &str) -> bool {
    normalize(observed) == normalize(requested)
}

fn normalize(value: &str) -> String {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .strip_suffix('\n')
        .map_or_else(|| normalized.clone(), str::to_owned)
}

fn char_len(value: &str) -> u32 {
    u32::try_from(value.chars().count()).unwrap_or(u32::MAX)
}

fn postcondition_error(
    lens: &str,
    cdp_target_id: &str,
    requested: &str,
    observed: &str,
) -> ErrorData {
    tracing::error!(
        code = error_codes::ACTION_POSTCONDITION_FAILED,
        tool = TOOL,
        lens,
        cdp_target_id,
        requested_len = char_len(requested),
        observed_len = char_len(observed),
        requested_sha256 = %text_signature(requested),
        observed_sha256 = %text_signature(observed),
        "browser_set_value separate readback did not equal the requested replacement text"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{TOOL} Source-of-Truth postcondition failed ({lens}): readback does not equal the requested replacement text"
        ),
        Some(json!({
            "code": error_codes::ACTION_POSTCONDITION_FAILED,
            "tool": TOOL,
            "lens": lens,
            "cdp_target_id": cdp_target_id,
            "source_of_truth": SOURCE_OF_TRUTH,
            "verify": {
                "requested_len": char_len(requested),
                "observed_len": char_len(observed),
                "requested_sha256": text_signature(requested),
                "observed_sha256": text_signature(observed),
            },
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // #1551: top-level cdp_target_id/window_hwnd on the browser_form envelope are
    // accepted and alias the nested spec's target, resolving the SAME target as
    // the equivalent nested-spec form.
    #[test]
    fn browser_form_top_level_target_aliases_nested_spec_1551() {
        // Top-level cdp_target_id, no nested target.
        let mut top_level: BrowserFormParams = serde_json::from_value(json!({
            "operation": "set_value",
            "cdp_target_id": "TARGET-1551-ABC",
            "set_value": { "text": "x", "selector": "#q" },
        }))
        .expect("top-level cdp_target_id must deserialize under deny_unknown_fields");
        // Equivalent nested-only form.
        let nested: BrowserFormParams = serde_json::from_value(json!({
            "operation": "set_value",
            "set_value": { "text": "x", "selector": "#q", "cdp_target_id": "TARGET-1551-ABC" },
        }))
        .expect("nested cdp_target_id must deserialize");
        let top_cdp = top_level.cdp_target_id.clone();
        let top_hwnd = top_level.window_hwnd;
        let spec = top_level.set_value.as_mut().expect("set_value spec present");
        println!("readback=before cdp_target_id={:?}", spec.cdp_target_id);
        merge_top_level_target(
            FORM_TOOL,
            "set_value",
            top_cdp.as_deref(),
            top_hwnd,
            &mut spec.cdp_target_id,
            &mut spec.window_hwnd,
        )
        .expect("merge must succeed");
        println!("readback=after cdp_target_id={:?}", spec.cdp_target_id);
        assert_eq!(spec.cdp_target_id.as_deref(), Some("TARGET-1551-ABC"));
        assert_eq!(
            spec.cdp_target_id,
            nested.set_value.expect("nested spec").cdp_target_id
        );

        // Top-level window_hwnd (0x1234) aliases the nested spec's window_hwnd.
        let mut top_hwnd_params: BrowserFormParams = serde_json::from_value(json!({
            "operation": "set_value",
            "window_hwnd": 0x1234,
            "set_value": { "text": "x", "selector": "#q" },
        }))
        .expect("top-level window_hwnd must deserialize");
        let t_cdp = top_hwnd_params.cdp_target_id.clone();
        let t_hwnd = top_hwnd_params.window_hwnd;
        let spec = top_hwnd_params
            .set_value
            .as_mut()
            .expect("set_value spec present");
        println!("readback=before window_hwnd={:?}", spec.window_hwnd);
        merge_top_level_target(
            FORM_TOOL,
            "set_value",
            t_cdp.as_deref(),
            t_hwnd,
            &mut spec.cdp_target_id,
            &mut spec.window_hwnd,
        )
        .expect("merge must succeed");
        println!("readback=after window_hwnd={:?}", spec.window_hwnd);
        assert_eq!(spec.window_hwnd, Some(0x1234));
    }

    #[test]
    fn browser_form_conflicting_top_level_target_fails_closed_1551() {
        let mut params: BrowserFormParams = serde_json::from_value(json!({
            "operation": "set_value",
            "cdp_target_id": "TARGET-1551-ABC",
            "set_value": { "text": "x", "selector": "#q", "cdp_target_id": "OTHER-TARGET" },
        }))
        .expect("both target locations must deserialize");
        let top_cdp = params.cdp_target_id.clone();
        let top_hwnd = params.window_hwnd;
        let spec = params.set_value.as_mut().expect("set_value spec present");
        let err = merge_top_level_target(
            FORM_TOOL,
            "set_value",
            top_cdp.as_deref(),
            top_hwnd,
            &mut spec.cdp_target_id,
            &mut spec.window_hwnd,
        )
        .expect_err("conflicting top-level and nested cdp_target_id must fail closed");
        let code = err
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str);
        println!("readback=conflict code={code:?} message={}", err.message);
        assert_eq!(code, Some(error_codes::TOOL_PARAMS_INVALID));
    }

    #[test]
    fn browser_form_still_rejects_unknown_fields_1551() {
        let err = serde_json::from_value::<BrowserFormParams>(json!({
            "operation": "set_value",
            "set_value": { "text": "x", "selector": "#q" },
            "bogus_1551": true,
        }))
        .expect_err("deny_unknown_fields must still reject a genuinely unknown field");
        println!("readback=unknown_rejected err={err}");
    }

    fn params(
        selector: Option<&str>,
        element_id: Option<&str>,
        active: bool,
    ) -> BrowserSetValueParams {
        BrowserSetValueParams {
            text: "x".to_owned(),
            selector: selector.map(str::to_owned),
            element_id: element_id.map(str::to_owned),
            active_element: active,
            cdp_target_id: None,
            window_hwnd: None,
        }
    }

    #[test]
    fn locator_requires_exactly_one() {
        assert!(matches!(
            validate_locator(&params(Some("#q"), None, false)),
            Ok(Locator::Selector(_))
        ));
        assert!(matches!(
            validate_locator(&params(None, Some("chrome-tab:1:frame:0:path:0.1"), false)),
            Ok(Locator::ElementId(_))
        ));
        assert!(matches!(
            validate_locator(&params(None, None, true)),
            Ok(Locator::ActiveElement)
        ));
        // both
        let err = validate_locator(&params(Some("#q"), None, true)).expect_err("both must fail");
        assert!(err.message.contains("exactly one"));
        let err = validate_locator(&params(
            Some("#q"),
            Some("chrome-tab:1:frame:0:path:0.1"),
            false,
        ))
        .expect_err("selector plus element_id must fail");
        assert!(err.message.contains("exactly one"));
        let err = validate_locator(&params(None, Some("plain-dom-id"), false))
            .expect_err("plain DOM id must not be accepted as bridge element id");
        assert!(err.message.contains("normal Chrome bridge id"));
        // neither
        let err = validate_locator(&params(None, None, false)).expect_err("neither must fail");
        assert!(err.message.contains("selector"));
        // empty selector
        let err = validate_locator(&params(Some("  "), None, false)).expect_err("empty must fail");
        assert!(err.message.contains("selector"));
    }

    #[test]
    fn normalize_strips_trailing_newline_and_crlf() {
        assert_eq!(normalize("a\r\nb"), "a\nb");
        assert_eq!(normalize("a\n"), "a");
        assert!(value_matches("composer\n", "composer"));
        assert!(value_matches("", ""));
        assert!(!value_matches("leftover", ""));
    }

    fn fill_params(fields: Vec<BrowserFillFormField>) -> BrowserFillFormParams {
        BrowserFillFormParams {
            fields,
            continue_on_error: false,
            cdp_target_id: None,
            window_hwnd: None,
            wait_timeout_ms: None,
        }
    }

    fn text_field(selector: &str, text: &str) -> BrowserFillFormField {
        BrowserFillFormField {
            name: None,
            selector: selector.to_owned(),
            field_type: BrowserFillFormFieldType::Text,
            value: None,
            text: Some(text.to_owned()),
            checked: None,
            option: None,
            option_label: None,
            option_index: None,
            options: Vec::new(),
        }
    }

    #[test]
    fn fill_form_validates_field_count_and_timeout() {
        let err = validate_fill_form_params(&fill_params(Vec::new()))
            .expect_err("empty form fill must fail");
        assert!(err.message.contains("at least one field"));

        let mut params = fill_params(vec![text_field("#name", "Ada")]);
        params.wait_timeout_ms = Some(49);
        let err = validate_fill_form_params(&params).expect_err("low timeout must fail");
        assert!(err.message.contains("wait_timeout_ms"));

        params.wait_timeout_ms = Some(50);
        validate_fill_form_params(&params).expect("boundary timeout should pass");
    }

    #[test]
    fn fill_form_invalid_field_outcome_is_per_field() {
        let field = text_field("   ", "Ada");
        let outcome = invalid_fill_form_field(2, &field, "selector must be non-empty");
        assert!(!outcome.ok);
        assert_eq!(outcome.index, 2);
        assert_eq!(
            outcome.error_code.as_deref(),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
        assert!(outcome.error_message.unwrap().contains("selector"));
    }
}
