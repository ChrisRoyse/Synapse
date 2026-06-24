//! Cookie and storage-state tools for the debugger-free normal Chrome bridge.

use super::{
    ErrorData, Json, Parameters, SynapseService,
    m1_tools::{cdp_target_id_audit_ref, require_target_session_id},
    mcp_error, tool, tool_router,
};
use rmcp::{RoleServer, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synapse_core::error_codes;

const COOKIES_TOOL: &str = "browser_cookies";
const STORAGE_TOOL: &str = "browser_storage";
const CHROME_TAB_PREFIX: &str = "chrome-tab:";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum BrowserCookiesOperation {
    #[default]
    Get,
    Set,
    Clear,
}


#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserCookiesParams {
    /// get, set, or clear. Defaults to get.
    #[serde(default)]
    pub operation: BrowserCookiesOperation,
    /// URL scope. Defaults to the current URL of this session's owned tab.
    #[serde(default)]
    pub url: Option<String>,
    /// Cookie name for set/get/clear.
    #[serde(default)]
    pub name: Option<String>,
    /// Cookie value for set. Empty string is allowed.
    #[serde(default)]
    pub value: Option<String>,
    /// Optional cookie domain attribute/filter.
    #[serde(default)]
    pub domain: Option<String>,
    /// Optional cookie path attribute/filter.
    #[serde(default)]
    pub path: Option<String>,
    /// Optional secure attribute/filter.
    #[serde(default)]
    pub secure: Option<bool>,
    /// Optional httpOnly attribute for set.
    #[serde(default)]
    pub http_only: Option<bool>,
    /// Optional sameSite value for set: lax, strict, none/no_restriction, or unspecified.
    #[serde(default)]
    pub same_site: Option<String>,
    /// Optional expiration time in Unix seconds for set.
    #[serde(default)]
    pub expires_unix_seconds: Option<f64>,
    /// Optional session-cookie filter for get/clear.
    #[serde(default)]
    pub session: Option<bool>,
    /// Chrome bridge tab target id (`chrome-tab:<id>`). Defaults to this session's active target.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND owning the target. Defaults to the session target's window.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserCookiesResponse {
    pub ok: bool,
    pub required_foreground: bool,
    pub transport: String,
    pub window_hwnd: i64,
    pub cdp_target_id: String,
    pub operation: BrowserCookiesOperation,
    pub source_of_truth: String,
    pub cookie_count: u32,
    pub affected_count: u32,
    pub readback: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum BrowserStorageOperation {
    #[default]
    Get,
    Set,
    Clear,
    SaveState,
    LoadState,
}


#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum BrowserStorageStore {
    #[default]
    Local,
    Session,
}


#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserStorageParams {
    /// get, set, clear, save_state, or load_state. Defaults to get.
    #[serde(default)]
    pub operation: BrowserStorageOperation,
    /// local or session. Ignored by save_state/load_state except for ordinary get/set/clear.
    #[serde(default)]
    pub store: BrowserStorageStore,
    /// Key for get/set/clear. Omit key to get/clear the whole selected store.
    #[serde(default)]
    pub key: Option<String>,
    /// Value for set. Strings are stored directly; other JSON values are JSON-stringified in page.
    #[serde(default)]
    pub value: Option<Value>,
    /// Playwright-style storageState object for load_state.
    #[serde(default)]
    pub state: Option<Value>,
    /// Include sessionStorage in save_state/load_state extension fields.
    #[serde(default)]
    pub include_session_storage: bool,
    /// Clear current-origin localStorage/sessionStorage before load_state.
    #[serde(default)]
    pub clear_before_load: bool,
    /// URL scope for cookies during save_state. Defaults to the current tab URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Chrome bridge tab target id (`chrome-tab:<id>`). Defaults to this session's active target.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND owning the target. Defaults to the session target's window.
    #[serde(default)]
    pub window_hwnd: Option<i64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserStorageResponse {
    pub ok: bool,
    pub required_foreground: bool,
    pub transport: String,
    pub window_hwnd: i64,
    pub cdp_target_id: String,
    pub operation: BrowserStorageOperation,
    pub store: BrowserStorageStore,
    pub source_of_truth: String,
    pub item_count: u32,
    pub origin_count: u32,
    pub readback: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_state: Option<Value>,
}

#[tool_router(router = browser_storage_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Get, set, or clear cookies for this session's owned normal Chrome bridge tab via chrome.cookies (#1152). Preserves domain/path/expires/httpOnly/secure/sameSite attributes where Chrome exposes them. Background-safe and debugger-free: never activates Chrome, never uses OS foreground input, and never falls back to the human foreground tab. Target must be a session-owned chrome-tab:* target from browser_tabs/cdp_open_tab."
    )]
    pub async fn browser_cookies(
        &self,
        params: Parameters<BrowserCookiesParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserCookiesResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = COOKIES_TOOL,
            "tool.invocation kind=browser_cookies"
        );
        let session_id = require_target_session_id(&request_context)?;
        let (window_hwnd, cdp_target_id) = self.resolve_normal_bridge_target(
            COOKIES_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "cdp_target_id": &cdp_target_id,
            "operation": params.0.operation,
            "url_len": params.0.url.as_deref().map(str::len),
            "name": params.0.name.as_deref(),
            "domain": params.0.domain.as_deref(),
            "path": params.0.path.as_deref(),
            "secure": params.0.secure,
            "http_only": params.0.http_only,
            "same_site": params.0.same_site.as_deref(),
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            COOKIES_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_cookies_impl(window_hwnd, &cdp_target_id, &params.0)
            .await;
        self.audit_action_result_for_session(COOKIES_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Get/set/clear localStorage or sessionStorage, save Playwright-style storageState (cookies + per-origin localStorage), or load storageState into this session's owned normal Chrome bridge tab (#1153/#1154/#1155). Runs through typed chrome.scripting/chrome.cookies bridge commands, not arbitrary browser_evaluate, debugger attach, tab activation, or OS foreground input. Target must be a session-owned chrome-tab:* target."
    )]
    pub async fn browser_storage(
        &self,
        params: Parameters<BrowserStorageParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserStorageResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = STORAGE_TOOL,
            "tool.invocation kind=browser_storage"
        );
        let session_id = require_target_session_id(&request_context)?;
        validate_storage_params(&params.0)?;
        let (window_hwnd, cdp_target_id) = self.resolve_normal_bridge_target(
            STORAGE_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "cdp_target_id": &cdp_target_id,
            "operation": params.0.operation,
            "store": params.0.store,
            "key": params.0.key.as_deref(),
            "value_present": params.0.value.is_some(),
            "state_present": params.0.state.is_some(),
            "include_session_storage": params.0.include_session_storage,
            "clear_before_load": params.0.clear_before_load,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            STORAGE_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_storage_impl(window_hwnd, &cdp_target_id, &params.0)
            .await;
        self.audit_action_result_for_session(STORAGE_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    fn resolve_normal_bridge_target(
        &self,
        tool: &str,
        session_id: &str,
        window_hwnd_param: Option<i64>,
        cdp_target_id_param: Option<&str>,
    ) -> Result<(i64, String), ErrorData> {
        let (window_hwnd, cdp_target_id) = self.resolve_cdp_tab_mutation_target(
            tool,
            session_id,
            window_hwnd_param,
            cdp_target_id_param,
        )?;
        if synapse_a11y::endpoint_for_window(window_hwnd).is_some() {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} targets the normal Chrome extension bridge, but window {window_hwnd} exposes a raw CDP debug endpoint; use raw-CDP primitives for a Synapse automation profile"
                ),
            ));
        }
        if !cdp_target_id.starts_with(CHROME_TAB_PREFIX) {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} requires a normal Chrome bridge tab target ({CHROME_TAB_PREFIX}<id>); got {cdp_target_id:?}"
                ),
            ));
        }
        Ok((window_hwnd, cdp_target_id))
    }

    async fn browser_cookies_impl(
        &self,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &BrowserCookiesParams,
    ) -> Result<BrowserCookiesResponse, ErrorData> {
        validate_cookies_params(params)?;
        let bridge_params = json!({
            "operation": params.operation,
            "url": params.url,
            "name": params.name,
            "value": params.value,
            "domain": params.domain,
            "path": params.path,
            "secure": params.secure,
            "httpOnly": params.http_only,
            "sameSite": params.same_site,
            "expiresUnixSeconds": params.expires_unix_seconds,
            "session": params.session,
        });
        let readback = crate::chrome_debugger_bridge::cookies(
            window_hwnd,
            cdp_target_id,
            bridge_params,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "{COOKIES_TOOL} bridge cookies command failed for target {cdp_target_id:?}: {}",
                    error.detail()
                ),
            )
        })?;
        Ok(BrowserCookiesResponse {
            ok: readback_bool(&readback, "ok", true),
            required_foreground: false,
            transport: "chrome_tabs_extension".to_owned(),
            window_hwnd,
            cdp_target_id: cdp_target_id.to_owned(),
            operation: params.operation,
            source_of_truth: "chrome.cookies readback from the owned normal Chrome bridge tab"
                .to_owned(),
            cookie_count: readback_u32(&readback, "cookie_count"),
            affected_count: readback_u32(&readback, "affected_count"),
            readback,
        })
    }

    async fn browser_storage_impl(
        &self,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &BrowserStorageParams,
    ) -> Result<BrowserStorageResponse, ErrorData> {
        let bridge_params = json!({
            "operation": params.operation,
            "store": params.store,
            "key": params.key,
            "value": params.value,
            "state": params.state,
            "includeSessionStorage": params.include_session_storage,
            "clearBeforeLoad": params.clear_before_load,
            "url": params.url,
        });
        let readback = crate::chrome_debugger_bridge::storage_state(
            window_hwnd,
            cdp_target_id,
            bridge_params,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "{STORAGE_TOOL} bridge storageState command failed for target {cdp_target_id:?}: {}",
                    error.detail()
                ),
            )
        })?;
        let result = readback.get("result").cloned().unwrap_or(Value::Null);
        let storage_state = readback
            .get("storage_state")
            .filter(|value| !value.is_null())
            .cloned();
        Ok(BrowserStorageResponse {
            ok: readback_bool(&readback, "ok", true),
            required_foreground: false,
            transport: "chrome_tabs_extension".to_owned(),
            window_hwnd,
            cdp_target_id: cdp_target_id.to_owned(),
            operation: params.operation,
            store: params.store,
            source_of_truth:
                "chrome.scripting local/session storage readback plus chrome.cookies storageState"
                    .to_owned(),
            item_count: result
                .get("items")
                .and_then(Value::as_array)
                .map(|items| u32::try_from(items.len()).unwrap_or(u32::MAX))
                .unwrap_or_else(|| readback_u32(&result, "local_after_count")),
            origin_count: result
                .get("origin_count")
                .and_then(Value::as_u64)
                .map(|value| u32::try_from(value).unwrap_or(u32::MAX))
                .unwrap_or(0),
            readback,
            storage_state,
        })
    }
}

fn validate_cookies_params(params: &BrowserCookiesParams) -> Result<(), ErrorData> {
    if matches!(params.operation, BrowserCookiesOperation::Set)
        && params.name.as_deref().unwrap_or_default().trim().is_empty()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_cookies operation=set requires non-empty name",
        ));
    }
    Ok(())
}

fn validate_storage_params(params: &BrowserStorageParams) -> Result<(), ErrorData> {
    if matches!(params.operation, BrowserStorageOperation::Set)
        && params.key.as_deref().unwrap_or_default().trim().is_empty()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_storage operation=set requires non-empty key",
        ));
    }
    if matches!(params.operation, BrowserStorageOperation::LoadState) && params.state.is_none() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_storage operation=load_state requires state",
        ));
    }
    Ok(())
}

fn readback_bool(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn readback_u32(value: &Value, key: &str) -> u32 {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| u32::try_from(value).unwrap_or(u32::MAX))
        .unwrap_or(0)
}
