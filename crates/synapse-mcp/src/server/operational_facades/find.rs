//! Fused find-similar on the public `find` tool (#1676).
//!
//! `find` used to be perception-only: it searched the accessibility tree and the
//! detected entities of the *latest observation*. That is recall over what is on
//! screen right now. The fused memory recall built for #1676 — per-slot DiskANN
//! and BM25 recall over the persisted search generation, fused at the rank level
//! by Reciprocal Rank Fusion — shipped behind `storage operation=find_similar`,
//! a maintenance-shaped facade nobody reaches for when they want to find
//! something. This module puts that exact path on the tool whose name already
//! means "find", without a second implementation of the fusion.
//!
//! Doctrine held here:
//! * **One implementation.** The request/response contract and the whole query
//!   path are the shipped [`crate::m3::storage::run_find_similar`]. This module
//!   only gates permissions, moves the work off the runtime, and wraps failures
//!   in the facade's structured-error shape. If the two surfaces ever disagree
//!   it is a compile error, not a silent divergence.
//! * **Fail closed, on the real states.** A fused query fails when the persisted
//!   generation is absent, when a rebuild-required marker is staked, or when the
//!   generation has fallen further behind the vault than the bounded
//!   delta-reconciliation limit (`CALYX_SEARCH_DELTA_REBASE_REQUIRED`). All
//!   surface as the substrate's structured errors naming
//!   `storage operation=search_rebuild` as the exact repair, and none is softened
//!   into an empty result set here. `health` subsystem
//!   `calyx_search_generation` reports which state the vault is actually in
//!   (#1891) instead of leaving the first observer to be whoever calls `find`.
//!
//!   An earlier version of this note claimed the live vault's `index_inverted`
//!   CF was empty and that the BM25 sparse lane had never been built. That was
//!   wrong: the published manifest carries two `sparse_dot` lanes. The real
//!   blockers measured on this host were generation staleness and, for
//!   `query_mode=by_text`, a separate lens-modality defect.
//! * **Not a perception query.** A fused call needs no bound window, so it must
//!   not be routed through the perception target resolution — and it must never
//!   be silently mixed with perception filters. Combining them is refused.

use rmcp::{ErrorData, Json};
use synapse_core::error_codes;

use crate::m1::FindResponse;
use crate::m3::storage::{StorageFindSimilarParams, StorageFindSimilarResponse};
use crate::server::SynapseService;

use super::{FIND_SIMILAR_SOT, FIND_TOOL, errors::facade_delegate_error};

/// Perception filter fields that are meaningless for a fused memory query.
/// Naming them individually keeps the refusal precise instead of "bad params".
const PERCEPTION_ONLY_FIELDS: &[&str] = &[
    "query",
    "role",
    "name_substring",
    "automation_id",
    "scope",
    "limit",
    "in_window",
    "window_hwnd",
];

/// Refuses a request that asks for both perception element recall and fused
/// memory recall in one call.
///
/// There is no defensible merge: the two answer different questions over
/// different sources of truth (the live observation vs. the persisted vault
/// generation), and quietly honoring one while dropping the other is exactly the
/// silent degradation this codebase forbids.
pub(crate) fn reject_mixed_find_request(present_perception_fields: &[&'static str]) -> ErrorData {
    crate::m1::mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!(
            "find received both the fused-memory `similar` block and perception filter(s) {present_perception_fields:?}; \
             these read different sources of truth (persisted Calyx search generation vs. the latest observation) and are never merged. \
             Issue two calls: one with `similar` alone for fused memory recall, one with the perception filters alone for on-screen elements. \
             Perception-only fields are {PERCEPTION_ONLY_FIELDS:?}"
        ),
    )
}

/// Runs one fused find-similar pass for the public `find` tool and wraps it in
/// the `find` response envelope.
///
/// `results` is empty by construction: perception element hits and fused memory
/// hits are different kinds of evidence, and padding one with the other would
/// misrepresent where a result came from. The fused hits live under `similar`,
/// each carrying its per-lens RRF contributions, agree/disagree lenses, verified
/// ledger provenance, and the explicit Ward guard-state readback.
///
/// # Errors
///
/// Returns the facade's structured error when the caller lacks `READ_STORAGE`,
/// when storage/Calyx is not initialized, when the blocking task dies, or when
/// the fused pass itself fails closed (missing or stale persisted generation, a
/// staked rebuild-required marker, a cross-panel example, an unindexable query,
/// or an unavailable temporal policy).
pub(crate) async fn run_fused_find(
    service: &SynapseService,
    spec: StorageFindSimilarParams,
) -> Result<Json<FindResponse>, ErrorData> {
    service.require_m3_permissions(
        FIND_TOOL,
        &crate::m3::storage::required_permissions_find_similar(&spec),
    )?;
    let db = service.m3_storage().map_err(|error| {
        facade_delegate_error(
            FIND_TOOL,
            "similar",
            "calyx_search",
            FIND_SIMILAR_SOT,
            error,
            "repair storage/Calyx initialization and retry find with a `similar` block",
        )
    })?;
    // Fused find opens the persisted per-slot indexes and runs DiskANN/BM25
    // recall: CPU/IO-bound work that must not park a runtime worker serving MCP
    // requests. Offload it exactly as the storage facade does.
    let response: StorageFindSimilarResponse =
        tokio::task::spawn_blocking(move || crate::m3::storage::run_find_similar(&db, &spec))
            .await
            .map_err(|error| {
                facade_delegate_error(
                    FIND_TOOL,
                    "similar",
                    "calyx_search",
                    FIND_SIMILAR_SOT,
                    crate::m1::mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!("fused find blocking task failed to join: {error}"),
                    ),
                    "inspect daemon logs; the fused-find task terminated abnormally",
                )
            })?
            .map_err(|error| {
                facade_delegate_error(
                    FIND_TOOL,
                    "similar",
                    "calyx_search",
                    FIND_SIMILAR_SOT,
                    error,
                    "run storage operation=search_rebuild when the persisted generation is missing, stale, or marked rebuild-required; a generation lagging further behind the vault than the bounded delta-reconciliation limit fails every query with CALYX_SEARCH_DELTA_REBASE_REQUIRED, and `health` subsystem calyx_search_generation reports which of those states the vault is in, or correct query_mode/fusion/example before retrying",
                )
            })?;
    tracing::info!(
        code = "MCP_FIND_FUSED_RECALL",
        kind = FIND_TOOL,
        panel_version = response.panel_version,
        fusion = %response.fusion,
        query_kind = %response.query_kind,
        rrf_k = response.rrf_k,
        consulted_slots = response.consulted_slots.len(),
        hits = response.hits.len(),
        temporal_applied = response.temporal_applied,
        guard_applied = response.guard.applied,
        guard_state = %response.guard.state_code,
        "find fused memory recall completed"
    );
    Ok(Json(FindResponse {
        results: Vec::new(),
        perceived_text_notice: None,
        suspected_injection: Vec::new(),
        similar: Some(response),
    }))
}
