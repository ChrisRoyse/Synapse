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

/// One panel generation's state on the last search-generation sweep (#2075).
///
/// Reported per panel because the defect this closes was a rollup: the
/// `calyx_search_generation` subsystem measured exactly one generation — the
/// vault's active panel — and published its verdict as the verdict for search.
/// A panel whose generation had never been built was not "unhealthy" in that
/// measurement; it was absent from it, which read as `ok`.
///
/// Every field here is carried out of the sweep the daemon already ran. Nothing
/// in this struct re-measures a corpus, so publishing per-panel truth costs a
/// `health` call nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CalyxSearchGenerationPanel {
    pub panel_version: u32,
    /// Whether the vault manifest publishes this generation as active.
    pub is_active_panel: bool,
    /// Whether a fused query may name this version in `panel_version`. A
    /// missing manifest matters here and nowhere else: an index nobody can ask
    /// for is an absence, an index the tool contract promises is a defect.
    pub is_declared_queryable: bool,
    /// Whether a persisted manifest exists for this generation — the exact file
    /// a fused query opens, and therefore whether recall can serve it at all.
    pub manifest_present: bool,
    /// `maintained` | `unmaintainable_no_contract` |
    /// `retirable_superseded_generation` | `failed`.
    pub disposition: String,
    /// Changed keys this generation can still absorb before its queries fail
    /// closed. `None` when unmeasured, which is never the same as zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys_to_bound: Option<u64>,
}

/// Bounded identity of one build input that differed from the checked-in tree.
///
/// Contents are never embedded; the digest and length are sufficient to prove
/// which exact bytes entered the build without leaking source or secrets.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildInputChange {
    pub path: String,
    /// Git porcelain-v1 `XY` status (`??` for a non-ignored untracked input).
    pub status: String,
    /// `tracked` | `untracked` | `missing_tracked`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
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
    /// Canonical algorithm used for the input inventory and manifest digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_input_schema: Option<String>,
    /// Exact number of tracked and non-ignored untracked build inputs hashed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_input_file_count: Option<u64>,
    /// SHA-256 of canonical path/kind/length/content-digest records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_input_manifest_sha256: Option<String>,
    /// SHA-256 of the complete raw Git porcelain-v1 `-z` status at build time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_git_status_sha256: Option<String>,
    /// Build-relevant input rows whose Git state was not clean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_changed_input_count: Option<u64>,
    /// At most 16 changed identities; content is represented only by SHA-256.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_changed_input_examples: Option<Vec<BuildInputChange>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_changed_input_omitted: Option<u64>,
    /// Exact build-time failure when provenance was deliberately allowed to be
    /// unknown. Presence always makes `build_provenance.status=error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_input_attestation_error: Option<String>,
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
    /// True only when the build was clean and its commit is still the checkout
    /// commit. Dirty input bytes are an established mismatch (`false`), while
    /// unreadable/unknown provenance leaves this absent.
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
    /// The one pinned committed MVCC sequence the last successful GC pass took
    /// its derived-source protection set from (#2058). Its presence is what
    /// distinguishes a GC pass that actually adjudicated deletions from one
    /// that was skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_source_census_pinned_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_source_census_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_source_census_previous_pinned_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_source_census_pages: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_source_census_base_rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_source_census_changed_base_keys: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_source_census_rebase_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_source_census_referenced_rows: Option<u64>,
    /// In-RAM MVCC version-chain versions reclaimed by the last successful
    /// `storage_gc` tick (#2122).
    ///
    /// Absent means no reclamation pass has completed in this daemon
    /// generation. It was structurally absent for the whole life of every
    /// daemon before this field existed, because `snapshot_version_gc` had no
    /// caller anywhere in Synapse: every commit's value bytes were cloned into
    /// an in-RAM version chain and never freed, which is the mechanism behind
    /// the 1.07 GB/hour private-commit ratchet #2115 measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_snapshot_versions_reclaimed: Option<u64>,
    /// That pass's full readback as one parseable `key=value` line: the pinned
    /// floor it reclaimed below and the vault's current sequence (a floor stuck
    /// far below is a leaked reader lease, the fix's one silent failure mode),
    /// bytes and chains reclaimed, whether the sweep completed, the longest
    /// row-table shard write-guard hold it took, and the process's committed
    /// private memory around it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gc_last_successful_snapshot_version_detail: Option<String>,
    /// Verdict of the last scheduled physical vault verification (#2059):
    /// `verified` | `unverifiable` | `corrupt` | `unreadable`.
    ///
    /// Absent until a tick completes in this daemon generation. Before this
    /// existed the verdict reached only daemon stderr, so an unattended operator
    /// could not tell a vault that had verified clean from one whose
    /// verification had never run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_verify_verdict: Option<String>,
    /// False when `SYNAPSE_VAULT_VERIFY_INTERVAL_SECS=0` disabled the schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_verify_scheduled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_verify_interval_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_verify_last_started_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_verify_last_completed_unix_ms: Option<u64>,
    /// When the vault last verified *clean*, retained across later non-green
    /// ticks so a run of refusals cannot hide how stale the last real pass is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_verify_last_verified_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_verify_vault_id: Option<String>,
    /// `incremental_tail` | `full_chain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_verify_scan_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_verify_verified_from_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_verify_verified_to_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_verify_ledger_head_height: Option<u64>,
    /// Fraction of the durable ledger the chain re-walk covered. An `intact`
    /// verdict over 4,096 of 1,056,804 entries and one over all of them are
    /// different facts, so the verdict never travels without its coverage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_verify_coverage_fraction: Option<f64>,
    /// `SYNAPSE_HYGIENE_VAULT_VERIFY_UNVERIFIABLE` or
    /// `SYNAPSE_HYGIENE_VAULT_VERIFY_FAILED` — which alarm this verdict is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_verify_reason_code: Option<String>,
    // --- Assist next-action readiness (#2068 clause 5) ---
    //
    // Whether the assist surface can compose a next action right now was
    // previously observable only by calling `assist operation=suggestion_tick`
    // and reading the response — i.e. by triggering the very work being
    // diagnosed. An operator reading `/health` could not distinguish an assist
    // surface that had never composed from one whose composer refuses on every
    // tick for want of upstream evidence (#2076): both look identical from
    // outside, because both produce no suggestions.
    //
    // These fields are filled by a read-only point-read of the `CF_KV`
    // `assist_next_action/v1/current` pointer and its content-addressed frozen
    // row. Health never composes.
    /// True when `CF_KV assist_next_action/v1/current` resolves to a frozen
    /// artifact whose content fingerprint verifies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_artifact_present: Option<bool>,
    /// `content_sha256` of the current frozen artifact — the fingerprint the
    /// tick response reports, so a `/health` reading and a tick response can be
    /// compared for identity rather than for plausibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_artifact_sha256: Option<String>,
    /// When the current artifact was composed (`produced_ts_ns`, milliseconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_artifact_built_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_artifact_age_secs: Option<u64>,
    /// `SYNAPSE_ASSIST_NEXT_ACTION_STALENESS_SECS`. Always present, even with no
    /// artifact, so the age never travels without the bound it is judged against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_staleness_bound_secs: Option<u64>,
    /// `age_secs > staleness_bound_secs`: the artifact still answers, but the
    /// next non-dry tick will recompose rather than serve it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_artifact_stale: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_artifact_candidates: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_artifact_kernel_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_artifact_panel_name: Option<String>,
    /// The Calyx panel version the artifact was composed over — the source
    /// sequence, so an artifact can be told to be behind the live panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_artifact_panel_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_artifact_content_slot: Option<u32>,
    /// The kernel recall achieved, and the gate it had to clear. A grounded
    /// artifact never travels without the gate that admitted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_artifact_recall_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_artifact_min_recall_ratio: Option<f64>,
    /// Outcome of the last composition pass in this daemon generation:
    /// `disabled` | `frozen_artifact` | `calyx_kernel_graph` | `refused` |
    /// `error`. Absent until one runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_last_composition_outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_last_composition_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_last_composition_grounded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_last_composition_candidates: Option<u32>,
    /// When live Calyx composition last actually succeeded, retained across
    /// later refusals so a refusal streak cannot hide how old the last real
    /// composition is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_last_composed_unix_ms: Option<u64>,
    /// The exact refusal code of the last non-composing pass, e.g.
    /// `ASSIST_NEXT_ACTION_NO_HOP_EVIDENCE`. Retained across later
    /// `frozen_artifact` passes: a frozen artifact serving inside its staleness
    /// bound must not hide that live composition is refusing underneath it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_last_refusal_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_last_refusal_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_last_refusal_unix_ms: Option<u64>,
    /// Composition passes that have refused or errored since the last success.
    /// This is what makes a *persistent* upstream refusal legible as persistent
    /// rather than as one unlucky tick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assist_next_action_consecutive_refusals: Option<u32>,
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
    pub calyx_vault_open_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_restore_mvcc_rows: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_vault_eager_router_lookup_on_open: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_mvcc_resident_keys: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_mvcc_resident_versions: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_mvcc_resident_key_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_mvcc_resident_value_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_mvcc_resident_payload_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_memtable_used_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_memtable_cap_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_memtable_high_water_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_sst_reader_cache_entries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_sst_reader_cache_estimated_heap_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_sst_reader_cache_mapped_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_sst_reader_cache_max_entries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_sst_reader_cache_max_estimated_heap_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_sst_reader_cache_max_mapped_bytes: Option<u64>,
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
    /// Exact-panel derived-content watermark observed atomically with the
    /// status snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_panel_content_seq: Option<u64>,
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

    // --- the declared-queryable set (issue #2075) ---
    // Every field above measures generations that EXIST. None of them could
    // report a generation that was never built, because a generation with no
    // directory on disk is invisible to a disk census — so the three
    // outcome-bearing corpora sat with no search index at all while this
    // subsystem reported `ok` from the active panel's healthy generation, and
    // `find panel_version=…` hard-errored with SYNAPSE_CALYX_FIND_INDEX_STALE
    // on every one of them. These fields report the set a caller may query
    // against the set that is actually built, which is the only comparison that
    // answers "can fused recall serve".
    /// Live panel generations a caller may name in `panel_version` on a fused
    /// query — the versions with a code-declared slot contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generations_declared_queryable: Option<u64>,
    /// Declared-queryable generations with **no persisted manifest**. Any value
    /// above zero means fused find is failing closed for that many panels right
    /// now, so this subsystem may not report `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generations_unbuilt_declared_queryable: Option<u64>,
    /// The exact panel versions behind the count above, so remediation needs no
    /// log dive and no second measurement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generations_unbuilt_declared_queryable_panel_versions: Option<Vec<u32>>,
    /// Per-panel generation state for every generation the last sweep
    /// considered.
    ///
    /// The scalar counts above answer "how many"; only this answers "which one,
    /// and can it serve". A rollup was what let a missing manifest read as `ok`
    /// in the first place, so the per-panel facts are published rather than
    /// summarised away.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_search_generation_panels: Option<Vec<CalyxSearchGenerationPanel>>,

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
    /// Tick-granularity counters, published together because they only mean
    /// anything together (#2080 ask 1): `success + failure + skipped` accounts
    /// for every `attempts`, and `failure` can no longer exceed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_success_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_skipped_total: Option<u64>,
    /// Sub-pass failures across every tick — the diagnostic layer. Deliberately
    /// a separate field from `failure_total`: one tick can hold many of these,
    /// and reporting them through the tick counter is what published
    /// `failure=15` against `attempts=6`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_subpass_failures_total: Option<u64>,
    /// Cost and quality advisories. Visible, and never a reason `health.ok` is
    /// false (#2080 ask 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_advisories_total: Option<u64>,
    /// Whether the last **completed** tick failed — the fact this subsystem's
    /// status is computed from, so a clean tick clears it and a lifetime counter
    /// can never hold it red forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_tick_failed: Option<bool>,
    /// That tick's sub-pass failures, verbatim, so the rollup can be decomposed
    /// into the components that actually broke.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_tick_subpass_failures: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_advisory_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_advisory_detail: Option<String>,
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
    /// Per-panel `XTerm` rows submitted and durably flushed by the last
    /// completed incremental-weave pass. This is an additive write count, not
    /// a vault-global CF gauge and not a claim that every upsert grew the CF.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_weave_xterm_rows_written: Option<BTreeMap<u32, usize>>,
    /// Per-panel Graph rows submitted and durably flushed by the last
    /// completed incremental-weave pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_weave_graph_rows_written: Option<BTreeMap<u32, usize>>,
    /// Vault-global physical `XTerm` CF gauges observed after each panel's last
    /// completed interval part. These are deliberately non-additive snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_weave_global_xterm_cf_rows_after: Option<BTreeMap<u32, usize>>,
    /// Vault-global physical Graph CF gauges observed after each panel's last
    /// completed interval part. Completion order may differ across workers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_weave_global_graph_cf_rows_after: Option<BTreeMap<u32, usize>>,
    /// Physical-read provenance adjacent to each `XTerm` global gauge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_weave_global_xterm_cf_rows_readback: Option<BTreeMap<u32, String>>,
    /// Physical-read provenance adjacent to each Graph global gauge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_weave_global_graph_cf_rows_readback: Option<BTreeMap<u32, String>>,
    /// Per declared `panel_name:group_key` causal-map maintenance disposition.
    /// A published disposition is accompanied by the exact normalized-scope
    /// pointer and immutable Graph artifact identities below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_causal_map_actions: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_causal_map_pointer_keys: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_causal_map_artifact_keys: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_causal_map_artifact_sha256: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_causal_map_source_fingerprint_sha256:
        Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_causal_map_source_records: Option<BTreeMap<String, usize>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_causal_map_latest_event_ns: Option<BTreeMap<String, u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_derived_state_last_causal_map_rebuild_unix_ms: Option<u64>,

    // --- coverage-backfill rotation and anchor-debt quarantine (#2061) ---
    /// Coverage targets owed a sweep, and how many the last tick swept. Equal on
    /// a healthy tick. `owed=4 attempted=1` is what starvation looked like from
    /// the outside while every other backfill field reported real work — for one
    /// panel out of four.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_backfill_targets_owed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_backfill_targets_attempted: Option<u64>,
    /// Owed targets the last tick did not sweep, each with its reason. Empty on
    /// a healthy tick; the only permitted way for `attempted` to fall short.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_backfill_targets_skipped: Option<Vec<String>>,
    /// Exact stranded anchor identities quarantined as unrepairable, named in
    /// full with their reason codes (#2061 ask 2). A refusal that keeps being
    /// reported is truthful; one that is silently skipped is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_anchor_debt_quarantined_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_panel_anchor_debt_quarantined_identities: Option<Vec<String>>,

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
    pub calyx_assay_compute_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_backend_requested: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_cuda_compiled: Option<bool>,
    /// Whether the running daemon contains calyx-registry's optional local
    /// embedding execution stack. Synapse deliberately reports `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_registry_embedding_runtimes_compiled: Option<bool>,
    /// Whether the running daemon contains Ward's optional ONNX/tokenizer model
    /// lenses. Synapse deliberately reports `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_ward_model_lenses_compiled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_device_vram_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_cpu_simd_path: Option<String>,
    /// Process-local CUDA serving epoch, reset only after startup probes pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_dispatch_epoch_started_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_dispatch_sampled_at_unix_ms: Option<u64>,
    /// Fixed-cardinality records for every Forge backend operation. Zero rows
    /// are present from epoch start so absence is never confused with zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_dispatch_operations: Option<Vec<CalyxMathDispatchOperation>>,
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
    /// Real CUDA commissioning of the device-resident indexed L2 gather path,
    /// including its CPU near-tie contract and reservation lifecycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calyx_math_probe_resident_l2_gather: Option<CalyxMathResidentL2GatherProbe>,
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
    pub audit_timestamp_invalid_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_lifecycle_pending: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_lifecycle_prepared_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_lifecycle_committed_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_lifecycle_failed_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_lifecycle_failure_reflex_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_lifecycle_failure_intent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_lifecycle_failure_phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_lifecycle_failure_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reload_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ring_buffer_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_timeline_discontinuities_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_timeline_gap_frames_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_timeline_last_discontinuity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_model_loaded: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_backend_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_selected_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_device_memory_policy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_gpu_reservation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_gpu_reservation_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_fallback_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_fallback_detail: Option<String>,
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
    /// Whether neural detection actually runs (#2054). `None` for every
    /// subsystem except `perception`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perception_detection: Option<PerceptionDetectionHealth>,
    /// Exact recursive filesystem-watcher ownership and bounded ingress state.
    /// `None` for every subsystem except `perception`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perception_fs_watch: Option<serde_json::Value>,
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

/// Whether neural detection actually runs on this daemon right now (#2054).
///
/// The perception subsystem used to report `status="ok"` with a
/// `detection_model=...` blob describing the detector the *executable bundles*,
/// regardless of whether the active profile asked for any inference. A profile
/// with no `[detection]` section performed no model inference at all and was
/// indistinguishable from one whose detector runs on every observe.
///
/// Capability is therefore reported separately from subsystem readiness, the
/// same split inference servers draw between "server ready" and "model ready":
/// `perception.status` stays the readiness of the M1 runtime, and this struct
/// is the only authority on whether a detector runs. It is a typed field rather
/// than prose in `detail` because compact health responses (the default) drop
/// `detail` entirely, and a capability that is only visible in `detail=full` is
/// not visible on demand.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PerceptionDetectionHealth {
    /// True only when the active profile names a detector *and* allows more
    /// than zero detections, i.e. an observe in a pixel-bearing perception mode
    /// runs real model inference. False means no inference happens, ever, until
    /// the configuration changes.
    pub inference_configured: bool,
    /// `configured` | `not_configured` | `misconfigured`. Deliberately never
    /// `ok`/`healthy`: the word this field exists to stop being reused is the one
    /// that hid the gap.
    ///
    /// `misconfigured` (#2064) is the third state, and it is not a shade of
    /// either neighbour: the profile *did* ask for inference, but it names a
    /// detector this daemon cannot load, so every observe in a pixel-bearing
    /// mode fails rather than completing without detections. It reported
    /// `configured` until #2064, leaving `configured_model_registered: false` as
    /// the only tell — a field a reader had to already suspect to look at.
    pub status: String,
    /// Machine-readable cause, present exactly when `inference_configured` is
    /// false. `DETECTION_NOT_CONFIGURED` for a profile that asks for no
    /// detector; `DETECTION_MODEL_NOT_LOADED` — the same code the detection
    /// worker raises — for one that names a detector that cannot be loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    /// The exact configuration change that would turn inference on, present
    /// exactly when `inference_configured` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    /// Detector id the active profile names, `None` when it names none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_model_id: Option<String>,
    /// True when `configured_model_id` resolves in the model registry. `None`
    /// when the profile names no model. `Some(false)` is a fail-loud
    /// misconfiguration: every observe in a pixel-bearing mode will error, and
    /// since #2064 `status` says `misconfigured` for it rather than leaving this
    /// field to carry the verdict alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_model_registered: Option<bool>,
    pub max_detections: u32,
    pub confidence_threshold: f32,
    /// Perception mode in force. Detection inference is only reached in
    /// `PixelOnly` and `Hybrid`; the other modes disable the producer outright.
    pub perception_mode: PerceptionMode,
    /// True when `perception_mode` admits detection at all.
    pub mode_admits_detection: bool,
    /// Where the effective detection configuration came from
    /// (`daemon_default:no_profile_applied` or `profile:<id>`).
    pub config_source: String,
    /// When that configuration took effect — the honest answer to "since when
    /// has detection been off". Daemon start for the built-in default.
    pub config_applied_unix_ms: u64,
    /// Detector the executable bundles, which is what *could* be loaded. Named
    /// `bundled_` throughout because it is not evidence that anything runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundled_model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundled_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundled_materialized: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundled_materialized_verified: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundled_materialized_path: Option<String>,
    /// Runtime state of the isolated persistent detector: `ready`,
    /// `not_started`, or `busy` while an observation owns the runtime.
    pub persistent_worker_status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent_worker_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent_worker_model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent_worker_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent_worker_session_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent_worker_requests_started: Option<u64>,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CalyxMathResidentL2GatherProbe {
    pub block_id: u64,
    pub dataset_rows: usize,
    pub dim: usize,
    pub query_count: usize,
    pub stride: usize,
    pub raw_gpu_scores: Vec<f32>,
    pub cpu_reverified_scores: Vec<f32>,
    pub numeric_contract: String,
    pub raw_gpu_topology_exact: bool,
    pub cpu_reverified_topology_exact: bool,
    pub output_cells_reverified: usize,
    pub persistent_reserved_bytes: usize,
    pub process_reserved_bytes_before: usize,
    pub process_reserved_bytes_during: usize,
    pub process_reserved_bytes_after: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CalyxMathDispatchOperation {
    pub operation: String,
    pub attempted_total: u64,
    pub in_flight: u64,
    pub succeeded_total: u64,
    pub refused_total: u64,
    pub failed_total: u64,
    pub attempted_measured_bytes_total: u64,
    pub in_flight_measured_bytes: u64,
    pub succeeded_measured_bytes_total: u64,
    pub refused_measured_bytes_total: u64,
    pub failed_measured_bytes_total: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_code: Option<String>,
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
