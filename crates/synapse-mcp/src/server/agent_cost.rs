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
//! exactly (the #901 FSV requirement) with no reconciliation job to run.
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
//! Per-turn deltas and exact multi-model attribution from Claude `modelUsage`
//! are tracked as follow-ups (#900 stores only the top-level result usage);
//! the source-cost reconciliation delta surfaces any multi-model gap rather
//! than hiding it.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use synapse_core::{
    AgentTranscriptRecord, BillableUsage, CostBreakdown, CostOutcome, MODEL_PRICE_VERSION,
    ModelPrice, TranscriptSource, error_codes,
};
use synapse_storage::{
    Db, agent_transcripts::agent_transcript_spawn_prefix, agent_transcripts::decode_agent_transcript_key,
    cf,
};

use super::{
    ErrorData, Json, Parameters, SynapseService, agent_events::unix_time_ns_now, mcp_error, tool,
    tool_router,
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
    /// Cache-creation (write) price, US dollars per million tokens.
    #[serde(default)]
    pub cache_creation_usd_per_mtok: f64,
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

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentCostParams {
    /// Restrict the rollup to one spawn (`agent-spawn-*`). Omit for a
    /// fleet-wide rollup over every spawn's transcripts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    /// Lower bound (inclusive) on transcript-row ingestion time, unix ns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ns: Option<u64>,
    /// Upper bound (exclusive) on transcript-row ingestion time, unix ns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ns: Option<u64>,
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

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentCostResponse {
    pub ok: bool,
    pub now_ns: u64,
    /// Echoes the time window applied to transcript rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until_ns: Option<u64>,
    /// Total transcript rows scanned — the honesty figure that lets a caller
    /// confirm the rollup was not truncated.
    pub scanned_rows: u64,
    pub fleet: AgentFleetCost,
    pub per_model: Vec<AgentModelCost>,
    pub per_spawn: Vec<AgentSpawnCost>,
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

impl SynapseService {
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
        if let Some(spawn_id) = params.spawn_id.as_deref() {
            validate_spawn_id(spawn_id)?;
            let rows = db
                .scan_cf_prefix(cf::CF_AGENT_TRANSCRIPTS, &agent_transcript_spawn_prefix(spawn_id))
                .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            for (key, value) in rows {
                scanned_rows += 1;
                ingest_row(&mut spawns, &key, &value, params.since_ns, params.until_ns)?;
            }
        } else {
            let mut start: Vec<u8> = Vec::new();
            loop {
                if usize::try_from(scanned_rows).unwrap_or(usize::MAX) >= MAX_SCAN_ROWS_PER_CALL {
                    return Err(mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        format!(
                            "AGENT_COST_SCAN_BUDGET_EXHAUSTED after {MAX_SCAN_ROWS_PER_CALL} \
                             CF_AGENT_TRANSCRIPTS rows; pass spawn_id or a narrower since_ns/\
                             until_ns window — a truncated rollup would under-report cost"
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

        for (spawn_id, acc) in spawns {
            fleet.spawns_total += 1;
            let resolved = acc.resolve()?;
            let Some(resolved) = resolved else {
                fleet.spawns_incomplete += 1;
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

            let model_label = resolved
                .model
                .clone()
                .unwrap_or_else(|| "unknown".to_owned());
            let priced = prices.get(&model_label);
            let (cost_outcome, computed_micro) = match priced {
                Some(price) => {
                    let breakdown = price.cost_micro_usd(&usage).map_err(|detail| {
                        mcp_error(error_codes::TOOL_INTERNAL_ERROR, detail)
                    })?;
                    let total = breakdown.total_micro_usd;
                    fleet.computed_micro_usd = fleet.computed_micro_usd.saturating_add(total);
                    (CostOutcome::Priced { cost: breakdown }, Some(total))
                }
                None => {
                    unpriced_set.insert(model_label.clone());
                    (
                        CostOutcome::Unpriced {
                            model_id: model_label.clone(),
                        },
                        None,
                    )
                }
            };

            let reconciliation_delta = match (resolved.source_reported_micro_usd, computed_micro) {
                (Some(src), Some(computed)) => {
                    Some(i64::try_from(src).unwrap_or(i64::MAX)
                        - i64::try_from(computed).unwrap_or(i64::MAX))
                }
                _ => None,
            };

            // Fold into per-model aggregate.
            let entry = per_model
                .entry(model_label.clone())
                .or_insert_with(|| AgentModelCost {
                    model: model_label.clone(),
                    priced: priced.is_some(),
                    spawns: 0,
                    usage: BillableUsage::default(),
                    total_tokens: 0,
                    computed_micro_usd: if priced.is_some() { Some(0) } else { None },
                });
            entry.spawns += 1;
            add_usage(&mut entry.usage, &usage);
            entry.total_tokens = entry.total_tokens.saturating_add(total_tokens);
            if let (Some(existing), Some(add)) = (entry.computed_micro_usd.as_mut(), computed_micro) {
                *existing = existing.saturating_add(add);
            }

            per_spawn.push(AgentSpawnCost {
                spawn_id,
                source: resolved.source_label(),
                model: resolved.model,
                status: "complete".to_owned(),
                usage,
                total_tokens,
                cost: cost_outcome,
                source_reported_micro_usd: resolved.source_reported_micro_usd,
                reconciliation_delta_micro_usd: reconciliation_delta,
                authoritative_line_no: Some(resolved.line_no),
            });
        }

        fleet.unpriced_models = unpriced_set.into_iter().collect();
        let per_model_vec = per_model.into_values().collect();

        Ok(AgentCostResponse {
            ok: true,
            now_ns: unix_time_ns_now(),
            since_ns: params.since_ns,
            until_ns: params.until_ns,
            scanned_rows,
            fleet,
            per_model: per_model_vec,
            per_spawn,
        })
    }

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
    mixed_source: bool,
}

#[derive(Clone, Debug)]
struct ClaudeResult {
    line_no: u64,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
    cost_micro_usd: Option<u64>,
}

#[derive(Clone, Debug, Default)]
struct CodexMax {
    line_no: u64,
    input: u64,
    output: u64,
    cached: u64,
}

/// What a spawn resolves to once its authoritative row is chosen.
struct ResolvedSpawn {
    source: TranscriptSource,
    model: Option<String>,
    usage: BillableUsage,
    source_reported_micro_usd: Option<u64>,
    line_no: u64,
}

impl ResolvedSpawn {
    fn source_label(&self) -> Option<String> {
        Some(source_label(self.source))
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
                    cost_micro_usd: usage.total_cost_micro_usd,
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
            _ => {}
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
        match (self.source, &self.claude_result, &self.codex_max) {
            (Some(TranscriptSource::ClaudeStreamJson), Some(result), _) => Ok(Some(ResolvedSpawn {
                source: TranscriptSource::ClaudeStreamJson,
                model: self.model.clone(),
                usage: BillableUsage::from_claude(
                    result.input,
                    result.output,
                    result.cache_read,
                    result.cache_creation,
                ),
                source_reported_micro_usd: result.cost_micro_usd,
                line_no: result.line_no,
            })),
            (Some(TranscriptSource::CodexExecJson), _, Some(codex)) => {
                let usage =
                    BillableUsage::from_codex_cumulative(codex.input, codex.output, codex.cached)
                        .map_err(|detail| mcp_error(error_codes::TOOL_INTERNAL_ERROR, detail))?;
                Ok(Some(ResolvedSpawn {
                    source: TranscriptSource::CodexExecJson,
                    model: self.model.clone(),
                    usage,
                    source_reported_micro_usd: None,
                    line_no: codex.line_no,
                }))
            }
            _ => Ok(None),
        }
    }
}

fn acc_source_label(acc: &SpawnAccumulator) -> Option<String> {
    acc.source.map(source_label)
}

fn source_label(source: TranscriptSource) -> String {
    match source {
        TranscriptSource::ClaudeStreamJson => "claude_stream_json".to_owned(),
        TranscriptSource::CodexExecJson => "codex_exec_json".to_owned(),
    }
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
    spawns
        .entry(spawn_id)
        .or_default()
        .observe(&record)
}

fn add_usage(acc: &mut BillableUsage, add: &BillableUsage) {
    acc.input_tokens = acc.input_tokens.saturating_add(add.input_tokens);
    acc.output_tokens = acc.output_tokens.saturating_add(add.output_tokens);
    acc.cache_read_tokens = acc.cache_read_tokens.saturating_add(add.cache_read_tokens);
    acc.cache_creation_tokens = acc
        .cache_creation_tokens
        .saturating_add(add.cache_creation_tokens);
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

#[cfg(test)]
mod tests;
