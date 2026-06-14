//! `intent_current` MCP tool (#854) — its own router, merged in `server.rs`.
//!
//! Kept in a self-contained server submodule (not folded into `m3_tools.rs`)
//! so the tool surface composes additively without contending on the shared
//! M3 tool router. The handler is a thin wrapper: permission gate, structured
//! log line, then delegate to [`crate::m3::intent::current_intents`], which
//! owns the storage reads and the call into the pure matcher.

use super::{ErrorData, Json, Parameters, SynapseService, tool, tool_router};

use crate::m3::intent::{IntentCurrentParams, IntentCurrentResponse, current_intents};

#[tool_router(router = intent_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Rank the routines the operator appears to be executing RIGHT NOW by prefix-matching the recent activity stream (CF_EPISODES) against mined routines (CF_ROUTINES) joined with operator lifecycle (CF_ROUTINE_STATE). Each candidate carries combined confidence (routine reliability × prefix depth × schedule alignment), the matched observed steps (episode ids resolvable via episode_get), a remaining-step preview, and schedule context. Disabled/archived routines never match; an empty list is the honest 'nothing matches' (no forced top-1). Read-only; pass now_ts_ns to evaluate a past instant (replay)."
    )]
    pub async fn intent_current(
        &self,
        params: Parameters<IntentCurrentParams>,
    ) -> Result<Json<IntentCurrentResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "intent_current",
            now_ts_ns = params.0.now_ts_ns,
            lookback_hours = params.0.lookback_hours,
            min_confidence = params.0.min_confidence,
            max_candidates = params.0.max_candidates,
            include_agent_activity = params.0.include_agent_activity,
            "tool.invocation kind=intent_current"
        );
        self.require_m3_permissions(
            "intent_current",
            &crate::m3::intent::required_permissions(&params.0),
        )?;
        let db = self.m3_storage()?;
        current_intents(&db, &params.0).map(Json)
    }
}
