//! `timeline_redact` MCP tool (#875).
//!
//! The mop for the hygiene scanner: where `hygiene_scan_*` writes flags and
//! `hygiene_report` traces their downstream impact, `timeline_redact` masks the
//! flagged spans in their physical source rows — replacing only the adversarial
//! substring inside a string field with a benign marker, so the row's JSON
//! structure (and therefore mining continuity) is preserved. Hard deletion is
//! the sibling path: `timeline_purge { flag_ids }` removes the rows outright.
//!
//! Both cleaning paths INVALIDATE derived state — every routine, episode, and
//! profile-authoring candidate that was derived from a cleaned poisoned row
//! gets a taint record in the `hygiene/taint/v1/` ledger — and both are
//! audit-logged with flag ids and counts only, never the cleaned content.
//!
//! It lives in its own tool router (merged in `server.rs`) rather than the M3
//! dispatch table, so the cleaning surface stays decoupled from the read-only
//! report and the scanner write tools. The redaction/invalidation logic is
//! owned by [`crate::m3::hygiene`]; this module is the thin MCP wrapper: log,
//! enforce `Read+WriteStorage`, resolve the calling session, run.

use rmcp::{RoleServer, service::RequestContext};

use super::{ErrorData, Json, Parameters, SynapseService, tool, tool_router};

use crate::m3::hygiene::{
    HygieneRedactParams, HygieneRedactResponse, redact, required_permissions_redact,
};

#[tool_router(router = data_cleaning_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Redact (mask) prompt-injection hygiene flags in their physical source rows. Selects flags by explicit flag_ids or by a source_cf/source_key_hex/min_score query, reads each flagged CF_TIMELINE/CF_OBSERVATIONS/CF_OCR_CACHE row, and replaces ONLY the flagged substring inside its string field with a benign marker (default [REDACTED]) — the row's JSON structure is preserved so episode segmentation and routine mining stay continuous. Content-anchored and idempotent: each span is located by its recorded text+SHA-256, so re-running is a safe no-op and offset drift from a prior redaction is tolerated. Cleaning INVALIDATES derived state: every routine/episode/authoring-candidate derived from a cleaned poisoned timeline row gets a taint record in the hygiene/taint/v1 ledger. Returns a per-flag outcome (redacted/already_redacted/stale_source/field_missing/source_missing), the invalidation summary, and the audit row key. dry_run resolves+verifies without mutating any row, taint, or audit. Audit records flag ids and counts only, never content. Use timeline_purge { flag_ids } to hard-delete timeline rows instead of masking."
    )]
    pub async fn timeline_redact(
        &self,
        params: Parameters<HygieneRedactParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<HygieneRedactResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "timeline_redact",
            has_flag_ids = params.0.flag_ids.is_some(),
            source_cf = ?params.0.source_cf,
            min_score = ?params.0.min_score,
            dry_run = params.0.dry_run,
            invalidate = params.0.invalidate,
            "tool.invocation kind=timeline_redact"
        );
        self.require_m3_permissions("timeline_redact", &required_permissions_redact(&params.0))?;
        let by_session = super::context::mcp_session_id_from_request_context(&request_context)?
            .unwrap_or_else(|| "stdio".to_owned());
        let runtime = self.reflex_runtime()?;
        redact(&runtime, &params.0, &by_session).map(Json)
    }
}
