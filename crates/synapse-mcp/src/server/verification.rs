//! Verification Inbox — first slice (#1345).
//!
//! Lets an agent read verification/OTP codes out of the user's already-logged-in
//! web surface (e.g. Gmail) through the existing Chrome bridge — no Twilio / paid
//! SIM / paid SaaS. This slice reuses `browser_content` (debugger-free normal
//! Chrome bridge page read) on the session's bound tab, runs a keyword-gated OTP
//! extractor over the page text, journals every read to a CF_KV audit
//! (`verification/audit/v1/...`, masked codes only), and returns the extracted
//! codes with surrounding context so the agent can pick the right one by service.
//!
//! Providers (Gmail/Outlook/Messages/Voice) and the binding/poll tools are
//! follow-on slices; this is the Gmail-via-bridge read + audit backbone.

use std::sync::atomic::{AtomicU64, Ordering};

use rmcp::{RoleServer, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use synapse_storage::{Db, cf};

use super::{ErrorData, Json, Parameters, SynapseService, tool, tool_router};
use crate::m1::{CdpTargetInfoParams, mcp_error};
use crate::server::url_redaction::redact_url_for_public_readback;

const AUDIT_PREFIX: &str = "verification/audit/v1/";
const BINDING_PREFIX: &str = "verification/binding/v1/";
const AUDIT_SCHEMA: u32 = 1;
const BINDING_SCHEMA: u32 = 1;
const MAX_CODES: usize = 25;
static AUDIT_SEQ: AtomicU64 = AtomicU64::new(0);

fn validate_persisted_verification_hwnd(tool: &str, hwnd: i64) -> Result<(), ErrorData> {
    if crate::m1::window_hwnd_shape_is_canonical(hwnd) {
        return Ok(());
    }
    tracing::error!(
        code = synapse_core::error_codes::STORAGE_CORRUPTED,
        tool,
        field = "window_hwnd",
        actual_value = hwnd,
        accepted_range = "1..=u32::MAX",
        source_of_truth = "CF_KV verification/binding/v1 persisted binding",
        remediation = "delete or repair the named corrupted verification binding, then recreate it through verification operation=bind",
        "persisted verification binding contains a noncanonical HWND"
    );
    Err(mcp_error(
        synapse_core::error_codes::STORAGE_CORRUPTED,
        format!(
            "{tool} persisted verification binding window_hwnd must be in 1..=4294967295; got {hwnd}"
        ),
    ))
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationInboxParams {
    /// Bridge tab to read (`chrome-tab:<id>`). Defaults to this session's bound
    /// CDP target. The tab should already be on the logged-in mail/SMS surface.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND owning the target. Defaults to the session target's window.
    #[serde(default)]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub window_hwnd: Option<i64>,
    /// Logical source label for the audit trail (e.g. `gmail`, `outlook`).
    #[serde(default)]
    pub source: Option<String>,
    /// Max codes to return (default 25).
    #[serde(default)]
    pub max_codes: Option<usize>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationCode {
    /// The extracted code (full value — for the agent to use).
    pub code: String,
    /// `google_g`, `numeric`, or `alphanumeric`.
    pub kind: String,
    /// ~50 chars of page text around the code, for service/sender attribution.
    pub context: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationInboxResponse {
    pub ok: bool,
    pub source: String,
    pub url: String,
    pub title: String,
    pub text_len: usize,
    pub codes: Vec<VerificationCode>,
    /// CF_KV key of the audit row written for this read.
    pub audit_key: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationPollParams {
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub window_hwnd: Option<i64>,
    /// Logical source label for the audit trail (e.g. `gmail`).
    #[serde(default)]
    pub source: Option<String>,
    /// Only return a code whose value or surrounding context contains this
    /// substring (case-insensitive) — e.g. the service name `stripe`. Omit to
    /// return the first code found.
    #[serde(default)]
    pub service: Option<String>,
    /// Total poll budget in ms (default 60000, max 300000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_codes: Option<usize>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationPollResponse {
    pub ok: bool,
    pub matched: bool,
    pub timed_out: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<VerificationCode>,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    pub polls: u32,
    pub elapsed_ms: u64,
    pub audit_key: String,
}

/// Internal result of a single inbox read (not a tool type).
struct VerificationReadOnce {
    url: String,
    title: String,
    text_len: usize,
    codes: Vec<VerificationCode>,
    audit_key: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationAuditParams {
    /// Max rows to return, newest first (default 50).
    #[serde(default)]
    pub max: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationAuditRow {
    pub schema_version: u32,
    pub source: String,
    pub url: String,
    pub title: String,
    /// Masked codes only (e.g. `12***6`) — the audit never stores the raw code.
    pub masked_codes: Vec<String>,
    pub code_count: usize,
    pub read_at_unix_ms: u64,
    pub by_session: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationAuditResponse {
    pub ok: bool,
    pub count: usize,
    pub rows: Vec<VerificationAuditRow>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationBindParams {
    /// Source label to bind (e.g. `gmail`, `outlook`). Required.
    pub source: String,
    /// Bridge tab id (`chrome-tab:<id>`) of the logged-in surface for this source.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND owning that tab.
    #[serde(default)]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub window_hwnd: Option<i64>,
    /// Whether verification_inbox/poll may auto-resolve to this binding (default true).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Optional sender/subject substrings to scope reads (advisory; stored for
    /// future provider use).
    #[serde(default)]
    pub sender_allowlist: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationBinding {
    pub schema_version: u32,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_hwnd: Option<i64>,
    pub enabled: bool,
    #[serde(default)]
    pub sender_allowlist: Vec<String>,
    pub bound_at_unix_ms: u64,
    pub by_session: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationBindResponse {
    pub ok: bool,
    pub binding: VerificationBinding,
    /// Physical CF_KV key written, for state verification.
    pub cf_key: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationSourcesParams {
    #[serde(default)]
    pub max: Option<usize>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationSourcesResponse {
    pub ok: bool,
    pub count: usize,
    pub sources: Vec<VerificationBinding>,
}

#[tool_router(router = verification_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Read verification/OTP codes from this MCP session's bound Chrome tab (Verification Inbox, #1345). Reuses the debugger-free normal Chrome bridge page read (browser_content) on the bound tab — point it at the user's already-logged-in Gmail/Outlook/Messages surface — then runs a keyword-gated extractor (Google G-#### codes, and 4–8 digit / 5–8 alphanumeric codes that appear near words like code/verification/OTP/passcode/confirm/2FA) and returns each code with ~50 chars of surrounding context for service attribution. No Twilio/paid SIM/paid SaaS. Every read is journaled to a CF_KV audit (verification/audit/v1, MASKED codes only) and the audit key is returned. Use verification_audit to inspect the trail."
    )]
    pub async fn verification_inbox(
        &self,
        params: Parameters<VerificationInboxParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<VerificationInboxResponse>, ErrorData> {
        let params = params.0;
        let source = params
            .source
            .clone()
            .unwrap_or_else(|| "unspecified".to_owned());
        let max_codes = params.max_codes.unwrap_or(MAX_CODES).min(200);
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "verification_inbox",
            source = %source,
            "tool.invocation kind=verification_inbox"
        );

        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());

        let read = self
            .verification_read_once(
                params.cdp_target_id.clone(),
                params.window_hwnd,
                &source,
                max_codes,
                &session_id,
                &request_context,
            )
            .await?;

        Ok(Json(VerificationInboxResponse {
            ok: true,
            source,
            url: read.url,
            title: read.title,
            text_len: read.text_len,
            codes: read.codes,
            audit_key: read.audit_key,
        }))
    }

    #[tool(
        description = "Poll the bound Chrome tab until a verification/OTP code arrives, then return it (Verification Inbox, #1345). Re-reads the tab (browser_content) on an exponential backoff (2s→15s) up to timeout_ms (default 60s, max 300s), running the same keyword-clause-gated extractor as verification_inbox; returns the first code whose code or context matches the optional `service` substring (case-insensitive), or the first code found when no service filter is given. Each poll iteration is journaled to the CF_KV audit (masked). Returns matched=false + timed_out=true if no matching code appears in the window. Use for autonomous sign-up/2FA flows: point the tab at the user's logged-in Gmail/Outlook/Messages and poll for the code. No paid SaaS."
    )]
    pub async fn verification_poll(
        &self,
        params: Parameters<VerificationPollParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<VerificationPollResponse>, ErrorData> {
        let params = params.0;
        let source = params
            .source
            .clone()
            .unwrap_or_else(|| "unspecified".to_owned());
        let service = params.service.clone();
        let timeout_ms = params.timeout_ms.unwrap_or(60_000).min(300_000);
        let max_codes = params.max_codes.unwrap_or(MAX_CODES).min(200);
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "verification_poll",
            source = %source,
            service = ?service,
            timeout_ms,
            "tool.invocation kind=verification_poll"
        );
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());

        let started = std::time::Instant::now();
        let mut polls = 0u32;
        let mut backoff_ms = 0u64; // first read immediately
        let mut last_audit_key = String::new();
        loop {
            if backoff_ms > 0 {
                let remaining = timeout_ms.saturating_sub(
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                );
                if remaining == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms.min(remaining)))
                    .await;
            }
            polls += 1;
            let read = self
                .verification_read_once(
                    params.cdp_target_id.clone(),
                    params.window_hwnd,
                    &source,
                    max_codes,
                    &session_id,
                    &request_context,
                )
                .await?;
            last_audit_key = read.audit_key.clone();
            if let Some(code) = verification_match(&read.codes, service.as_deref()) {
                return Ok(Json(VerificationPollResponse {
                    ok: true,
                    matched: true,
                    timed_out: false,
                    code: Some(code),
                    source,
                    service,
                    polls,
                    elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(0),
                    audit_key: last_audit_key,
                }));
            }
            // exponential backoff 2s -> 4s -> 8s -> 15s (cap), per 2FA polling guidance
            backoff_ms = if backoff_ms == 0 {
                2_000
            } else {
                (backoff_ms * 2).min(15_000)
            };
            if u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX) >= timeout_ms {
                break;
            }
        }
        Ok(Json(VerificationPollResponse {
            ok: true,
            matched: false,
            timed_out: true,
            code: None,
            source,
            service,
            polls,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(0),
            audit_key: last_audit_key,
        }))
    }

    /// Shared read: browser_content on the bound tab -> HTML-to-text -> OTP
    /// extraction -> masked CF_KV audit row. Used by verification_inbox and
    /// verification_poll so both share one extraction + audit path.
    async fn verification_read_once(
        &self,
        cdp_target_id: Option<String>,
        window_hwnd: Option<i64>,
        source: &str,
        max_codes: usize,
        session_id: &str,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<VerificationReadOnce, ErrorData> {
        // Auto-resolve from a stored binding when no explicit tab is given, so
        // `verification_inbox source=gmail` finds the bound Gmail tab without a
        // per-call cdp_target_id. Falls back to the session target otherwise.
        let (cdp_target_id, window_hwnd) = if cdp_target_id.is_none() && window_hwnd.is_none() {
            match self.verification_resolve_binding(source)? {
                Some(binding) => (binding.cdp_target_id, binding.window_hwnd),
                None => (None, None),
            }
        } else {
            (cdp_target_id, window_hwnd)
        };
        if let Some(window_hwnd) = window_hwnd {
            crate::m1::validate_window_hwnd_shape("verification", window_hwnd)?;
        }
        // Read the page's VISIBLE TEXT (cross-frame innerText via the bridge's
        // targetInfoPageText), not outerHTML: real webmail (Gmail ~5 MB) is
        // head-heavy and the bridge caps pageContent at 2 MiB, so outerHTML
        // truncates before the inbox body. Visible text is small, head/CSS-free,
        // and holds the codes (recent OTP sits at the top of the inbox) (#1345).
        let info = self
            .cdp_target_info(
                Parameters(CdpTargetInfoParams {
                    window_hwnd,
                    cdp_target_id,
                }),
                request_context.clone(),
            )
            .await?
            .0;
        let text = info
            .page_text
            .and_then(|page_text| page_text.text)
            .unwrap_or_default();
        let mut codes = extract_verification_codes(&text);
        codes.truncate(max_codes);
        let read_at_unix_ms = unix_time_ms_now();
        let masked_codes: Vec<String> = codes.iter().map(|c| mask_code(&c.code)).collect();
        let redacted_url = redact_url_for_public_readback(&info.url);
        let audit_row = VerificationAuditRow {
            schema_version: AUDIT_SCHEMA,
            source: source.to_owned(),
            url: redacted_url.clone(),
            title: info.title.clone(),
            masked_codes,
            code_count: codes.len(),
            read_at_unix_ms,
            by_session: session_id.to_owned(),
        };
        let audit_key = self.verification_write_audit(&audit_row, read_at_unix_ms)?;
        Ok(VerificationReadOnce {
            url: redacted_url,
            title: info.title,
            text_len: text.len(),
            codes,
            audit_key,
        })
    }

    #[tool(
        description = "Read the Verification Inbox audit trail (#1345): the CF_KV journal (verification/audit/v1) of every verification_inbox read — source, tab url/title, MASKED codes, count, read timestamp, and consuming session — newest first. This is the inspectable source of truth for which codes were surfaced to agents; raw codes are never stored."
    )]
    pub async fn verification_audit(
        &self,
        params: Parameters<VerificationAuditParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<VerificationAuditResponse>, ErrorData> {
        let max = params.0.max.unwrap_or(50).min(1000);
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "verification_audit",
            "tool.invocation kind=verification_audit"
        );
        let db = self.verification_db()?;
        let rows = db
            .scan_cf_prefix(cf::CF_KV, AUDIT_PREFIX.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("verification_audit failed to scan audit rows: {error}"),
                )
            })?;
        let mut decoded: Vec<VerificationAuditRow> = Vec::new();
        for (_key, raw) in rows {
            if let Ok(row) = serde_json::from_slice::<VerificationAuditRow>(&raw) {
                decoded.push(row);
            }
        }
        // Keys are zero-padded by timestamp+seq, so scan order is ascending; newest first.
        decoded.reverse();
        decoded.truncate(max);
        Ok(Json(VerificationAuditResponse {
            ok: true,
            count: decoded.len(),
            rows: decoded,
        }))
    }

    #[tool(
        description = "Bind a verification source (e.g. gmail, outlook) to a specific logged-in Chrome tab so verification_inbox/verification_poll can auto-resolve it (#1345). Stores source->{cdp_target_id, window_hwnd, enabled, sender_allowlist} in CF_KV (verification/binding/v1). Multi-user by construction: each user binds their own surfaces. When verification_inbox/poll are called with a source and no explicit cdp_target_id, the enabled binding's tab is used."
    )]
    pub async fn verification_bind(
        &self,
        params: Parameters<VerificationBindParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<VerificationBindResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "verification_bind",
            source = %params.source,
            "tool.invocation kind=verification_bind"
        );
        if params.source.trim().is_empty() {
            return Err(mcp_error(
                synapse_core::error_codes::TOOL_PARAMS_INVALID,
                "verification_bind requires a non-empty source",
            ));
        }
        if let Some(window_hwnd) = params.window_hwnd {
            crate::m1::validate_window_hwnd_shape("verification_bind", window_hwnd)?;
        }
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());
        let binding = VerificationBinding {
            schema_version: BINDING_SCHEMA,
            source: params.source.clone(),
            cdp_target_id: params.cdp_target_id.clone(),
            window_hwnd: params.window_hwnd,
            enabled: params.enabled.unwrap_or(true),
            sender_allowlist: params.sender_allowlist.clone().unwrap_or_default(),
            bound_at_unix_ms: unix_time_ms_now(),
            by_session: session_id,
        };
        let db = self.verification_db()?;
        let key = format!("{BINDING_PREFIX}{}", params.source);
        let encoded = serde_json::to_vec(&binding).map_err(|error| {
            mcp_error(
                synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                format!("verification binding encode failed: {error}"),
            )
        })?;
        db.put_batch(cf::CF_KV, [(key.clone().into_bytes(), encoded)])
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("verification binding persist failed: {error}"),
                )
            })?;
        db.flush().map_err(|error| {
            mcp_error(
                error.code(),
                format!("verification binding flush failed: {error}"),
            )
        })?;
        Ok(Json(VerificationBindResponse {
            ok: true,
            binding,
            cf_key: key,
        }))
    }

    #[tool(
        description = "List bound verification sources and their status (#1345): the CF_KV verification/binding/v1 rows — source, tab/window, enabled, sender_allowlist, bound timestamp, and binding session."
    )]
    pub async fn verification_sources(
        &self,
        params: Parameters<VerificationSourcesParams>,
        _request_context: RequestContext<RoleServer>,
    ) -> Result<Json<VerificationSourcesResponse>, ErrorData> {
        let max = params.0.max.unwrap_or(100).min(1000);
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "verification_sources",
            "tool.invocation kind=verification_sources"
        );
        let db = self.verification_db()?;
        let rows = db
            .scan_cf_prefix(cf::CF_KV, BINDING_PREFIX.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("verification_sources scan failed: {error}"),
                )
            })?;
        let mut sources = Vec::new();
        for (key, raw) in rows {
            let binding = serde_json::from_slice::<VerificationBinding>(&raw).map_err(|error| {
                mcp_error(
                    synapse_core::error_codes::STORAGE_CORRUPTED,
                    format!(
                        "verification_sources could not decode persisted binding key {:?}: {error}",
                        String::from_utf8_lossy(&key)
                    ),
                )
            })?;
            if let Some(window_hwnd) = binding.window_hwnd {
                validate_persisted_verification_hwnd(
                    "verification_sources_persisted_binding",
                    window_hwnd,
                )?;
            }
            sources.push(binding);
            if sources.len() >= max {
                break;
            }
        }
        sources.sort_by(|a, b| a.source.cmp(&b.source));
        Ok(Json(VerificationSourcesResponse {
            ok: true,
            count: sources.len(),
            sources,
        }))
    }

    /// Resolve a source's enabled binding (verification/binding/v1/<source>) for
    /// verification_inbox/poll auto-resolution. An absent exact key returns
    /// `None`; storage, decode, and HWND-shape failures are surfaced rather than
    /// silently masquerading as a missing binding.
    fn verification_resolve_binding(
        &self,
        source: &str,
    ) -> Result<Option<VerificationBinding>, ErrorData> {
        let db = self.verification_db()?;
        let key = format!("{BINDING_PREFIX}{source}");
        let rows = db
            .scan_cf_prefix(cf::CF_KV, key.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("verification binding lookup failed for source {source:?}: {error}"),
                )
            })?;
        for (raw_key, raw) in rows {
            if raw_key.as_slice() != key.as_bytes() {
                continue; // exact-source match only (avoid prefix collisions)
            }
            let binding = serde_json::from_slice::<VerificationBinding>(&raw).map_err(|error| {
                mcp_error(
                    synapse_core::error_codes::STORAGE_CORRUPTED,
                    format!(
                        "verification binding decode failed for source {source:?} key {key:?}: {error}"
                    ),
                )
            })?;
            if let Some(window_hwnd) = binding.window_hwnd {
                validate_persisted_verification_hwnd(
                    "verification_persisted_binding",
                    window_hwnd,
                )?;
            }
            return Ok(binding.enabled.then_some(binding));
        }
        Ok(None)
    }

    fn verification_db(&self) -> Result<std::sync::Arc<Db>, ErrorData> {
        let state = self.m3_state_handle();
        let mut guard = state.lock().map_err(|_error| {
            mcp_error(
                synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned while opening verification storage",
            )
        })?;
        guard
            .ensure_storage()
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    fn verification_write_audit(
        &self,
        row: &VerificationAuditRow,
        read_at_unix_ms: u64,
    ) -> Result<String, ErrorData> {
        let db = self.verification_db()?;
        let seq = AUDIT_SEQ.fetch_add(1, Ordering::Relaxed);
        let key = format!("{AUDIT_PREFIX}{read_at_unix_ms:020}-{seq:08}");
        let encoded = serde_json::to_vec(row).map_err(|error| {
            mcp_error(
                synapse_core::error_codes::TOOL_INTERNAL_ERROR,
                format!("verification audit row encode failed: {error}"),
            )
        })?;
        db.put_batch(cf::CF_KV, [(key.clone().into_bytes(), encoded)])
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("verification audit row persist failed: {error}"),
                )
            })?;
        db.flush().map_err(|error| {
            mcp_error(
                error.code(),
                format!("verification audit row flush failed: {error}"),
            )
        })?;
        Ok(key)
    }
}

fn unix_time_ms_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// First two + masked middle + last char, so the audit proves a code was surfaced
/// without storing it. Short codes collapse to all-stars.
fn mask_code(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    if chars.len() <= 3 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..2].iter().collect();
    let last = chars[chars.len() - 1];
    format!("{head}{}{last}", "*".repeat(chars.len() - 3))
}

/// Strip HTML tags + decode the common entities so the OTP extractor sees page
/// text rather than markup. Deliberately simple (no full HTML parse): the goal is
/// to surface code-bearing text, not to reconstruct the DOM.
const CODE_KEYWORDS: &[&str] = &[
    "code",
    "verification",
    "verify",
    "otp",
    "one-time",
    "one time",
    "passcode",
    "pass code",
    "confirm",
    "2fa",
    "two-factor",
    "two factor",
    "security code",
    "login code",
    "authentication",
    "auth code",
    "access code",
    "pin",
];

/// Keyword-gated OTP/verification-code extraction from page text. Google `G-####`
/// codes are always included; bare numeric (4–8 digits) and alphanumeric (5–8,
/// mixed letter+digit) tokens are included only when a code keyword appears within
/// ~60 chars, which suppresses the timestamps/counts/ids that pollute raw page
/// text. Deduplicated; each carries ~50 chars of context for attribution.
pub fn extract_verification_codes(text: &str) -> Vec<VerificationCode> {
    let lower = text.to_ascii_lowercase();
    let chars: Vec<char> = text.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();
    let n = chars.len();
    let mut out: Vec<VerificationCode> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // A code is associated with a keyword only when the keyword is in the SAME
    // CLAUSE — i.e. no sentence terminator (.!?;: newline) sits between them. This
    // is what distinguishes "your verification code is 481923" (associated) from
    // "...verification code. Order 559001 shipped" (the 559001 belongs to a new
    // sentence). Scans up to 50 chars each side, stopping at the first terminator.
    let is_terminator = |c: char| matches!(c, '.' | '!' | '?' | ';' | '\n' | '\r');
    let keyword_near = |start: usize, end: usize| -> bool {
        // backward clause
        let mut back = String::new();
        let mut k = start;
        let lo = start.saturating_sub(50);
        while k > lo {
            k -= 1;
            if is_terminator(lower_chars[k]) {
                break;
            }
            back.push(lower_chars[k]);
        }
        let back: String = back.chars().rev().collect();
        if CODE_KEYWORDS.iter().any(|kw| back.contains(kw)) {
            return true;
        }
        // forward clause (e.g. "481923 is your login code")
        let mut fwd = String::new();
        let hi = (end + 50).min(n);
        let mut f = end;
        while f < hi {
            if is_terminator(lower_chars[f]) {
                break;
            }
            fwd.push(lower_chars[f]);
            f += 1;
        }
        CODE_KEYWORDS.iter().any(|kw| fwd.contains(kw))
    };
    let context_of = |start: usize, end: usize| -> String {
        // Clause-bounded: expand to the code's own sentence (stop at terminators)
        // so the context captures the preceding sender/service name (e.g.
        // "Stripe: your verification code is 224488") for verification_poll's
        // `service` match, WITHOUT bleeding into the next message's text.
        let mut from = start;
        let lo = start.saturating_sub(70);
        while from > lo && !is_terminator(lower_chars[from - 1]) {
            from -= 1;
        }
        let mut to = end;
        let hi = (end + 40).min(n);
        while to < hi && !is_terminator(lower_chars[to]) {
            to += 1;
        }
        chars[from..to]
            .iter()
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let is_tok = |c: char| c.is_ascii_alphanumeric() || c == '-';

    let mut i = 0;
    while i < n {
        if !chars[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        // token boundary: preceding char must not be alnum/- (so we get whole tokens)
        if i > 0 && is_tok(chars[i - 1]) && chars[i - 1] != '-' {
            // still inside a token start scan; advance to token end
        }
        let start = i;
        let mut j = i;
        while j < n && is_tok(chars[j]) {
            j += 1;
        }
        let token: String = chars[start..j].iter().collect();
        let kind = classify_code(&token);
        if let Some(kind) = kind {
            let gated = kind == "google_g" || keyword_near(start, j);
            if gated && seen.insert(token.clone()) {
                out.push(VerificationCode {
                    code: token.clone(),
                    kind: kind.to_owned(),
                    context: context_of(start, j),
                });
            }
        }
        i = j.max(start + 1);
    }
    out
}

/// Pick the first extracted code matching the optional service filter (the
/// service substring must appear in the code or its surrounding context,
/// case-insensitive). With no filter, returns the first code.
fn verification_match(
    codes: &[VerificationCode],
    service: Option<&str>,
) -> Option<VerificationCode> {
    match service {
        None => codes.first().cloned(),
        Some(service) => {
            let needle = service.to_ascii_lowercase();
            codes
                .iter()
                .find(|c| {
                    c.code.to_ascii_lowercase().contains(&needle)
                        || c.context.to_ascii_lowercase().contains(&needle)
                })
                .cloned()
        }
    }
}

fn classify_code(token: &str) -> Option<&'static str> {
    // Google G-#### style.
    if let Some(rest) = token
        .strip_prefix("G-")
        .or_else(|| token.strip_prefix("g-"))
    {
        if (4..=8).contains(&rest.len()) && rest.chars().all(|c| c.is_ascii_digit()) {
            return Some("google_g");
        }
    }
    if token.contains('-') {
        return None;
    }
    let len = token.chars().count();
    let all_digit = token.chars().all(|c| c.is_ascii_digit());
    if all_digit && (4..=8).contains(&len) {
        // A bare 4-digit 19xx/20xx is almost always a year, not an OTP — drop it.
        if len == 4
            && let Ok(year) = token.parse::<u32>()
            && (1900..=2099).contains(&year)
        {
            return None;
        }
        return Some("numeric");
    }
    let has_digit = token.chars().any(|c| c.is_ascii_digit());
    let has_alpha = token.chars().any(|c| c.is_ascii_alphabetic());
    let all_upper_alnum = token
        .chars()
        .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase());
    if has_digit && has_alpha && all_upper_alnum && (5..=8).contains(&len) {
        return Some("alphanumeric");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_google_g_code_unconditionally() {
        let codes =
            extract_verification_codes("Your Google verification code is G-739212 for sign-in");
        assert!(
            codes
                .iter()
                .any(|c| c.code == "G-739212" && c.kind == "google_g")
        );
    }

    #[test]
    fn numeric_code_only_with_keyword() {
        let with = extract_verification_codes("Your verification code is 481923. Do not share it.");
        assert!(
            with.iter()
                .any(|c| c.code == "481923" && c.kind == "numeric")
        );
        // Bare numbers with no code keyword nearby are NOT treated as codes.
        let without = extract_verification_codes("Order 481923 shipped on 2026 at 14:32 to 90210");
        assert!(without.iter().all(|c| c.code != "481923"));
    }

    #[test]
    fn clause_boundary_excludes_adjacent_sentence_numbers() {
        // The keyword "code." ends one sentence; the order number / year / zip in
        // the NEXT sentence must NOT be picked up as codes.
        let text = "Your verification code is 481923. Order 559001 shipped 2026 to 90210.";
        let got: Vec<String> = extract_verification_codes(text)
            .into_iter()
            .map(|c| c.code)
            .collect();
        assert!(got.contains(&"481923".to_owned()), "got {got:?}");
        assert!(!got.contains(&"559001".to_owned()), "got {got:?}");
        assert!(!got.contains(&"2026".to_owned()), "got {got:?}");
        assert!(!got.contains(&"90210".to_owned()), "got {got:?}");
    }

    #[test]
    fn alphanumeric_code_with_keyword() {
        let codes = extract_verification_codes("Enter passcode A1B2C3 to confirm your account");
        assert!(
            codes
                .iter()
                .any(|c| c.code == "A1B2C3" && c.kind == "alphanumeric")
        );
    }

    #[test]
    fn poll_match_filters_by_service_context() {
        let codes = extract_verification_codes(
            "Stripe: your verification code is 224488. Acme login code: A1B2C3.",
        );
        // service filter matches by surrounding context
        let stripe = verification_match(&codes, Some("stripe")).expect("stripe code");
        assert_eq!(stripe.code, "224488");
        let acme = verification_match(&codes, Some("acme")).expect("acme code");
        assert_eq!(acme.code, "A1B2C3");
        // no filter -> first code
        assert_eq!(verification_match(&codes, None).unwrap().code, "224488");
        // unknown service -> none
        assert!(verification_match(&codes, Some("paypal")).is_none());
    }

    #[test]
    fn masking_hides_the_code() {
        assert_eq!(mask_code("481923"), "48***3");
        assert_eq!(mask_code("G-739212"), "G-*****2");
        assert_eq!(mask_code("99"), "**");
    }
}
