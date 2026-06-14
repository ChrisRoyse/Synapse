//! `hygiene_report` MCP tool (#874).
//!
//! Closes the "flags point at raw rows, but is my *learned state* poisoned?"
//! gap. The injection scanner (#872) writes flag rows that link back to a
//! physical `CF_TIMELINE`/`CF_OBSERVATIONS` source row. This tool joins those
//! flags forward through the real derivation chain — `CF_TIMELINE` →
//! `CF_EPISODES` → `CF_ROUTINES` — so an agent (or the human via the dashboard)
//! can see exactly which episodes and mined routines were derived from flagged
//! content, and judge whether learned state was poisoned.
//!
//! It lives in its own tool router (merged in `server.rs`) rather than the M3
//! dispatch table, so the read-only report surface stays decoupled from the
//! scanner/cleaner write tools. The join logic and types are owned by
//! [`crate::m3::hygiene`]; this module is the thin MCP wrapper: log, enforce
//! `ReadStorage`, run.

use super::{ErrorData, Json, Parameters, SynapseService, tool, tool_router};

use crate::m3::hygiene::{HygieneReportParams, HygieneReportResponse, report};

#[tool_router(router = hygiene_report_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Report prompt-injection hygiene flags with downstream-impact: for each flagged CF_TIMELINE/CF_OBSERVATIONS row, the CF_EPISODES rows it fed and the CF_ROUTINES mined from those episodes (with operator lifecycle). Links flags → physical source rows → derived artifacts so a caller can tell whether learned state was poisoned, not just which raw rows are flagged. Paged (limit/cursor), filterable (source_cf/source_key_hex/min_score/time_range), and honest-empty when clean. Returns scanned_flag/episode/routine row counts so the derivation is verifiably non-truncated."
    )]
    pub async fn hygiene_report(
        &self,
        params: Parameters<HygieneReportParams>,
    ) -> Result<Json<HygieneReportResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "hygiene_report",
            source_cf = ?params.0.source_cf,
            source_key_hex = ?params.0.source_key_hex,
            min_score = params.0.min_score,
            has_time_range = params.0.time_range.is_some(),
            limit = params.0.limit,
            has_cursor = params.0.cursor.is_some(),
            "tool.invocation kind=hygiene_report"
        );
        self.require_m3_permissions(
            "hygiene_report",
            &crate::m3::hygiene::required_permissions_report(&params.0),
        )?;
        let runtime = self.reflex_runtime()?;
        report(&runtime, &params.0).map(Json)
    }
}
