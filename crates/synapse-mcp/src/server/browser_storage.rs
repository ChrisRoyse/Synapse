//! Cookie and storage-state tools for the debugger-free normal Chrome bridge.

use super::{
    ErrorData, Json, Parameters, SynapseService,
    m1_tools::{cdp_target_id_audit_ref, require_target_session_id},
    mcp_error, tool, tool_router,
};
use rmcp::{RoleServer, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use synapse_core::error_codes;

use crate::server::url_redaction::redact_url_fields_for_public_readback;

const COOKIES_TOOL: &str = "browser_cookies";
const STORAGE_TOOL: &str = "browser_storage";
const CHROME_TAB_PREFIX: &str = "chrome-tab:";
const REDACTION_POLICY: &str = "browser_storage_secret_value_and_url_v2";
const REDACTED_VALUE: &str = "[redacted]";

mod load_state_validation;

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
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
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
    pub redaction_policy: String,
    pub redacted_value_count: u32,
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
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
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
    pub redaction_policy: String,
    pub redacted_value_count: u32,
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
        if matches!(
            params.operation,
            BrowserCookiesOperation::Set | BrowserCookiesOperation::Clear
        ) {
            super::operator_panic_boundary::ensure_mcp_mutation(
                "browser_cookies_before_set_or_clear",
            )?;
        }
        let mut readback = crate::chrome_debugger_bridge::cookies(
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
        if matches!(
            params.operation,
            BrowserCookiesOperation::Set | BrowserCookiesOperation::Clear
        ) {
            super::operator_panic_boundary::ensure_mcp_mutation(
                "browser_cookies_after_set_or_clear",
            )?;
        }
        let redacted_value_count = redact_browser_secret_values(&mut readback)
            + u32::try_from(redact_url_fields_for_public_readback(&mut readback))
                .unwrap_or(u32::MAX);
        log_redaction_summary(COOKIES_TOOL, cdp_target_id, redacted_value_count);
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
            redaction_policy: REDACTION_POLICY.to_owned(),
            redacted_value_count,
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
        if matches!(
            params.operation,
            BrowserStorageOperation::Set
                | BrowserStorageOperation::Clear
                | BrowserStorageOperation::LoadState
        ) {
            super::operator_panic_boundary::ensure_mcp_mutation(
                "browser_storage_before_page_state_mutation",
            )?;
        }
        let mut readback = crate::chrome_debugger_bridge::storage_state(
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
        if matches!(
            params.operation,
            BrowserStorageOperation::Set
                | BrowserStorageOperation::Clear
                | BrowserStorageOperation::LoadState
        ) {
            super::operator_panic_boundary::ensure_mcp_mutation(
                "browser_storage_after_page_state_mutation",
            )?;
        }
        let redacted_value_count = redact_browser_secret_values(&mut readback)
            + u32::try_from(redact_url_fields_for_public_readback(&mut readback))
                .unwrap_or(u32::MAX);
        normalize_keyed_storage_get_result_value(
            &mut readback,
            params.operation,
            params.key.as_deref(),
        );
        log_redaction_summary(STORAGE_TOOL, cdp_target_id, redacted_value_count);
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
            redaction_policy: REDACTION_POLICY.to_owned(),
            redacted_value_count,
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
    if matches!(params.operation, BrowserStorageOperation::LoadState) {
        load_state_validation::validate_load_state_params(params)?;
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

fn redact_browser_secret_values(value: &mut Value) -> u32 {
    match value {
        Value::Array(items) => items.iter_mut().map(redact_browser_secret_values).sum(),
        Value::Object(fields) => redact_browser_secret_fields(fields),
        _ => 0,
    }
}

fn normalize_keyed_storage_get_result_value(
    readback: &mut Value,
    operation: BrowserStorageOperation,
    key: Option<&str>,
) {
    if !matches!(operation, BrowserStorageOperation::Get) {
        return;
    }
    let Some(key) = key else {
        return;
    };
    let Some(result) = readback.get_mut("result").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(items) = result.get("items").and_then(Value::as_array) else {
        return;
    };
    let Some(item) = items
        .iter()
        .find(|item| item.get("name").and_then(Value::as_str) == Some(key))
        .and_then(Value::as_object)
        .cloned()
    else {
        return;
    };
    for field in [
        "value",
        "value_kind",
        "value_len",
        "value_redacted",
        "value_sha256",
        "redaction_policy",
    ] {
        if let Some(value) = item.get(field).cloned() {
            result.insert(field.to_owned(), value);
        }
    }
}

fn redact_browser_secret_fields(fields: &mut Map<String, Value>) -> u32 {
    let mut redacted = 0;
    if let Some(raw_value) = fields.get("value").cloned() {
        let already_placeholder = raw_value.as_str() == Some(REDACTED_VALUE);
        if !already_placeholder {
            if fields
                .get("value_redacted")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                tracing::error!(
                    code = "BROWSER_STORAGE_PRE_REDACTED_VALUE_INCONSISTENT",
                    "browser storage payload claimed value_redacted=true while value still contained non-placeholder data; raw value suppressed"
                );
            }
            let evidence = secret_value_evidence(&raw_value);
            fields.insert("value".to_owned(), Value::String(REDACTED_VALUE.to_owned()));
            fields.insert("value_redacted".to_owned(), Value::Bool(true));
            fields.insert(
                "value_len".to_owned(),
                Value::Number(serde_json::Number::from(evidence.value_len)),
            );
            fields.insert(
                "value_sha256".to_owned(),
                Value::String(evidence.value_sha256),
            );
            fields.insert("value_kind".to_owned(), Value::String(evidence.value_kind));
            fields.insert(
                "redaction_policy".to_owned(),
                Value::String(REDACTION_POLICY.to_owned()),
            );
            redacted += 1;
        }
    }
    redacted
        + fields
            .values_mut()
            .map(redact_browser_secret_values)
            .sum::<u32>()
}

#[derive(Debug, Eq, PartialEq)]
struct SecretValueEvidence {
    value_len: u64,
    value_sha256: String,
    value_kind: String,
}

fn secret_value_evidence(value: &Value) -> SecretValueEvidence {
    let (value_kind, bytes) = match value {
        Value::String(text) => ("string".to_owned(), text.as_bytes().to_vec()),
        Value::Null => ("null".to_owned(), b"null".to_vec()),
        Value::Bool(flag) => ("bool".to_owned(), flag.to_string().into_bytes()),
        Value::Number(number) => ("number".to_owned(), number.to_string().into_bytes()),
        Value::Array(_) => (
            "array".to_owned(),
            serialized_value_bytes_for_redaction(value),
        ),
        Value::Object(_) => (
            "object".to_owned(),
            serialized_value_bytes_for_redaction(value),
        ),
    };
    SecretValueEvidence {
        value_len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        value_sha256: format!("sha256:{}", sha256_hex(&bytes)),
        value_kind,
    }
}

fn serialized_value_bytes_for_redaction(value: &Value) -> Vec<u8> {
    match serde_json::to_vec(value) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(
                code = "BROWSER_STORAGE_REDACTION_SERIALIZE_FAILED",
                error = %error,
                "failed to serialize non-string browser storage value for redaction evidence; raw value suppressed"
            );
            b"serialization_failed".to_vec()
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut encoded, "{byte:02x}");
    }
    encoded
}

fn log_redaction_summary(tool: &str, cdp_target_id: &str, redacted_value_count: u32) {
    tracing::info!(
        code = "BROWSER_STORAGE_VALUES_REDACTED",
        tool,
        cdp_target_id,
        redacted_value_count,
        redaction_policy = REDACTION_POLICY,
        "redacted browser cookie/storage values from MCP output"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redacts_cookie_and_storage_values_for_save_state_payload() {
        let raw_cookie = "auth-cookie-secret-1437";
        let waf_cookie = "__cf-bm-secret-1437";
        let stripe_cookie = "stripe-session-secret-1437";
        let jwt_value = "eyJhbGciOiJIUzI1NiJ9.issue1437.payload";
        let session_value = "session-storage-secret-1437";
        let mut readback = json!({
            "ok": true,
            "storage_state": {
                "cookies": [
                    {
                        "name": "auth_session",
                        "value": raw_cookie,
                        "domain": "example.test",
                        "path": "/",
                        "expires": -1,
                        "httpOnly": true,
                        "secure": true,
                        "sameSite": "lax"
                    },
                    {
                        "name": "__cf_bm",
                        "value": waf_cookie,
                        "domain": "example.test",
                        "path": "/",
                        "httpOnly": true,
                        "secure": true,
                        "sameSite": "no_restriction"
                    },
                    {
                        "name": "stripe_session",
                        "value": stripe_cookie,
                        "domain": "example.test",
                        "path": "/checkout",
                        "httpOnly": false,
                        "secure": true,
                        "sameSite": "strict"
                    }
                ],
                "origins": [
                    {
                        "origin": "https://example.test",
                        "localStorage": [
                            {"name": "jwt", "value": jwt_value}
                        ],
                        "sessionStorage": [
                            {"name": "session_token", "value": session_value}
                        ]
                    }
                ]
            },
            "result": {
                "storage_state": {
                    "cookies": [
                        {
                            "name": "auth_session",
                            "value": raw_cookie,
                            "domain": "example.test",
                            "path": "/",
                            "httpOnly": true,
                            "secure": true,
                            "sameSite": "lax"
                        }
                    ],
                    "origins": [
                        {
                            "origin": "https://example.test",
                            "localStorage": [
                                {"name": "jwt", "value": jwt_value}
                            ],
                            "sessionStorage": [
                                {"name": "session_token", "value": session_value}
                            ]
                        }
                    ]
                }
            },
            "frame_results": [
                {
                    "result": {
                        "origin": "https://example.test",
                        "localStorage": [
                            {"name": "jwt", "value": jwt_value}
                        ],
                        "sessionStorage": [
                            {"name": "session_token", "value": session_value}
                        ]
                    }
                }
            ]
        });

        let redacted_count = redact_browser_secret_values(&mut readback);
        assert_eq!(redacted_count, 10);
        let serialized = serialize_json(&readback);
        for raw in [
            raw_cookie,
            waf_cookie,
            stripe_cookie,
            jwt_value,
            session_value,
        ] {
            assert!(
                !serialized.contains(raw),
                "raw browser storage secret remained in sanitized payload: {raw}"
            );
        }

        let cookie = object_at(&readback, "/storage_state/cookies/0");
        assert_eq!(
            cookie.get("name").and_then(Value::as_str),
            Some("auth_session")
        );
        assert_eq!(
            cookie.get("domain").and_then(Value::as_str),
            Some("example.test")
        );
        assert_eq!(cookie.get("path").and_then(Value::as_str), Some("/"));
        assert_eq!(cookie.get("httpOnly").and_then(Value::as_bool), Some(true));
        assert_eq!(cookie.get("secure").and_then(Value::as_bool), Some(true));
        assert_eq!(cookie.get("sameSite").and_then(Value::as_str), Some("lax"));
        assert_redacted_value(cookie, raw_cookie);

        let local = object_at(&readback, "/storage_state/origins/0/localStorage/0");
        assert_eq!(local.get("name").and_then(Value::as_str), Some("jwt"));
        assert_redacted_value(local, jwt_value);

        let session = object_at(&readback, "/storage_state/origins/0/sessionStorage/0");
        assert_eq!(
            session.get("name").and_then(Value::as_str),
            Some("session_token")
        );
        assert_redacted_value(session, session_value);
    }

    #[test]
    fn redacts_empty_string_with_hash_evidence() {
        let mut readback = json!({
            "items": [
                {"name": "empty_cookie", "value": ""}
            ]
        });

        let redacted_count = redact_browser_secret_values(&mut readback);
        assert_eq!(redacted_count, 1);
        let item = object_at(&readback, "/items/0");
        assert_redacted_value(item, "");
    }

    #[test]
    fn keyed_get_result_value_uses_matching_item_evidence() {
        let raw = "issue1482-present-value";
        let mut readback = json!({
            "result": {
                "operation": "get",
                "key": "issue1482_key",
                "value": null,
                "items": [
                    {"name": "issue1482_key", "value": raw}
                ]
            }
        });

        let redacted_count = redact_browser_secret_values(&mut readback);
        assert_eq!(redacted_count, 2);
        normalize_keyed_storage_get_result_value(
            &mut readback,
            BrowserStorageOperation::Get,
            Some("issue1482_key"),
        );

        let result = object_at(&readback, "/result");
        assert_redacted_value(result, raw);
        let item = object_at(&readback, "/result/items/0");
        assert_redacted_value(item, raw);
    }

    #[test]
    fn keyed_get_result_value_stays_null_when_item_absent() {
        let mut readback = json!({
            "result": {
                "operation": "get",
                "key": "issue1482_missing",
                "value": null,
                "items": []
            }
        });

        let redacted_count = redact_browser_secret_values(&mut readback);
        assert_eq!(redacted_count, 1);
        normalize_keyed_storage_get_result_value(
            &mut readback,
            BrowserStorageOperation::Get,
            Some("issue1482_missing"),
        );

        let result = object_at(&readback, "/result");
        assert_redacted_value_kind(
            result,
            "null",
            4,
            "sha256:74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b",
        );
    }

    #[test]
    fn whole_store_get_does_not_copy_item_to_result_value() {
        let raw = "issue1482-whole-store";
        let mut readback = json!({
            "result": {
                "operation": "get",
                "items": [
                    {"name": "issue1482_key", "value": raw}
                ]
            }
        });

        let redacted_count = redact_browser_secret_values(&mut readback);
        assert_eq!(redacted_count, 1);
        normalize_keyed_storage_get_result_value(&mut readback, BrowserStorageOperation::Get, None);

        let result = object_at(&readback, "/result");
        assert!(result.get("value_kind").is_none());
        let item = object_at(&readback, "/result/items/0");
        assert_redacted_value(item, raw);
    }

    #[test]
    fn redacts_non_string_value_without_serializing_raw_payload() {
        let nested_secret = "nested-secret-1437";
        let mut readback = json!({
            "items": [
                {"name": "object_value", "value": {"nested": nested_secret}}
            ]
        });

        let redacted_count = redact_browser_secret_values(&mut readback);
        assert_eq!(redacted_count, 1);
        let serialized = serialize_json(&readback);
        assert!(!serialized.contains(nested_secret));
        let item = object_at(&readback, "/items/0");
        assert_eq!(
            item.get("value_kind").and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(item.get("value_len").and_then(Value::as_u64), Some(31));
        assert_eq!(
            item.get("value_sha256").and_then(Value::as_str),
            Some("sha256:7127d4b7883b88bb832613427ca9d416edc60cd9ddb20b3da407c519cb4016a9")
        );
    }

    #[test]
    fn redacts_inconsistent_pre_redacted_marker() {
        let raw_cookie = "pre-redacted-marker-with-raw-secret-1437";
        let mut readback = json!({
            "cookies": [
                {
                    "name": "auth_session",
                    "value": raw_cookie,
                    "value_redacted": true
                }
            ]
        });

        let redacted_count = redact_browser_secret_values(&mut readback);
        assert_eq!(redacted_count, 1);
        let serialized = serialize_json(&readback);
        assert!(!serialized.contains(raw_cookie));
        let cookie = object_at(&readback, "/cookies/0");
        assert_redacted_value(cookie, raw_cookie);
    }

    fn assert_redacted_value(fields: &Map<String, Value>, raw: &str) {
        assert_eq!(
            fields.get("value").and_then(Value::as_str),
            Some(REDACTED_VALUE)
        );
        assert_eq!(
            fields.get("value_redacted").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            fields.get("redaction_policy").and_then(Value::as_str),
            Some(REDACTION_POLICY)
        );
        assert_eq!(
            fields.get("value_kind").and_then(Value::as_str),
            Some("string")
        );
        assert_eq!(
            fields.get("value_len").and_then(Value::as_u64),
            Some(u64::try_from(raw.len()).unwrap_or(u64::MAX))
        );
        assert_eq!(
            fields.get("value_sha256").and_then(Value::as_str),
            Some(format!("sha256:{}", sha256_hex(raw.as_bytes())).as_str())
        );
    }

    fn assert_redacted_value_kind(fields: &Map<String, Value>, kind: &str, len: u64, sha256: &str) {
        assert_eq!(
            fields.get("value").and_then(Value::as_str),
            Some(REDACTED_VALUE)
        );
        assert_eq!(
            fields.get("value_redacted").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            fields.get("redaction_policy").and_then(Value::as_str),
            Some(REDACTION_POLICY)
        );
        assert_eq!(fields.get("value_kind").and_then(Value::as_str), Some(kind));
        assert_eq!(fields.get("value_len").and_then(Value::as_u64), Some(len));
        assert_eq!(
            fields.get("value_sha256").and_then(Value::as_str),
            Some(sha256)
        );
    }

    fn object_at<'a>(value: &'a Value, pointer: &str) -> &'a Map<String, Value> {
        match value.pointer(pointer).and_then(Value::as_object) {
            Some(object) => object,
            None => panic!("missing object at {pointer}: {value}"),
        }
    }

    fn serialize_json(value: &Value) -> String {
        match serde_json::to_string(value) {
            Ok(serialized) => serialized,
            Err(error) => panic!("failed to serialize test JSON: {error}"),
        }
    }
}
