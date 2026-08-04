use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{CaptureRuntimeReadback, ObservationCaptureConfig, PerceptionMode, ProfileId};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Health {
    pub ok: bool,
    pub version: String,
    pub build: String,
    /// OS process ID of the daemon serving this payload. Lets bridges and
    /// `doctor` confirm which process answered and that all clients share one
    /// daemon.
    pub pid: u32,
    pub uptime_s: u64,
    /// Number of currently advertised MCP tools after schema sanitization.
    pub tool_count: usize,
    /// Stable SHA-256 fingerprint of the currently advertised sanitized tools/list
    /// surface, sorted by tool name.
    pub tool_surface_sha256: String,
    /// Current sanitized tool names, sorted for deterministic stale-client
    /// readback.
    pub tool_names: Vec<String>,
    pub subsystems: BTreeMap<String, SubsystemHealth>,
}

/// How a Calyx tuning knob's configured value relates to the value the daemon
/// actually behaves according to (#1883).
///
/// A knob is only `LoadBearing` when the configured number is the one the code
/// reads. Anything else is named for what it is, because an operator who tunes
/// an inert knob and restarts observes no change and has no way to tell that
/// from "the change had no effect on this workload".
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CalyxTuningKnobEnforcement {
    /// The configured value is read on the path it names. Tuning it changes
    /// behaviour.
    LoadBearing,
    /// The effective value is a hardcoded constant somewhere else. Tuning this
    /// knob changes nothing; `effective_value_declared_at` names the code that
    /// actually decides.
    #[default]
    InertHardcodedElsewhere,
    /// Nothing anywhere reads this knob, not even a hardcoded twin. It is
    /// validated, carried and reported, and that is all.
    InertNoConsumer,
}

impl CalyxTuningKnobEnforcement {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LoadBearing => "load_bearing",
            Self::InertHardcodedElsewhere => "inert_hardcoded_elsewhere",
            Self::InertNoConsumer => "inert_no_consumer",
        }
    }

    /// Whether the configured value reaches any consumer.
    #[must_use]
    pub const fn is_load_bearing(self) -> bool {
        matches!(self, Self::LoadBearing)
    }
}

/// One Calyx tuning knob reported as a measurement rather than a decoration
/// (#1883).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CalyxTuningKnobStatus {
    /// Config field name as it appears in `SynapseCalyxTuningConfig`, e.g.
    /// `fusion_k`.
    pub knob: String,
    /// The configured value, rendered exactly as the daemon holds it.
    pub configured_value: String,
    pub enforcement: CalyxTuningKnobEnforcement,
    /// The exact code location that decides this quantity today. For a
    /// load-bearing knob that is the consumer; for an inert one it is the
    /// hardcoded constant that wins instead, `none` when there is no consumer
    /// at all.
    pub effective_value_declared_at: String,
    /// The issue that must land before an inert knob becomes load bearing.
    /// Empty for a load-bearing knob.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub blocked_by_issue: String,
    /// Plain statement of what tuning this knob does today.
    pub effect_of_tuning: String,
}

/// One row-table read-guard call site's tallies (#1952 ask 3).
///
/// `holds` counts every acquisition. `over_budget_holds` counts only the subset
/// that exceeded the 25 ms budget and emitted a slow-guard event. The pair is
/// the point: `holds=0` means the path did not run, `holds=4812,
/// over_budget_holds=0` means it ran 4,812 times and stayed inside the budget.
/// The log alone renders both as silence.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CalyxRowGuardSiteStatus {
    /// Call site name, matching the `site` field of
    /// `CALYX_ASTER_ROW_READ_GUARD_SLOW` so log and census join directly.
    pub site: String,
    pub holds: u64,
    pub total_held_us: u64,
    pub max_held_us: u64,
    /// Mean hold in microseconds, absent when the site never ran. Deliberately
    /// not `0.0` — a site with no holds has no mean, and zero reads as "fast".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_held_us: Option<f64>,
    pub over_budget_holds: u64,
    /// Over-budget holds whose thread was descheduled for most of the hold.
    pub starved_holds: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubsystemHealth {
    pub status: String,
    pub detail: Option<String>,
    /// Last persisted six-tier Oracle readiness snapshot. Health reads this
    /// artifact without recomputation or mutation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle_readiness: Option<serde_json::Value>,
    /// Full 40-hex commit the running binary was compiled from (#1971).
    ///
    /// `None` means the binary was built with
    /// `SYNAPSE_BUILD_ALLOW_UNKNOWN_PROVENANCE=1` and genuinely cannot name its
    /// own commit; the subsystem reports `error` in that case rather than
    /// substituting a plausible-looking value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_ref: Option<String>,
    /// `clean` | `dirty` | `unknown` at the moment the binary was compiled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_tree_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_profile: Option<String>,
    /// Checkout the binary was built from, recorded at compile time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_source_dir: Option<String>,
    /// Commit that checkout is on *now*, read live from `.git` at health time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_checkout_commit: Option<String>,
    /// Why the checkout's current commit could not be read, when it could not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_checkout_unavailable_reason: Option<String>,
    /// True exactly when the running bytes and the checkout are the same commit.
    ///
    /// This is the reading that was missing: a daemon many commits behind `main`
    /// was indistinguishable from a current one, so the whole accumulated delta
    /// shipped at once on the next unrelated deploy (#1971).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_matches_checkout: Option<bool>,
    /// Path, size and mtime of the executable actually serving this payload, so
    /// a redeploy that did not replace the installed bytes is visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_exe_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_exe_len: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_exe_modified_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_profile_id: Option<ProfileId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_maintenance_supported: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_maintenance_unsupported_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_sizes: Option<BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_cf_sizes_skipped_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_task_running: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_tick_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_task_running: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_tick_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_task_running: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_probe_active: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_probe_observed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_last_free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_last_level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_started_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_completed_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_error_classification: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_attempt_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_next_retry_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_retry_exhausted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_cf_readback_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_total_examined_rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_total_evicted_rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_after_value_sum: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storage_gc_last_unsupported_policy_skips: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_last_started_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_last_completed_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_last_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_last_error_classification: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_last_attempt_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_next_retry_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_retry_exhausted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_last_successful_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_last_successful_cf_readback_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_last_successful_total_examined_rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_last_successful_total_evicted_rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_checkpoint_last_successful_after_value_sum: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_last_started_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_last_completed_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_last_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_pressure_last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_open: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_identity_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_machine_salt_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_lock_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_pid_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_latest_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_last_recovered_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_torn_tail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_last_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_last_calyx_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_remediation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_bit_floor_bits: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_correlation_ceiling: Option<f32>,
    /// #1883: every configurable Calyx tuning knob, each reported WITH whether
    /// anything actually reads it. The nine `calyx_*` knobs that used to be
    /// printed here as bare numbers were validated, lowered and echoed, and read
    /// by nothing — the values that took effect were hardcoded elsewhere. A
    /// number with no enforcement state attached misrepresents an inert config
    /// field as a measured one, so the value never travels without its verdict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calyx_tuning_knobs: Vec<CalyxTuningKnobStatus>,
    /// Count of knobs in `calyx_tuning_knobs` whose configured value does not
    /// reach any consumer. Non-zero means the tuning surface is partly
    /// decorative and the subsystem must not report a clean `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_inert_tuning_knob_count: Option<usize>,

    // --- row-table read-guard census (issue #1952 ask 3, #1950 ask 2) ---
    // `CALYX_ASTER_ROW_READ_GUARD_SLOW` only fires above a 25 ms budget, so a
    // path that runs constantly and stays inside it is indistinguishable from a
    // path that never runs: both are silence. That ambiguity is what left #1952
    // ask 3 unanswerable after its conversion produced zero events. These
    // counters tally EVERY hold, so a zero is an observation.
    /// Per-call-site row-guard tallies. Every declared site is present, whether
    /// or not it has ever been taken.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calyx_row_guard_sites: Vec<CalyxRowGuardSiteStatus>,
    /// Sites with at least one hold. Below the count of declared sites means
    /// some read path has not executed since open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_row_guard_sites_exercised: Option<usize>,
    /// Total holds across every site since open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_row_guard_holds_total: Option<u64>,
    /// Total holds that exceeded the budget and emitted a slow-guard event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_row_guard_over_budget_total: Option<u64>,
    /// Over-budget holds whose thread was not running for most of the hold
    /// (#1955). Non-zero means latency numbers from this window measure machine
    /// load, not the vault, and must not be tuned against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_row_guard_starved_total: Option<u64>,

    // --- persisted search generation (issue #1891) ---
    // Every recall path depends on this generation. Before these fields, an
    // absent or badly-lagged index was announced by nothing: the first observer
    // was whoever called `find` and read the error. An absent generation on a
    // vault that holds derived rows is an `error`, not silence.
    /// `absent` | `rebuild_required` | `lagging` | `built`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_state: Option<String>,
    /// Active durable panel the generation belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_panel_version: Option<u32>,
    /// Expected manifest path, reported even when the file is absent so the
    /// operator can go look at the exact location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_manifest_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_manifest_present: Option<bool>,
    /// Vault sequence the generation was built at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_built_at_seq: Option<u64>,
    /// How far the generation is behind the vault, in sequences.
    ///
    /// Informational. It is NOT the quantity the limit below is enforced on and
    /// is not a proxy for it — 739 sequences carried 17,785 changed keys on the
    /// production vault. Read `calyx_search_generation_delta_changed_keys`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_seq_lag: Option<u64>,
    /// Distinct changed keys between the generation and the current snapshot —
    /// the exact quantity `max_reconciled_delta_keys` bounds, and therefore the
    /// only number that answers whether a query can reconcile the generation.
    /// Measured by the derived-state maintainer on its tick, not on this
    /// request; `None` means not measured, never zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_delta_changed_keys: Option<u64>,
    /// Composition of the measured delta: `Base` keys scanned across all panels,
    /// how many belong to this panel, how many were another panel's churn, and
    /// each slot CF's contribution (#1901). A count without this could not say
    /// whether the generation is stale or a bystander.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_delta_composition: Option<String>,
    /// When that delta was measured, so a stale measurement is visible as one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_delta_measured_at_unix_ms: Option<u64>,
    /// Bounded delta-reconciliation budget; past it every query fails closed
    /// with `CALYX_SEARCH_DELTA_REBASE_REQUIRED`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_max_reconciled_delta_keys: Option<u64>,
    /// Rows recall can actually reach — the largest per-slot index length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_rows_covered: Option<u64>,
    /// Dense (ANN) lanes built. Reported separately from sparse because the two
    /// fail independently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_dense_slot_count: Option<u64>,
    /// Sparse lexical lanes built. The lane's actual law is per-slot: a
    /// `sparse_bm25` lane ranks by BM25, a `sparse_dot` lane by a plain dot
    /// product with no IDF and no length saturation (#1900).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_sparse_slot_count: Option<u64>,
    /// Age of the last build in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_age_ms: Option<u64>,
    /// A staked rebuild-required intent, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_rebuild_required: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_remediation: Option<String>,

    // --- every published search generation (issue #1938) ---
    // The fields above describe exactly one generation: the active panel's.
    // That was the whole defect. #1668 made non-active panels queryable, and
    // every MCP tool call advances the vault sequence, so a non-active
    // generation's reconciliation delta grows continuously until its queries
    // fail closed — invisibly, because nothing reported it. These fields report
    // the bound-distance for EVERY published generation, so "this panel is about
    // to stop answering" is visible before the first failed query.
    /// Published search generations the unattended sweep considered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generations_total: Option<u64>,
    /// Generations swept without a failure and inside their bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generations_maintained: Option<u64>,
    /// Generations whose panel version has no code-declared slot contract **and
    /// no place in any live panel's declared lineage**, so no rebuild can ever
    /// return them to their bound and nothing establishes what they are. A
    /// declared terminal state, not a transient one — investigate before
    /// deleting anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generations_unmaintainable: Option<u64>,
    /// Generations published for a **closed superseded version of a live
    /// panel** (#1972). Reclaimable rather than unknown: the live generation of
    /// the same panel carries the corpus, so `storage
    /// operation=retire_search_generation` clears each one.
    ///
    /// Counted apart from `unmaintainable` because sharing that field put a
    /// permanent floor under this subsystem's status, which made the next
    /// genuinely-unknown generation invisible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generations_retirable: Option<u64>,
    /// The exact panel versions behind `calyx_search_generations_retirable`, so
    /// remediation needs no log dive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generations_retirable_panel_versions: Option<Vec<u32>>,
    /// Generations whose maintenance failed on the last sweep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generations_failed: Option<u64>,
    /// The panel holding the least remaining headroom to the reconciliation
    /// bound. The vault is as close to a failing query as its closest
    /// generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generations_closest_panel_version: Option<u32>,
    /// Changed keys that panel can still absorb before its queries fail closed.
    /// Zero means the bound is already crossed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generations_closest_keys_to_bound: Option<u64>,
    /// One line per generation: version, disposition, action, delta, headroom.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generations_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generations_swept_at_unix_ms: Option<u64>,

    // --- unattended derived-state maintenance (issues #1891, #1894) ---
    // The generation above is only ever `built` because something keeps it that
    // way. These fields report whether that something is running and what it
    // last decided, so a maintainer that has silently stopped is visible before
    // the generation it maintains expires.
    /// `initial_build` | `refresh_over_existing` | `none_needed` |
    /// `deferred_by_interval`, for the **active panel's** generation. Every
    /// published generation's outcome is in `calyx_search_generations_detail`
    /// (#1938); this field is the active one so it means what it always meant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_search_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_search_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_run_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_success_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_attempts_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_failure_total: Option<u64>,
    /// The last failure, retained across later successes so a lifetime failure
    /// counter never outlives its own evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_failure_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_failure_detail: Option<String>,
    /// Changed-key delta at which the maintainer refreshes — deliberately below
    /// the query-time reconciliation limit, so the index is repaired before
    /// recall dies rather than after.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_refresh_delta_keys_threshold: Option<u64>,
    /// The changed-key delta the maintainer measured on its last tick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_delta_changed_keys: Option<u64>,

    // --- panel lens coverage (issue #1894, ask 2) ---
    // `abundance` computed `blind_spot_records = 1740` on a panel of 1,745
    // constellations and nothing raised it. These fields are that alarm.
    /// Panels measured in the last coverage pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_lens_coverage_panels_measured: Option<u64>,
    /// Panels carrying fewer than two lenses, or whose blind-spot fraction
    /// exceeds the ceiling. Non-zero is an `error`: every association-derived
    /// surface on such a panel can only report a vacuous zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_lens_coverage_deficient_panels: Option<u64>,
    /// Records measured across all panels that carry fewer than two co-present
    /// dense lenses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_lens_coverage_blind_spot_records: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_lens_coverage_records_measured: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_lens_coverage_measured_at_unix_ms: Option<u64>,
    /// Dense lanes that took one value across every measured record carrying
    /// them, so they cannot rank (#1970).
    ///
    /// Reported as `degraded`, not `error`: the lane is real and its rows are
    /// intact, but it contributes nothing to recall, and finding those one at a
    /// time whenever somebody thinks to look is how one stayed unnoticed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_lens_degenerate_lanes: Option<u64>,
    /// `panel:slot` for each degenerate lane, so remediation needs no log dive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_lens_degenerate_lane_keys: Option<Vec<String>>,

    // --- panel coverage and grounding census (issues #1927, #1920) ---
    // A panel version bump left the active generation measuring 1.7% of its
    // source CF and `health` reported `ok` throughout, because nothing compared
    // the two counts. These fields are that comparison, published by the
    // derived-state maintainer and read here without touching a corpus.
    /// Declared panels in the census.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_coverage_panels: Option<u64>,
    /// Full-CF panels whose active generation is below the coverage floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_coverage_deficient_panels: Option<u64>,
    /// Deficient panels with NO re-measure path. The maintainer cannot repair
    /// these, so they are counted apart from the ones it can.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_coverage_unbackfillable_panels: Option<u64>,
    /// Outcome-bearing panels below the grounding floor. Observation-shaped
    /// panels are excluded by declaration, not by threshold (#1920 ask 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_grounding_deficient_panels: Option<u64>,
    /// **#1962.** Outcome-bearing panels holding records but carrying zero
    /// anchor kinds — a strict subset of
    /// [`Self::calyx_panel_grounding_deficient_panels`], and a categorically
    /// different state from the rest of it.
    ///
    /// Thin coverage is provisional; no outcome axis at all is *undefined*.
    /// A single deficient count could not tell an operator which one they had,
    /// which is how the active panel sat with zero anchors of any kind behind a
    /// count that read as thin coverage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_no_outcome_axis_panels: Option<u64>,
    /// The `panel@version` names behind
    /// [`Self::calyx_panel_no_outcome_axis_panels`], so the fact that one of
    /// them is the panel every operator-facing surface reads is legible without
    /// a second call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_no_outcome_axis_panel_names: Option<Vec<String>>,
    /// Panels whose grounded anchors a panel-version bump stranded on a
    /// superseded generation (#1980).
    ///
    /// Distinct from [`Self::calyx_panel_no_outcome_axis_panels`], which is the
    /// symptom. This is the cause and it names a different remedy: the outcomes
    /// still physically exist one generation back, so the panel is repaired by
    /// driving the backfill's carry-forward rather than by re-observing
    /// anything. `syn-episode-v1` reached the active generation with all 171
    /// records re-measured at coverage 1.0 and none of its 171 anchors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_anchors_stranded_panels: Option<u64>,
    /// The `panel@version` names behind
    /// [`Self::calyx_panel_anchors_stranded_panels`], each carrying the stranded
    /// count and the generation holding the anchors to carry FROM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_anchors_stranded_panel_names: Option<Vec<String>>,
    /// Lowest active-generation coverage fraction across every full-CF panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_coverage_min_fraction: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_coverage_floor: Option<f32>,
    /// `Base` rows on a superseded or unclaimed generation: read by no
    /// active-panel surface, still occupying the CF (#1927 ask 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_superseded_records: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_base_cf_rows: Option<u64>,
    /// `Base` rows that would not decode during the census. Non-zero means the
    /// counts above are over a subset, and the report says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_census_decode_failures: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_coverage_measured_at_unix_ms: Option<u64>,
    /// What the maintainer's backfill driver did last tick: `none_owed` |
    /// `owed_but_unbackfillable` | `sweep_complete` | `budget_exhausted` |
    /// `page_failed` | `cursor_absent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_backfill_action: Option<String>,
    /// Independent predicate that selected the panel: coverage debt, anchor
    /// debt, or both. Kept separate from the execution outcome above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_backfill_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_backfill_panel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_backfill_pages: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_backfill_inserted_rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_backfill_outcome_anchored_rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_backfill_elapsed_ms: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vram_budget_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vram_budget_enforced: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vram_dispatch_soft_cap_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vram_dispatch_allocated_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vram_dispatch_serving_allocated_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vram_dispatch_anneal_allocated_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vram_dispatch_device_free_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_basis: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_state_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_state_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_device_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_device_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_device_total_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_host_cap_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_required_free_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_headroom_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_last_physical_free_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_reserved_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_available_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_admitted_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_rejected_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_stale_reaped_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_requested_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_acquired_unix_ms: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_lease_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_reservation_last_rejection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_runtime_readback_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_gpu_runtime_readback_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_backend_requested: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_cuda_compiled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_device_vram_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_device_avx512: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_cpu_avx512_available: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_cpu_simd_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_fallback_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_fallback_source_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_fallback_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_tolerance: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_dot: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_cosine: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_l2_squared: Option<Vec<f32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_topk: Option<Vec<CalyxMathProbeTopKEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_clock_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_fixed_clock_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_rng_seed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_tick_jitter_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_tick_jitter_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_tick_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_miss_streak: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_miss_audit_after: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severe_deadline_miss_after_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded_tick_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recursion_clamps_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reload_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ring_buffer_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_model_loaded: Option<bool>,
    /// Whether the optional speech-to-text model is packaged in this build
    /// (#1863).
    ///
    /// Reported independently of `enable_audio`, because "you turned audio off"
    /// and "this binary physically cannot transcribe" are different facts and an
    /// operator enabling audio needs to know the second one in advance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_model_available: Option<bool>,
    /// Why the STT model is unavailable, with the exact remediation. Present
    /// only when `stt_model_available` is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_model_unavailable_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_addr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_sessions: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sse_subscribers: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_resolution: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_shell_inline_await_limit_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_shell_inline_client_call_budget_ms: Option<u64>,
    /// Outer `None` omits the field for unrelated subsystems; inner `None`
    /// serializes as JSON null to make an unbounded durable shell policy visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_shell_durable_default_timeout_ms: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_shell_durable_max_timeout_ms: Option<Option<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perception_mode: Option<PerceptionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_config: Option<ObservationCaptureConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_runtime: Option<CaptureRuntimeReadback>,
    /// Structured `chrome_bridge` verdict. `None` for every subsystem except
    /// `chrome_bridge`; the MCP health builder populates it so the bridge
    /// readiness is machine-readable instead of a single concatenated
    /// `detail` string. In compact health responses only the verdict-critical
    /// fields are retained; full responses populate every parsed field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_bridge: Option<ChromeBridgeDetail>,
    /// Structured `calyx_hot_path` verdict (#1686). `None` for every subsystem
    /// except `calyx_hot_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_hot_path: Option<CalyxHotPathBoundaryHealth>,
    /// Structured `process_qos` verdict (#1910). `None` for every subsystem
    /// except `process_qos`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_qos: Option<ProcessQosHealth>,
    /// Structured `usage_writer` verdict (#1936). `None` for every subsystem
    /// except `usage_writer`.
    ///
    /// Typed rather than folded into `detail` because compact health responses
    /// null `detail` outright, and this is the daemon's evidence backlog:
    /// every tool call's outcome passes through that queue on the way into the
    /// corpus. A backlog that is only readable by asking for full health is not
    /// readable on demand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_writer: Option<UsageWriterHealth>,
}

/// Grounded-usage writer state (#1936).
///
/// The observation for each tool call is committed off the response path by a
/// dedicated writer. These three numbers are the whole contract: nothing
/// waiting, everything that was accepted was written, and nothing failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UsageWriterHealth {
    /// Observations accepted from callers but not yet durable. Signed: a
    /// negative value would be an accounting bug, and seeing it beats wrapping.
    pub queue_depth: i64,
    /// Observations committed since this daemon started.
    pub committed: u64,
    /// Observations that could not be committed. Any non-zero value means the
    /// corpus is missing outcomes that callers were told had succeeded, which
    /// is a grounding fault rather than a degraded-service note.
    pub failed: u64,
}

/// How the OS is scheduling this daemon process, and whether the daemon
/// successfully said what it wanted (#1910).
///
/// This exists because the defect it reports was invisible on every surface the
/// system is normally verified through: the daemon ran at
/// `BELOW_NORMAL_PRIORITY_CLASS` on every install, and the only way to see that
/// was an out-of-band `Get-Process`. Scheduling `QoS` is part of whether the
/// daemon is healthy, so it is reported where the rest of health is.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProcessQosHealth {
    /// Priority class the process was launched with, before it asserted its own.
    /// `BelowNormal` here with `Normal` after means the launcher handed down the
    /// background default and the daemon corrected it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_class_before: Option<String>,
    /// Priority class read back from the OS after the assertion. This is the
    /// value that governs scheduling, and it is a readback rather than the value
    /// that was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_class_after: Option<String>,
    /// Whether the daemon actually had to raise itself. `true` means the launch
    /// path is still handing down a background priority — worth repairing at the
    /// source even though the daemon compensated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_raised: Option<bool>,
    /// `ControlMask` from `GetProcessInformation(ProcessPowerThrottling)`. Zero
    /// means the process made no explicit choice and Windows is inferring a `QoS`
    /// level heuristically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_throttling_control_mask: Option<u32>,
    /// `StateMask` from the same readback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power_throttling_state_mask: Option<u32>,
    /// Proven by readback: execution-speed throttling is explicitly controlled
    /// and off, so the process is not classified `EcoQoS` and not biased onto
    /// efficiency cores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_speed_throttling_disabled: Option<bool>,
    /// Structured code for an assertion that did not reach its intended state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CalyxMathProbeTopKEntry {
    pub index: usize,
    pub score: f32,
}

/// Externally readable state of the Calyx hot-path boundary (#1686).
///
/// The boundary doctrine is that latency-critical loops consume only *lowered*,
/// frozen, fingerprinted artifacts and never issue a live Calyx call. This
/// struct is the evidence surface for that claim, and every field is designed to
/// be checkable from `health` alone, without a debugger:
///
/// * `violations_total` is an **always-on** counter — it is maintained
///   identically in debug and release, because the in-crate `debug_assert!`
///   boundary check is compiled out of optimized builds by default
///   (see `std::debug_assert!`), which would make a release acceptance run
///   worthless on its own.
/// * `artifact_file_sha256` is the SHA-256 of the *whole published file*, so an
///   operator can compare it directly against `Get-FileHash`.
/// * `artifact_content_sha256` is the fingerprint the envelope records over its
///   frozen payload, re-verified at read time by the artifact consumer.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CalyxHotPathBoundaryHealth {
    /// Whether the reflex scheduler thread is currently tagged as a hot context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_thread_tagged: Option<bool>,
    /// Ticks executed under the hot-context tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hot_ticks_total: Option<u64>,
    /// Always-on count of live-Calyx-from-hot-context violations. Must be `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub violations_total: Option<u64>,
    /// Operation named by the most recent violation, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_violation_operation: Option<String>,
    /// Wall-clock time of the most recent violation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_violation_unix_ms: Option<u64>,
    /// Structured code carried by the most recent violation.
    ///
    /// Present **only** when `violations_total > 0`. It names an observation
    /// that happened, not a label this subsystem always wears: emitting a
    /// violation code alongside `violations_total = 0` reads to any operator or
    /// scraper as "a violation occurred", which is the same constant-that-looks-
    /// like-a-measurement defect as #1883/#1884/#1886.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub violation_code: Option<String>,
    /// Whether the reflex scheduler (and therefore the tick thread) exists.
    ///
    /// This is what makes `tick_thread_tagged = false` interpretable: `false`
    /// here means there is nothing to tag, not that the tagging is broken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler_started: Option<bool>,
    /// Structured code naming why the `artifact_*` / `refresher_*` group could
    /// not be computed. Present exactly when that group is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed_unavailable_code: Option<String>,
    /// Why the `artifact_*` / `refresher_*` group could not be computed, with
    /// the remediation that would make it computable.
    ///
    /// An absent field and a field that cannot be computed are different facts,
    /// so the second one is stated rather than left to inference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed_unavailable_reason: Option<String>,
    /// Absolute path of the lowered guard-threshold artifact the tick consumes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_path: Option<String>,
    /// `fresh` or `safe_default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_state: Option<String>,
    /// Fail-closed code in force while `artifact_state` is `safe_default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_safe_default_code: Option<String>,
    /// Detail for the fail-closed safe default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_safe_default_detail: Option<String>,
    /// Remediation for the fail-closed safe default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_safe_default_remediation: Option<String>,
    /// Payload fingerprint recorded in the envelope and re-verified on read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_content_sha256: Option<String>,
    /// SHA-256 over the entire published file; compare with `Get-FileHash`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_file_sha256: Option<String>,
    /// Size of the published file in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_file_bytes: Option<u64>,
    /// Error encountered while hashing the published file for readback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_file_read_error: Option<String>,
    /// Monotonic artifact generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_generation: Option<u64>,
    /// Vault ledger sequence the artifact was lowered from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_source_ledger_seq: Option<u64>,
    /// Vault id the artifact was lowered from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_vault_id: Option<String>,
    /// When the artifact was produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_produced_at_unix_ms: Option<u64>,
    /// Reader-side staleness bound in force (`0` disables the wall-clock bound).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_staleness_bound_ms: Option<u64>,
    /// Hot-path `load()` calls served from the frozen in-memory pointer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_hot_reads_total: Option<u64>,
    /// Off-tick refresh passes performed by the reflex refresher thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_refreshes_total: Option<u64>,
    /// Refreshes that landed on a verified fresh artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_fresh_refreshes_total: Option<u64>,
    /// Refreshes that degraded to the documented safe default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_safe_default_refreshes_total: Option<u64>,
    /// Whether the off-tick refresher thread is alive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresher_running: Option<bool>,
    /// Off-tick refresher cadence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresher_interval_ms: Option<u64>,
    /// Publish attempts made by the storage maintenance pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_attempts_total: Option<u64>,
    /// Publishes that wrote and re-verified an artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_success_total: Option<u64>,
    /// Publishes that failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_failure_total: Option<u64>,
    /// Publishes skipped because no open Calyx vault was registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_skipped_total: Option<u64>,
    /// When the last successful publish completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_last_success_unix_ms: Option<u64>,
    /// Fingerprint written by the last successful publish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_last_content_sha256: Option<String>,
    /// Structured code of the last publish failure or skip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_last_error_code: Option<String>,
    /// Detail of the last publish failure or skip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publish_last_error: Option<String>,
}

/// Structured replacement for the `chrome_bridge` subsystem's concatenated
/// `detail` blob.
///
/// Each field names one piece the blob previously encoded as
/// `key=value` text. Every field is optional so partially-observed hosts and
/// the no-host/unavailable branch omit what they cannot report, and so compact
/// health responses can drop the verbose identity fields while keeping the
/// readiness verdict.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChromeBridgeDetail {
    /// Whether tab-control debugger commands can currently be issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_control_available: Option<bool>,
    /// Whether the connected extension identity is stale versus expectations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_stale: Option<bool>,
    /// Pipe-joined stale reasons, or `none` when the identity is current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_stale_reasons: Option<String>,
    /// Reason code emitted when no active bridge host is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Number of registered bridge hosts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_count: Option<u64>,
    /// Number of commands queued for the active host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_count: Option<u64>,
    /// Number of commands pending acknowledgement from the active host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_count: Option<u64>,
    /// Extension id reported by the active host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_id: Option<String>,
    /// Extension id the bridge expects (identity anchor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_extension_id: Option<String>,
    /// Extension version reported by the active host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_version: Option<String>,
    /// Transport carrying bridge traffic (e.g. `native_messaging`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Chrome extension health endpoint URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}
