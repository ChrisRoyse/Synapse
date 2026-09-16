//! Fleet metrics rollups over the agent event journal (#903).
//!
//! `agent_stats { since_ns?, until_ns?, spawn_id?, session_id?, group_by? }`
//! is the one queryable metrics surface that the dashboard analytics panels
//! and external agents both read, so the numbers can never disagree between
//! views. It is computed by a bounded, budget-guarded scan of
//! `CF_AGENT_EVENTS` (#897) — the durable, authoritative journal — and every
//! returned figure is re-derivable from physical journal rows.
//!
//! ## Why the journal is the only source
//!
//! The counters are derived on read by scanning the journal rows, never
//! maintained as a second incrementally-updated copy. A counter *is* the
//! folded sum of the rows it reports, so it reconciles with them exactly
//! (the #903 manual FSV requirement) with no reconciliation job to run. The honesty
//! figure `scanned_rows` is returned so a caller can confirm the rollup was
//! not truncated, and the scan errors loudly when its row budget is exhausted
//! — a truncated stat is silently wrong, so it is refused, never returned.
//!
//! ## What is derived, and from which physical fields
//!
//! - **actions / min** — `tool_call_started` (Claude `PreToolUse`) rows over
//!   the scope's observed time span. Rate is `None`, not infinity, when the
//!   span is a single instant.
//! - **tool-call latency P50/P95/P99** — the exact percentile of the
//!   `payload.duration_ms` sample carried on `tool_call_finished` rows
//!   (Claude `PostToolUse` reports it directly). Because the whole sample is
//!   held in memory (bounded by the scan budget), this is the true percentile
//!   of the observed calls, not a histogram-bucket approximation. `count`,
//!   `min`, and `max` are always returned so a low-sample P99 is visibly weak.
//! - **error rates by `error.type`** — rows carrying the OTel `error.type`
//!   attribute (e.g. `tool_failure` from `PostToolUseFailure`), grouped, with
//!   the rate over completed tool calls.
//! - **time-in-state** — reconstructed from the authoritative machine-emitted
//!   `state_changed` rows (#898, `origin = agent_state_machine`), which carry
//!   one row per real transition. Each state's duration is the gap to the next
//!   transition; the final open state is closed at the anchor's last journal
//!   row (a physical timestamp, never wall-clock), so the whole breakdown is
//!   re-derivable from rows.
//! - **lease contention** — `lease_acquired` / `lease_released` counts and the
//!   currently-held-open delta.
//! - **end-state distribution** — the OTel `StatusCode`-aligned `end_state` on
//!   terminal (`exited` / `killed`) rows.
//! - **tokens** — summed from the OTel `gen_ai.usage.*` attributes when a row
//!   carries them. Today's hook ingress (#899) does not populate usage on
//!   journal rows — Claude hooks and Codex notify carry none — so this is `0`
//!   in practice and the field is honest about that. PRICED cost is
//!   deliberately *not* computed here: its authoritative source is
//!   `CF_AGENT_TRANSCRIPTS` via `agent_cost`, and recomputing it from a second
//!   source is exactly the cross-view disagreement this tool exists to avoid.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use synapse_core::{AgentEndState, AgentEventKind, AgentEventRecord, error_codes};
use synapse_storage::{
    Db, agent_events::agent_event_scan_start, agent_events::decode_agent_event_key, cf, decode_json,
};

use super::{
    ErrorData, Json, Parameters, SynapseService, agent_events::unix_time_ns_now,
    agent_state::is_state_machine_row, mcp_error, tool, tool_router,
};

/// Upper bound on journal rows scanned in one `agent_stats` call. Mirrors the
/// cost rollup's budget so a truncated rollup is a loud error, never a
/// silently-wrong number.
const MAX_SCAN_ROWS_PER_CALL: usize = 200_000;

/// Bounded default lookback for the dashboard analytics panel (#1326/#1328).
/// The `agent_stats`/`agent_cost` MCP tools stay unbounded-capable, but the
/// dashboard snapshots default to this window so the panel reports CURRENT fleet
/// state (not a multi-week cumulative error rate) and never exhausts the scan
/// budget over the full retained journal. The window is surfaced in the snapshot
/// (`since_ns`/`first_event_ns`) so the UI can state it.
pub(crate) const DASHBOARD_ANALYTICS_WINDOW_NS: u64 = 7 * 24 * 60 * 60 * 1_000_000_000;

/// Rows pulled per `scan_cf_from` page.
const SCAN_CHUNK_ROWS: usize = 4_096;

/// Milliseconds in a minute, for per-minute rate denominators.
const MS_PER_MIN: f64 = 60_000.0;

// ----------------------------------------------------------------------------
// Parameters
// ----------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentStatsParams {
    /// Lower bound (inclusive) on event time, unix ns. Omit to start at the
    /// oldest retained journal row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ns: Option<u64>,
    /// Upper bound (exclusive) on event time, unix ns. Omit to include the
    /// newest journal row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ns: Option<u64>,
    /// Restrict to one spawned agent (`agent-spawn-*`). A row matches when its
    /// `spawn_id` equals this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    /// Restrict to one MCP session. A row matches when its `session_id` equals
    /// this value. Combined with `spawn_id`, both must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// `agent` to additionally return a per-agent breakdown keyed by anchor
    /// (spawn id, else session id); `fleet` (default) returns only the fleet
    /// rollup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
}

// ----------------------------------------------------------------------------
// Response
// ----------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentStatsResponse {
    pub ok: bool,
    pub now_ns: u64,
    /// Echoes the applied event-time window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until_ns: Option<u64>,
    /// In-window journal rows folded into the rollup — the honesty figure that
    /// lets a caller confirm the scan was not truncated.
    pub scanned_rows: u64,
    /// Distinct agents (anchors) seen in the window.
    pub agents_total: u64,
    pub fleet: ScopeStats,
    /// Per-agent breakdown, present only when `group_by = "agent"`. Ordered by
    /// anchor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_agent: Option<Vec<AgentScopeStats>>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentScopeStats {
    /// Attribution anchor: the spawn id for spawned agents, else the MCP
    /// session id.
    pub anchor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spawn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub stats: ScopeStats,
}

/// The metric rollup for one scope (the whole fleet, or one agent).
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeStats {
    pub events_total: u64,
    /// Count per `AgentEventKind` — the foundational figure every other count
    /// reconciles against.
    pub events_by_kind: BTreeMap<String, u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_event_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_ns: Option<u64>,
    /// `last_event_ns - first_event_ns`, the span the per-minute rates use.
    pub observed_span_ms: u64,
    pub actions: ActionStats,
    pub tool_latency_ms: LatencyStats,
    pub errors: ErrorStats,
    pub leases: LeaseStats,
    /// Milliseconds spent in each lifecycle state (`working`, `idle`,
    /// `needs_input`, `stuck`, …), reconstructed from authoritative transition
    /// rows. Ordered by state name.
    pub time_in_state_ms: BTreeMap<String, u64>,
    /// End-state of terminal rows, keyed `success` / `error` / `indeterminate`
    /// / `unreported`.
    pub end_states: BTreeMap<String, u64>,
    pub tokens: TokenStats,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_field_names)] // the shared `tool_calls_` prefix is the domain noun, not redundant
pub struct ActionStats {
    pub tool_calls_started: u64,
    pub tool_calls_finished: u64,
    /// Started tool calls per minute over `observed_span_ms`. `None` when the
    /// span is a single instant (a rate is undefined, never infinity).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls_started_per_min: Option<f64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LatencyStats {
    /// Number of `tool_call_finished` rows that carried a `duration_ms`. The
    /// statistical weight of the percentiles — a small count makes P99 noise.
    pub count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p50_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p95_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p99_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_ms: Option<f64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ErrorStats {
    /// `tool_call_finished` rows carrying an `error.type` attribute.
    pub errored_tool_calls: u64,
    /// `errored_tool_calls / tool_calls_finished`. `None` when no tool call
    /// finished in the window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_rate: Option<f64>,
    /// Count per OTel `error.type` among the same errored `tool_call_finished`
    /// rows counted by `errored_tool_calls`. Ordered by type.
    pub by_type: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseStats {
    pub acquired: u64,
    pub released: u64,
    /// `acquired - released`, floored at 0 — leases still held when the window
    /// ended (a contention signal).
    pub held_open: u64,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TokenStats {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub total: u64,
    /// `total` tokens per minute over `observed_span_ms`. `None` when the span
    /// is a single instant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_per_min: Option<f64>,
}

// ----------------------------------------------------------------------------
// Tool
// ----------------------------------------------------------------------------

#[tool_router(router = agent_stats_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Fleet metrics rollup over the durable agent event journal (CF_AGENT_EVENTS): per-agent and fleet actions/min, tool-call latency P50/P95/P99, error rates by error.type, time-in-state, lease contention, and end-state distribution. Counters are derived by a budget-guarded scan so they reconcile exactly with physical journal rows (scanned_rows is returned). Priced cost lives in agent_cost (transcript-authoritative); this tool surfaces token throughput from the journal only. Pass group_by=agent for a per-agent breakdown."
    )]
    pub async fn agent_stats(
        &self,
        params: Parameters<AgentStatsParams>,
    ) -> Result<Json<AgentStatsResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "agent_stats",
            "tool.invocation kind=agent_stats"
        );
        self.agent_stats_impl(params.0).map(Json)
    }
}

impl SynapseService {
    pub(crate) fn dashboard_agent_stats_snapshot(&self) -> Result<AgentStatsResponse, ErrorData> {
        // Bounded default window (#1326): the dashboard error rate must reflect
        // current fleet state, not a multi-week cumulative count over the entire
        // retained journal. The since_ns is echoed in the response so the UI can
        // state the window. Explicit MCP agent_stats calls remain unbounded.
        let since_ns =
            super::agent_events::unix_time_ns_now().saturating_sub(DASHBOARD_ANALYTICS_WINDOW_NS);
        self.agent_stats_impl(AgentStatsParams {
            since_ns: Some(since_ns),
            until_ns: None,
            spawn_id: None,
            session_id: None,
            group_by: Some("agent".to_owned()),
        })
    }

    fn agent_stats_db(&self) -> Result<std::sync::Arc<Db>, ErrorData> {
        let state = self.m3_state_handle();
        let mut guard = state.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned while opening agent stats storage",
            )
        })?;
        guard
            .ensure_storage()
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    fn agent_stats_impl(&self, params: AgentStatsParams) -> Result<AgentStatsResponse, ErrorData> {
        let include_per_agent = resolve_options(&params)?;
        let db = self.agent_stats_db()?;
        let (anchors, scanned_rows) = collect_anchors(&db, &params, MAX_SCAN_ROWS_PER_CALL)?;

        // Resolve each anchor, and fold the fleet aggregate from the same data.
        let mut fleet = ScopeAccumulator::default();
        let mut per_agent: Vec<AgentScopeStats> = Vec::with_capacity(anchors.len());
        for (anchor, acc) in &anchors {
            fleet.merge(&acc.scope);
            if include_per_agent {
                per_agent.push(AgentScopeStats {
                    anchor: anchor.clone(),
                    spawn_id: acc.spawn_id.clone(),
                    session_id: acc.session_id.clone(),
                    stats: acc.scope.finish(),
                });
            }
        }

        Ok(AgentStatsResponse {
            ok: true,
            now_ns: unix_time_ns_now(),
            since_ns: params.since_ns,
            until_ns: params.until_ns,
            scanned_rows,
            agents_total: anchors.len() as u64,
            fleet: fleet.finish(),
            per_agent: include_per_agent.then_some(per_agent),
        })
    }
}

/// Validates the request and resolves whether to emit a per-agent breakdown.
/// Pure, so the param contract is unit-testable without a service instance.
///
/// # Errors
///
/// Refuses an inverted/degenerate time window and an unknown `group_by`.
fn resolve_options(params: &AgentStatsParams) -> Result<bool, ErrorData> {
    if let (Some(since), Some(until)) = (params.since_ns, params.until_ns)
        && since >= until
    {
        return Err(invalid_params(format!(
            "AGENT_STATS_RANGE_INVALID: since_ns {since} must be < until_ns {until}"
        )));
    }
    match params.group_by.as_deref() {
        None | Some("fleet") => Ok(false),
        Some("agent") => Ok(true),
        Some(other) => Err(invalid_params(format!(
            "AGENT_STATS_GROUP_BY_INVALID: group_by must be `fleet` or `agent`, got {other:?}"
        ))),
    }
}

/// Budget-guarded chronological scan of `CF_AGENT_EVENTS`, folding each
/// in-window, filter-passing row into its anchor accumulator. Returns the
/// per-anchor map and the count of rows actually folded (`scanned_rows`, the
/// honesty figure). Errors loudly — never truncates — when `max_scan_rows` is
/// exhausted, because a truncated rollup is silently wrong. `max_scan_rows` is
/// a parameter so tests can exercise the budget path with a small bound.
fn collect_anchors(
    db: &Db,
    params: &AgentStatsParams,
    max_scan_rows: usize,
) -> Result<(BTreeMap<String, AnchorAccumulator>, u64), ErrorData> {
    let mut anchors: BTreeMap<String, AnchorAccumulator> = BTreeMap::new();
    // `examined` guards total in-window iteration cost; `scanned_rows` counts
    // rows actually folded, so the returned honesty figure obeys the invariant
    // `fleet.events_total == scanned_rows`.
    let mut examined: u64 = 0;
    let mut scanned_rows: u64 = 0;
    let mut start: Vec<u8> = agent_event_scan_start(params.since_ns.unwrap_or(0));
    'paging: loop {
        let (rows, more) = db
            .scan_cf_from(cf::CF_AGENT_EVENTS, &start, SCAN_CHUNK_ROWS)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        if rows.is_empty() {
            break;
        }
        for (key, value) in &rows {
            let (ts_ns, seq) = decode_agent_event_key(key)
                .map_err(|error| mcp_error(error.code(), error.to_string()))?;
            // Keys iterate chronologically: once we pass `until` the rest of
            // the column family is out of window, so stop entirely.
            if let Some(until) = params.until_ns
                && ts_ns >= until
            {
                break 'paging;
            }
            // Budget is checked against in-window rows examined, before any
            // decode: a window larger than the budget can hold would
            // under-report, so refuse it rather than truncate.
            if usize::try_from(examined).unwrap_or(usize::MAX) >= max_scan_rows {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "AGENT_STATS_SCAN_BUDGET_EXHAUSTED after {max_scan_rows} \
                         CF_AGENT_EVENTS rows; pass spawn_id/session_id or a narrower \
                         since_ns/until_ns window — a truncated rollup would under-report"
                    ),
                ));
            }
            examined += 1;
            let record: AgentEventRecord = decode_json(value).map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "AGENT_EVENT_ROW_CORRUPT: row at ts_ns={ts_ns} seq={seq} failed to \
                         decode: {error}"
                    ),
                )
            })?;
            // Anchor/filters use the row's own ids; a record always carries at
            // least one (the journal writer's validate() guarantees it).
            if let Some(want) = params.spawn_id.as_deref()
                && record.spawn_id.as_deref() != Some(want)
            {
                continue;
            }
            if let Some(want) = params.session_id.as_deref()
                && record.session_id.as_deref() != Some(want)
            {
                continue;
            }
            let Some(anchor) = anchor_of(&record) else {
                // A row with neither id cannot exist past validate(); guard
                // rather than silently mis-attribute it.
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!(
                        "AGENT_EVENT_ROW_UNANCHORED: row at ts_ns={ts_ns} seq={seq} carries \
                         neither spawn_id nor session_id"
                    ),
                ));
            };
            scanned_rows += 1;
            anchors
                .entry(anchor)
                .or_default()
                .observe(ts_ns, seq, &record);
        }
        if !more {
            break;
        }
        let Some((last, _value)) = rows.last() else {
            break;
        };
        start = key_after(last);
    }
    Ok((anchors, scanned_rows))
}

/// The attribution anchor: spawn id for spawned agents, else the MCP session
/// id. Mirrors the #898 state machine so a per-agent breakdown lines up with
/// `session_list` reads.
fn anchor_of(record: &AgentEventRecord) -> Option<String> {
    record
        .spawn_id
        .clone()
        .or_else(|| record.session_id.clone())
}

// ----------------------------------------------------------------------------
// Accumulation
// ----------------------------------------------------------------------------

/// Per-anchor fold state: the rollup counters plus the identity fields needed
/// to label a per-agent row.
#[derive(Debug, Default)]
struct AnchorAccumulator {
    spawn_id: Option<String>,
    session_id: Option<String>,
    scope: ScopeAccumulator,
}

impl AnchorAccumulator {
    fn observe(&mut self, ts_ns: u64, seq: u32, record: &AgentEventRecord) {
        if self.spawn_id.is_none() {
            self.spawn_id.clone_from(&record.spawn_id);
        }
        if self.session_id.is_none() {
            self.session_id.clone_from(&record.session_id);
        }
        self.scope.observe(ts_ns, seq, record);
    }
}

/// The mutable rollup state for one scope. `merge` folds an anchor's scope into
/// the fleet aggregate; `finish` resolves it to the serializable [`ScopeStats`].
#[derive(Debug, Default)]
struct ScopeAccumulator {
    events_total: u64,
    events_by_kind: BTreeMap<String, u64>,
    first_event_ns: Option<u64>,
    last_event_ns: Option<u64>,
    tool_calls_started: u64,
    tool_calls_finished: u64,
    errored_tool_calls: u64,
    errors_by_type: BTreeMap<String, u64>,
    lease_acquired: u64,
    lease_released: u64,
    durations_ms: Vec<u64>,
    end_states: BTreeMap<String, u64>,
    token_input: u64,
    token_output: u64,
    token_cache_read: u64,
    token_cache_creation: u64,
    /// Authoritative machine-emitted transitions for this scope, kept ordered
    /// for time-in-state reconstruction. Each is `(ts_ns, seq, state_to)`.
    transitions: Vec<(u64, u32, String)>,
    /// Fleet-only: the summed per-anchor time-in-state, folded in [`Self::merge`].
    /// Empty for a single anchor, whose timeline comes from `transitions`.
    merged_time_in_state: BTreeMap<String, u64>,
}

impl ScopeAccumulator {
    fn observe(&mut self, ts_ns: u64, seq: u32, record: &AgentEventRecord) {
        self.events_total += 1;
        *self
            .events_by_kind
            .entry(kind_str(record.kind).to_owned())
            .or_insert(0) += 1;
        self.first_event_ns = Some(self.first_event_ns.map_or(ts_ns, |v| v.min(ts_ns)));
        self.last_event_ns = Some(self.last_event_ns.map_or(ts_ns, |v| v.max(ts_ns)));

        match record.kind {
            AgentEventKind::ToolCallStarted => self.tool_calls_started += 1,
            AgentEventKind::ToolCallFinished => {
                self.tool_calls_finished += 1;
                if let Some(error_type) = record.attributes.error_type.as_deref() {
                    self.errored_tool_calls += 1;
                    *self
                        .errors_by_type
                        .entry(error_type.to_owned())
                        .or_insert(0) += 1;
                }
                if let Some(duration_ms) = record
                    .payload
                    .get("duration_ms")
                    .and_then(serde_json::Value::as_u64)
                {
                    self.durations_ms.push(duration_ms);
                }
            }
            AgentEventKind::LeaseAcquired => self.lease_acquired += 1,
            AgentEventKind::LeaseReleased => self.lease_released += 1,
            AgentEventKind::Exited | AgentEventKind::Killed => {
                let label = record
                    .end_state
                    .map_or("unreported", end_state_str)
                    .to_owned();
                *self.end_states.entry(label).or_insert(0) += 1;
            }
            _ => {}
        }

        // Usage tokens fold from whichever rows carry them (forward-compatible
        // with #950; zero under today's content-free hook ingress).
        self.token_input = self
            .token_input
            .saturating_add(record.attributes.usage_input_tokens.unwrap_or(0));
        self.token_output = self
            .token_output
            .saturating_add(record.attributes.usage_output_tokens.unwrap_or(0));
        self.token_cache_read = self
            .token_cache_read
            .saturating_add(record.attributes.usage_cache_read_input_tokens.unwrap_or(0));
        self.token_cache_creation = self.token_cache_creation.saturating_add(
            record
                .attributes
                .usage_cache_creation_input_tokens
                .unwrap_or(0),
        );

        // Only authoritative machine transitions define the state timeline;
        // sender-pushed state_changed rows are superseded by the reduced row
        // the machine emits, so counting both would double the timeline.
        if is_state_machine_row(record)
            && let Some(state_to) = record.state_to.clone()
        {
            self.transitions.push((ts_ns, seq, state_to));
        }
    }

    /// Folds another scope (one anchor) into this aggregate. Vectors are
    /// concatenated so the fleet percentile is over every agent's calls and the
    /// fleet time-in-state sums each agent's intervals.
    fn merge(&mut self, other: &Self) {
        self.events_total += other.events_total;
        merge_counts(&mut self.events_by_kind, &other.events_by_kind);
        self.first_event_ns = min_opt(self.first_event_ns, other.first_event_ns);
        self.last_event_ns = max_opt(self.last_event_ns, other.last_event_ns);
        self.tool_calls_started += other.tool_calls_started;
        self.tool_calls_finished += other.tool_calls_finished;
        self.errored_tool_calls += other.errored_tool_calls;
        merge_counts(&mut self.errors_by_type, &other.errors_by_type);
        self.lease_acquired += other.lease_acquired;
        self.lease_released += other.lease_released;
        self.durations_ms.extend_from_slice(&other.durations_ms);
        merge_counts(&mut self.end_states, &other.end_states);
        self.token_input = self.token_input.saturating_add(other.token_input);
        self.token_output = self.token_output.saturating_add(other.token_output);
        self.token_cache_read = self.token_cache_read.saturating_add(other.token_cache_read);
        self.token_cache_creation = self
            .token_cache_creation
            .saturating_add(other.token_cache_creation);
        // Per-anchor time-in-state is summed here (the fleet timeline is the
        // sum of each agent's, not a single interleaved timeline), so resolve
        // the other scope's intervals before extending. We keep raw transitions
        // separate per-anchor by folding the *resolved* durations instead.
        let other_durations = other.time_in_state_ms();
        merge_counts(&mut self.merged_time_in_state, &other_durations);
    }

    /// Reconstructs milliseconds-in-state for THIS scope's own transitions.
    /// Duration of a state is the gap to the next transition; the final open
    /// state is closed at this scope's last journal row — a physical timestamp,
    /// so the breakdown stays re-derivable from rows. An anchor with fewer than
    /// two anchoring points (no transition, or one transition and no later
    /// row) contributes nothing, honestly.
    fn time_in_state_ms(&self) -> BTreeMap<String, u64> {
        let mut out: BTreeMap<String, u64> = BTreeMap::new();
        let mut ordered = self.transitions.clone();
        ordered.sort_by_key(|&(ts_ns, seq, _)| (ts_ns, seq));
        for window in ordered.windows(2) {
            let (start_ts, _seq, state) = &window[0];
            let (end_ts, _seq2, _next) = &window[1];
            let ms = end_ts.saturating_sub(*start_ts) / 1_000_000;
            *out.entry(state.clone()).or_insert(0) += ms;
        }
        if let Some((last_ts, _seq, last_state)) = ordered.last()
            && let Some(scope_end) = self.last_event_ns
            && scope_end > *last_ts
        {
            let ms = scope_end.saturating_sub(*last_ts) / 1_000_000;
            *out.entry(last_state.clone()).or_insert(0) += ms;
        }
        out
    }

    fn finish(&self) -> ScopeStats {
        let observed_span_ms = match (self.first_event_ns, self.last_event_ns) {
            (Some(first), Some(last)) => last.saturating_sub(first) / 1_000_000,
            _ => 0,
        };
        let span_min = (observed_span_ms > 0).then(|| observed_span_ms as f64 / MS_PER_MIN);

        let tool_calls_started_per_min = span_min.map(|m| self.tool_calls_started as f64 / m);
        let token_total = self
            .token_input
            .saturating_add(self.token_output)
            .saturating_add(self.token_cache_read)
            .saturating_add(self.token_cache_creation);
        let tokens_per_min = span_min.map(|m| token_total as f64 / m);

        let error_rate = (self.tool_calls_finished > 0)
            .then(|| self.errored_tool_calls as f64 / self.tool_calls_finished as f64);

        // For the fleet aggregate the resolved per-anchor durations were summed
        // into `merged_time_in_state`; for a single anchor that map is empty
        // and the timeline comes from this scope's own transitions.
        let time_in_state_ms = if self.merged_time_in_state.is_empty() {
            self.time_in_state_ms()
        } else {
            self.merged_time_in_state.clone()
        };

        ScopeStats {
            events_total: self.events_total,
            events_by_kind: self.events_by_kind.clone(),
            first_event_ns: self.first_event_ns,
            last_event_ns: self.last_event_ns,
            observed_span_ms,
            actions: ActionStats {
                tool_calls_started: self.tool_calls_started,
                tool_calls_finished: self.tool_calls_finished,
                tool_calls_started_per_min,
            },
            tool_latency_ms: latency_stats(&self.durations_ms),
            errors: ErrorStats {
                errored_tool_calls: self.errored_tool_calls,
                error_rate,
                by_type: self.errors_by_type.clone(),
            },
            leases: LeaseStats {
                acquired: self.lease_acquired,
                released: self.lease_released,
                held_open: self.lease_acquired.saturating_sub(self.lease_released),
            },
            time_in_state_ms,
            end_states: self.end_states.clone(),
            tokens: TokenStats {
                input: self.token_input,
                output: self.token_output,
                cache_read: self.token_cache_read,
                cache_creation: self.token_cache_creation,
                total: token_total,
                tokens_per_min,
            },
        }
    }
}

/// Computes the exact latency percentiles of the observed duration sample.
/// Because the full sample is in memory, this is the true percentile of the
/// calls, not a histogram approximation. Uses the nearest-rank method: for a
/// sorted ascending sample of `n` values and percentile `p`, the rank is
/// `ceil(p/100 * n)` and the result is the value at that 1-based rank. The
/// method is exact, deterministic, and hand-verifiable during manual FSV.
fn latency_stats(durations_ms: &[u64]) -> LatencyStats {
    if durations_ms.is_empty() {
        return LatencyStats {
            count: 0,
            p50_ms: None,
            p95_ms: None,
            p99_ms: None,
            min_ms: None,
            max_ms: None,
            mean_ms: None,
        };
    }
    let mut sorted = durations_ms.to_vec();
    sorted.sort_unstable();
    let count = sorted.len() as u64;
    let sum: u128 = sorted.iter().map(|&v| u128::from(v)).sum();
    #[allow(clippy::cast_precision_loss)]
    let mean_ms = sum as f64 / sorted.len() as f64;
    LatencyStats {
        count,
        p50_ms: percentile_nearest_rank(&sorted, 50),
        p95_ms: percentile_nearest_rank(&sorted, 95),
        p99_ms: percentile_nearest_rank(&sorted, 99),
        min_ms: sorted.first().copied(),
        max_ms: sorted.last().copied(),
        mean_ms: Some(mean_ms),
    }
}

/// Nearest-rank percentile over a pre-sorted ascending slice. `p` is in
/// `1..=100`. Returns `None` for an empty slice.
fn percentile_nearest_rank(sorted: &[u64], p: u32) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let n = sorted.len();
    // rank = ceil(p/100 * n), as integer arithmetic: ceil(p*n / 100).
    let rank = (u64::from(p) * n as u64).div_ceil(100);
    // rank is in 1..=n for p in 1..=100; clamp defensively.
    let idx = (rank.max(1) as usize - 1).min(n - 1);
    Some(sorted[idx])
}

fn kind_str(kind: AgentEventKind) -> &'static str {
    match kind {
        AgentEventKind::SpawnRequested => "spawn_requested",
        AgentEventKind::SpawnReady => "spawn_ready",
        AgentEventKind::StateChanged => "state_changed",
        AgentEventKind::ToolCallStarted => "tool_call_started",
        AgentEventKind::ToolCallFinished => "tool_call_finished",
        AgentEventKind::TurnStarted => "turn_started",
        AgentEventKind::TurnFinished => "turn_finished",
        AgentEventKind::MessageSent => "message_sent",
        AgentEventKind::MessageReceived => "message_received",
        AgentEventKind::LeaseAcquired => "lease_acquired",
        AgentEventKind::LeaseReleased => "lease_released",
        AgentEventKind::Interrupted => "interrupted",
        AgentEventKind::Killed => "killed",
        AgentEventKind::Exited => "exited",
    }
}

fn end_state_str(state: AgentEndState) -> &'static str {
    match state {
        AgentEndState::Success => "success",
        AgentEndState::Error => "error",
        AgentEndState::Indeterminate => "indeterminate",
    }
}

fn merge_counts(into: &mut BTreeMap<String, u64>, from: &BTreeMap<String, u64>) {
    for (key, value) in from {
        *into.entry(key.clone()).or_insert(0) += value;
    }
}

const fn min_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a < b { a } else { b }),
        (Some(v), None) | (None, Some(v)) => Some(v),
        (None, None) => None,
    }
}

const fn max_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a > b { a } else { b }),
        (Some(v), None) | (None, Some(v)) => Some(v),
        (None, None) => None,
    }
}

fn invalid_params(detail: String) -> ErrorData {
    mcp_error(error_codes::TOOL_PARAMS_INVALID, detail)
}

/// Smallest key strictly greater than `key` (append a zero byte). Pages past
/// the last row of a scan window without re-reading it.
fn key_after(key: &[u8]) -> Vec<u8> {
    let mut next = key.to_vec();
    next.push(0);
    next
}

#[cfg(test)]
mod tests;
