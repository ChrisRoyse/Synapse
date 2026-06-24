//! Browser assertions and ARIA snapshot tools (#1193/#1194/#1195).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    time::{Duration, Instant},
};

use super::{
    ErrorData, Json, Parameters, SynapseService,
    m1_tools::{
        browser_raw_cdp_required_error, cdp_target_id_audit_ref, require_target_session_id,
        validate_cdp_target_id,
    },
    tool, tool_router,
};
use crate::{
    m1::mcp_error,
    m1::{BrowserLayoutRelation, BrowserLocateEngine},
};
use regex::Regex;
use rmcp::{RoleServer, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::{AccessibleNode, error_codes};

const ARIA_TOOL: &str = "browser_aria_snapshot";
const ASSERT_TOOL: &str = "browser_assert";

const DEFAULT_ARIA_MAX_NODES: usize = 500;
const MAX_ARIA_MAX_NODES: usize = 5_000;
const DEFAULT_ARIA_MAX_DEPTH: u32 = 32;
const MAX_ARIA_MAX_DEPTH: u32 = 128;

const DEFAULT_ASSERT_TIMEOUT_MS: u64 = 5_000;
const MAX_ASSERT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_ASSERT_INTERVAL_MS: u64 = 100;
const MIN_ASSERT_INTERVAL_MS: u64 = 10;
const MAX_ASSERT_INTERVAL_MS: u64 = 5_000;
const ASSERT_LOCATOR_MAX_QUERY_BYTES: usize = 16 * 1024;
const ASSERT_MAX_TEXT_CHARS: usize = 64 * 1024;
const ASSERT_MAX_ATTRIBUTE_NAME_CHARS: usize = 512;

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserAriaSnapshotParams {
    /// CDP TargetID to snapshot. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never a fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    /// Optional CDP element id to scope the ARIA snapshot to a subtree.
    #[serde(default)]
    pub root_element_id: Option<String>,
    /// Maximum AX-backed nodes to return. Defaults to 500, max 5000.
    #[serde(default)]
    pub max_nodes: Option<usize>,
    /// Maximum relative tree depth to render. Defaults to 32, max 128.
    #[serde(default)]
    pub max_depth: Option<u32>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserAriaSnapshotNode {
    pub element_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_element_id: Option<String>,
    pub depth: u32,
    pub role: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub enabled: bool,
    pub focused: bool,
    pub children_count: u32,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserAriaSnapshotResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_element_id: Option<String>,
    pub snapshot: String,
    pub nodes: Vec<BrowserAriaSnapshotNode>,
    pub node_count: usize,
    pub total_ax_nodes: u32,
    pub max_nodes: usize,
    pub max_depth: u32,
    pub truncated_by_max_nodes: bool,
    pub truncated_by_depth: bool,
    pub frame_tree_frame_count: u32,
    pub attached_frame_target_count: u32,
    pub blocked_frame_targets: Vec<String>,
    pub frame_snapshot_errors: Vec<String>,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

#[derive(Clone, Debug)]
struct NormalizedAriaSnapshotParams {
    root_element_id: Option<String>,
    root_target_id: Option<String>,
    max_nodes: usize,
    max_depth: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserAssertLocator {
    /// The primary query: CSS/XPath text, visible text, ARIA role token, label /
    /// placeholder / alt / title text, test-id value, or layout base selector.
    pub query: String,
    /// Which selector engine interprets `query` (default `css`).
    #[serde(default)]
    pub engine: BrowserLocateEngine,
    /// Exact match for text-like engines.
    #[serde(default)]
    pub exact: Option<bool>,
    /// Interpret `query` as a JavaScript regular expression body.
    #[serde(default)]
    pub regex: Option<bool>,
    /// `role` only: accessible-name filter.
    #[serde(default)]
    pub name: Option<String>,
    /// `role` only: exact accessible-name match.
    #[serde(default)]
    pub name_exact: Option<bool>,
    /// `role` only: interpret `name` as a regular expression.
    #[serde(default)]
    pub name_regex: Option<bool>,
    /// `testid` only: attribute name to read (default `data-testid`).
    #[serde(default)]
    pub testid_attribute: Option<String>,
    /// `role` only: require `aria-checked` to equal this.
    #[serde(default)]
    pub checked: Option<bool>,
    /// `role` only: require `aria-pressed` to equal this.
    #[serde(default)]
    pub pressed: Option<bool>,
    /// `role` only: require `aria-expanded` to equal this.
    #[serde(default)]
    pub expanded: Option<bool>,
    /// `role` only: require `aria-selected` to equal this.
    #[serde(default)]
    pub selected: Option<bool>,
    /// `role` only: require disabled state to equal this.
    #[serde(default)]
    pub disabled: Option<bool>,
    /// `role` only: require `aria-level` to equal this.
    #[serde(default)]
    pub level: Option<i64>,
    /// `role` only: include nodes ignored for accessibility.
    #[serde(default)]
    pub include_hidden: Option<bool>,
    /// `layout` only: relation to the anchor.
    #[serde(default)]
    pub relation: Option<BrowserLayoutRelation>,
    /// `layout` only: anchor CSS selector.
    #[serde(default)]
    pub anchor: Option<String>,
    /// `layout` only: maximum CSS-pixel distance.
    #[serde(default)]
    pub max_distance: Option<f64>,
    /// Keep only matches whose normalized text contains this.
    #[serde(default)]
    pub has_text: Option<String>,
    /// Positional pick: 0-based; negative counts from the end.
    #[serde(default)]
    pub nth: Option<i64>,
    /// Require exactly one matched element for element-scoped matchers. Defaults
    /// true for all matchers except `to_have_count`.
    #[serde(default)]
    pub strict: Option<bool>,
    /// Resolve only within this element id.
    #[serde(default)]
    pub root_element_id: Option<String>,
    /// Maximum node ids to fetch per poll. Defaults to the matcher-specific cap.
    #[serde(default)]
    pub limit: Option<usize>,
}

// The `To`-prefix is intentional domain naming: these mirror Playwright's
// assertion comparison operators (to_be_visible, to_have_text, ...).
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserAssertMatcher {
    ToBeVisible,
    ToHaveText,
    ToHaveValue,
    ToBeChecked,
    ToBeEnabled,
    ToHaveAttribute,
    ToHaveCount,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserAssertTextMatch {
    #[default]
    Exact,
    Contains,
    Regex,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserAssertParams {
    /// CDP TargetID to assert against. Defaults to the active session CDP target.
    /// Must be owned by this session; the human foreground tab is never a fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
    pub locator: BrowserAssertLocator,
    pub matcher: BrowserAssertMatcher,
    /// Expected text for `to_have_text`.
    #[serde(default)]
    pub expected_text: Option<String>,
    /// Expected field value for `to_have_value`.
    #[serde(default)]
    pub expected_value: Option<String>,
    /// Expected boolean for `to_be_visible`, `to_be_checked`, or `to_be_enabled`.
    /// Defaults to true.
    #[serde(default)]
    pub expected_bool: Option<bool>,
    /// Expected selector count for `to_have_count`.
    #[serde(default)]
    pub expected_count: Option<usize>,
    /// Attribute name for `to_have_attribute`.
    #[serde(default)]
    pub attribute_name: Option<String>,
    /// Optional exact expected attribute value. Omit to assert existence only.
    #[serde(default)]
    pub expected_attribute_value: Option<String>,
    /// Text comparison mode for `to_have_text`. Defaults to exact.
    #[serde(default)]
    pub text_match: Option<BrowserAssertTextMatch>,
    /// Collapse whitespace for `to_have_text`. Defaults to true.
    #[serde(default)]
    pub normalize_whitespace: Option<bool>,
    /// Invert the matcher result.
    #[serde(default)]
    pub negate: Option<bool>,
    /// Bounded retry timeout in milliseconds. Defaults to 5000, max 30000.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Poll interval in milliseconds. Defaults to 100, allowed 10..=5000.
    #[serde(default)]
    pub interval_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserAssertResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub matcher: BrowserAssertMatcher,
    pub pass: bool,
    pub timed_out: bool,
    pub negate: bool,
    pub elapsed_ms: u64,
    pub timeout_ms: u64,
    pub interval_ms: u64,
    pub poll_count: u32,
    pub match_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_id: Option<String>,
    pub actual: Value,
    pub expected: Value,
    pub message: String,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
}

#[derive(Clone, Debug)]
struct NormalizedBrowserAssertParams {
    locator: BrowserAssertLocator,
    matcher: BrowserAssertMatcher,
    expected_text: Option<String>,
    expected_value: Option<String>,
    expected_bool: Option<bool>,
    expected_count: Option<usize>,
    attribute_name: Option<String>,
    expected_attribute_value: Option<String>,
    text_match: BrowserAssertTextMatch,
    normalize_whitespace: bool,
    negate: bool,
    timeout_ms: u64,
    interval_ms: u64,
    root_backend_node_id: Option<i64>,
    root_target_id: Option<String>,
    limit: usize,
    strict: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct AssertElementState {
    tag_name: String,
    text: String,
    value: Option<String>,
    attributes: BTreeMap<String, String>,
    is_visible: bool,
    is_enabled: bool,
    is_checked: Option<bool>,
}

#[derive(Clone, Debug)]
struct AssertPoll {
    pass: bool,
    url: String,
    title: String,
    ready_state: String,
    match_count: usize,
    element_id: Option<String>,
    actual: Value,
    expected: Value,
    message: String,
}

#[derive(Clone, Debug)]
struct AriaSnapshotBuild {
    snapshot: String,
    nodes: Vec<BrowserAriaSnapshotNode>,
    truncated_by_depth: bool,
}

#[tool_router(router = browser_assert_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Emit a Playwright-style ARIA snapshot for the calling session's owned browser tab. Raw-CDP targets use Accessibility.getFullAXTree; normal Chrome bridge targets use debugger-free chrome.scripting DOM/ARIA readback. Returns a stable YAML-like role/name/value tree plus structured node entries, and can scope to a subtree by element id. Background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_aria_snapshot(
        &self,
        params: Parameters<BrowserAriaSnapshotParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserAriaSnapshotResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = ARIA_TOOL,
            "tool.invocation kind=browser_aria_snapshot"
        );
        let session_id = require_target_session_id(&request_context)?;
        let snapshot_params = validate_aria_snapshot_params(&params.0)?;
        let resolution_target = params
            .0
            .cdp_target_id
            .clone()
            .or_else(|| snapshot_params.root_target_id.clone());
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(resolution_target.as_deref()),
            "root_element_id": params.0.root_element_id,
            "max_nodes": snapshot_params.max_nodes,
            "max_depth": snapshot_params.max_depth,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            ARIA_TOOL,
            &session_id,
            params.0.window_hwnd,
            resolution_target.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            ARIA_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "root_element_id": snapshot_params.root_element_id,
            "max_nodes": snapshot_params.max_nodes,
            "max_depth": snapshot_params.max_depth,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            ARIA_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_aria_snapshot_impl(&session_id, window_hwnd, &cdp_target_id, &snapshot_params)
            .await;
        self.audit_action_result_for_session(ARIA_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Assert a Playwright-style locator against the calling session's owned browser tab with bounded retry. Supports to_be_visible, to_have_text, to_have_value, to_be_checked, to_be_enabled, to_have_attribute, and to_have_count, returning pass/fail plus actual vs expected diagnostics after polling until pass or timeout. Raw-CDP targets use CDP locator/Runtime readback; normal Chrome bridge targets use debugger-free chrome.scripting DOM readback. Background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_assert(
        &self,
        params: Parameters<BrowserAssertParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserAssertResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = ASSERT_TOOL,
            "tool.invocation kind=browser_assert"
        );
        let session_id = require_target_session_id(&request_context)?;
        let assertion = validate_browser_assert_params(&params.0)?;
        let resolution_target = params
            .0
            .cdp_target_id
            .clone()
            .or_else(|| assertion.root_target_id.clone());
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(resolution_target.as_deref()),
            "matcher": assertion.matcher,
            "locator_engine": assertion.locator.engine,
            "locator_query_len": assertion.locator.query.len(),
            "timeout_ms": assertion.timeout_ms,
            "interval_ms": assertion.interval_ms,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            ASSERT_TOOL,
            &session_id,
            params.0.window_hwnd,
            resolution_target.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            ASSERT_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "matcher": assertion.matcher,
            "locator_engine": assertion.locator.engine,
            "locator_query_len": assertion.locator.query.len(),
            "timeout_ms": assertion.timeout_ms,
            "interval_ms": assertion.interval_ms,
            "negate": assertion.negate,
            "strict": assertion.strict,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            ASSERT_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_assert_impl(&session_id, window_hwnd, &cdp_target_id, &assertion)
            .await;
        self.audit_action_result_for_session(ASSERT_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[cfg(windows)]
    async fn browser_aria_snapshot_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedAriaSnapshotParams,
    ) -> Result<BrowserAriaSnapshotResponse, ErrorData> {
        if is_chrome_bridge_target_id(cdp_target_id) {
            return browser_aria_snapshot_bridge(session_id, window_hwnd, cdp_target_id, params)
                .await;
        }
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            return Err(browser_raw_cdp_required_error(ARIA_TOOL, window_hwnd));
        };
        let snapshot = synapse_a11y::fetch_dom_snapshot(
            &endpoint,
            window_hwnd,
            "",
            None,
            Some(cdp_target_id),
            params.max_nodes,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{ARIA_TOOL} raw CDP Accessibility.getFullAXTree failed: {error}"),
            )
        })?;
        let state =
            synapse_a11y::cdp_evaluate_expression(&endpoint, cdp_target_id, "null", false, true)
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("{ARIA_TOOL} page state readback failed: {error}"),
                    )
                })?;
        let built = build_aria_snapshot(
            &snapshot.nodes,
            params.root_element_id.as_deref(),
            params.max_depth,
        )?;
        let truncated_by_max_nodes =
            usize::try_from(snapshot.total_ax_nodes).is_ok_and(|total| total > params.max_nodes);
        tracing::info!(
            code = "CDP_BACKGROUND_ARIA_SNAPSHOT",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %snapshot.target_id,
            node_count = built.nodes.len(),
            total_ax_nodes = snapshot.total_ax_nodes,
            root_scoped = params.root_element_id.is_some(),
            target_url = %state.url,
            "readback=Accessibility.getFullAXTree outcome=aria_snapshot"
        );
        Ok(BrowserAriaSnapshotResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: snapshot.target_id,
            url: state.url,
            title: state.title,
            ready_state: state.ready_state,
            root_element_id: params.root_element_id.clone(),
            snapshot: built.snapshot,
            node_count: built.nodes.len(),
            nodes: built.nodes,
            total_ax_nodes: snapshot.total_ax_nodes,
            max_nodes: params.max_nodes,
            max_depth: params.max_depth,
            truncated_by_max_nodes,
            truncated_by_depth: built.truncated_by_depth,
            frame_tree_frame_count: snapshot.frame_tree_frame_count,
            attached_frame_target_count: snapshot.attached_frame_target_count,
            blocked_frame_targets: snapshot.blocked_frame_targets,
            frame_snapshot_errors: snapshot.frame_snapshot_errors,
            readback_backend: "Accessibility.getFullAXTree + Runtime.evaluate(page state)"
                .to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_aria_snapshot_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _params: &NormalizedAriaSnapshotParams,
    ) -> Result<BrowserAriaSnapshotResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_aria_snapshot is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_assert_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &NormalizedBrowserAssertParams,
    ) -> Result<BrowserAssertResponse, ErrorData> {
        if is_chrome_bridge_target_id(cdp_target_id) {
            return browser_assert_bridge_loop(session_id, window_hwnd, cdp_target_id, params)
                .await;
        }
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            return Err(browser_raw_cdp_required_error(ASSERT_TOOL, window_hwnd));
        };
        let started = Instant::now();
        let mut poll_count = 0_u32;
        loop {
            poll_count = poll_count.saturating_add(1);
            let poll = browser_assert_poll(&endpoint, window_hwnd, cdp_target_id, params).await?;
            let elapsed_ms = duration_millis(started.elapsed());
            if poll.pass {
                tracing::info!(
                    code = "CDP_BACKGROUND_ASSERT",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    endpoint = %endpoint,
                    cdp_target_id = %cdp_target_id,
                    matcher = ?params.matcher,
                    pass = true,
                    poll_count,
                    elapsed_ms,
                    "readback=cdp_locate+Runtime.callFunctionOn outcome=assertion_passed"
                );
                return Ok(browser_assert_response(
                    session_id,
                    window_hwnd,
                    endpoint,
                    cdp_target_id,
                    params,
                    poll,
                    poll_count,
                    elapsed_ms,
                    false,
                    "raw_cdp",
                    "cdp_locate + Runtime.callFunctionOn",
                    "cdp",
                ));
            }
            if elapsed_ms >= params.timeout_ms {
                tracing::info!(
                    code = "CDP_BACKGROUND_ASSERT",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    endpoint = %endpoint,
                    cdp_target_id = %cdp_target_id,
                    matcher = ?params.matcher,
                    pass = false,
                    poll_count,
                    elapsed_ms,
                    "readback=cdp_locate+Runtime.callFunctionOn outcome=assertion_timeout"
                );
                return Ok(browser_assert_response(
                    session_id,
                    window_hwnd,
                    endpoint,
                    cdp_target_id,
                    params,
                    poll,
                    poll_count,
                    elapsed_ms,
                    true,
                    "raw_cdp",
                    "cdp_locate + Runtime.callFunctionOn",
                    "cdp",
                ));
            }
            let remaining = params.timeout_ms.saturating_sub(elapsed_ms);
            tokio::time::sleep(Duration::from_millis(params.interval_ms.min(remaining))).await;
        }
    }

    #[cfg(not(windows))]
    async fn browser_assert_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _params: &NormalizedBrowserAssertParams,
    ) -> Result<BrowserAssertResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_assert is only available on Windows in this build",
        ))
    }
}

fn validate_aria_snapshot_params(
    params: &BrowserAriaSnapshotParams,
) -> Result<NormalizedAriaSnapshotParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    let root_target_id = params
        .root_element_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .map(parse_root_element_target)
        .transpose()?
        .map(|(_, target)| target);
    if let (Some(explicit), Some(root_target)) = (params.cdp_target_id.as_deref(), &root_target_id)
        && !explicit.eq_ignore_ascii_case(root_target)
    {
        return Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "{ARIA_TOOL} root_element_id resolves to CDP target {root_target:?} but cdp_target_id {explicit:?} was also supplied; they must match"
            ),
        ));
    }
    Ok(NormalizedAriaSnapshotParams {
        root_element_id: params.root_element_id.clone(),
        root_target_id,
        max_nodes: params
            .max_nodes
            .unwrap_or(DEFAULT_ARIA_MAX_NODES)
            .clamp(1, MAX_ARIA_MAX_NODES),
        max_depth: params
            .max_depth
            .unwrap_or(DEFAULT_ARIA_MAX_DEPTH)
            .clamp(1, MAX_ARIA_MAX_DEPTH),
    })
}

fn validate_browser_assert_params(
    params: &BrowserAssertParams,
) -> Result<NormalizedBrowserAssertParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    validate_assert_locator(&params.locator)?;
    validate_optional_text(
        ASSERT_TOOL,
        "expected_text",
        params.expected_text.as_deref(),
    )?;
    validate_optional_text(
        ASSERT_TOOL,
        "expected_value",
        params.expected_value.as_deref(),
    )?;
    validate_optional_text(
        ASSERT_TOOL,
        "expected_attribute_value",
        params.expected_attribute_value.as_deref(),
    )?;
    validate_attribute_name(params.attribute_name.as_deref())?;

    let (root_backend_node_id, root_target_id) = params
        .locator
        .root_element_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
        .map(parse_root_element_target)
        .transpose()?
        .map_or((None, None), |(backend, target)| (backend, Some(target)));
    if let (Some(explicit), Some(root_target)) = (params.cdp_target_id.as_deref(), &root_target_id)
        && !explicit.eq_ignore_ascii_case(root_target)
    {
        return Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "{ASSERT_TOOL} root_element_id resolves to CDP target {root_target:?} but cdp_target_id {explicit:?} was also supplied; they must match"
            ),
        ));
    }

    let timeout_ms = params.timeout_ms.unwrap_or(DEFAULT_ASSERT_TIMEOUT_MS);
    if timeout_ms == 0 || timeout_ms > MAX_ASSERT_TIMEOUT_MS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ASSERT_TOOL} timeout_ms must be 1..={MAX_ASSERT_TIMEOUT_MS}"),
        ));
    }
    let interval_ms = params.interval_ms.unwrap_or(DEFAULT_ASSERT_INTERVAL_MS);
    if !(MIN_ASSERT_INTERVAL_MS..=MAX_ASSERT_INTERVAL_MS).contains(&interval_ms) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{ASSERT_TOOL} interval_ms must be {MIN_ASSERT_INTERVAL_MS}..={MAX_ASSERT_INTERVAL_MS}"
            ),
        ));
    }

    match params.matcher {
        BrowserAssertMatcher::ToHaveText => require_expected(
            params.expected_text.as_ref(),
            "expected_text",
            "to_have_text",
        )?,
        BrowserAssertMatcher::ToHaveValue => require_expected(
            params.expected_value.as_ref(),
            "expected_value",
            "to_have_value",
        )?,
        BrowserAssertMatcher::ToHaveCount => require_expected(
            params.expected_count.as_ref(),
            "expected_count",
            "to_have_count",
        )?,
        BrowserAssertMatcher::ToHaveAttribute => require_expected(
            params.attribute_name.as_ref(),
            "attribute_name",
            "to_have_attribute",
        )?,
        BrowserAssertMatcher::ToBeVisible
        | BrowserAssertMatcher::ToBeChecked
        | BrowserAssertMatcher::ToBeEnabled => {}
    }

    if !matches!(params.matcher, BrowserAssertMatcher::ToHaveText) && params.expected_text.is_some()
    {
        return Err(unused_expected_error("expected_text", params.matcher));
    }
    if !matches!(params.matcher, BrowserAssertMatcher::ToHaveValue)
        && params.expected_value.is_some()
    {
        return Err(unused_expected_error("expected_value", params.matcher));
    }
    if !matches!(params.matcher, BrowserAssertMatcher::ToHaveCount)
        && params.expected_count.is_some()
    {
        return Err(unused_expected_error("expected_count", params.matcher));
    }
    if !matches!(params.matcher, BrowserAssertMatcher::ToHaveAttribute)
        && (params.attribute_name.is_some() || params.expected_attribute_value.is_some())
    {
        return Err(unused_expected_error(
            "attribute_name/expected_attribute_value",
            params.matcher,
        ));
    }

    let strict = params
        .locator
        .strict
        .unwrap_or(!matches!(params.matcher, BrowserAssertMatcher::ToHaveCount));
    let default_limit = if matches!(params.matcher, BrowserAssertMatcher::ToHaveCount) {
        1
    } else if strict {
        2
    } else {
        1
    };
    let limit = params.locator.limit.unwrap_or(default_limit).clamp(1, 500);

    Ok(NormalizedBrowserAssertParams {
        locator: params.locator.clone(),
        matcher: params.matcher,
        expected_text: params.expected_text.clone(),
        expected_value: params.expected_value.clone(),
        expected_bool: params.expected_bool,
        expected_count: params.expected_count,
        attribute_name: params.attribute_name.clone(),
        expected_attribute_value: params.expected_attribute_value.clone(),
        text_match: params.text_match.unwrap_or_default(),
        normalize_whitespace: params.normalize_whitespace.unwrap_or(true),
        negate: params.negate.unwrap_or(false),
        timeout_ms,
        interval_ms,
        root_backend_node_id,
        root_target_id,
        limit,
        strict,
    })
}

fn validate_assert_locator(locator: &BrowserAssertLocator) -> Result<(), ErrorData> {
    if locator.query.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ASSERT_TOOL} locator.query must not be empty"),
        ));
    }
    if locator.query.len() > ASSERT_LOCATOR_MAX_QUERY_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{ASSERT_TOOL} locator.query is {} bytes; maximum is {ASSERT_LOCATOR_MAX_QUERY_BYTES}",
                locator.query.len()
            ),
        ));
    }
    if locator.exact == Some(true) && locator.regex == Some(true) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ASSERT_TOOL} locator exact and regex are mutually exclusive"),
        ));
    }
    if locator.name_exact == Some(true) && locator.name_regex == Some(true) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ASSERT_TOOL} locator name_exact and name_regex are mutually exclusive"),
        ));
    }
    if locator.engine == BrowserLocateEngine::Layout {
        if locator.relation.is_none() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{ASSERT_TOOL} layout locator requires relation"),
            ));
        }
        if locator
            .anchor
            .as_deref()
            .is_none_or(|anchor| anchor.trim().is_empty())
        {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{ASSERT_TOOL} layout locator requires a non-empty anchor"),
            ));
        }
    }
    Ok(())
}

fn require_expected<T>(value: Option<&T>, field: &str, matcher: &str) -> Result<(), ErrorData> {
    if value.is_some() {
        Ok(())
    } else {
        Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ASSERT_TOOL} matcher={matcher} requires {field}"),
        ))
    }
}

fn unused_expected_error(field: &str, matcher: BrowserAssertMatcher) -> ErrorData {
    mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!("{ASSERT_TOOL} {field} is not valid for matcher={matcher:?}"),
    )
}

fn validate_optional_text(tool: &str, field: &str, value: Option<&str>) -> Result<(), ErrorData> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} {field} must not contain NUL"),
        ));
    }
    if value.chars().count() > ASSERT_MAX_TEXT_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} {field} must be at most {ASSERT_MAX_TEXT_CHARS} characters"),
        ));
    }
    Ok(())
}

fn validate_attribute_name(value: Option<&str>) -> Result<(), ErrorData> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ASSERT_TOOL} attribute_name must not be empty"),
        ));
    }
    if value.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ASSERT_TOOL} attribute_name must not contain NUL"),
        ));
    }
    if value.chars().count() > ASSERT_MAX_ATTRIBUTE_NAME_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{ASSERT_TOOL} attribute_name must be at most {ASSERT_MAX_ATTRIBUTE_NAME_CHARS} characters"
            ),
        ));
    }
    Ok(())
}

fn parse_cdp_element_id(element_id: &str) -> Result<(i64, String), ErrorData> {
    let parsed = synapse_core::ElementId::parse(element_id).map_err(|err| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{ASSERT_TOOL} element_id {element_id:?} is not valid: {err}"),
        )
    })?;
    let backend = synapse_a11y::cdp_backend_from_element_id(&parsed).ok_or_else(|| {
        mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!("{ASSERT_TOOL} element_id {element_id:?} is not a CDP web element"),
        )
    })?;
    let target = synapse_a11y::cdp_target_from_element_id(&parsed).ok_or_else(|| {
        mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "{ASSERT_TOOL} element_id {element_id:?} has no embedded CDP target id; re-resolve it against the owned tab"
            ),
        )
    })?;
    Ok((backend, target))
}

fn parse_root_element_target(element_id: &str) -> Result<(Option<i64>, String), ErrorData> {
    if let Some(target) = chrome_bridge_target_id_from_element_id(element_id) {
        return Ok((None, target));
    }
    let (backend, target) = parse_cdp_element_id(element_id)?;
    Ok((Some(backend), target))
}

fn is_chrome_bridge_target_id(target_id: &str) -> bool {
    target_id
        .strip_prefix("chrome-tab:")
        .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()))
}

fn chrome_bridge_target_id_from_element_id(element_id: &str) -> Option<String> {
    let suffix = element_id.strip_prefix("chrome-tab:")?;
    let digit_count = suffix.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digit_count == 0 {
        return None;
    }
    let (tab_id, remainder) = suffix.split_at(digit_count);
    if !remainder.starts_with(":frame:") {
        return None;
    }
    Some(format!("chrome-tab:{tab_id}"))
}

#[cfg(windows)]
async fn browser_aria_snapshot_bridge(
    session_id: &str,
    window_hwnd: i64,
    cdp_target_id: &str,
    params: &NormalizedAriaSnapshotParams,
) -> Result<BrowserAriaSnapshotResponse, ErrorData> {
    let snapshot = crate::chrome_debugger_bridge::aria_snapshot(
        window_hwnd,
        cdp_target_id,
        params.root_element_id.as_deref(),
        params.max_nodes,
        params.max_depth,
    )
    .await
    .map_err(|error| {
        let code = error.code();
        let detail = error.detail().to_owned();
        mcp_error(
            code,
            format!("{ARIA_TOOL} normal Chrome bridge ariaSnapshot failed: {detail}"),
        )
    })?;
    tracing::info!(
        code = "CHROME_BRIDGE_ARIA_SNAPSHOT",
        session_id = %session_id,
        hwnd = window_hwnd,
        cdp_target_id = %snapshot.target_id,
        node_count = snapshot.nodes.len(),
        root_scoped = params.root_element_id.is_some(),
        target_url = %snapshot.url,
        "readback=chrome.scripting.executeScript outcome=aria_snapshot"
    );
    Ok(BrowserAriaSnapshotResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: "chrome_tabs_extension".to_owned(),
        endpoint: "chrome_bridge".to_owned(),
        cdp_target_id: snapshot.target_id,
        url: snapshot.url,
        title: snapshot.title,
        ready_state: snapshot.ready_state,
        root_element_id: snapshot
            .root_element_id
            .or_else(|| params.root_element_id.clone()),
        snapshot: snapshot.snapshot,
        node_count: snapshot.nodes.len(),
        nodes: snapshot
            .nodes
            .into_iter()
            .map(|node| BrowserAriaSnapshotNode {
                element_id: node.element_id,
                parent_element_id: node.parent_element_id,
                depth: node.depth,
                role: node.role,
                name: node.name,
                value: node.value,
                enabled: node.enabled,
                focused: node.focused,
                children_count: node.children_count,
            })
            .collect(),
        total_ax_nodes: snapshot.total_ax_nodes,
        max_nodes: snapshot.max_nodes,
        max_depth: snapshot.max_depth,
        truncated_by_max_nodes: snapshot.truncated_by_max_nodes,
        truncated_by_depth: snapshot.truncated_by_depth,
        frame_tree_frame_count: snapshot.frame_tree_frame_count,
        attached_frame_target_count: snapshot.attached_frame_target_count,
        blocked_frame_targets: snapshot.blocked_frame_targets,
        frame_snapshot_errors: snapshot.frame_snapshot_errors,
        readback_backend: snapshot.readback_backend,
        backend_tier_used: snapshot.backend_tier_used,
        required_foreground: snapshot.required_foreground,
    })
}

#[cfg(windows)]
async fn browser_assert_bridge_loop(
    session_id: &str,
    window_hwnd: i64,
    cdp_target_id: &str,
    params: &NormalizedBrowserAssertParams,
) -> Result<BrowserAssertResponse, ErrorData> {
    let started = Instant::now();
    let mut poll_count = 0_u32;
    loop {
        poll_count = poll_count.saturating_add(1);
        let poll = browser_assert_bridge_poll(window_hwnd, cdp_target_id, params).await?;
        let elapsed_ms = duration_millis(started.elapsed());
        if poll.pass {
            tracing::info!(
                code = "CHROME_BRIDGE_ASSERT",
                session_id = %session_id,
                hwnd = window_hwnd,
                cdp_target_id = %cdp_target_id,
                matcher = ?params.matcher,
                pass = true,
                poll_count,
                elapsed_ms,
                "readback=chrome.scripting.executeScript outcome=assertion_passed"
            );
            return Ok(browser_assert_response(
                session_id,
                window_hwnd,
                "chrome_bridge".to_owned(),
                cdp_target_id,
                params,
                poll,
                poll_count,
                elapsed_ms,
                false,
                "chrome_tabs_extension",
                "chrome.scripting.executeScript(assertPoll)",
                "chrome_tabs_extension",
            ));
        }
        if elapsed_ms >= params.timeout_ms {
            tracing::info!(
                code = "CHROME_BRIDGE_ASSERT",
                session_id = %session_id,
                hwnd = window_hwnd,
                cdp_target_id = %cdp_target_id,
                matcher = ?params.matcher,
                pass = false,
                poll_count,
                elapsed_ms,
                "readback=chrome.scripting.executeScript outcome=assertion_timeout"
            );
            return Ok(browser_assert_response(
                session_id,
                window_hwnd,
                "chrome_bridge".to_owned(),
                cdp_target_id,
                params,
                poll,
                poll_count,
                elapsed_ms,
                true,
                "chrome_tabs_extension",
                "chrome.scripting.executeScript(assertPoll)",
                "chrome_tabs_extension",
            ));
        }
        let remaining = params.timeout_ms.saturating_sub(elapsed_ms);
        tokio::time::sleep(Duration::from_millis(params.interval_ms.min(remaining))).await;
    }
}

#[cfg(windows)]
async fn browser_assert_bridge_poll(
    window_hwnd: i64,
    cdp_target_id: &str,
    params: &NormalizedBrowserAssertParams,
) -> Result<AssertPoll, ErrorData> {
    let locator = serde_json::to_value(&params.locator).map_err(|error| {
        mcp_error(
            error_codes::OBSERVE_INTERNAL,
            format!("{ASSERT_TOOL} locator serialization failed: {error}"),
        )
    })?;
    let located = crate::chrome_debugger_bridge::assert_poll(
        window_hwnd,
        cdp_target_id,
        locator,
        params.limit,
    )
    .await
    .map_err(|error| {
        let code = error.code();
        let detail = error.detail().to_owned();
        mcp_error(
            code,
            format!("{ASSERT_TOOL} normal Chrome bridge assertPoll failed: {detail}"),
        )
    })?;
    if matches!(params.matcher, BrowserAssertMatcher::ToHaveCount) {
        return Ok(assert_count_poll_from_count(
            located.match_count,
            located.url,
            located.title,
            located.ready_state,
            params,
        )?);
    }
    if located.match_count == 0 {
        return Ok(AssertPoll {
            pass: false,
            url: located.url,
            title: located.title,
            ready_state: located.ready_state,
            match_count: 0,
            element_id: None,
            actual: json!({"match_count": 0}),
            expected: expected_value(params),
            message: format!("{ASSERT_TOOL} locator matched no elements"),
        });
    }
    if params.strict && located.match_count != 1 {
        return Ok(AssertPoll {
            pass: apply_negate(false, params.negate),
            url: located.url,
            title: located.title,
            ready_state: located.ready_state,
            match_count: located.match_count,
            element_id: None,
            actual: json!({"match_count": located.match_count}),
            expected: json!({"match_count": 1, "strict": true}),
            message: format!(
                "{ASSERT_TOOL} strict locator expected exactly one element but matched {}",
                located.match_count
            ),
        });
    }
    let Some(state) = located.state else {
        return Ok(AssertPoll {
            pass: false,
            url: located.url,
            title: located.title,
            ready_state: located.ready_state,
            match_count: located.match_count,
            element_id: None,
            actual: json!({"match_count": located.match_count, "returned_count": 0}),
            expected: expected_value(params),
            message: format!("{ASSERT_TOOL} locator returned no inspectable element state"),
        });
    };
    assert_element_poll(
        params,
        AssertElementState {
            tag_name: state.tag_name,
            text: state.text,
            value: state.value,
            attributes: state.attributes,
            is_visible: state.is_visible,
            is_enabled: state.is_enabled,
            is_checked: state.is_checked,
        },
        located.match_count,
        located.element_id,
        located.url,
        located.title,
        located.ready_state,
    )
}

#[cfg(windows)]
async fn browser_assert_poll(
    endpoint: &str,
    window_hwnd: i64,
    cdp_target_id: &str,
    params: &NormalizedBrowserAssertParams,
) -> Result<AssertPoll, ErrorData> {
    let request = cdp_locate_request(params);
    let located = synapse_a11y::cdp_locate(endpoint, cdp_target_id, request)
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{ASSERT_TOOL} raw CDP locator resolution failed: {error}"),
            )
        })?;
    if matches!(params.matcher, BrowserAssertMatcher::ToHaveCount) {
        return Ok(assert_count_poll(&located, params)?);
    }
    if located.match_count == 0 {
        let poll = AssertPoll {
            pass: false,
            url: located.url,
            title: located.title,
            ready_state: String::new(),
            match_count: 0,
            element_id: None,
            actual: json!({"match_count": 0}),
            expected: expected_value(params),
            message: format!("{ASSERT_TOOL} locator matched no elements"),
        };
        return Ok(poll);
    }
    if params.strict && located.match_count != 1 {
        let poll = AssertPoll {
            pass: apply_negate(false, params.negate),
            url: located.url,
            title: located.title,
            ready_state: String::new(),
            match_count: located.match_count,
            element_id: None,
            actual: json!({"match_count": located.match_count}),
            expected: json!({"match_count": 1, "strict": true}),
            message: format!(
                "{ASSERT_TOOL} strict locator expected exactly one element but matched {}",
                located.match_count
            ),
        };
        return Ok(poll);
    }
    let Some(backend_node_id) = located.backend_node_ids.first().copied() else {
        return Ok(AssertPoll {
            pass: false,
            url: located.url,
            title: located.title,
            ready_state: String::new(),
            match_count: located.match_count,
            element_id: None,
            actual: json!({"match_count": located.match_count, "returned_count": 0}),
            expected: expected_value(params),
            message: format!("{ASSERT_TOOL} locator returned no inspectable element ids"),
        });
    };
    let evaluated = match synapse_a11y::cdp_evaluate_on_element(
        endpoint,
        cdp_target_id,
        backend_node_id,
        ASSERT_ELEMENT_FUNCTION,
        &[],
        true,
        true,
    )
    .await
    {
        Ok(evaluated) => evaluated,
        Err(error) => {
            let detail = error.to_string();
            if detail.contains("returned no objectId")
                || detail.contains("not present")
                || detail.contains("detached")
            {
                return Ok(AssertPoll {
                    pass: apply_negate(false, params.negate),
                    url: located.url,
                    title: located.title,
                    ready_state: String::new(),
                    match_count: located.match_count,
                    element_id: None,
                    actual: json!({"element_state": "detached"}),
                    expected: expected_value(params),
                    message: format!("{ASSERT_TOOL} matched element detached before inspection"),
                });
            }
            return Err(mcp_error(
                error.code(),
                format!("{ASSERT_TOOL} element state readback failed: {error}"),
            ));
        }
    };
    let state: AssertElementState =
        serde_json::from_value(evaluated.value.clone()).map_err(|error| {
            mcp_error(
                error_codes::OBSERVE_INTERNAL,
                format!("{ASSERT_TOOL} element state payload decode failed: {error}"),
            )
        })?;
    let element_id =
        synapse_a11y::cdp_element_id_for_target(window_hwnd, &located.target_id, backend_node_id)
            .to_string();
    assert_element_poll(
        params,
        state,
        located.match_count,
        Some(element_id),
        evaluated.url,
        evaluated.title,
        evaluated.ready_state,
    )
}

#[cfg(windows)]
fn cdp_locate_request(params: &NormalizedBrowserAssertParams) -> synapse_a11y::CdpLocateRequest {
    synapse_a11y::CdpLocateRequest {
        engine: locate_engine_to_a11y(params.locator.engine),
        query: params.locator.query.clone(),
        exact: params.locator.exact.unwrap_or(false),
        regex: params.locator.regex.unwrap_or(false),
        name: params.locator.name.clone(),
        name_exact: params.locator.name_exact.unwrap_or(false),
        name_regex: params.locator.name_regex.unwrap_or(false),
        testid_attribute: params.locator.testid_attribute.clone(),
        checked: params.locator.checked,
        pressed: params.locator.pressed,
        expanded: params.locator.expanded,
        selected: params.locator.selected,
        disabled: params.locator.disabled,
        level: params.locator.level,
        include_hidden: params.locator.include_hidden.unwrap_or(false),
        relation: params.locator.relation.map(layout_relation_to_a11y),
        anchor: params.locator.anchor.clone(),
        max_distance: params.locator.max_distance,
        has_text: params.locator.has_text.clone(),
        nth: params.locator.nth,
        strict: false,
        root_backend_node_id: params.root_backend_node_id,
        frame_id: None,
        limit: params.limit,
    }
}

#[cfg(windows)]
fn locate_engine_to_a11y(engine: BrowserLocateEngine) -> synapse_a11y::CdpLocateEngine {
    match engine {
        BrowserLocateEngine::Css => synapse_a11y::CdpLocateEngine::Css,
        BrowserLocateEngine::Xpath => synapse_a11y::CdpLocateEngine::Xpath,
        BrowserLocateEngine::Text => synapse_a11y::CdpLocateEngine::Text,
        BrowserLocateEngine::Role => synapse_a11y::CdpLocateEngine::Role,
        BrowserLocateEngine::Label => synapse_a11y::CdpLocateEngine::Label,
        BrowserLocateEngine::Placeholder => synapse_a11y::CdpLocateEngine::Placeholder,
        BrowserLocateEngine::AltText => synapse_a11y::CdpLocateEngine::AltText,
        BrowserLocateEngine::Title => synapse_a11y::CdpLocateEngine::Title,
        BrowserLocateEngine::TestId => synapse_a11y::CdpLocateEngine::TestId,
        BrowserLocateEngine::Layout => synapse_a11y::CdpLocateEngine::Layout,
    }
}

#[cfg(windows)]
fn layout_relation_to_a11y(relation: BrowserLayoutRelation) -> synapse_a11y::CdpLayoutRelation {
    match relation {
        BrowserLayoutRelation::Near => synapse_a11y::CdpLayoutRelation::Near,
        BrowserLayoutRelation::RightOf => synapse_a11y::CdpLayoutRelation::RightOf,
        BrowserLayoutRelation::LeftOf => synapse_a11y::CdpLayoutRelation::LeftOf,
        BrowserLayoutRelation::Above => synapse_a11y::CdpLayoutRelation::Above,
        BrowserLayoutRelation::Below => synapse_a11y::CdpLayoutRelation::Below,
    }
}

#[cfg(windows)]
fn assert_count_poll(
    located: &synapse_a11y::CdpLocateResult,
    params: &NormalizedBrowserAssertParams,
) -> Result<AssertPoll, ErrorData> {
    assert_count_poll_from_count(
        located.match_count,
        located.url.clone(),
        located.title.clone(),
        String::new(),
        params,
    )
}

fn assert_count_poll_from_count(
    match_count: usize,
    url: String,
    title: String,
    ready_state: String,
    params: &NormalizedBrowserAssertParams,
) -> Result<AssertPoll, ErrorData> {
    let expected_count = params.expected_count.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("{ASSERT_TOOL} to_have_count poll missing validated expected_count"),
        )
    })?;
    let base_pass = match_count == expected_count;
    let pass = apply_negate(base_pass, params.negate);
    Ok(AssertPoll {
        pass,
        url,
        title,
        ready_state,
        match_count,
        element_id: None,
        actual: json!({"count": match_count}),
        expected: json!({"count": expected_count}),
        message: assertion_message(pass, params.negate, "count", match_count, expected_count),
    })
}

fn assert_element_poll(
    params: &NormalizedBrowserAssertParams,
    state: AssertElementState,
    match_count: usize,
    element_id: Option<String>,
    url: String,
    title: String,
    ready_state: String,
) -> Result<AssertPoll, ErrorData> {
    let (base_pass, actual, expected, message) = match params.matcher {
        BrowserAssertMatcher::ToBeVisible => {
            let expected = params.expected_bool.unwrap_or(true);
            let actual = state.is_visible;
            (
                actual == expected,
                json!({"visible": actual, "tag_name": state.tag_name}),
                json!({"visible": expected}),
                assertion_message(
                    actual == expected,
                    params.negate,
                    "visible",
                    actual,
                    expected,
                ),
            )
        }
        BrowserAssertMatcher::ToBeEnabled => {
            let expected = params.expected_bool.unwrap_or(true);
            let actual = state.is_enabled;
            (
                actual == expected,
                json!({"enabled": actual, "tag_name": state.tag_name}),
                json!({"enabled": expected}),
                assertion_message(
                    actual == expected,
                    params.negate,
                    "enabled",
                    actual,
                    expected,
                ),
            )
        }
        BrowserAssertMatcher::ToBeChecked => {
            let expected = params.expected_bool.unwrap_or(true);
            let actual = state.is_checked.unwrap_or(false);
            (
                actual == expected,
                json!({"checked": actual, "tag_name": state.tag_name}),
                json!({"checked": expected}),
                assertion_message(
                    actual == expected,
                    params.negate,
                    "checked",
                    actual,
                    expected,
                ),
            )
        }
        BrowserAssertMatcher::ToHaveText => {
            let expected_raw = params.expected_text.as_deref().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("{ASSERT_TOOL} to_have_text poll missing validated expected_text"),
                )
            })?;
            let actual_text = if params.normalize_whitespace {
                normalize_whitespace(&state.text)
            } else {
                state.text.clone()
            };
            let expected_text = if params.normalize_whitespace {
                normalize_whitespace(expected_raw)
            } else {
                expected_raw.to_owned()
            };
            let matched = text_matches(&actual_text, &expected_text, params.text_match);
            (
                matched,
                json!({"text": actual_text, "tag_name": state.tag_name}),
                json!({
                    "text": expected_text,
                    "text_match": params.text_match,
                    "normalize_whitespace": params.normalize_whitespace,
                }),
                if matched {
                    "text matched expected value".to_owned()
                } else {
                    "text did not match expected value".to_owned()
                },
            )
        }
        BrowserAssertMatcher::ToHaveValue => {
            let expected = params.expected_value.as_deref().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("{ASSERT_TOOL} to_have_value poll missing validated expected_value"),
                )
            })?;
            let actual = state.value.unwrap_or_default();
            (
                actual == expected,
                json!({"value": actual, "tag_name": state.tag_name}),
                json!({"value": expected}),
                assertion_message(
                    actual == expected,
                    params.negate,
                    "value",
                    actual.as_str(),
                    expected,
                ),
            )
        }
        BrowserAssertMatcher::ToHaveAttribute => {
            let attribute = params.attribute_name.as_deref().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("{ASSERT_TOOL} to_have_attribute poll missing validated attribute_name"),
                )
            })?;
            let actual = state.attributes.get(attribute).cloned();
            let matched = match params.expected_attribute_value.as_deref() {
                Some(expected) => actual.as_deref() == Some(expected),
                None => actual.is_some(),
            };
            (
                matched,
                json!({"attribute_name": attribute, "attribute_value": actual}),
                json!({
                    "attribute_name": attribute,
                    "attribute_value": params.expected_attribute_value,
                    "exists_only": params.expected_attribute_value.is_none(),
                }),
                if matched {
                    "attribute matched expected condition".to_owned()
                } else {
                    "attribute did not match expected condition".to_owned()
                },
            )
        }
        BrowserAssertMatcher::ToHaveCount => unreachable!("count handled before element inspect"),
    };
    let pass = apply_negate(base_pass, params.negate);
    Ok(AssertPoll {
        pass,
        url,
        title,
        ready_state,
        match_count,
        element_id,
        actual,
        expected,
        message,
    })
}

fn apply_negate(pass: bool, negate: bool) -> bool {
    if negate { !pass } else { pass }
}

fn text_matches(actual: &str, expected: &str, mode: BrowserAssertTextMatch) -> bool {
    match mode {
        BrowserAssertTextMatch::Exact => actual == expected,
        BrowserAssertTextMatch::Contains => actual.contains(expected),
        BrowserAssertTextMatch::Regex => {
            Regex::new(expected).is_ok_and(|regex| regex.is_match(actual))
        }
    }
}

fn expected_value(params: &NormalizedBrowserAssertParams) -> Value {
    match params.matcher {
        BrowserAssertMatcher::ToBeVisible => {
            json!({"visible": params.expected_bool.unwrap_or(true)})
        }
        BrowserAssertMatcher::ToHaveText => json!({
            "text": params.expected_text,
            "text_match": params.text_match,
            "normalize_whitespace": params.normalize_whitespace,
        }),
        BrowserAssertMatcher::ToHaveValue => json!({"value": params.expected_value}),
        BrowserAssertMatcher::ToBeChecked => {
            json!({"checked": params.expected_bool.unwrap_or(true)})
        }
        BrowserAssertMatcher::ToBeEnabled => {
            json!({"enabled": params.expected_bool.unwrap_or(true)})
        }
        BrowserAssertMatcher::ToHaveAttribute => json!({
            "attribute_name": params.attribute_name,
            "attribute_value": params.expected_attribute_value,
        }),
        BrowserAssertMatcher::ToHaveCount => json!({"count": params.expected_count}),
    }
}

fn assertion_message<T: std::fmt::Debug>(
    pass: bool,
    negate: bool,
    field: &str,
    actual: T,
    expected: T,
) -> String {
    if pass {
        if negate {
            format!("{field} did not equal forbidden value")
        } else {
            format!("{field} matched expected value")
        }
    } else if negate {
        format!("{field} unexpectedly matched forbidden value {actual:?}")
    } else {
        format!("{field} mismatch: actual {actual:?}, expected {expected:?}")
    }
}

fn browser_assert_response(
    session_id: &str,
    window_hwnd: i64,
    endpoint: String,
    cdp_target_id: &str,
    params: &NormalizedBrowserAssertParams,
    poll: AssertPoll,
    poll_count: u32,
    elapsed_ms: u64,
    timed_out: bool,
    transport: &str,
    readback_backend: &str,
    backend_tier_used: &str,
) -> BrowserAssertResponse {
    BrowserAssertResponse {
        session_id: session_id.to_owned(),
        window_hwnd,
        transport: transport.to_owned(),
        endpoint,
        cdp_target_id: cdp_target_id.to_owned(),
        url: poll.url,
        title: poll.title,
        ready_state: poll.ready_state,
        matcher: params.matcher,
        pass: poll.pass,
        timed_out,
        negate: params.negate,
        elapsed_ms,
        timeout_ms: params.timeout_ms,
        interval_ms: params.interval_ms,
        poll_count,
        match_count: poll.match_count,
        element_id: poll.element_id,
        actual: poll.actual,
        expected: poll.expected,
        message: poll.message,
        readback_backend: readback_backend.to_owned(),
        backend_tier_used: backend_tier_used.to_owned(),
        required_foreground: false,
    }
}

fn build_aria_snapshot(
    nodes: &[AccessibleNode],
    root_element_id: Option<&str>,
    max_depth: u32,
) -> Result<AriaSnapshotBuild, ErrorData> {
    let node_by_id: HashMap<String, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.element_id.to_string(), index))
        .collect();
    let root_id = root_element_id.map(str::to_owned);
    if let Some(root_id) = root_id.as_deref()
        && !node_by_id.contains_key(root_id)
    {
        return Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!("{ARIA_TOOL} root_element_id {root_id:?} was not present in the AX snapshot"),
        ));
    }

    let include = included_aria_node_ids(nodes, &node_by_id, root_id.as_deref());
    let mut depth_cache = HashMap::new();
    let mut out_nodes = Vec::new();
    let mut lines = Vec::new();
    let mut truncated_by_depth = false;
    for node in nodes {
        let id = node.element_id.to_string();
        if !include.contains(&id) {
            continue;
        }
        let depth = relative_aria_depth(&id, nodes, &node_by_id, &include, &mut depth_cache, 256);
        if depth > max_depth {
            truncated_by_depth = true;
            continue;
        }
        lines.push(render_aria_line(depth, node));
        out_nodes.push(BrowserAriaSnapshotNode {
            element_id: id,
            parent_element_id: node.parent.as_ref().map(ToString::to_string),
            depth,
            role: node.role.clone(),
            name: node.name.clone(),
            value: node.value.clone(),
            enabled: node.enabled,
            focused: node.focused,
            children_count: node.children_count,
        });
    }
    Ok(AriaSnapshotBuild {
        snapshot: lines.join("\n"),
        nodes: out_nodes,
        truncated_by_depth,
    })
}

fn included_aria_node_ids(
    nodes: &[AccessibleNode],
    node_by_id: &HashMap<String, usize>,
    root_id: Option<&str>,
) -> BTreeSet<String> {
    let Some(root_id) = root_id else {
        return nodes
            .iter()
            .map(|node| node.element_id.to_string())
            .collect::<BTreeSet<_>>();
    };
    nodes
        .iter()
        .filter_map(|node| {
            let id = node.element_id.to_string();
            if aria_node_reaches_root(&id, root_id, nodes, node_by_id) {
                Some(id)
            } else {
                None
            }
        })
        .collect()
}

fn aria_node_reaches_root(
    id: &str,
    root_id: &str,
    nodes: &[AccessibleNode],
    node_by_id: &HashMap<String, usize>,
) -> bool {
    let mut current = Some(id.to_owned());
    for _ in 0..256 {
        let Some(current_id) = current else {
            return false;
        };
        if current_id == root_id {
            return true;
        }
        let Some(index) = node_by_id.get(&current_id).copied() else {
            return false;
        };
        current = nodes[index].parent.as_ref().map(ToString::to_string);
    }
    false
}

fn relative_aria_depth(
    id: &str,
    nodes: &[AccessibleNode],
    node_by_id: &HashMap<String, usize>,
    include: &BTreeSet<String>,
    cache: &mut HashMap<String, u32>,
    guard: u32,
) -> u32 {
    if let Some(depth) = cache.get(id) {
        return *depth;
    }
    if guard == 0 {
        return 0;
    }
    let depth = node_by_id
        .get(id)
        .and_then(|index| nodes[*index].parent.as_ref())
        .map(ToString::to_string)
        .filter(|parent| include.contains(parent))
        .map_or(0, |parent| {
            relative_aria_depth(&parent, nodes, node_by_id, include, cache, guard - 1) + 1
        });
    cache.insert(id.to_owned(), depth);
    depth
}

fn render_aria_line(depth: u32, node: &AccessibleNode) -> String {
    let mut line = format!("{}- {}", "  ".repeat(depth as usize), node.role);
    if !node.name.is_empty() {
        line.push(' ');
        line.push('"');
        line.push_str(&escape_snapshot_scalar(&node.name));
        line.push('"');
    }
    if let Some(value) = node.value.as_deref().filter(|value| !value.is_empty()) {
        line.push_str(": ");
        line.push('"');
        line.push_str(&escape_snapshot_scalar(value));
        line.push('"');
    }
    if !node.enabled {
        line.push_str(" [disabled]");
    }
    if node.focused {
        line.push_str(" [focused]");
    }
    line
}

fn escape_snapshot_scalar(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(windows)]
const ASSERT_ELEMENT_FUNCTION: &str = r#"(el) => {
    const str = v => String(v == null ? "" : v);
    const attrs = {};
    if (el && el.attributes) {
        for (const a of el.attributes) { attrs[a.name] = str(a.value); }
    }
    const rect = el && el.getBoundingClientRect ? el.getBoundingClientRect() : { width: 0, height: 0 };
    const cs = (el && el.nodeType === 1 && el.ownerDocument && el.ownerDocument.defaultView)
        ? el.ownerDocument.defaultView.getComputedStyle(el) : null;
    const hasLayout = !!(el && (el.offsetWidth || el.offsetHeight || (el.getClientRects && el.getClientRects().length)));
    const visible = !!(el && el.isConnected && hasLayout && rect.width >= 0 && rect.height >= 0 &&
        (!cs || (cs.visibility !== "hidden" && cs.display !== "none" && cs.opacity !== "0")));
    const getAttr = name => (el && el.getAttribute) ? str(el.getAttribute(name)) : "";
    const ariaDisabled = getAttr("aria-disabled").toLowerCase() === "true";
    const enabled = !!(el && !(("disabled" in el) ? !!el.disabled : false) && !ariaDisabled);
    let checked = null;
    if (el && "checked" in el) { checked = !!el.checked; }
    else if (el) {
        const ariaChecked = getAttr("aria-checked").toLowerCase();
        if (ariaChecked === "true") { checked = true; }
        else if (ariaChecked === "false") { checked = false; }
    }
    return {
        tag_name: str(el && el.tagName),
        text: str((el && el.innerText) || (el && el.textContent) || ""),
        value: (el && "value" in el) ? str(el.value) : null,
        attributes: attrs,
        is_visible: visible,
        is_enabled: enabled,
        is_checked: checked
    };
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use synapse_core::{ElementId, Rect, element_id};

    fn node(
        runtime: &str,
        parent: Option<&ElementId>,
        role: &str,
        name: &str,
        depth: u32,
    ) -> AccessibleNode {
        AccessibleNode {
            element_id: element_id(0x1000, runtime),
            parent: parent.cloned(),
            name: name.to_owned(),
            role: role.to_owned(),
            automation_id: None,
            value: None,
            bbox: Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 10,
            },
            enabled: true,
            focused: false,
            patterns: Vec::new(),
            children_count: 0,
            depth,
        }
    }

    #[test]
    fn aria_snapshot_renderer_is_stable_and_scopes_subtree() {
        let root = element_id(0x1000, "cdcd000000000001");
        let child = element_id(0x1000, "cdcd000000000002");
        let nodes = vec![
            node("cdcd000000000001", None, "main", "", 0),
            node("cdcd000000000002", Some(&root), "button", "Save", 1),
            node("cdcd000000000003", Some(&child), "text", "Now", 2),
        ];
        let built = build_aria_snapshot(&nodes, Some(&child.to_string()), 8).expect("snapshot");
        assert_eq!(built.nodes.len(), 2);
        assert_eq!(built.snapshot, "- button \"Save\"\n  - text \"Now\"");
    }

    #[test]
    fn text_matching_modes_cover_exact_contains_regex() {
        assert!(text_matches(
            &normalize_whitespace("Save   changes"),
            "Save changes",
            BrowserAssertTextMatch::Exact
        ));
        assert!(text_matches(
            "Save changes",
            "changes",
            BrowserAssertTextMatch::Contains
        ));
        assert!(text_matches(
            "Save changes",
            "Save\\s+changes",
            BrowserAssertTextMatch::Regex
        ));
        assert!(!text_matches(
            "Save changes",
            "[",
            BrowserAssertTextMatch::Regex
        ));
    }

    #[test]
    fn browser_assert_validation_edges() {
        let base_locator = BrowserAssertLocator {
            query: "#save".to_owned(),
            ..Default::default()
        };
        let ok = validate_browser_assert_params(&BrowserAssertParams {
            locator: base_locator.clone(),
            matcher: BrowserAssertMatcher::ToHaveText,
            expected_text: Some("Save".to_owned()),
            timeout_ms: Some(100),
            interval_ms: Some(10),
            ..assert_defaults()
        })
        .expect("valid text assertion");
        assert_eq!(ok.timeout_ms, 100);
        assert_eq!(ok.interval_ms, 10);
        assert!(ok.strict);

        for error in [
            validate_browser_assert_params(&BrowserAssertParams {
                locator: base_locator.clone(),
                matcher: BrowserAssertMatcher::ToHaveText,
                ..assert_defaults()
            })
            .expect_err("to_have_text requires expected_text"),
            validate_browser_assert_params(&BrowserAssertParams {
                locator: BrowserAssertLocator {
                    query: " ".to_owned(),
                    ..Default::default()
                },
                matcher: BrowserAssertMatcher::ToBeVisible,
                ..assert_defaults()
            })
            .expect_err("empty query rejected"),
            validate_browser_assert_params(&BrowserAssertParams {
                locator: base_locator,
                matcher: BrowserAssertMatcher::ToHaveCount,
                expected_count: Some(1),
                interval_ms: Some(1),
                ..assert_defaults()
            })
            .expect_err("interval rejected"),
        ] {
            let code = error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(Value::as_str);
            assert_eq!(code, Some(error_codes::TOOL_PARAMS_INVALID));
        }
    }

    fn assert_defaults() -> BrowserAssertParams {
        BrowserAssertParams {
            cdp_target_id: None,
            window_hwnd: None,
            locator: BrowserAssertLocator {
                query: "#x".to_owned(),
                ..Default::default()
            },
            matcher: BrowserAssertMatcher::ToBeVisible,
            expected_text: None,
            expected_value: None,
            expected_bool: None,
            expected_count: None,
            attribute_name: None,
            expected_attribute_value: None,
            text_match: None,
            normalize_whitespace: None,
            negate: None,
            timeout_ms: None,
            interval_ms: None,
        }
    }
}
