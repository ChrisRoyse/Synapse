//! Token/cost accounting tools (#901).
//!
//! Closes the "spawned agents are cost-opaque" gap. Three things live here:
//!
//! 1. An **operator-editable price table** (`agent_cost_price_put/list/delete`)
//!    stored under `CF_KV`. No external pricing API is ever consulted.
//! 2. A **cost rollup** tool (`agent_cost`) that derives per-agent, per-model,
//!    and fleet token/cost counters by a bounded, budget-guarded scan of the
//!    authoritative `CF_AGENT_TRANSCRIPTS` rows ingested in #900.
//! 3. The honesty contract: an unpriced model yields `unpriced` (model id
//!    surfaced) rather than a guessed number, and Claude's own
//!    `total_cost_usd` is surfaced alongside the locally-computed cost as a
//!    reconciliation cross-check (`source_reported`/`reconciliation_delta`).
//!
//! ## Why counters are derived, not materialized
//!
//! The counters are computed on read by scanning the durable transcript rows
//! — the single source of truth — never maintained as a second incrementally
//! updated copy. That eliminates dual-write drift by construction: a counter
//! *is* the priced sum of the rows it reports, so it reconciles with them
//! exactly (the #901 manual FSV requirement) with no reconciliation job to run.
//!
//! ## Determining one authoritative usage total per session
//!
//! Both CLIs emit usage in shapes where naive summing is wrong; the rules here
//! are verified against the real fixtures (`tests/fixtures/*_real.jsonl`):
//!
//! - **Claude** repeats each assistant message's `usage` on several
//!   consecutive lines (same `message.id`), and the streaming `output_tokens`
//!   on those lines are partial snapshots. The terminal `result` row carries
//!   the authoritative cumulative session totals **and** `total_cost_usd`, so
//!   the result row is the one billable row. (Summing assistant rows
//!   multi-counts and undercounts output.)
//! - **Codex** `turn.completed` reports **cumulative** session totals, so the
//!   maximum across `turn.completed` rows (== the last, since totals are
//!   monotonic) is the session total. (Summing turns over-counts.)
//!
//! ## Per-turn series (#950)
//!
//! With `include_per_turn`, each completed spawn also carries a per-turn cost
//! series reconstructed from the `turn_index`-stamped transcript rows:
//!
//! - **Codex** `turn.completed` rows are cumulative, so the per-turn usage is
//!   the consecutive-`turn_index` delta. The deltas telescope to the spawn
//!   total, so the series reconciles exactly; a non-monotonic cumulative is
//!   corrupt data and is surfaced loudly, never clamped.
//! - **Claude** per-turn is per-message (deduped by distinct message id, the
//!   final streaming line winning). Input/cache are reliable but `output_tokens`
//!   is a partial streaming snapshot, so each turn is flagged `partial_snapshot`
//!   and the series is marked non-reconciling — the authoritative session output
//!   stays the spawn-level `result` total, with the gap surfaced not hidden.
//! - **Local** `local.turn.finished` rows are already per-turn and exact.
//!
//! Exact multi-model attribution from Claude `modelUsage` (#949) is per-spawn;
//! per-turn cost is priced by the spawn's primary model.

use std::collections::{BTreeMap, BTreeSet};

use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use synapse_core::{
    AgentEventKind, AgentEventRecord, AgentTranscriptRecord, BillableUsage, CostBreakdown,
    CostOutcome, MODEL_PRICE_VERSION, ModelPrice, TranscriptModelUsage, TranscriptSource,
    error_codes,
};
use synapse_storage::{
    Db,
    agent_transcripts::{
        AGENT_TRANSCRIPT_TS_INDEX_PREFIX, agent_transcript_spawn_prefix,
        agent_transcript_ts_index_key, agent_transcript_ts_index_lower_bound,
        decode_agent_transcript_key, decode_agent_transcript_ts_index_key_ts,
    },
    cf,
};

use super::{
    ErrorData, Json, Parameters, SynapseService, agent_events::unix_time_ns_now, mcp_error, tool,
    tool_profiles::ToolProfileKind, tool_router,
};

/// `CF_KV` key prefix for operator price rows. Versioned so a future codec can
/// coexist during migration.
const PRICE_KEY_PREFIX: &str = "cost/price/v1/";

/// Upper bound on rows scanned in one `agent_cost` fleet call. Mirrors the
/// routine miner's budget so a truncated rollup is a loud error, never a
/// silently-wrong number.
const MAX_SCAN_ROWS_PER_CALL: usize = 200_000;

/// Rows pulled per `scan_cf_from` page.
const SCAN_CHUNK_ROWS: usize = 4_096;

/// Upper bound on `CF_AGENT_EVENTS` rows scanned to build the spawn→template
/// join map for a `group_by: template` rollup. The journal carries many event
/// kinds (only `SpawnRequested` rows carry template provenance), so it can be
/// larger than the transcript stream; a truncated join would mis-attribute
/// cost, so exhaustion is a loud error, never a silent partial map.
const MAX_EVENT_SCAN_ROWS_PER_CALL: usize = 1_000_000;

/// The reserved group key for cost that could not be attributed to any
/// template/task. Parentheses are not legal in a kebab `template_id`/`task_id`
/// (`[a-z0-9._-]`), so this can never collide with a real id. Surfacing the
/// residual (rather than dropping it) keeps every group rollup reconciling
/// exactly with the fleet total — the FinOps "no unallocated spend hidden"
/// rule.
const UNATTRIBUTED_KEY: &str = "(unattributed)";
const COST_TOOL: &str = "cost";
const COST_SOURCE_OF_TRUTH: &str = "CF_AGENT_TRANSCRIPTS transcript rows + CF_KV cost/price/v1 rows + CF_KV agent-cost/transcript-ts-index/v1 rows";
const COST_TS_INDEX_META_KEY: &str = "agent-cost/transcript-ts-index/v1/__meta";
const COST_TS_INDEX_VERSION: u32 = 1;
const DEFAULT_FLEET_WINDOW_NS: u64 = 7 * 24 * 60 * 60 * 1_000_000_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CostOperation {
    Summarize,
    PriceList,
    PricePut,
    PriceDelete,
}

impl CostOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Summarize => "summarize",
            Self::PriceList => "price_list",
            Self::PricePut => "price_put",
            Self::PriceDelete => "price_delete",
        }
    }

    fn parse(raw: &str) -> Result<Self, ErrorData> {
        match raw {
            "summarize" => Ok(Self::Summarize),
            "price_list" => Ok(Self::PriceList),
            "price_put" => Ok(Self::PricePut),
            "price_delete" => Ok(Self::PriceDelete),
            other => Err(cost_invalid_operation(other)),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CostParams {
    #[schemars(schema_with = "cost_operation_schema")]
    pub operation: String,
    #[serde(default)]
    pub summarize: Option<AgentCostParams>,
    #[serde(default)]
    pub price_list: Option<AgentCostPriceListParams>,
    #[serde(default)]
    pub price_put: Option<AgentCostPricePutParams>,
    #[serde(default)]
    pub price_delete: Option<AgentCostPriceDeleteParams>,
}

fn cost_operation_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "string",
        "enum": ["summarize", "price_list", "price_put", "price_delete"]
    })
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CostResponse {
    pub operation: CostOperation,
    pub source_of_truth: String,
    pub readback_source_of_truth: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summarize: Option<AgentCostResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_list: Option<AgentCostPriceListResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_put: Option<AgentCostPricePutResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_delete: Option<AgentCostPriceDeleteResponse>,
}

// ----------------------------------------------------------------------------
// Price-table parameters and responses
// ----------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentCostPricePutParams {
    /// Model id as it appears on transcript rows (e.g. `claude-fable-5`,
    /// `gpt-5.2`). Trimmed and lower-cased for storage and lookup.
    pub model_id: String,
    /// Optional provider label (`anthropic`, `openai`, `local`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Full (uncached) input price, US dollars per million tokens.
    pub input_usd_per_mtok: f64,
    /// Output price, US dollars per million tokens.
    pub output_usd_per_mtok: f64,
    /// Cache-read (hit) price, US dollars per million tokens.
    #[serde(default)]
    pub cache_read_usd_per_mtok: f64,
    /// Aggregate cache-creation (write) price, US dollars per million tokens.
    /// Used for cache writes with no reported TTL tier and as the fallback for
    /// either tier rate below.
    #[serde(default)]
    pub cache_creation_usd_per_mtok: f64,
    /// Anthropic 5-minute-TTL cache-write price (1.25x base input), US dollars
    /// per million tokens. Omit to fall back to `cache_creation_usd_per_mtok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_5m_usd_per_mtok: Option<f64>,
    /// Anthropic 1-hour-TTL cache-write price (2x base input), US dollars per
    /// million tokens. Omit to fall back to `cache_creation_usd_per_mtok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_1h_usd_per_mtok: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentCostPriceListParams {}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentCostPriceDeleteParams {
    /// Model id to remove from the price table.
    pub model_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KvRowReadback {
    pub cf_name: String,
    pub row_key: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentCostPricePutResponse {
    pub ok: bool,
    pub price: ModelPrice,
    pub storage_readback: KvRowReadback,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentCostPriceListResponse {
    pub ok: bool,
    pub count: usize,
    pub prices: Vec<ModelPrice>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentCostPriceDeleteResponse {
    pub ok: bool,
    pub model_id: String,
    pub existed: bool,
    pub row_key: String,
}

// ----------------------------------------------------------------------------
// Cost rollup parameters and responses
// ----------------------------------------------------------------------------

/// Extra attribution dimension for an `agent_cost` rollup (#951). The per-spawn,
/// per-model and fleet rollups are always returned; requesting one of these adds
/// a derived-on-read join that buckets each spawn's cost by a higher-level
/// identity recorded at spawn time.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentCostGroupBy {
    /// Bucket by the durable agent_template (#909) recorded on each spawn's
    /// `SpawnRequested` journal event. Covers direct template spawns and
    /// task-dispatched spawns alike.
    Template,
    /// Bucket by the durable task (#910) whose attempt bound the spawn
    /// (`TaskAttempt.spawn_id`). Carries the task's dispatch `template_id`.
    Task,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentCostParams {
    /// Restrict the rollup to one spawn (`agent-spawn-*`). Omit for a
    /// fleet-wide rollup over every spawn's transcripts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    /// Explicitly scan all retained transcript rows exactly. Fleet calls without
    /// this flag use a bounded default window instead of accidentally walking
    /// all history.
    #[serde(default)]
    pub all_history: bool,
    /// Lower bound (inclusive) on transcript-row ingestion time, unix ns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ns: Option<u64>,
    /// Upper bound (exclusive) on transcript-row ingestion time, unix ns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ns: Option<u64>,
    /// When true, each completed `per_spawn` entry additionally carries a
    /// per-turn cost series (`turns` + `turns_summary`). Codex turns are exact
    /// cumulative-delta reconstructions that reconcile to the spawn total;
    /// Claude turns expose reliable per-message input/cache with output flagged
    /// as a partial streaming snapshot (the stream carries no authoritative
    /// per-turn output — only the session `result` row does). Off by default so
    /// the common rollup stays compact.
    #[serde(default)]
    pub include_per_turn: bool,
    /// Extra rollup dimensions. Each requested dimension adds a derived-on-read
    /// rollup (`per_template` / `per_task`) that buckets the same per-spawn
    /// costs by template or task. Costs that map to no template/task surface in
    /// an explicit `(unattributed)` bucket so every rollup reconciles exactly
    /// with the fleet total. Omit for the spawn/model/fleet rollups only.
    #[serde(default)]
    pub group_by: Vec<AgentCostGroupBy>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSpawnCost {
    pub spawn_id: String,
    /// Which CLI produced the transcript (`claude_stream_json` / `codex_exec_json`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Primary model id used to price this spawn's authoritative usage row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `complete` once an authoritative terminal usage row was found;
    /// `no_terminal_usage` while the agent is still running / produced none.
    pub status: String,
    /// Canonical disjoint token usage billed for this spawn.
    pub usage: BillableUsage,
    pub total_tokens: u64,
    /// Locally-computed cost (or `unpriced` with the model id surfaced).
    pub cost: CostOutcome,
    /// Cost the CLI reported for itself (Claude `total_cost_usd`), micro-USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_reported_micro_usd: Option<u64>,
    /// `source_reported - locally_computed` in micro-USD when both exist. A
    /// non-zero delta is a visible signal (e.g. multi-model sessions whose
    /// side-model usage #900 does not yet capture), never silently absorbed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconciliation_delta_micro_usd: Option<i64>,
    /// Line number of the authoritative row, for physical-row verification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authoritative_line_no: Option<u64>,
    /// Per-model breakdown when the authoritative row carried a Claude
    /// `modelUsage` map (#949). Each entry is priced independently; the
    /// spawn-level `cost` is the sum of the priced entries. A single-model
    /// spawn carries exactly one entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<AgentSpawnModelCost>,
    /// Per-turn cost series — present only when `include_per_turn` was set and
    /// the spawn completed. Ordered by `turn_index`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns: Option<Vec<AgentTurnCost>>,
    /// Reconstruction method + reconciliation status for `turns`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns_summary: Option<TurnsSummary>,
}

/// One model's slice of a single spawn's cost, derived from the Claude
/// `modelUsage` breakdown (#949).
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentSpawnModelCost {
    pub model: String,
    pub usage: BillableUsage,
    pub total_tokens: u64,
    pub cost: CostOutcome,
    /// The CLI's own per-model cost (`modelUsage[model].costUSD`), micro-USD.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_reported_micro_usd: Option<u64>,
}

/// Whether a per-turn `output_tokens` figure is exact or a partial snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutputBasis {
    /// Output is exact for this turn (Codex cumulative-delta reconstruction).
    Exact,
    /// Output is a partial streaming snapshot that undercounts the spawn's
    /// authoritative session total (Claude repeats each message's `usage` across
    /// streaming lines and the per-line `output_tokens` is partial). Use the
    /// spawn-level total for billable output; this per-turn output is indicative.
    PartialSnapshot,
}

/// One turn's cost within a spawn.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentTurnCost {
    /// 1-based turn index within the session (from the transcript row's
    /// `turn_index`: Codex `turn.started`, Claude distinct assistant message id).
    pub turn_index: u64,
    /// Line number of the source row this turn was derived from, for
    /// physical-row verification.
    pub line_no: u64,
    /// Canonical disjoint usage attributed to this turn.
    pub usage: BillableUsage,
    pub total_tokens: u64,
    /// This turn priced by the spawn's primary model (or `unpriced`).
    pub cost: CostOutcome,
    /// Whether `usage.output_tokens` is exact or a partial snapshot.
    pub output_basis: TurnOutputBasis,
}

/// How a spawn's per-turn series was reconstructed and whether it reconciles to
/// the spawn's authoritative usage.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TurnsSummary {
    /// `codex_cumulative_delta` or `claude_per_message`.
    pub method: String,
    pub turn_count: usize,
    /// True only when the per-turn usage sums exactly to the spawn's
    /// authoritative usage (Codex). False for Claude, whose per-turn output is a
    /// partial snapshot — `turns_usage_sum` then differs from the spawn total in
    /// output, by design, never silently.
    pub reconciles: bool,
    /// Sum of every turn's usage — lets the caller verify reconciliation against
    /// the spawn-level `usage` directly.
    pub turns_usage_sum: BillableUsage,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentModelCost {
    pub model: String,
    pub priced: bool,
    pub spawns: usize,
    pub usage: BillableUsage,
    pub total_tokens: u64,
    /// Sum of locally-computed cost across spawns using this model. Present
    /// only when the model is priced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub computed_micro_usd: Option<u64>,
    /// Sum of the CLI's own per-model `costUSD` where reported (Claude
    /// `modelUsage`), micro-USD. Lets a multi-model rollup be cross-checked per
    /// model, not just per session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_reported_micro_usd: Option<u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentFleetCost {
    pub spawns_total: usize,
    pub spawns_complete: usize,
    pub spawns_incomplete: usize,
    pub usage: BillableUsage,
    pub total_tokens: u64,
    /// Sum of locally-computed cost across all priced spawns, micro-USD.
    pub computed_micro_usd: u64,
    /// Sum of CLI self-reported cost where available, micro-USD.
    pub source_reported_micro_usd: u64,
    /// Models seen with no price row. Their token throughput is still counted;
    /// their cost is deliberately excluded from `computed_micro_usd`.
    pub unpriced_models: Vec<String>,
}

/// One template's or task's slice of the fleet cost (#951). Built by joining the
/// per-spawn costs to the template/task identity recorded at spawn time, then
/// summing. The sum of every group's `computed_micro_usd` (including the
/// `(unattributed)` bucket) equals `fleet.computed_micro_usd` exactly — the
/// reconciliation invariant manual FSV asserts.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentGroupCost {
    /// The group identity: a `template_id` (for `per_template`), a `task_id`
    /// (for `per_task`), or the reserved `(unattributed)` residual bucket.
    pub key: String,
    /// `false` only for the `(unattributed)` residual bucket — cost with no
    /// template/task id to attribute to (a direct `cli` spawn, or a spawn whose
    /// provenance row is missing).
    pub attributed: bool,
    /// Template versions observed across this group's spawns (`per_template`
    /// only; empty for task groups). A group that ran two template versions
    /// lists both, so a version bump is visible in the cost trend.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub template_versions: Vec<u32>,
    /// The task's dispatch template (`per_task` only) — lets a task rollup be
    /// cross-checked against the matching `per_template` group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    /// Spawns attributed to this group (complete + still-running).
    pub spawns: usize,
    /// Of `spawns`, those with an authoritative terminal usage row (billed).
    pub spawns_complete: usize,
    pub usage: BillableUsage,
    pub total_tokens: u64,
    /// Sum of locally-computed cost across this group's priced spawns, micro-USD.
    pub computed_micro_usd: u64,
    /// Sum of CLI self-reported cost where available, micro-USD.
    pub source_reported_micro_usd: u64,
    /// Models seen in this group with no price row (tokens counted, cost
    /// excluded) — the same honesty marker the fleet rollup carries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unpriced_models: Vec<String>,
    /// The spawn ids folded into this group, so a caller can verify the join
    /// against the physical `SpawnRequested`/task rows during manual FSV.
    pub spawn_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentCostResponse {
    pub ok: bool,
    pub now_ns: u64,
    /// How transcript rows were physically reached.
    pub query_strategy: String,
    /// True when the server supplied the default fleet window.
    pub default_window_applied: bool,
    /// Honest completeness marker. This is never "partial".
    pub completeness: String,
    /// Timestamp-index schema version used for indexed fleet windows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_index_version: Option<u32>,
    /// `CF_KV` timestamp-index rows examined before exact transcript readback.
    pub scanned_index_rows: u64,
    /// Echoes the time window applied to transcript rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until_ns: Option<u64>,
    /// Total transcript rows scanned — the honesty figure that lets a caller
    /// confirm the rollup was not truncated.
    pub scanned_rows: u64,
    /// `CF_AGENT_EVENTS` rows scanned to build the spawn→template join. `0` when
    /// `group_by` did not request a template rollup.
    pub scanned_event_rows: u64,
    pub fleet: AgentFleetCost,
    pub per_model: Vec<AgentModelCost>,
    pub per_spawn: Vec<AgentSpawnCost>,
    /// Per-template rollup. Present only when `group_by` includes `template`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_template: Option<Vec<AgentGroupCost>>,
    /// Per-task rollup. Present only when `group_by` includes `task`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_task: Option<Vec<AgentGroupCost>>,
}

// ----------------------------------------------------------------------------
// Tools
// ----------------------------------------------------------------------------

#[tool_router(router = agent_cost_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Upsert a model's prices into the local, operator-editable cost table (no external pricing API). Rates are US dollars per million tokens; an unpriced model is reported as `unpriced` by agent_cost rather than guessed. Returns the stored row with an exact CF_KV readback."
    )]
    pub async fn agent_cost_price_put(
        &self,
        params: Parameters<AgentCostPricePutParams>,
    ) -> Result<Json<AgentCostPricePutResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_cost_price_put",
            "tool.invocation kind=agent_cost_price_put"
        );
        self.agent_cost_price_put_impl(params.0).map(Json)
    }

    #[tool(
        description = "List every model price row in the local cost table, ordered by model id."
    )]
    pub async fn agent_cost_price_list(
        &self,
        _params: Parameters<AgentCostPriceListParams>,
    ) -> Result<Json<AgentCostPriceListResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_cost_price_list",
            "tool.invocation kind=agent_cost_price_list"
        );
        self.agent_cost_price_list_impl().map(Json)
    }

    #[tool(
        description = "Delete a model's price row from the local cost table. Reports whether a row existed; never errors on an unknown model id."
    )]
    pub async fn agent_cost_price_delete(
        &self,
        params: Parameters<AgentCostPriceDeleteParams>,
    ) -> Result<Json<AgentCostPriceDeleteResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_cost_price_delete",
            "tool.invocation kind=agent_cost_price_delete"
        );
        self.agent_cost_price_delete_impl(params.0).map(Json)
    }

    #[tool(
        description = "Roll up token usage and cost from durable agent transcripts: per-spawn, per-model, and fleet totals. Counters are derived by a budget-guarded scan of CF_AGENT_TRANSCRIPTS so they reconcile exactly with physical rows (scanned_rows is returned). Unpriced models surface as `unpriced`; Claude's own total_cost is surfaced for cross-check."
    )]
    pub async fn agent_cost(
        &self,
        params: Parameters<AgentCostParams>,
    ) -> Result<Json<AgentCostResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_cost",
            "tool.invocation kind=agent_cost"
        );
        self.agent_cost_impl(params.0).map(Json)
    }
}

#[tool_router(router = cost_facade_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Public cost facade for the <=40 MCP surface. operation=summarize rolls up token/cost from CF_AGENT_TRANSCRIPTS; price_list reads CF_KV price rows; price_put/price_delete are maintenance-gated mutations with exact CF_KV readback."
    )]
    pub async fn cost(
        &self,
        params: Parameters<CostParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<CostResponse>, ErrorData> {
        let operation = validate_cost_params(&params.0)?;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = COST_TOOL,
            operation = operation.as_str(),
            "tool.invocation kind=cost"
        );

        match operation {
            CostOperation::Summarize => {
                let summarize = params
                    .0
                    .summarize
                    .ok_or_else(|| cost_missing_spec("summarize"))?;
                let response = self.agent_cost_impl(summarize).map_err(|error| {
                    cost_delegate_error(
                        operation.as_str(),
                        "agent_cost",
                        error,
                        "pass a spawn_id or bounded window, add missing price rows, or repair corrupt transcript/price rows",
                    )
                })?;
                Ok(Json(CostResponse {
                    operation,
                    source_of_truth: COST_SOURCE_OF_TRUTH.to_owned(),
                    readback_source_of_truth: format!(
                        "strategy={} CF_KV_ts_index_rows={} CF_AGENT_TRANSCRIPTS_scanned_rows={} spawns={} models={} unpriced_models={}",
                        response.query_strategy,
                        response.scanned_index_rows,
                        response.scanned_rows,
                        response.fleet.spawns_total,
                        response.per_model.len(),
                        response.fleet.unpriced_models.len()
                    ),
                    summarize: Some(response),
                    price_list: None,
                    price_put: None,
                    price_delete: None,
                }))
            }
            CostOperation::PriceList => {
                let _spec = params
                    .0
                    .price_list
                    .ok_or_else(|| cost_missing_spec("price_list"))?;
                let response = self.agent_cost_price_list_impl().map_err(|error| {
                    cost_delegate_error(
                        operation.as_str(),
                        "agent_cost_price_list",
                        error,
                        "repair corrupt CF_KV cost/price/v1 rows and retry price_list",
                    )
                })?;
                Ok(Json(CostResponse {
                    operation,
                    source_of_truth: COST_SOURCE_OF_TRUTH.to_owned(),
                    readback_source_of_truth: format!(
                        "CF_KV prefix {PRICE_KEY_PREFIX} rows={}",
                        response.count
                    ),
                    summarize: None,
                    price_list: Some(response),
                    price_put: None,
                    price_delete: None,
                }))
            }
            CostOperation::PricePut => {
                require_cost_maintenance_profile(
                    self,
                    &request_context,
                    operation.as_str(),
                    "CF_KV cost/price/v1",
                )?;
                let price_put = params
                    .0
                    .price_put
                    .ok_or_else(|| cost_missing_spec("price_put"))?;
                let response = self.agent_cost_price_put_impl(price_put).map_err(|error| {
                    cost_delegate_error(
                        operation.as_str(),
                        "agent_cost_price_put",
                        error,
                        "fix the price payload and retry; model prices are never guessed",
                    )
                })?;
                Ok(Json(CostResponse {
                    operation,
                    source_of_truth: COST_SOURCE_OF_TRUTH.to_owned(),
                    readback_source_of_truth: format!(
                        "{} {} sha256={}",
                        response.storage_readback.cf_name,
                        response.storage_readback.row_key,
                        response.storage_readback.value_sha256
                    ),
                    summarize: None,
                    price_list: None,
                    price_put: Some(response),
                    price_delete: None,
                }))
            }
            CostOperation::PriceDelete => {
                require_cost_maintenance_profile(
                    self,
                    &request_context,
                    operation.as_str(),
                    "CF_KV cost/price/v1",
                )?;
                let price_delete = params
                    .0
                    .price_delete
                    .ok_or_else(|| cost_missing_spec("price_delete"))?;
                let response =
                    self.agent_cost_price_delete_impl(price_delete)
                        .map_err(|error| {
                            cost_delegate_error(
                                operation.as_str(),
                                "agent_cost_price_delete",
                                error,
                                "pass a valid model_id and retry price_delete",
                            )
                        })?;
                Ok(Json(CostResponse {
                    operation,
                    source_of_truth: COST_SOURCE_OF_TRUTH.to_owned(),
                    readback_source_of_truth: format!(
                        "CF_KV {} existed_before={}",
                        response.row_key, response.existed
                    ),
                    summarize: None,
                    price_list: None,
                    price_put: None,
                    price_delete: Some(response),
                }))
            }
        }
    }
}

#[derive(Clone, Debug)]
struct AgentCostQueryPlan {
    params: AgentCostParams,
    now_ns: u64,
    query_strategy: String,
    default_window_applied: bool,
    explicit_all_history: bool,
    use_timestamp_index: bool,
}

impl AgentCostQueryPlan {
    fn from_params(mut params: AgentCostParams) -> Result<Self, ErrorData> {
        let now_ns = unix_time_ns_now();
        if params.all_history
            && (params.spawn_id.is_some() || params.since_ns.is_some() || params.until_ns.is_some())
        {
            return Err(invalid_params(
                "AGENT_COST_ALL_HISTORY_CONFLICT: all_history=true is mutually exclusive with spawn_id/since_ns/until_ns".to_owned(),
            ));
        }

        let explicit_all_history = params.all_history;
        let mut default_window_applied = false;
        let mut use_timestamp_index = false;
        let query_strategy;
        if params.spawn_id.is_some() {
            query_strategy = "spawn_prefix_scan".to_owned();
        } else if explicit_all_history {
            query_strategy = "explicit_all_history_full_cf_scan_exact".to_owned();
        } else {
            if params.until_ns.is_none() {
                params.until_ns = Some(now_ns);
            }
            if params.since_ns.is_none() {
                let window_end = params.until_ns.unwrap_or(now_ns);
                params.since_ns = Some(window_end.saturating_sub(DEFAULT_FLEET_WINDOW_NS));
                default_window_applied = true;
            }
            query_strategy = "timestamp_index_window_scan".to_owned();
            use_timestamp_index = true;
        }

        if let (Some(since), Some(until)) = (params.since_ns, params.until_ns)
            && since >= until
        {
            return Err(invalid_params(format!(
                "AGENT_COST_RANGE_INVALID: since_ns {since} must be < until_ns {until}"
            )));
        }

        Ok(Self {
            params,
            now_ns,
            query_strategy,
            default_window_applied,
            explicit_all_history,
            use_timestamp_index,
        })
    }
}

impl SynapseService {
    pub(crate) fn dashboard_agent_cost_snapshot(&self) -> Result<AgentCostResponse, ErrorData> {
        // Bounded default window (#1328): an unbounded fleet rollup scans the
        // entire retained CF_AGENT_TRANSCRIPTS and (correctly) fails closed with
        // AGENT_COST_SCAN_BUDGET_EXHAUSTED rather than under-report. This window is
        // necessary but NOT yet sufficient: CF_AGENT_TRANSCRIPTS is spawn-keyed,
        // not time-keyed, so `since_ns` is a post-read filter that cannot prune the
        // scan — the dashboard cost panel still needs a recent-spawn-scoped rollup
        // (enumerate spawns active in the window from CF_AGENT_EVENTS, then scan
        // only those spawn prefixes) or an indexed cost SoT to clear the budget.
        // Tracked in #1328. Explicit MCP agent_cost calls keep the fail-closed contract.
        let since_ns = super::agent_events::unix_time_ns_now()
            .saturating_sub(super::agent_stats::DASHBOARD_ANALYTICS_WINDOW_NS);
        // Enumerate spawns active in the window from the (time-stamped, much
        // smaller) CF_AGENT_EVENTS journal, then roll up cost over ONLY those
        // spawns' transcript prefixes — CF_AGENT_TRANSCRIPTS is spawn-keyed, so a
        // bare since_ns cannot prune the full-table scan (#1328).
        let db = self.agent_cost_db()?;
        let recent_spawn_ids = collect_recent_spawn_ids(&db, since_ns)?;
        let plan = AgentCostQueryPlan::from_params(AgentCostParams {
            spawn_id: None,
            all_history: false,
            since_ns: Some(since_ns),
            until_ns: None,
            include_per_turn: true,
            group_by: vec![AgentCostGroupBy::Template, AgentCostGroupBy::Task],
        })?;
        self.agent_cost_impl_scoped(plan, Some(&recent_spawn_ids))
    }

    fn agent_cost_db(&self) -> Result<std::sync::Arc<Db>, ErrorData> {
        let state = self.m3_state_handle();
        let mut guard = state.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned while opening agent cost storage",
            )
        })?;
        guard
            .ensure_storage()
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    fn agent_cost_price_put_impl(
        &self,
        params: AgentCostPricePutParams,
    ) -> Result<AgentCostPricePutResponse, ErrorData> {
        let model_id = ModelPrice::normalize_id(&params.model_id);
        let price = ModelPrice {
            version: MODEL_PRICE_VERSION,
            model_id,
            provider: params.provider.map(|p| p.trim().to_owned()),
            input_micro_usd_per_mtok: usd_per_mtok_to_micro(
                params.input_usd_per_mtok,
                "input_usd_per_mtok",
            )?,
            output_micro_usd_per_mtok: usd_per_mtok_to_micro(
                params.output_usd_per_mtok,
                "output_usd_per_mtok",
            )?,
            cache_read_micro_usd_per_mtok: usd_per_mtok_to_micro(
                params.cache_read_usd_per_mtok,
                "cache_read_usd_per_mtok",
            )?,
            cache_creation_micro_usd_per_mtok: usd_per_mtok_to_micro(
                params.cache_creation_usd_per_mtok,
                "cache_creation_usd_per_mtok",
            )?,
            cache_creation_5m_micro_usd_per_mtok: params
                .cache_creation_5m_usd_per_mtok
                .map(|usd| usd_per_mtok_to_micro(usd, "cache_creation_5m_usd_per_mtok"))
                .transpose()?,
            cache_creation_1h_micro_usd_per_mtok: params
                .cache_creation_1h_usd_per_mtok
                .map(|usd| usd_per_mtok_to_micro(usd, "cache_creation_1h_usd_per_mtok"))
                .transpose()?,
            updated_ts_ns: unix_time_ns_now(),
        };
        price.validate().map_err(invalid_params)?;

        let row_key = price_row_key(&price.model_id);
        let encoded = serde_json::to_vec(&price).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("serialize model price row: {error}"),
            )
        })?;
        let db = self.agent_cost_db()?;
        db.put_batch_pressure_bypass(cf::CF_KV, [(row_key.as_bytes().to_vec(), encoded)])
            .map_err(|error| {
                mcp_error(error.code(), format!("write price row {row_key}: {error}"))
            })?;
        let storage_readback = readback_exact_kv_row(&db, &row_key)?;
        Ok(AgentCostPricePutResponse {
            ok: true,
            price,
            storage_readback,
        })
    }

    fn agent_cost_price_list_impl(&self) -> Result<AgentCostPriceListResponse, ErrorData> {
        let db = self.agent_cost_db()?;
        let rows = db
            .scan_cf_prefix(cf::CF_KV, PRICE_KEY_PREFIX.as_bytes())
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let mut prices = Vec::with_capacity(rows.len());
        for (key, value) in rows {
            let price: ModelPrice = serde_json::from_slice(&value).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "PRICE_ROW_CORRUPT: key {} failed to decode: {error}",
                        String::from_utf8_lossy(&key)
                    ),
                )
            })?;
            prices.push(price);
        }
        prices.sort_by(|a, b| a.model_id.cmp(&b.model_id));
        Ok(AgentCostPriceListResponse {
            ok: true,
            count: prices.len(),
            prices,
        })
    }

    fn agent_cost_price_delete_impl(
        &self,
        params: AgentCostPriceDeleteParams,
    ) -> Result<AgentCostPriceDeleteResponse, ErrorData> {
        let model_id = ModelPrice::normalize_id(&params.model_id);
        if model_id.is_empty() {
            return Err(invalid_params(
                "MODEL_PRICE_INVALID: model_id must not be empty".to_owned(),
            ));
        }
        let row_key = price_row_key(&model_id);
        let db = self.agent_cost_db()?;
        let existed = get_exact_kv_row(&db, &row_key)?.is_some();
        if existed {
            db.delete_batch(cf::CF_KV, [row_key.as_bytes().to_vec()])
                .map_err(|error| {
                    mcp_error(error.code(), format!("delete price row {row_key}: {error}"))
                })?;
        }
        Ok(AgentCostPriceDeleteResponse {
            ok: true,
            model_id,
            existed,
            row_key,
        })
    }

    fn agent_cost_impl(&self, params: AgentCostParams) -> Result<AgentCostResponse, ErrorData> {
        let plan = AgentCostQueryPlan::from_params(params)?;
        self.agent_cost_impl_scoped(plan, None)
    }

    /// `agent_cost` rollup with an optional restriction to a fixed set of spawn
    /// ids. When `restrict_spawn_ids` is `Some`, only those spawns' transcript
    /// prefixes are scanned (bounded per spawn) instead of the full
    /// CF_AGENT_TRANSCRIPTS. Public fleet windows pass `None` and are pruned by
    /// the timestamp index in the query plan; only explicit all-history scans
    /// use the full CF path.
    fn agent_cost_impl_scoped(
        &self,
        plan: AgentCostQueryPlan,
        restrict_spawn_ids: Option<&[String]>,
    ) -> Result<AgentCostResponse, ErrorData> {
        let params = &plan.params;
        if let (Some(since), Some(until)) = (params.since_ns, params.until_ns)
            && since >= until
        {
            return Err(invalid_params(format!(
                "AGENT_COST_RANGE_INVALID: since_ns {since} must be < until_ns {until}"
            )));
        }
        let db = self.agent_cost_db()?;
        let prices = load_price_table(&db)?;

        // Accumulate per-spawn state from the transcript rows.
        let mut spawns: BTreeMap<String, SpawnAccumulator> = BTreeMap::new();
        let mut scanned_rows: u64 = 0;
        let mut scanned_index_rows: u64 = 0;
        if let Some(spawn_id) = params.spawn_id.as_deref() {
            validate_spawn_id(spawn_id)?;
            let rows = db
                .scan_cf_prefix(
                    cf::CF_AGENT_TRANSCRIPTS,
                    &agent_transcript_spawn_prefix(spawn_id),
                )
                .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            for (key, value) in rows {
                scanned_rows += 1;
                ingest_row(&mut spawns, &key, &value, params.since_ns, params.until_ns)?;
            }
        } else if let Some(spawn_ids) = restrict_spawn_ids {
            // #1328: scan only the window-active spawns' transcript prefixes,
            // not the whole CF. Still budget-guarded so a runaway set fails loud.
            for spawn_id in spawn_ids {
                validate_spawn_id(spawn_id)?;
                if usize::try_from(scanned_rows).unwrap_or(usize::MAX) >= MAX_SCAN_ROWS_PER_CALL {
                    return Err(mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "AGENT_COST_SCAN_BUDGET_EXHAUSTED after {MAX_SCAN_ROWS_PER_CALL} \
                             CF_AGENT_TRANSCRIPTS rows across the window-active spawn set; \
                             narrow the dashboard window — a truncated rollup would under-report cost"
                        ),
                    ));
                }
                let rows = db
                    .scan_cf_prefix(
                        cf::CF_AGENT_TRANSCRIPTS,
                        &agent_transcript_spawn_prefix(spawn_id),
                    )
                    .map_err(|error| mcp_error(error.code(), error.to_string()))?;
                for (key, value) in rows {
                    scanned_rows += 1;
                    ingest_row(&mut spawns, &key, &value, params.since_ns, params.until_ns)?;
                }
            }
        } else if plan.use_timestamp_index {
            let Some(since_ns) = params.since_ns else {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "AGENT_COST_QUERY_PLAN_INVALID: timestamp-index plan missing since_ns",
                ));
            };
            let read = scan_transcripts_by_timestamp_index(&db, since_ns, params.until_ns)?;
            scanned_rows = read.scanned_rows;
            scanned_index_rows = read.scanned_index_rows;
            for (key, value) in read.transcript_rows {
                ingest_row(&mut spawns, &key, &value, params.since_ns, params.until_ns)?;
            }
        } else {
            let mut start: Vec<u8> = Vec::new();
            loop {
                if !plan.explicit_all_history
                    && usize::try_from(scanned_rows).unwrap_or(usize::MAX) >= MAX_SCAN_ROWS_PER_CALL
                {
                    return Err(mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "AGENT_COST_SCAN_BUDGET_EXHAUSTED after {MAX_SCAN_ROWS_PER_CALL} \
                             CF_AGENT_TRANSCRIPTS rows; pass spawn_id, use the default indexed \
                             fleet window, or set all_history=true for an explicit exact \
                             all-history scan — a truncated rollup would under-report cost"
                        ),
                    ));
                }
                let (rows, more) = db
                    .scan_cf_from(cf::CF_AGENT_TRANSCRIPTS, &start, SCAN_CHUNK_ROWS)
                    .map_err(|error| mcp_error(error.code(), error.to_string()))?;
                if rows.is_empty() {
                    break;
                }
                for (key, value) in &rows {
                    scanned_rows += 1;
                    ingest_row(&mut spawns, key, value, params.since_ns, params.until_ns)?;
                }
                if !more {
                    break;
                }
                let Some((last, _value)) = rows.last() else {
                    break;
                };
                start = key_after(last);
            }
        }

        // Resolve each spawn to a billable usage + cost.
        let mut per_spawn = Vec::with_capacity(spawns.len());
        let mut per_model: BTreeMap<String, AgentModelCost> = BTreeMap::new();
        let mut fleet = AgentFleetCost {
            spawns_total: 0,
            spawns_complete: 0,
            spawns_incomplete: 0,
            usage: BillableUsage::default(),
            total_tokens: 0,
            computed_micro_usd: 0,
            source_reported_micro_usd: 0,
            unpriced_models: Vec::new(),
        };
        let mut unpriced_set: BTreeSet<String> = BTreeSet::new();
        // Per-spawn rollup capture for the optional template/task joins (#951).
        // Holds the same numbers folded into the fleet total, keyed by spawn id,
        // so a group rollup is a regrouping of these — never a re-derivation.
        let mut spawn_rollups: Vec<SpawnRollup> = Vec::new();

        for (spawn_id, acc) in spawns {
            fleet.spawns_total += 1;
            let resolved = acc.resolve()?;
            let Some(resolved) = resolved else {
                fleet.spawns_incomplete += 1;
                spawn_rollups.push(SpawnRollup::incomplete(spawn_id.clone()));
                per_spawn.push(AgentSpawnCost {
                    spawn_id,
                    source: acc_source_label(&acc),
                    model: acc.model.clone(),
                    status: "no_terminal_usage".to_owned(),
                    usage: BillableUsage::default(),
                    total_tokens: 0,
                    cost: CostOutcome::Priced {
                        cost: CostBreakdown::default(),
                    },
                    source_reported_micro_usd: None,
                    reconciliation_delta_micro_usd: None,
                    authoritative_line_no: None,
                    models: Vec::new(),
                    turns: None,
                    turns_summary: None,
                });
                continue;
            };
            fleet.spawns_complete += 1;
            let usage = resolved.usage;
            let total_tokens = usage.total_tokens();
            add_usage(&mut fleet.usage, &usage);
            fleet.total_tokens = fleet.total_tokens.saturating_add(total_tokens);
            if let Some(src) = resolved.source_reported_micro_usd {
                fleet.source_reported_micro_usd =
                    fleet.source_reported_micro_usd.saturating_add(src);
            }

            // Price each model in the breakdown independently. The spawn-level
            // computed cost is the sum of the priced models; an unpriced model
            // contributes its tokens (counted) but no cost (surfaced honestly
            // via `unpriced_models` and the per-model breakdown), so it never
            // inflates the spawn cost with a guess.
            let mut spawn_models: Vec<AgentSpawnModelCost> =
                Vec::with_capacity(resolved.models.len());
            let mut spawn_computed: u64 = 0;
            let mut any_priced = false;
            let mut spawn_unpriced: Vec<String> = Vec::new();
            for resolved_model in &resolved.models {
                let model_label = resolved_model
                    .model
                    .clone()
                    .unwrap_or_else(|| "unknown".to_owned());
                // Price rows are keyed by the normalized (trimmed, lower-cased)
                // model id, so the lookup must normalize the transcript's raw
                // model id too — otherwise a mixed-case id silently misses.
                let lookup_key = ModelPrice::normalize_id(&model_label);
                let priced = prices.get(&lookup_key);
                let model_usage = resolved_model.usage;
                let model_total_tokens = model_usage.total_tokens();
                let (cost_outcome, computed_micro) = match priced {
                    Some(price) => {
                        let breakdown = price.cost_micro_usd(&model_usage).map_err(|detail| {
                            mcp_error(error_codes::TOOL_INTERNAL_ERROR, detail)
                        })?;
                        let total = breakdown.total_micro_usd;
                        spawn_computed = spawn_computed.saturating_add(total);
                        any_priced = true;
                        (CostOutcome::Priced { cost: breakdown }, Some(total))
                    }
                    None => {
                        unpriced_set.insert(model_label.clone());
                        spawn_unpriced.push(model_label.clone());
                        (
                            CostOutcome::Unpriced {
                                model_id: model_label.clone(),
                            },
                            None,
                        )
                    }
                };

                // Fold into the per-model fleet aggregate.
                let entry =
                    per_model
                        .entry(model_label.clone())
                        .or_insert_with(|| AgentModelCost {
                            model: model_label.clone(),
                            priced: priced.is_some(),
                            spawns: 0,
                            usage: BillableUsage::default(),
                            total_tokens: 0,
                            computed_micro_usd: if priced.is_some() { Some(0) } else { None },
                            source_reported_micro_usd: None,
                        });
                entry.spawns += 1;
                add_usage(&mut entry.usage, &model_usage);
                entry.total_tokens = entry.total_tokens.saturating_add(model_total_tokens);
                if let (Some(existing), Some(add)) =
                    (entry.computed_micro_usd.as_mut(), computed_micro)
                {
                    *existing = existing.saturating_add(add);
                }
                if let Some(src) = resolved_model.source_reported_micro_usd {
                    let acc = entry.source_reported_micro_usd.get_or_insert(0);
                    *acc = acc.saturating_add(src);
                }

                spawn_models.push(AgentSpawnModelCost {
                    model: model_label,
                    usage: model_usage,
                    total_tokens: model_total_tokens,
                    cost: cost_outcome,
                    source_reported_micro_usd: resolved_model.source_reported_micro_usd,
                });
            }
            fleet.computed_micro_usd = fleet.computed_micro_usd.saturating_add(spawn_computed);

            // Spawn-level outcome: the sum of the priced models when at least
            // one is priced; otherwise an honest `unpriced` carrying the
            // primary model id.
            let spawn_cost = if any_priced {
                let mut breakdown = CostBreakdown::default();
                for model in &spawn_models {
                    if let CostOutcome::Priced { cost } = &model.cost {
                        breakdown.input_micro_usd = breakdown
                            .input_micro_usd
                            .saturating_add(cost.input_micro_usd);
                        breakdown.output_micro_usd = breakdown
                            .output_micro_usd
                            .saturating_add(cost.output_micro_usd);
                        breakdown.cache_read_micro_usd = breakdown
                            .cache_read_micro_usd
                            .saturating_add(cost.cache_read_micro_usd);
                        breakdown.cache_creation_micro_usd = breakdown
                            .cache_creation_micro_usd
                            .saturating_add(cost.cache_creation_micro_usd);
                        breakdown.total_micro_usd = breakdown
                            .total_micro_usd
                            .saturating_add(cost.total_micro_usd);
                    }
                }
                CostOutcome::Priced { cost: breakdown }
            } else {
                CostOutcome::Unpriced {
                    model_id: resolved
                        .model
                        .clone()
                        .unwrap_or_else(|| "unknown".to_owned()),
                }
            };

            let reconciliation_delta = match (resolved.source_reported_micro_usd, any_priced) {
                (Some(src), true) => Some(
                    i64::try_from(src).unwrap_or(i64::MAX)
                        - i64::try_from(spawn_computed).unwrap_or(i64::MAX),
                ),
                _ => None,
            };

            // A single-model spawn is fully described by the top-level fields;
            // only surface the per-model breakdown when it adds information
            // (a genuine multi-model session).
            let models = if spawn_models.len() > 1 {
                spawn_models
            } else {
                Vec::new()
            };

            // Optional per-turn series (#950). Built from the per-turn rows the
            // accumulator collected, priced by the spawn's primary model.
            let (turns, turns_summary) = if params.include_per_turn {
                match build_spawn_turns(&acc, &resolved, &prices)? {
                    Some((turns, summary)) => (Some(turns), Some(summary)),
                    None => (None, None),
                }
            } else {
                (None, None)
            };

            // Per-spawn rollup row (#951) feeding the optional per_template /
            // per_task group_by dimensions.
            spawn_rollups.push(SpawnRollup {
                spawn_id: spawn_id.clone(),
                complete: true,
                usage,
                total_tokens,
                computed_micro_usd: spawn_computed,
                source_reported_micro_usd: resolved.source_reported_micro_usd.unwrap_or(0),
                unpriced_models: spawn_unpriced,
            });

            per_spawn.push(AgentSpawnCost {
                spawn_id,
                source: resolved.source_label(),
                model: resolved.model,
                status: "complete".to_owned(),
                usage,
                total_tokens,
                cost: spawn_cost,
                source_reported_micro_usd: resolved.source_reported_micro_usd,
                reconciliation_delta_micro_usd: reconciliation_delta,
                authoritative_line_no: Some(resolved.line_no),
                models,
                turns,
                turns_summary,
            });
        }

        fleet.unpriced_models = unpriced_set.into_iter().collect();
        let per_model_vec = per_model.into_values().collect();

        // Optional higher-level rollups (#951). Each is a pure regrouping of the
        // per-spawn rollups above through a durable join recorded at spawn time,
        // so the sum over groups reconciles exactly with the fleet total.
        let want_template = params.group_by.contains(&AgentCostGroupBy::Template);
        let want_task = params.group_by.contains(&AgentCostGroupBy::Task);
        let mut scanned_event_rows: u64 = 0;
        let per_template = if want_template {
            let (spawn_template, scanned) = build_spawn_template_map(&db)?;
            scanned_event_rows = scanned;
            Some(group_rollups(&spawn_rollups, |spawn_id| {
                spawn_template
                    .get(spawn_id)
                    .map(|(template_id, version)| Attribution {
                        key: template_id.clone(),
                        template_id: None,
                        template_version: *version,
                    })
            }))
        } else {
            None
        };
        let per_task = if want_task {
            let spawn_task = build_spawn_task_map(&db)?;
            Some(group_rollups(&spawn_rollups, |spawn_id| {
                spawn_task
                    .get(spawn_id)
                    .map(|(task_id, template_id)| Attribution {
                        key: task_id.clone(),
                        template_id: Some(template_id.clone()),
                        template_version: None,
                    })
            }))
        } else {
            None
        };

        Ok(AgentCostResponse {
            ok: true,
            now_ns: plan.now_ns,
            query_strategy: plan.query_strategy,
            default_window_applied: plan.default_window_applied,
            completeness: "exact".to_owned(),
            transcript_index_version: plan.use_timestamp_index.then_some(COST_TS_INDEX_VERSION),
            scanned_index_rows,
            since_ns: params.since_ns,
            until_ns: params.until_ns,
            scanned_rows,
            scanned_event_rows,
            fleet,
            per_model: per_model_vec,
            per_spawn,
            per_template,
            per_task,
        })
    }
}

fn validate_cost_params(params: &CostParams) -> Result<CostOperation, ErrorData> {
    let operation = CostOperation::parse(params.operation.as_str())?;
    let matches = [
        (
            CostOperation::Summarize,
            params.summarize.is_some(),
            "summarize",
        ),
        (
            CostOperation::PriceList,
            params.price_list.is_some(),
            "price_list",
        ),
        (
            CostOperation::PricePut,
            params.price_put.is_some(),
            "price_put",
        ),
        (
            CostOperation::PriceDelete,
            params.price_delete.is_some(),
            "price_delete",
        ),
    ];
    let supplied = matches
        .iter()
        .filter(|(_operation, present, _name)| *present)
        .count();
    if supplied != 1 {
        return Err(cost_missing_spec(operation.as_str()));
    }
    let matched = matches
        .iter()
        .any(|(candidate, present, _name)| *candidate == operation && *present);
    if matched {
        Ok(operation)
    } else {
        Err(cost_missing_spec(operation.as_str()))
    }
}

fn require_cost_maintenance_profile(
    service: &SynapseService,
    request_context: &RequestContext<RoleServer>,
    operation: &'static str,
    source_id: &str,
) -> Result<(), ErrorData> {
    let session_id = super::context::mcp_session_id_from_request_context(request_context)?;
    let snapshot = service.tool_profile_snapshot(session_id.as_deref())?;
    if matches!(
        snapshot.profile,
        ToolProfileKind::BreakGlass | ToolProfileKind::FullCapability
    ) {
        return Ok(());
    }
    Err(ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{COST_TOOL} operation={operation} is not allowed for profile {}",
            snapshot.profile.as_str()
        ),
        Some(json!({
            "code": error_codes::TOOL_PROFILE_POLICY_DENIED,
            "tool": COST_TOOL,
            "operation": operation,
            "source_id": source_id,
            "profile": snapshot.profile.as_str(),
            "source_of_truth": COST_SOURCE_OF_TRUTH,
            "remediation": "switch to an explicit maintenance profile with operator intent before mutating the cost price table; normal_agent may use summarize or price_list first",
        })),
    ))
}

fn cost_missing_spec(operation: &str) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32602),
        format!(
            "{COST_TOOL} operation={operation} requires exactly one matching operation payload"
        ),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": COST_TOOL,
            "operation": operation,
            "source_of_truth": "MCP request parameters",
            "source_id": operation,
            "remediation": "pass exactly one payload object whose field name matches operation",
        })),
    )
}

fn cost_invalid_operation(operation: &str) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32602),
        format!(
            "{COST_TOOL} operation={operation} is invalid; expected summarize, price_list, price_put, or price_delete"
        ),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": COST_TOOL,
            "operation": operation,
            "source_of_truth": "MCP request parameters",
            "source_id": "operation",
            "allowed_operations": ["summarize", "price_list", "price_put", "price_delete"],
            "remediation": "set operation to one of the allowed values and pass exactly the matching payload object",
        })),
    )
}

fn cost_delegate_error(
    operation: &'static str,
    source_id: &str,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "{COST_TOOL} operation={operation} failed for {source_id}: {}",
            error.message
        ),
        Some(json!({
            "code": error_code_from(&error),
            "tool": COST_TOOL,
            "operation": operation,
            "source_id": source_id,
            "source_of_truth": COST_SOURCE_OF_TRUTH,
            "remediation": remediation,
            "cause": {
                "message": error.message.to_string(),
                "data": error.data,
            },
        })),
    )
}

fn error_code_from(error: &ErrorData) -> String {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(|code| code.as_str())
        .map_or_else(
            || error_codes::TOOL_INTERNAL_ERROR.to_owned(),
            str::to_owned,
        )
}

/// Loads the full operator price table from `CF_KV`, keyed by normalized model
/// id. A corrupt row in our own namespace is surfaced, never skipped.
fn load_price_table(db: &Db) -> Result<BTreeMap<String, ModelPrice>, ErrorData> {
    let rows = db
        .scan_cf_prefix(cf::CF_KV, PRICE_KEY_PREFIX.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let mut table = BTreeMap::new();
    for (key, value) in rows {
        let price: ModelPrice = serde_json::from_slice(&value).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "PRICE_ROW_CORRUPT: key {} failed to decode: {error}",
                    String::from_utf8_lossy(&key)
                ),
            )
        })?;
        table.insert(price.model_id.clone(), price);
    }
    Ok(table)
}

// ----------------------------------------------------------------------------
// Per-spawn accumulation
// ----------------------------------------------------------------------------

/// Mutable per-spawn state folded over the transcript rows in one pass.
#[derive(Clone, Debug, Default)]
struct SpawnAccumulator {
    source: Option<TranscriptSource>,
    /// Primary model id (from `system/init`, refreshed by assistant rows and
    /// stamped onto the terminal row by #900).
    model: Option<String>,
    /// Highest-line `result/*` row usage + line + reported cost (Claude).
    claude_result: Option<ClaudeResult>,
    /// Max across `turn.completed` cumulative usage (Codex), with the line of
    /// the row carrying the running maximum.
    codex_max: Option<CodexMax>,
    /// Sum across `local.turn.finished` rows. Local OpenAI-compatible
    /// endpoints report per-turn prompt/completion usage, not a cumulative
    /// terminal row.
    local_sum: Option<LocalSum>,
    /// Per-turn raw usage keyed by `turn_index`, for the optional per-turn
    /// series. Holds the turn's cumulative usage (Codex `turn.completed`), the
    /// per-message usage (Claude `assistant`), or the per-turn usage (local
    /// `local.turn.finished`). When a turn repeats across streaming lines the
    /// highest-line row wins — the final chunk is the authoritative one;
    /// first-seen would keep a placeholder snapshot.
    turns: BTreeMap<u64, TurnRaw>,
    mixed_source: bool,
}

/// Raw token counts collected for one turn before delta / per-message
/// resolution. Dimensions are source-native: for Codex `input`/`cache_read`
/// are cumulative-through-this-turn; for Claude/local they are per-turn.
#[derive(Clone, Copy, Debug, Default)]
struct TurnRaw {
    line_no: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    cache_creation_5m: u64,
    cache_creation_1h: u64,
}

#[derive(Clone, Debug)]
struct ClaudeResult {
    line_no: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    /// Global cache-creation TTL split from the result row's
    /// `usage.cache_creation` (#949). `0` when the stream reported no split.
    cache_creation_5m: u64,
    cache_creation_1h: u64,
    cost_micro_usd: Option<u64>,
    /// Claude `result.modelUsage` per-model breakdown. Empty for a
    /// single-model session (or a stream that predates the field).
    model_usage: Vec<TranscriptModelUsage>,
}

#[derive(Clone, Debug, Default)]
struct CodexMax {
    line_no: u64,
    input: u64,
    output: u64,
    cached: u64,
}

#[derive(Clone, Debug, Default)]
struct LocalSum {
    line_no: u64,
    input: u64,
    output: u64,
}

/// One model's resolved, canonical usage within a spawn. A single-model spawn
/// resolves to one of these; a Claude `modelUsage` session resolves to one per
/// model (#949).
struct ResolvedModel {
    model: Option<String>,
    usage: BillableUsage,
    /// The CLI's own per-model cost (`costUSD`) when reported, micro-USD.
    source_reported_micro_usd: Option<u64>,
}

/// What a spawn resolves to once its authoritative row is chosen.
struct ResolvedSpawn {
    source: TranscriptSource,
    /// Primary model label (the session's last-seen model), for the
    /// spawn-level `model` field and single-model fallbacks.
    model: Option<String>,
    /// Per-model breakdown; always at least one entry.
    models: Vec<ResolvedModel>,
    /// Sum of every model's canonical usage.
    usage: BillableUsage,
    /// Whole-session cost the CLI reported (`total_cost_usd`), micro-USD.
    source_reported_micro_usd: Option<u64>,
    line_no: u64,
}

impl ResolvedSpawn {
    fn source_label(&self) -> Option<String> {
        Some(source_label(self.source))
    }
}

/// Splits a model's aggregate cache-creation count into 5m/1h tiers using the
/// session-global TTL split. Exact when the whole session used a single tier
/// (the common case — agents set one `cache_control` TTL); when the session
/// mixed tiers, `modelUsage` does not carry a per-model split, so the tokens
/// are left untagged and priced at the aggregate rate (the cost engine then
/// surfaces any residual through the reconciliation delta rather than guessing).
const fn attribute_cache_creation_tier(
    model_cache_creation: u64,
    global_5m: u64,
    global_1h: u64,
) -> (u64, u64) {
    if global_1h > 0 && global_5m == 0 {
        (0, model_cache_creation)
    } else if global_5m > 0 && global_1h == 0 {
        (model_cache_creation, 0)
    } else {
        (0, 0)
    }
}

impl SpawnAccumulator {
    fn observe(&mut self, record: &AgentTranscriptRecord) -> Result<(), ErrorData> {
        match self.source {
            None => self.source = Some(record.source),
            Some(existing) if existing != record.source => self.mixed_source = true,
            Some(_) => {}
        }
        if let Some(model) = record.model.clone() {
            // Prefer the most recent (highest-line) non-empty model id.
            self.model = Some(model);
        }
        let Some(usage) = record.usage.as_ref() else {
            return Ok(());
        };
        let kind = record.event_kind.as_deref().unwrap_or("");
        match record.source {
            TranscriptSource::ClaudeStreamJson if kind.starts_with("result/") => {
                let candidate = ClaudeResult {
                    line_no: record.line_no,
                    input: usage.input_tokens.unwrap_or(0),
                    output: usage.output_tokens.unwrap_or(0),
                    cache_read: usage.cache_read_input_tokens.unwrap_or(0),
                    cache_creation: usage.cache_creation_input_tokens.unwrap_or(0),
                    cache_creation_5m: usage.cache_creation_5m_input_tokens.unwrap_or(0),
                    cache_creation_1h: usage.cache_creation_1h_input_tokens.unwrap_or(0),
                    cost_micro_usd: usage.total_cost_micro_usd,
                    model_usage: usage.model_usage.clone(),
                };
                if self
                    .claude_result
                    .as_ref()
                    .is_none_or(|existing| candidate.line_no >= existing.line_no)
                {
                    self.claude_result = Some(candidate);
                }
            }
            TranscriptSource::CodexExecJson if kind == "turn.completed" => {
                let entry = self.codex_max.get_or_insert_with(CodexMax::default);
                // Cumulative totals are monotonic; take the elementwise max so
                // the result is robust to row ordering and equals the session
                // total.
                entry.input = entry.input.max(usage.input_tokens.unwrap_or(0));
                entry.output = entry.output.max(usage.output_tokens.unwrap_or(0));
                entry.cached = entry.cached.max(usage.cache_read_input_tokens.unwrap_or(0));
                entry.line_no = entry.line_no.max(record.line_no);
            }
            TranscriptSource::CodexAppServerJsonRpc
                if kind == "codex_app_server/thread/tokenUsage/updated" =>
            {
                let entry = self.codex_max.get_or_insert_with(CodexMax::default);
                entry.input = entry.input.max(usage.input_tokens.unwrap_or(0));
                entry.output = entry.output.max(usage.output_tokens.unwrap_or(0));
                entry.cached = entry.cached.max(usage.cache_read_input_tokens.unwrap_or(0));
                entry.line_no = entry.line_no.max(record.line_no);
            }
            TranscriptSource::LocalModelJson if kind == "local.turn.finished" => {
                let entry = self.local_sum.get_or_insert_with(LocalSum::default);
                entry.input = entry.input.saturating_add(usage.input_tokens.unwrap_or(0));
                entry.output = entry
                    .output
                    .saturating_add(usage.output_tokens.unwrap_or(0));
                entry.line_no = entry.line_no.max(record.line_no);
            }
            _ => {}
        }

        // Per-turn collection (consumed only when include_per_turn is set).
        // The row class that carries a turn's usage differs by source; a turn
        // repeated across streaming lines keeps the highest-line row.
        let is_turn_usage_row = match record.source {
            // Ambient session transcripts carry per-message usage on `assistant`
            // rows exactly like stream-json; they have no terminal `result` row,
            // so the spawn resolves to an honest `None` session total (the
            // per-turn output series is still recovered below).
            TranscriptSource::ClaudeStreamJson | TranscriptSource::ClaudeSessionJsonl => {
                kind == "assistant"
            }
            TranscriptSource::CodexExecJson => kind == "turn.completed",
            TranscriptSource::CodexAppServerJsonRpc => {
                kind == "codex_app_server/thread/tokenUsage/updated"
            }
            TranscriptSource::LocalModelJson => kind == "local.turn.finished",
        };
        if is_turn_usage_row && let Some(turn_index) = record.turn_index {
            let entry = self.turns.entry(turn_index).or_default();
            if record.line_no >= entry.line_no {
                *entry = TurnRaw {
                    line_no: record.line_no,
                    input: usage.input_tokens.unwrap_or(0),
                    output: usage.output_tokens.unwrap_or(0),
                    cache_read: usage.cache_read_input_tokens.unwrap_or(0),
                    cache_creation: usage.cache_creation_input_tokens.unwrap_or(0),
                    cache_creation_5m: usage.cache_creation_5m_input_tokens.unwrap_or(0),
                    cache_creation_1h: usage.cache_creation_1h_input_tokens.unwrap_or(0),
                };
            }
        }
        Ok(())
    }

    fn resolve(&self) -> Result<Option<ResolvedSpawn>, ErrorData> {
        if self.mixed_source {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "AGENT_COST_MIXED_SOURCE: one spawn carried rows from two CLI sources; \
                 transcripts are corrupt",
            ));
        }
        match (
            self.source,
            &self.claude_result,
            &self.codex_max,
            &self.local_sum,
        ) {
            (Some(TranscriptSource::ClaudeStreamJson), Some(result), _, _) => {
                let models = if result.model_usage.is_empty() {
                    // Single-model session: the result row carries the model's
                    // own exact cache-creation TTL split.
                    vec![ResolvedModel {
                        model: self.model.clone(),
                        usage: BillableUsage::from_claude_with_ttl(
                            result.input,
                            result.output,
                            result.cache_read,
                            result.cache_creation,
                            result.cache_creation_5m,
                            result.cache_creation_1h,
                        ),
                        source_reported_micro_usd: result.cost_micro_usd,
                    }]
                } else {
                    // Multi-model session: attribute each model exactly from
                    // the `modelUsage` map; the session-global TTL split is
                    // applied per model (exact for single-tier sessions).
                    result
                        .model_usage
                        .iter()
                        .map(|model_usage| {
                            let (tier_5m, tier_1h) = attribute_cache_creation_tier(
                                model_usage.cache_creation_input_tokens,
                                result.cache_creation_5m,
                                result.cache_creation_1h,
                            );
                            ResolvedModel {
                                model: Some(model_usage.model.clone()),
                                usage: BillableUsage::from_claude_with_ttl(
                                    model_usage.input_tokens,
                                    model_usage.output_tokens,
                                    model_usage.cache_read_input_tokens,
                                    model_usage.cache_creation_input_tokens,
                                    tier_5m,
                                    tier_1h,
                                ),
                                source_reported_micro_usd: model_usage.cost_micro_usd,
                            }
                        })
                        .collect()
                };
                let usage = sum_usage(&models);
                Ok(Some(ResolvedSpawn {
                    source: TranscriptSource::ClaudeStreamJson,
                    model: self.model.clone(),
                    models,
                    usage,
                    source_reported_micro_usd: result.cost_micro_usd,
                    line_no: result.line_no,
                }))
            }
            (
                Some(
                    source @ (TranscriptSource::CodexExecJson
                    | TranscriptSource::CodexAppServerJsonRpc),
                ),
                _,
                Some(codex),
                _,
            ) => {
                let usage =
                    BillableUsage::from_codex_cumulative(codex.input, codex.output, codex.cached)
                        .map_err(|detail| mcp_error(error_codes::TOOL_INTERNAL_ERROR, detail))?;
                Ok(Some(ResolvedSpawn {
                    source,
                    model: self.model.clone(),
                    models: vec![ResolvedModel {
                        model: self.model.clone(),
                        usage,
                        source_reported_micro_usd: None,
                    }],
                    usage,
                    source_reported_micro_usd: None,
                    line_no: codex.line_no,
                }))
            }
            (Some(TranscriptSource::LocalModelJson), _, _, Some(local)) => {
                let usage = BillableUsage {
                    input_tokens: local.input,
                    output_tokens: local.output,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    cache_creation_5m_tokens: 0,
                    cache_creation_1h_tokens: 0,
                };
                Ok(Some(ResolvedSpawn {
                    source: TranscriptSource::LocalModelJson,
                    model: self.model.clone(),
                    models: vec![ResolvedModel {
                        model: self.model.clone(),
                        usage,
                        source_reported_micro_usd: None,
                    }],
                    usage,
                    source_reported_micro_usd: None,
                    line_no: local.line_no,
                }))
            }
            _ => Ok(None),
        }
    }
}

/// Sums the canonical usage across a spawn's resolved models.
fn sum_usage(models: &[ResolvedModel]) -> BillableUsage {
    let mut total = BillableUsage::default();
    for model in models {
        add_usage(&mut total, &model.usage);
    }
    total
}

/// Prices one turn's usage by the spawn's primary model, or returns an honest
/// `unpriced` marker carrying the model id (never a guess).
fn price_turn_usage(
    usage: &BillableUsage,
    price: Option<&ModelPrice>,
    model_label: &str,
) -> Result<CostOutcome, ErrorData> {
    match price {
        Some(price) => {
            let cost = price
                .cost_micro_usd(usage)
                .map_err(|detail| mcp_error(error_codes::TOOL_INTERNAL_ERROR, detail))?;
            Ok(CostOutcome::Priced { cost })
        }
        None => Ok(CostOutcome::Unpriced {
            model_id: model_label.to_owned(),
        }),
    }
}

/// Reconstructs a spawn's per-turn cost series from the raw per-turn rows.
///
/// - **Codex**: the rows hold cumulative-through-turn totals; consecutive
///   `turn_index` deltas recover the exact per-turn block (each turn's
///   `cached <= input` holds, so the delta is well-formed). The deltas
///   telescope to the spawn total, so the series reconciles exactly. A
///   non-monotonic cumulative is corrupt data and is surfaced, never clamped.
/// - **Claude**: the rows hold per-message usage. Input/cache are reliable;
///   `output_tokens` is a partial streaming snapshot, so the series is flagged
///   `partial_snapshot` and does not reconcile to the authoritative session
///   `result` total in output (by design, surfaced via `reconciles=false`).
/// - **Local**: `local.turn.finished` rows are already per-turn and exact.
fn build_spawn_turns(
    acc: &SpawnAccumulator,
    resolved: &ResolvedSpawn,
    prices: &BTreeMap<String, ModelPrice>,
) -> Result<Option<(Vec<AgentTurnCost>, TurnsSummary)>, ErrorData> {
    if acc.turns.is_empty() {
        return Ok(None);
    }
    let model_label = resolved
        .model
        .clone()
        .unwrap_or_else(|| "unknown".to_owned());
    let price = prices.get(&ModelPrice::normalize_id(&model_label));

    let (method, output_basis, exact) = match resolved.source {
        TranscriptSource::CodexExecJson => ("codex_cumulative_delta", TurnOutputBasis::Exact, true),
        TranscriptSource::CodexAppServerJsonRpc => (
            "codex_app_server_cumulative_delta",
            TurnOutputBasis::Exact,
            true,
        ),
        TranscriptSource::ClaudeStreamJson | TranscriptSource::ClaudeSessionJsonl => (
            "claude_per_message",
            TurnOutputBasis::PartialSnapshot,
            false,
        ),
        TranscriptSource::LocalModelJson => ("local_per_turn", TurnOutputBasis::Exact, true),
    };

    let mut turns = Vec::with_capacity(acc.turns.len());
    let mut turns_usage_sum = BillableUsage::default();
    // Codex delta state: previous turn's cumulative (input, output, cached).
    let mut prev_cumulative = (0u64, 0u64, 0u64);

    for (&turn_index, raw) in &acc.turns {
        let usage = match resolved.source {
            TranscriptSource::CodexExecJson | TranscriptSource::CodexAppServerJsonRpc => {
                let delta_input = raw.input.checked_sub(prev_cumulative.0);
                let delta_output = raw.output.checked_sub(prev_cumulative.1);
                let delta_cached = raw.cache_read.checked_sub(prev_cumulative.2);
                let (Some(delta_input), Some(delta_output), Some(delta_cached)) =
                    (delta_input, delta_output, delta_cached)
                else {
                    return Err(mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "AGENT_COST_TURN_NONMONOTONIC: spawn turn {turn_index} cumulative \
                             usage decreased (in={} out={} cached={} vs prev in={} out={} \
                             cached={}); Codex turn totals must be monotonic — transcript corrupt",
                            raw.input,
                            raw.output,
                            raw.cache_read,
                            prev_cumulative.0,
                            prev_cumulative.1,
                            prev_cumulative.2,
                        ),
                    ));
                };
                prev_cumulative = (raw.input, raw.output, raw.cache_read);
                BillableUsage::from_codex_cumulative(delta_input, delta_output, delta_cached)
                    .map_err(|detail| mcp_error(error_codes::TOOL_INTERNAL_ERROR, detail))?
            }
            TranscriptSource::ClaudeStreamJson | TranscriptSource::ClaudeSessionJsonl => {
                BillableUsage::from_claude_with_ttl(
                    raw.input,
                    raw.output,
                    raw.cache_read,
                    raw.cache_creation,
                    raw.cache_creation_5m,
                    raw.cache_creation_1h,
                )
            }
            TranscriptSource::LocalModelJson => BillableUsage {
                input_tokens: raw.input,
                output_tokens: raw.output,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                cache_creation_5m_tokens: 0,
                cache_creation_1h_tokens: 0,
            },
        };
        add_usage(&mut turns_usage_sum, &usage);
        let cost = price_turn_usage(&usage, price, &model_label)?;
        turns.push(AgentTurnCost {
            turn_index,
            line_no: raw.line_no,
            usage,
            total_tokens: usage.total_tokens(),
            cost,
            output_basis,
        });
    }

    let summary = TurnsSummary {
        method: method.to_owned(),
        turn_count: turns.len(),
        // Exact only for the cumulative-delta / per-turn sources AND only when
        // the reconstructed series actually sums to the spawn total — surfaced,
        // never asserted blindly.
        reconciles: exact && turns_usage_sum == resolved.usage,
        turns_usage_sum,
    };
    Ok(Some((turns, summary)))
}

fn acc_source_label(acc: &SpawnAccumulator) -> Option<String> {
    acc.source.map(source_label)
}

fn source_label(source: TranscriptSource) -> String {
    match source {
        TranscriptSource::ClaudeStreamJson => "claude_stream_json".to_owned(),
        TranscriptSource::ClaudeSessionJsonl => "claude_session_jsonl".to_owned(),
        TranscriptSource::CodexExecJson => "codex_exec_json".to_owned(),
        TranscriptSource::CodexAppServerJsonRpc => "codex_app_server_json_rpc".to_owned(),
        TranscriptSource::LocalModelJson => "local_model_json".to_owned(),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CostTranscriptIndexMeta {
    schema_version: u32,
    indexed_rows: u64,
    built_at_ns: u64,
    source_cf: String,
    index_prefix: String,
}

struct IndexedTranscriptRead {
    scanned_index_rows: u64,
    scanned_rows: u64,
    transcript_rows: Vec<(Vec<u8>, Vec<u8>)>,
}

fn ensure_transcript_ts_index(db: &Db) -> Result<CostTranscriptIndexMeta, ErrorData> {
    if let Some(value) = db
        .get_cf(cf::CF_KV, COST_TS_INDEX_META_KEY.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
    {
        let meta: CostTranscriptIndexMeta = serde_json::from_slice(&value).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("AGENT_COST_INDEX_META_CORRUPT: {COST_TS_INDEX_META_KEY}: {error}"),
            )
        })?;
        if meta.schema_version != COST_TS_INDEX_VERSION {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "AGENT_COST_INDEX_VERSION_UNSUPPORTED: meta version {} != expected {}",
                    meta.schema_version, COST_TS_INDEX_VERSION
                ),
            ));
        }
        return Ok(meta);
    }

    let mut indexed_rows = 0_u64;
    let mut start = Vec::new();
    loop {
        let (rows, more) = db
            .scan_cf_from(cf::CF_AGENT_TRANSCRIPTS, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        let mut index_rows = Vec::with_capacity(rows.len());
        for (key, value) in &rows {
            let (spawn_id, _line_no) = decode_agent_transcript_key(key)
                .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            let record: AgentTranscriptRecord = serde_json::from_slice(value).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "AGENT_COST_INDEX_BUILD_TRANSCRIPT_CORRUPT: spawn {spawn_id} row failed to decode: {error}"
                    ),
                )
            })?;
            index_rows.push((
                agent_transcript_ts_index_key(record.ts_ns, key),
                key.clone(),
            ));
        }
        if !index_rows.is_empty() {
            db.put_batch_pressure_bypass(cf::CF_KV, index_rows)
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("AGENT_COST_INDEX_BUILD_WRITE_FAILED: {error}"),
                    )
                })?;
            indexed_rows = indexed_rows.saturating_add(rows.len() as u64);
        }
        if !more {
            break;
        }
        let Some((last, _value)) = rows.last() else {
            break;
        };
        start = key_after(last);
    }

    let meta = CostTranscriptIndexMeta {
        schema_version: COST_TS_INDEX_VERSION,
        indexed_rows,
        built_at_ns: unix_time_ns_now(),
        source_cf: cf::CF_AGENT_TRANSCRIPTS.to_owned(),
        index_prefix: String::from_utf8_lossy(AGENT_TRANSCRIPT_TS_INDEX_PREFIX).to_string(),
    };
    let encoded = serde_json::to_vec(&meta).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("AGENT_COST_INDEX_META_ENCODE_FAILED: {error}"),
        )
    })?;
    db.put_batch_pressure_bypass(
        cf::CF_KV,
        [(COST_TS_INDEX_META_KEY.as_bytes().to_vec(), encoded)],
    )
    .map_err(|error| {
        mcp_error(
            error.code(),
            format!("AGENT_COST_INDEX_META_WRITE_FAILED: {error}"),
        )
    })?;
    Ok(meta)
}

fn scan_transcripts_by_timestamp_index(
    db: &Db,
    since_ns: u64,
    until_ns: Option<u64>,
) -> Result<IndexedTranscriptRead, ErrorData> {
    let _meta = ensure_transcript_ts_index(db)?;
    let upper = until_ns.map(agent_transcript_ts_index_lower_bound);
    let mut start = agent_transcript_ts_index_lower_bound(since_ns);
    let mut scanned_index_rows = 0_u64;
    let mut transcript_rows = Vec::new();
    'scan: loop {
        let (rows, more) = db
            .scan_cf_from(cf::CF_KV, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, transcript_key) in &rows {
            if !key.starts_with(AGENT_TRANSCRIPT_TS_INDEX_PREFIX) {
                break 'scan;
            }
            if let Some(upper) = &upper
                && key.as_slice() >= upper.as_slice()
            {
                break 'scan;
            }
            let ts_ns = decode_agent_transcript_ts_index_key_ts(key)
                .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            if ts_ns < since_ns {
                continue;
            }
            if let Some(until) = until_ns
                && ts_ns >= until
            {
                break 'scan;
            }
            scanned_index_rows = scanned_index_rows.saturating_add(1);
            let transcript_value = db
                .get_cf(cf::CF_AGENT_TRANSCRIPTS, transcript_key)
                .map_err(|error| mcp_error(error.code(), error.to_string()))?
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "AGENT_COST_INDEX_STALE: timestamp index row points at a missing CF_AGENT_TRANSCRIPTS row; rebuild or repair the transcript timestamp index",
                    )
                })?;
            transcript_rows.push((transcript_key.clone(), transcript_value));
        }
        if !more {
            break;
        }
        let Some((last, _value)) = rows.last() else {
            break;
        };
        start = key_after(last);
    }
    Ok(IndexedTranscriptRead {
        scanned_index_rows,
        scanned_rows: transcript_rows.len() as u64,
        transcript_rows,
    })
}

/// Decodes one transcript row and folds it into the per-spawn accumulator,
/// honoring the optional ingestion-time window.
fn ingest_row(
    spawns: &mut BTreeMap<String, SpawnAccumulator>,
    key: &[u8],
    value: &[u8],
    since_ns: Option<u64>,
    until_ns: Option<u64>,
) -> Result<(), ErrorData> {
    let (spawn_id, _line_no) = decode_agent_transcript_key(key)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let record: AgentTranscriptRecord = serde_json::from_slice(value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("TRANSCRIPT_ROW_CORRUPT: spawn {spawn_id} row failed to decode: {error}"),
        )
    })?;
    if let Some(since) = since_ns
        && record.ts_ns < since
    {
        return Ok(());
    }
    if let Some(until) = until_ns
        && record.ts_ns >= until
    {
        return Ok(());
    }
    spawns.entry(spawn_id).or_default().observe(&record)
}

fn add_usage(acc: &mut BillableUsage, add: &BillableUsage) {
    acc.input_tokens = acc.input_tokens.saturating_add(add.input_tokens);
    acc.output_tokens = acc.output_tokens.saturating_add(add.output_tokens);
    acc.cache_read_tokens = acc.cache_read_tokens.saturating_add(add.cache_read_tokens);
    acc.cache_creation_tokens = acc
        .cache_creation_tokens
        .saturating_add(add.cache_creation_tokens);
    acc.cache_creation_5m_tokens = acc
        .cache_creation_5m_tokens
        .saturating_add(add.cache_creation_5m_tokens);
    acc.cache_creation_1h_tokens = acc
        .cache_creation_1h_tokens
        .saturating_add(add.cache_creation_1h_tokens);
}

// ----------------------------------------------------------------------------
// Higher-level rollups: per-template / per-task (#951)
// ----------------------------------------------------------------------------

/// One spawn's contribution to the rollup, captured during the per-spawn pass so
/// the template/task groupings are a pure regrouping of these — never a second
/// derivation from the transcript rows.
struct SpawnRollup {
    spawn_id: String,
    complete: bool,
    usage: BillableUsage,
    total_tokens: u64,
    computed_micro_usd: u64,
    source_reported_micro_usd: u64,
    unpriced_models: Vec<String>,
}

impl SpawnRollup {
    /// A still-running spawn with no authoritative usage row yet: counted in its
    /// group's `spawns`, but contributing zero tokens/cost.
    fn incomplete(spawn_id: String) -> Self {
        Self {
            spawn_id,
            complete: false,
            usage: BillableUsage::default(),
            total_tokens: 0,
            computed_micro_usd: 0,
            source_reported_micro_usd: 0,
            unpriced_models: Vec::new(),
        }
    }
}

/// How one spawn attributes to a higher-level group.
struct Attribution {
    /// The group key (template_id or task_id).
    key: String,
    /// For task groups: the task's dispatch template. None for template groups.
    template_id: Option<String>,
    /// For template groups: the version this spawn ran. None for task groups.
    template_version: Option<u32>,
}

/// Mutable group accumulator folded over the spawn rollups.
struct GroupAcc {
    key: String,
    attributed: bool,
    template_id: Option<String>,
    versions: BTreeSet<u32>,
    spawns: usize,
    spawns_complete: usize,
    usage: BillableUsage,
    total_tokens: u64,
    computed_micro_usd: u64,
    source_reported_micro_usd: u64,
    unpriced: BTreeSet<String>,
    spawn_ids: Vec<String>,
}

impl GroupAcc {
    fn new(key: String, attributed: bool, template_id: Option<String>) -> Self {
        Self {
            key,
            attributed,
            template_id,
            versions: BTreeSet::new(),
            spawns: 0,
            spawns_complete: 0,
            usage: BillableUsage::default(),
            total_tokens: 0,
            computed_micro_usd: 0,
            source_reported_micro_usd: 0,
            unpriced: BTreeSet::new(),
            spawn_ids: Vec::new(),
        }
    }

    fn add(&mut self, rollup: &SpawnRollup, version: Option<u32>) {
        self.spawns += 1;
        if rollup.complete {
            self.spawns_complete += 1;
        }
        add_usage(&mut self.usage, &rollup.usage);
        self.total_tokens = self.total_tokens.saturating_add(rollup.total_tokens);
        self.computed_micro_usd = self
            .computed_micro_usd
            .saturating_add(rollup.computed_micro_usd);
        self.source_reported_micro_usd = self
            .source_reported_micro_usd
            .saturating_add(rollup.source_reported_micro_usd);
        for model in &rollup.unpriced_models {
            self.unpriced.insert(model.clone());
        }
        if let Some(version) = version {
            self.versions.insert(version);
        }
        self.spawn_ids.push(rollup.spawn_id.clone());
    }

    fn into_group(self) -> AgentGroupCost {
        AgentGroupCost {
            key: self.key,
            attributed: self.attributed,
            template_versions: self.versions.into_iter().collect(),
            template_id: self.template_id,
            spawns: self.spawns,
            spawns_complete: self.spawns_complete,
            usage: self.usage,
            total_tokens: self.total_tokens,
            computed_micro_usd: self.computed_micro_usd,
            source_reported_micro_usd: self.source_reported_micro_usd,
            unpriced_models: self.unpriced.into_iter().collect(),
            spawn_ids: self.spawn_ids,
        }
    }
}

/// Regroups the per-spawn rollups by the attribution `attr_of` returns for each
/// spawn id. A spawn that attributes to no group folds into the reserved
/// `(unattributed)` bucket, so the returned groups sum exactly to the fleet
/// total. Groups are ordered by key with `(unattributed)` always last.
fn group_rollups(
    rollups: &[SpawnRollup],
    attr_of: impl Fn(&str) -> Option<Attribution>,
) -> Vec<AgentGroupCost> {
    let mut groups: BTreeMap<String, GroupAcc> = BTreeMap::new();
    for rollup in rollups {
        let (key, attributed, template_id, version) = match attr_of(&rollup.spawn_id) {
            Some(attr) => (attr.key, true, attr.template_id, attr.template_version),
            None => (UNATTRIBUTED_KEY.to_owned(), false, None, None),
        };
        groups
            .entry(key.clone())
            .or_insert_with(|| GroupAcc::new(key, attributed, template_id))
            .add(rollup, version);
    }
    // BTreeMap iterates by key; emit the real groups first and append the
    // residual `(unattributed)` bucket last so it reads as a footer.
    let mut out: Vec<AgentGroupCost> = Vec::with_capacity(groups.len());
    let mut residual: Option<AgentGroupCost> = None;
    for (_key, acc) in groups {
        let group = acc.into_group();
        if group.attributed {
            out.push(group);
        } else {
            residual = Some(group);
        }
    }
    if let Some(residual) = residual {
        out.push(residual);
    }
    out
}

/// spawn_id → (template_id, template_version) join from the spawn journal.
type SpawnTemplateMap = BTreeMap<String, (String, Option<u32>)>;
/// spawn_id → (task_id, dispatch template_id) join from the task queue.
type SpawnTaskMap = BTreeMap<String, (String, String)>;

/// Builds the durable spawn→task join from the `CF_KV` task rows (#910): every
/// task attempt that bound a `spawn_id` maps that spawn to the task and its
/// dispatch template. A spawn bound by more than one attempt (a re-dispatch)
/// attributes to the most recent attempt's task (attempts are append-ordered).
fn build_spawn_task_map(db: &Db) -> Result<SpawnTaskMap, ErrorData> {
    let tasks = SynapseService::read_all_tasks(db)?;
    let mut map: SpawnTaskMap = BTreeMap::new();
    for task in &tasks {
        for attempt in &task.attempts {
            if let Some(spawn_id) = &attempt.spawn_id {
                map.insert(
                    spawn_id.clone(),
                    (task.task_id.clone(), task.template_id.clone()),
                );
            }
        }
    }
    Ok(map)
}

/// Collects the spawn ids with at least one `CF_AGENT_EVENTS` row at or after
/// `since_ns` — i.e. the spawns active within the dashboard window (#1328). The
/// events journal is time-stamped and far smaller than the transcript store, so
/// this scan is bounded by the event budget; the returned set lets the cost
/// rollup scan only those spawns' transcript prefixes instead of the whole CF.
fn collect_recent_spawn_ids(db: &Db, since_ns: u64) -> Result<Vec<String>, ErrorData> {
    let mut ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut scanned: u64 = 0;
    let mut start: Vec<u8> = Vec::new();
    loop {
        if usize::try_from(scanned).unwrap_or(usize::MAX) >= MAX_EVENT_SCAN_ROWS_PER_CALL {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "AGENT_COST_EVENT_SCAN_BUDGET_EXHAUSTED after {MAX_EVENT_SCAN_ROWS_PER_CALL} \
                     CF_AGENT_EVENTS rows collecting window-active spawns; narrow the window"
                ),
            ));
        }
        let (rows, more) = db
            .scan_cf_from(cf::CF_AGENT_EVENTS, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (_key, value) in &rows {
            scanned += 1;
            let record: AgentEventRecord = serde_json::from_slice(value).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("AGENT_EVENT_ROW_CORRUPT: row failed to decode: {error}"),
                )
            })?;
            if record.ts_ns < since_ns {
                continue;
            }
            if let Some(spawn_id) = record.spawn_id.clone() {
                ids.insert(spawn_id);
            }
        }
        if !more {
            break;
        }
        let Some((last, _value)) = rows.last() else {
            break;
        };
        start = key_after(last);
    }
    Ok(ids.into_iter().collect())
}

/// Builds the durable spawn→template join (#909) by scanning the
/// `CF_AGENT_EVENTS` journal for `SpawnRequested` rows, which record the
/// `(template_id, template_version)` the spawn was rendered from. Direct `cli`
/// spawns carry no `template_id` and are simply absent (→ they fall into the
/// `(unattributed)` bucket). Returns the map and the rows scanned, and errors
/// loudly if the journal exceeds the scan budget rather than returning a partial
/// — a truncated join would mis-attribute cost.
fn build_spawn_template_map(db: &Db) -> Result<(SpawnTemplateMap, u64), ErrorData> {
    let mut map: BTreeMap<String, (String, Option<u32>)> = BTreeMap::new();
    let mut scanned: u64 = 0;
    let mut start: Vec<u8> = Vec::new();
    loop {
        if usize::try_from(scanned).unwrap_or(usize::MAX) >= MAX_EVENT_SCAN_ROWS_PER_CALL {
            return Err(mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!(
                    "AGENT_COST_EVENT_SCAN_BUDGET_EXHAUSTED after {MAX_EVENT_SCAN_ROWS_PER_CALL} \
                     CF_AGENT_EVENTS rows building the spawn->template join; a truncated join \
                     would mis-attribute cost"
                ),
            ));
        }
        let (rows, more) = db
            .scan_cf_from(cf::CF_AGENT_EVENTS, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (_key, value) in &rows {
            scanned += 1;
            let record: AgentEventRecord = serde_json::from_slice(value).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("AGENT_EVENT_ROW_CORRUPT: row failed to decode: {error}"),
                )
            })?;
            if !matches!(record.kind, AgentEventKind::SpawnRequested) {
                continue;
            }
            let Some(spawn_id) = record.spawn_id.clone() else {
                continue;
            };
            let Some(template_id) = record
                .payload
                .get("template_id")
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            if template_id.is_empty() {
                continue;
            }
            let version = record
                .payload
                .get("template_version")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
            // First SpawnRequested per spawn wins (spawn ids are unique; this is
            // only defensive against a duplicated journal row).
            map.entry(spawn_id)
                .or_insert_with(|| (template_id.to_owned(), version));
        }
        if !more {
            break;
        }
        let Some((last, _value)) = rows.last() else {
            break;
        };
        start = key_after(last);
    }
    Ok((map, scanned))
}

// ----------------------------------------------------------------------------
// Small helpers
// ----------------------------------------------------------------------------

fn price_row_key(model_id: &str) -> String {
    format!("{PRICE_KEY_PREFIX}{model_id}")
}

/// Converts an operator-entered dollars-per-million-tokens rate into the
/// integer micro-USD-per-million-tokens stored on a [`ModelPrice`]. Rejects
/// negative, non-finite, or absurdly large inputs loudly.
fn usd_per_mtok_to_micro(usd: f64, field: &str) -> Result<u64, ErrorData> {
    if !usd.is_finite() {
        return Err(invalid_params(format!(
            "MODEL_PRICE_INVALID: {field} must be a finite number, got {usd}"
        )));
    }
    if usd < 0.0 {
        return Err(invalid_params(format!(
            "MODEL_PRICE_INVALID: {field} must be >= 0, got {usd}"
        )));
    }
    let micro = (usd * 1_000_000.0).round();
    if micro > u64::MAX as f64 {
        return Err(invalid_params(format!(
            "MODEL_PRICE_INVALID: {field} {usd} is implausibly large"
        )));
    }
    // round() yields a non-negative integral f64 within u64 range here.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(micro as u64)
}

fn validate_spawn_id(spawn_id: &str) -> Result<(), ErrorData> {
    if spawn_id.is_empty() || !spawn_id.starts_with("agent-spawn-") {
        return Err(invalid_params(format!(
            "AGENT_COST_SPAWN_ID_INVALID: spawn_id must start with `agent-spawn-`, got {spawn_id:?}"
        )));
    }
    if !spawn_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err(invalid_params(
            "AGENT_COST_SPAWN_ID_INVALID: spawn_id must be ASCII alphanumerics and dashes"
                .to_owned(),
        ));
    }
    Ok(())
}

/// Reads exactly one `CF_KV` row by full key. `scan_cf_prefix` returns every
/// row whose key *starts with* the argument, so the exact-key filter is
/// essential: `cost/price/v1/gpt-5` is a prefix of `cost/price/v1/gpt-5-mini`.
fn get_exact_kv_row(db: &Db, row_key: &str) -> Result<Option<Vec<u8>>, ErrorData> {
    Ok(db
        .scan_cf_prefix(cf::CF_KV, row_key.as_bytes())
        .map_err(|error| mcp_error(error.code(), error.to_string()))?
        .into_iter()
        .find_map(|(key, value)| (key == row_key.as_bytes()).then_some(value)))
}

fn readback_exact_kv_row(db: &Db, row_key: &str) -> Result<KvRowReadback, ErrorData> {
    let value = get_exact_kv_row(db, row_key)?.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("PRICE_ROW_READBACK_MISSING: wrote {row_key} but it is not present"),
        )
    })?;
    let digest = Sha256::digest(&value);
    let mut value_sha256 = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(value_sha256, "{byte:02x}");
    }
    Ok(KvRowReadback {
        cf_name: cf::CF_KV.to_owned(),
        row_key: row_key.to_owned(),
        value_len_bytes: value.len() as u64,
        value_sha256,
    })
}

fn invalid_params(detail: String) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, detail)
}

/// Smallest key strictly greater than `key` (append a zero byte). Used to page
/// past the last row of a scan window without re-reading it.
fn key_after(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
}
