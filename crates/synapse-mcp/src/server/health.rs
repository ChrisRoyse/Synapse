use super::{BTreeMap, ErrorData, Health, SubsystemHealth, SynapseService};
use rmcp::model::Tool;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use std::sync::TryLockError;
use synapse_action::BackendResolutionPolicy;
use synapse_core::{
    Backend, CalyxMathProbeTopKEntry, CalyxRowGuardSiteStatus, CalyxTuningKnobEnforcement,
    CalyxTuningKnobStatus, ChromeBridgeDetail, PerceptionDetectionHealth, PerceptionMode,
};

/// Verbosity control for the `health` tool response.
///
/// `health` is called frequently and its verbose per-subsystem `detail` prose
/// dominates the payload token cost (issue #1554). `Compact` (the default)
/// keeps every structured verdict field but drops the long human-readable
/// `detail` blobs, so callers still learn the health conclusion at a fraction
/// of the wire size. `Full` preserves the complete legacy output for
/// debugging.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum HealthDetail {
    /// Drop verbose per-subsystem `detail` prose; keep structured verdicts.
    #[default]
    Compact,
    /// Preserve every `detail` string (the legacy behavior).
    Full,
}

/// Request parameters for the `health` tool.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct HealthParams {
    /// `compact` (default) trims verbose per-subsystem detail prose while
    /// keeping every structured status field; `full` returns the complete
    /// diagnostic detail for every subsystem.
    pub detail: HealthDetail,
}

/// Whether this build can actually perform speech-to-text (#1863).
///
/// Returns `None` when STT is available, or `Some(reason)` carrying the exact
/// remediation when it is not. Audio STT is an optional capability, so a build
/// without the model is a legitimate install — but it must say so rather than
/// reporting a healthy audio subsystem that would fail on first use.
///
/// Deliberately cheap: it reads the executable's slot table, or the length of an
/// already-materialized model file. It is an availability signal, not the
/// integrity gate — the load path still verifies the full SHA-256 of whatever it
/// is about to hand to ONNX Runtime.
fn stt_model_availability() -> Option<String> {
    let expected_len = synapse_audio::stt::WHISPER_TINY_INT8_EXPECTED_LEN;
    let materialized = synapse_audio::stt::default_model_path();
    if let Ok(metadata) = std::fs::metadata(&materialized)
        && metadata.is_file()
        && metadata.len() == expected_len
    {
        return None;
    }

    match synapse_models::embedded_model_bundle() {
        Ok(bundle) => match bundle.slot(synapse_models::WHISPER_TINY_INT8_ONNX_ID) {
            Some(slot) if slot.is_present() => None,
            Some(_) => Some(format!(
                "the optional speech-to-text model is not packaged in {} and no verified model \
                 exists at {}; produce it with {} and re-run scripts/synapse-setup.ps1 with \
                 SYNAPSE_WHISPER_ONNX_SOURCE set to the produced file",
                bundle.executable.display(),
                materialized.display(),
                synapse_models::WHISPER_TINY_INT8_ONNX_RECIPE,
            )),
            None => Some(format!(
                "the embedded model bundle in {} has no speech-to-text slot; re-run \
                 scripts/synapse-setup.ps1 from this checkout",
                bundle.executable.display()
            )),
        },
        Err(error) => Some(format!(
            "speech-to-text model availability could not be determined: {error}"
        )),
    }
}

fn state_lock_unavailable_health<T>(
    state_name: &'static str,
    error: TryLockError<T>,
) -> SubsystemHealth {
    let detail = match error {
        TryLockError::WouldBlock => format!(
            "{state_name} state lock is busy; health is fail-closed and does not wait behind in-flight work"
        ),
        TryLockError::Poisoned(_poisoned) => {
            format!("{state_name} state lock poisoned")
        }
    };
    SubsystemHealth {
        status: "error".to_owned(),
        detail: Some(detail),
        ..SubsystemHealth::default()
    }
}

fn runtime_lock_unavailable_health<T>(
    runtime_name: &'static str,
    error: TryLockError<T>,
) -> SubsystemHealth {
    let detail = match &error {
        TryLockError::WouldBlock => format!(
            "{runtime_name} runtime lock is busy; health reports busy and does not wait behind in-flight work"
        ),
        TryLockError::Poisoned(_poisoned) => {
            format!("{runtime_name} runtime lock poisoned")
        }
    };
    SubsystemHealth {
        status: match &error {
            TryLockError::WouldBlock => "busy",
            TryLockError::Poisoned(_poisoned) => "error",
        }
        .to_owned(),
        detail: Some(detail),
        ..SubsystemHealth::default()
    }
}

/// Why the reflex tick's lowered-artifact feed can or cannot be described
/// (#1686).
///
/// The `artifact_*` / `refresher_*` group in `calyx_hot_path` is only
/// computable once a reflex scheduler exists, because the feed is owned by the
/// scheduler that reads it. Health must not silently omit that group: an absent
/// field and a field that cannot be computed are different facts, and only the
/// second one can be distinguished from "the mechanism is broken".
enum LoweredFeedReadback {
    Ready(Box<synapse_reflex::LoweredFeedSnapshot>),
    RuntimeAbsent,
    RuntimeLockBusy,
    SchedulerNotStarted,
}

impl LoweredFeedReadback {
    fn snapshot(&self) -> Option<&synapse_reflex::LoweredFeedSnapshot> {
        match self {
            Self::Ready(snapshot) => Some(snapshot),
            Self::RuntimeAbsent | Self::RuntimeLockBusy | Self::SchedulerNotStarted => None,
        }
    }

    const fn unavailable_code(&self) -> Option<&'static str> {
        match self {
            Self::Ready(_) => None,
            Self::RuntimeAbsent => Some("REFLEX_RUNTIME_NOT_INITIALIZED"),
            Self::RuntimeLockBusy => Some("REFLEX_RUNTIME_LOCK_BUSY"),
            Self::SchedulerNotStarted => Some("REFLEX_SCHEDULER_NOT_STARTED"),
        }
    }

    fn unavailable_reason(&self) -> Option<String> {
        let reason = match self {
            Self::Ready(_) => return None,
            Self::RuntimeAbsent => {
                "the reflex runtime has not been created, so there is no tick thread and no \
                 lowered-artifact feed to describe; the runtime initializes on the first reflex \
                 tool call"
            }
            Self::RuntimeLockBusy => {
                "the reflex runtime lock is busy and health is fail-closed rather than waiting \
                 behind in-flight work; retry health to read the lowered-artifact feed"
            }
            Self::SchedulerNotStarted => {
                "no reflex is registered, so the scheduler thread does not exist: \
                 tick_thread_tagged=false and hot_ticks_total=0 mean there is nothing to tag, not \
                 that tagging is broken. Register one through the public facade \
                 `routine operation=reflex_register` (requires a WRITE_REFLEX grant) to start the \
                 tick and populate the artifact_* fields"
            }
        };
        Some(reason.to_owned())
    }
}

/// Emitted when the lowered-artifact feed says it is unavailable but cannot say
/// why. The code names the defect (a `LoweredFeedReadback` variant whose
/// accessor arms were not filled in), never the operator's environment.
const FEED_UNAVAILABLE_UNCLASSIFIED_CODE: &str = "REFLEX_FEED_UNAVAILABLE_UNCLASSIFIED";

/// The `(code, reason)` pair health publishes when the lowered-artifact feed
/// could not be read — and the one place that holds them to each other.
///
/// `snapshot`, `unavailable_code` and `unavailable_reason` are three separate
/// matches over the same enum. The compiler forces a new variant to appear in
/// all three, but nothing forces the two unavailable accessors to answer `Some`
/// where `snapshot` answered `None`; a variant that returned `None` from
/// `unavailable_code` would make health report `scheduler_started = false` with
/// no code beside it, which reads to an operator or a scraper exactly like a
/// healthy feed. That pairing was previously asserted by an automated test
/// module; directive D1 (2026-07-15) removed that surface, so the invariant
/// lives here instead — on the live path, where a violation is a structured log
/// line naming its own cause rather than a silent field.
///
/// Fail-closed: an unclassified unavailability is still reported as an
/// unavailability, under a code that says the classification itself is broken.
fn feed_unavailability(readback: &LoweredFeedReadback) -> (Option<String>, Option<String>) {
    if readback.snapshot().is_some() {
        return (None, None);
    }
    let code = readback.unavailable_code().map(str::to_owned);
    let reason = readback.unavailable_reason();
    if code.is_some() && reason.is_some() {
        return (code, reason);
    }
    tracing::error!(
        code = FEED_UNAVAILABLE_UNCLASSIFIED_CODE,
        carried_code = code.as_deref().unwrap_or("<none>"),
        carried_reason = reason.as_deref().unwrap_or("<none>"),
        "the lowered-artifact feed is unavailable but did not name its condition: a \
         LoweredFeedReadback variant answered None from snapshot() while leaving \
         unavailable_code()/unavailable_reason() unfilled. Health is substituting a \
         self-describing code so the reading is not mute; fix the missing match arm in \
         crates/synapse-mcp/src/server/health.rs."
    );
    (
        Some(code.unwrap_or_else(|| FEED_UNAVAILABLE_UNCLASSIFIED_CODE.to_owned())),
        Some(reason.unwrap_or_else(|| {
            "the lowered-artifact feed is unavailable and the readback variant that reported it \
             carries no reason string; this is a defect in health's own classification, not a \
             condition of this host"
                .to_owned()
        })),
    )
}

/// The violation code to report, derived from whether one was actually
/// recorded.
///
/// Deliberately a function rather than an inline `Some(CONST)`: the code is a
/// property of an observed violation, and routing it through one place makes
/// "emit the label unconditionally" a change someone has to make on purpose.
fn violation_code_for(last_violation: Option<&synapse_reflex::HotPathViolation>) -> Option<String> {
    last_violation.map(|_observed| synapse_reflex::HOT_PATH_BOUNDARY_VIOLATION_CODE.to_owned())
}

fn lowered_feed_readback(
    reflex_runtime: Option<&std::sync::Arc<std::sync::Mutex<synapse_reflex::ReflexRuntime>>>,
) -> LoweredFeedReadback {
    let Some(runtime) = reflex_runtime else {
        return LoweredFeedReadback::RuntimeAbsent;
    };
    let Ok(runtime) = runtime.try_lock() else {
        return LoweredFeedReadback::RuntimeLockBusy;
    };
    runtime
        .lowered_guard_thresholds_snapshot()
        .map_or(LoweredFeedReadback::SchedulerNotStarted, |snapshot| {
            LoweredFeedReadback::Ready(Box::new(snapshot))
        })
}

fn storage_pressure_status(level: synapse_storage::DiskPressureLevel) -> String {
    match level {
        synapse_storage::DiskPressureLevel::Normal => "ok",
        synapse_storage::DiskPressureLevel::Level1 => "disk_pressure_l1",
        synapse_storage::DiskPressureLevel::Level2 => "disk_pressure_l2",
        synapse_storage::DiskPressureLevel::Level3 => "disk_pressure_l3",
        synapse_storage::DiskPressureLevel::Level4 => "disk_pressure_l4",
    }
    .to_owned()
}

fn storage_maintenance_error(readback: &crate::m3::StorageMaintenanceReadback) -> Option<String> {
    let mut reasons = Vec::new();
    let pressure_probe_active = storage_pressure_probe_active(readback);
    if readback.maintenance_supported && !readback.gc_task_running {
        reasons.push("storage GC task is not running".to_owned());
    }
    if readback.maintenance_supported && !readback.checkpoint_task_running {
        reasons.push("storage checkpoint task is not running".to_owned());
    }
    if !readback.pressure_task_running {
        reasons.push("storage pressure task is not running".to_owned());
    }
    if !readback.pressure_probe.observed && !pressure_probe_active {
        reasons.push("storage pressure probe has not completed successfully".to_owned());
    }
    if readback.maintenance_supported
        && let Some(error) = &readback.gc_task.last_error
    {
        reasons.push(format!("storage GC last_error={error}"));
    }
    if readback.maintenance_supported
        && let Some(error) = &readback.checkpoint_task.last_error
    {
        reasons.push(format!("storage checkpoint last_error={error}"));
    }
    if let Some(error) = &readback.pressure_probe.last_error {
        reasons.push(format!("storage pressure last_error={error}"));
    }
    (!reasons.is_empty()).then(|| reasons.join("; "))
}

fn storage_tick_active(started: Option<u64>, completed: Option<u64>) -> bool {
    started.is_some_and(|started| completed.is_none_or(|completed| started > completed))
}

fn storage_gc_tick_active(readback: &crate::m3::StorageMaintenanceReadback) -> bool {
    storage_tick_active(
        readback.gc_task.last_started_unix_ms,
        readback.gc_task.last_completed_unix_ms,
    )
}

fn storage_checkpoint_tick_active(readback: &crate::m3::StorageMaintenanceReadback) -> bool {
    storage_tick_active(
        readback.checkpoint_task.last_started_unix_ms,
        readback.checkpoint_task.last_completed_unix_ms,
    )
}

fn storage_pressure_probe_active(readback: &crate::m3::StorageMaintenanceReadback) -> bool {
    storage_tick_active(
        readback.pressure_probe.last_started_unix_ms,
        readback.pressure_probe.last_completed_unix_ms,
    )
}

fn storage_maintenance_active(readback: &crate::m3::StorageMaintenanceReadback) -> bool {
    storage_gc_tick_active(readback)
        || storage_checkpoint_tick_active(readback)
        || storage_pressure_probe_active(readback)
}

fn calyx_health_cf_sizes_skipped_reason() -> String {
    "calyx backend health skips scan-bound CF size estimates; use storage summary/inspect for explicit storage readback".to_owned()
}

/// Where a Calyx tuning knob's effective value is actually decided, and what
/// tuning the configured knob does today (#1883).
///
/// Every entry here was traced to a real consumer or proved to have none. The
/// table lives beside the health reporter on purpose: the value and the verdict
/// are emitted together so a bare number can never be reported again. Each
/// `declared_at` is a `file:symbol` an operator can open; when it names a
/// constant other than the knob, THAT constant is what the daemon obeys.
struct CalyxTuningKnobFacts {
    knob: &'static str,
    enforcement: CalyxTuningKnobEnforcement,
    declared_at: &'static str,
    blocked_by_issue: &'static str,
    effect_of_tuning: &'static str,
}

const CALYX_TUNING_KNOB_FACTS: &[CalyxTuningKnobFacts] = &[
    CalyxTuningKnobFacts {
        knob: "guard_far_identity",
        enforcement: CalyxTuningKnobEnforcement::LoadBearing,
        declared_at: "crates/synapse-calyx/src/ward.rs SynapseCalyxVault::configured_guard_target_far",
        blocked_by_issue: "",
        effect_of_tuning: "sets the default target false-accept rate hygiene operation=guard_calibrate certifies for identity slots; an explicit per-request target_far still wins, and a value above calyx-ward's per-aspect ceiling is refused loudly rather than ignored",
    },
    CalyxTuningKnobFacts {
        knob: "guard_far_content",
        enforcement: CalyxTuningKnobEnforcement::LoadBearing,
        declared_at: "crates/synapse-calyx/src/ward.rs SynapseCalyxVault::configured_guard_target_far",
        blocked_by_issue: "",
        effect_of_tuning: "sets the default target false-accept rate hygiene operation=guard_calibrate certifies for content slots; an explicit per-request target_far still wins, and a value above calyx-ward's per-aspect ceiling is refused loudly rather than ignored",
    },
    CalyxTuningKnobFacts {
        knob: "guard_far_stylistic",
        enforcement: CalyxTuningKnobEnforcement::LoadBearing,
        declared_at: "crates/synapse-calyx/src/ward.rs SynapseCalyxVault::configured_guard_target_far",
        blocked_by_issue: "",
        effect_of_tuning: "sets the default target false-accept rate hygiene operation=guard_calibrate certifies for stylistic slots; an explicit per-request target_far still wins, and a value above calyx-ward's per-aspect ceiling is refused loudly rather than ignored",
    },
    CalyxTuningKnobFacts {
        knob: "fusion_k",
        enforcement: CalyxTuningKnobEnforcement::LoadBearing,
        declared_at: "crates/synapse-calyx/src/find.rs -> calyx_search::FusionTuning::rrf_k -> calyx_sextant::FusionContext::rrf_k -> fusion::rrf::rrf_contribution; the untuned default is the single workspace declaration calyx_core::RRF_K_DEFAULT",
        blocked_by_issue: "",
        effect_of_tuning: "sets the k in the Reciprocal Rank Fusion law score(d) = SUM w_s/(k + rank_s(d)) that every fused find scores with; the reported rrf_k and rrf_formula on each find report are interpolated from this same value, and each reproducible fusion payload records the k it ran under so retuning cannot change what a past query replays to",
    },
    CalyxTuningKnobFacts {
        knob: "fusion_slot_weights",
        enforcement: CalyxTuningKnobEnforcement::LoadBearing,
        declared_at: "crates/synapse-calyx/src/find.rs -> calyx_search::FusionTuning::slot_weights -> engine_fusion::weights_for -> calyx_sextant::FusionContext::weights",
        blocked_by_issue: "",
        effect_of_tuning: "overrides the exact per-slot multiplier used by weighted RRF; unknown panel slots and an all-zero effective searched set fail closed",
    },
    CalyxTuningKnobFacts {
        knob: "index_m_max",
        enforcement: CalyxTuningKnobEnforcement::LoadBearing,
        declared_at: "calyx-search persisted generation manifest dense_index_config.m_max",
        blocked_by_issue: "",
        effect_of_tuning: "sets DiskANN graph maximum degree when a new generation is built; the immutable manifest records and query open reads the same configuration",
    },
    CalyxTuningKnobFacts {
        knob: "index_ef_construction",
        enforcement: CalyxTuningKnobEnforcement::LoadBearing,
        declared_at: "calyx-search persisted generation manifest dense_index_config.ef_construction",
        blocked_by_issue: "",
        effect_of_tuning: "sets the DiskANN construction candidate width for a newly published generation",
    },
    CalyxTuningKnobFacts {
        knob: "index_beamwidth",
        enforcement: CalyxTuningKnobEnforcement::LoadBearing,
        declared_at: "calyx-search persisted generation manifest dense_index_config.beamwidth",
        blocked_by_issue: "",
        effect_of_tuning: "sets query-time DiskANN beamwidth from the physically published generation manifest",
    },
    CalyxTuningKnobFacts {
        knob: "index_ef_search",
        enforcement: CalyxTuningKnobEnforcement::LoadBearing,
        declared_at: "calyx-search persisted generation manifest dense_index_config.ef_search",
        blocked_by_issue: "",
        effect_of_tuning: "sets query-time DiskANN search and rescore width from the physically published generation manifest",
    },
    CalyxTuningKnobFacts {
        knob: "index_alpha",
        enforcement: CalyxTuningKnobEnforcement::LoadBearing,
        declared_at: "calyx-search persisted generation manifest dense_index_config.alpha",
        blocked_by_issue: "",
        effect_of_tuning: "sets DiskANN RobustPrune alpha when a new generation is built",
    },
    CalyxTuningKnobFacts {
        knob: "index_quant_bits_by_slot",
        enforcement: CalyxTuningKnobEnforcement::LoadBearing,
        declared_at: "calyx-search persisted generation manifest dense_index_config.quant_bits_by_slot -> DiskAnn PQ candidate sidecar -> exact packed raw-sidecar rerank",
        blocked_by_issue: "",
        effect_of_tuning: "32 keeps full-precision candidate scoring; 4 or 8 builds a PQ sidecar, over-retrieves candidates, and requires exact raw-vector reranking before returning hits",
    },
];

/// Renders every tuning knob as `value + enforcement verdict + the code that
/// actually decides`.
fn calyx_tuning_knob_report(
    tuning: &synapse_calyx::SynapseCalyxTuningConfig,
) -> Vec<CalyxTuningKnobStatus> {
    CALYX_TUNING_KNOB_FACTS
        .iter()
        .map(|facts| CalyxTuningKnobStatus {
            knob: facts.knob.to_owned(),
            configured_value: calyx_tuning_knob_value(tuning, facts.knob),
            enforcement: facts.enforcement,
            effective_value_declared_at: facts.declared_at.to_owned(),
            blocked_by_issue: facts.blocked_by_issue.to_owned(),
            effect_of_tuning: facts.effect_of_tuning.to_owned(),
        })
        .collect()
}

/// Reads one knob out of the live tuning config by name.
///
/// An unknown name is reported as such rather than silently rendered as a
/// default, so a knob added to the config without a traced verdict in
/// `CALYX_TUNING_KNOB_FACTS` shows up as unreported instead of disappearing.
fn calyx_tuning_knob_value(tuning: &synapse_calyx::SynapseCalyxTuningConfig, knob: &str) -> String {
    match knob {
        "guard_far_identity" => tuning.guard_far_identity.to_string(),
        "guard_far_content" => tuning.guard_far_content.to_string(),
        "guard_far_stylistic" => tuning.guard_far_stylistic.to_string(),
        "fusion_k" => tuning.fusion_k.to_string(),
        "fusion_slot_weights" => format!("{:?}", tuning.fusion_slot_weights),
        "index_m_max" => tuning.index_m_max.to_string(),
        "index_ef_construction" => tuning.index_ef_construction.to_string(),
        "index_beamwidth" => tuning.index_beamwidth.to_string(),
        "index_ef_search" => tuning.index_ef_search.to_string(),
        "index_alpha" => tuning.index_alpha.to_string(),
        "index_quant_bits_by_slot" => format!("{:?}", tuning.index_quant_bits_by_slot),
        other => format!("<unmapped tuning knob {other}>"),
    }
}

fn apply_storage_maintenance_fields(
    health: &mut SubsystemHealth,
    readback: &crate::m3::StorageMaintenanceReadback,
) {
    health.storage_maintenance_supported = Some(readback.maintenance_supported);
    health.storage_maintenance_unsupported_reason = readback.unsupported_reason.clone();
    health.storage_gc_task_running = Some(readback.gc_task_running);
    health.storage_gc_tick_active = Some(storage_gc_tick_active(readback));
    health.storage_checkpoint_task_running = Some(readback.checkpoint_task_running);
    health.storage_checkpoint_tick_active = Some(storage_checkpoint_tick_active(readback));
    health.storage_pressure_task_running = Some(readback.pressure_task_running);
    health.storage_pressure_probe_active = Some(storage_pressure_probe_active(readback));
    health.storage_pressure_probe_observed = Some(readback.pressure_probe.observed);
    health.storage_pressure_last_free_bytes = readback.pressure_probe.last_free_bytes;
    health.storage_pressure_last_level = readback
        .pressure_probe
        .last_level
        .map(|level| format!("{level:?}"));
    health.storage_gc_last_started_unix_ms = readback.gc_task.last_started_unix_ms;
    health.storage_gc_last_completed_unix_ms = readback.gc_task.last_completed_unix_ms;
    health.storage_gc_last_duration_ms = readback.gc_task.last_duration_ms;
    health.storage_gc_last_error = readback.gc_task.last_error.clone();
    health.storage_gc_last_error_classification =
        readback.gc_task.last_error_classification.clone();
    health.storage_gc_last_attempt_count = Some(readback.gc_task.last_attempt_count);
    health.storage_gc_next_retry_unix_ms = readback.gc_task.next_retry_unix_ms;
    health.storage_gc_retry_exhausted = Some(readback.gc_task.retry_exhausted);
    health.storage_gc_last_successful_unix_ms = readback.gc_task.last_successful_unix_ms;
    health.storage_gc_last_successful_cf_readback_count =
        readback.gc_task.last_successful_cf_readback_count;
    health.storage_gc_last_successful_total_examined_rows =
        readback.gc_task.last_successful_total_examined_rows;
    health.storage_gc_last_successful_total_evicted_rows =
        readback.gc_task.last_successful_total_evicted_rows;
    health.storage_gc_last_successful_after_value_sum =
        readback.gc_task.last_successful_after_value_sum;
    health.storage_gc_last_unsupported_policy_skips =
        readback.gc_task.last_unsupported_policy_skips.clone();
    health.storage_gc_last_successful_source_census_pinned_seq =
        readback.gc_task.last_successful_source_census_pinned_seq;
    health.storage_gc_last_successful_source_census_pages =
        readback.gc_task.last_successful_source_census_pages;
    health.storage_gc_last_successful_source_census_base_rows =
        readback.gc_task.last_successful_source_census_base_rows;
    health.storage_gc_last_successful_source_census_referenced_rows = readback
        .gc_task
        .last_successful_source_census_referenced_rows;
    health.storage_gc_last_successful_snapshot_versions_reclaimed =
        readback.gc_task.last_successful_snapshot_versions_reclaimed;
    health.storage_gc_last_successful_snapshot_version_detail = readback
        .gc_task
        .last_successful_snapshot_version_detail
        .clone();
    health.storage_checkpoint_last_started_unix_ms = readback.checkpoint_task.last_started_unix_ms;
    health.storage_checkpoint_last_completed_unix_ms =
        readback.checkpoint_task.last_completed_unix_ms;
    health.storage_checkpoint_last_duration_ms = readback.checkpoint_task.last_duration_ms;
    health.storage_checkpoint_last_error = readback.checkpoint_task.last_error.clone();
    health.storage_checkpoint_last_error_classification =
        readback.checkpoint_task.last_error_classification.clone();
    health.storage_checkpoint_last_attempt_count =
        Some(readback.checkpoint_task.last_attempt_count);
    health.storage_checkpoint_next_retry_unix_ms = readback.checkpoint_task.next_retry_unix_ms;
    health.storage_checkpoint_retry_exhausted = Some(readback.checkpoint_task.retry_exhausted);
    health.storage_checkpoint_last_successful_unix_ms =
        readback.checkpoint_task.last_successful_unix_ms;
    health.storage_checkpoint_last_successful_cf_readback_count =
        readback.checkpoint_task.last_successful_cf_readback_count;
    health.storage_checkpoint_last_successful_total_examined_rows =
        readback.checkpoint_task.last_successful_total_examined_rows;
    health.storage_checkpoint_last_successful_total_evicted_rows =
        readback.checkpoint_task.last_successful_total_evicted_rows;
    health.storage_checkpoint_last_successful_after_value_sum =
        readback.checkpoint_task.last_successful_after_value_sum;
    health.storage_pressure_last_started_unix_ms = readback.pressure_probe.last_started_unix_ms;
    health.storage_pressure_last_completed_unix_ms = readback.pressure_probe.last_completed_unix_ms;
    health.storage_pressure_last_duration_ms = readback.pressure_probe.last_duration_ms;
    health.storage_pressure_last_error = readback.pressure_probe.last_error.clone();
}

impl SynapseService {
    pub(crate) fn health_payload_for_session(
        &self,
        session_id: Option<&str>,
        detail: HealthDetail,
    ) -> Health {
        self.health_payload_with_http_sessions_and_session_detail(None, session_id, detail, None)
    }

    pub(crate) fn health_payload_with_http_sessions_and_error(
        &self,
        active_sessions: Option<usize>,
        http_session_read_error: Option<String>,
    ) -> Health {
        self.health_payload_with_http_sessions_and_session_detail(
            active_sessions,
            None,
            HealthDetail::Full,
            http_session_read_error,
        )
    }

    pub(crate) fn health_payload_with_http_sessions_and_session_detail(
        &self,
        active_sessions: Option<usize>,
        session_id: Option<&str>,
        detail: HealthDetail,
        http_session_read_error: Option<String>,
    ) -> Health {
        let mut subsystems = BTreeMap::new();
        subsystems.insert("storage".to_owned(), self.storage_health());
        subsystems.insert("calyx_vault".to_owned(), self.calyx_vault_health());
        // The scheduled physical verification of that vault, as its own reading
        // (#2059). `calyx_vault` reports what the vault *is*; this reports
        // whether anything has proven it intact, and when.
        subsystems.insert(
            "vault_verify".to_owned(),
            crate::server::operational_facades::hygiene::health_subsystem(),
        );
        subsystems.insert(
            "oracle_readiness".to_owned(),
            self.oracle_readiness_health(),
        );
        // Whether the assist surface can compose a next action right now
        // (#2068 clause 5). Read-only: it point-reads the frozen artifact
        // pointer, and never asks the composer to run.
        subsystems.insert(
            "assist_next_action".to_owned(),
            self.assist_next_action_health(),
        );
        subsystems.insert("calyx_hot_path".to_owned(), self.calyx_hot_path_health());
        subsystems.insert(
            "calyx_search_generation".to_owned(),
            self.calyx_search_generation_health(),
        );
        subsystems.insert(
            "calyx_panel_coverage".to_owned(),
            Self::calyx_panel_coverage_health(),
        );
        subsystems.insert(
            "calyx_derived_state".to_owned(),
            Self::calyx_derived_state_health(),
        );
        subsystems.insert(
            "calyx_lens_coverage".to_owned(),
            Self::calyx_lens_coverage_health(),
        );
        subsystems.insert("reflex".to_owned(), self.reflex_health());
        subsystems.insert("profiles".to_owned(), self.profile_health());
        subsystems.insert("perception".to_owned(), self.perception_health());
        subsystems.insert("action".to_owned(), self.action_health());
        subsystems.insert("audio".to_owned(), self.audio_health());
        subsystems.insert(
            "chrome_bridge".to_owned(),
            crate::chrome_debugger_bridge::health_subsystem(),
        );
        subsystems.insert(
            "http".to_owned(),
            self.http_health(active_sessions, http_session_read_error),
        );
        subsystems.insert("process_qos".to_owned(), Self::process_qos_health());
        subsystems.insert("daemon_drain".to_owned(), self.daemon_drain_health());
        subsystems.insert("usage_writer".to_owned(), self.usage_writer_health());
        subsystems.insert(
            "daemon_lifecycle".to_owned(),
            crate::daemon_lifecycle::health_subsystem(),
        );
        subsystems.insert(
            "public_tool_registry".to_owned(),
            self.public_tool_registry_health(),
        );
        subsystems.insert("facade_contract".to_owned(), self.facade_contract_health());
        // #1971 finding 1: the commit these bytes were built from, and whether
        // the checkout has moved past it. Without this, a daemon running code
        // many commits behind `main` is indistinguishable from a current one,
        // and the whole accumulated delta ships on the next unrelated deploy.
        subsystems.insert(
            "build_provenance".to_owned(),
            crate::server::build_provenance::health_subsystem(),
        );
        subsystems.insert("shell_jobs".to_owned(), Self::shell_job_recovery_health());
        let tool_surface = self.tool_surface_fingerprint(session_id);
        if let Some(error) = &tool_surface.error {
            subsystems.insert(
                "tool_surface".to_owned(),
                SubsystemHealth {
                    status: "error".to_owned(),
                    detail: Some(error.clone()),
                    ..SubsystemHealth::default()
                },
            );
        }
        let ok = subsystems.values().all(|health| health.status != "error");
        apply_health_detail(&mut subsystems, detail);
        Health {
            ok,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            build: crate::server::build_provenance::short_commit(),
            pid: std::process::id(),
            uptime_s: self.started_at.elapsed().as_secs(),
            tool_count: tool_surface.names.len(),
            tool_surface_sha256: tool_surface.sha256,
            tool_names: tool_surface.names,
            subsystems,
        }
    }

    /// Reports the persisted search generation as a named deficiency rather than
    /// as silence (issue #1891).
    ///
    /// Recall depends entirely on this generation. Before this subsystem existed,
    /// `health` said nothing about it: an absent index, a staked rebuild marker,
    /// or a generation lagging so far behind the vault that every query fails
    /// closed were all invisible until an operator called `find` and read the
    /// error. An absent or unusable generation is an `error` here, because the
    /// capability every recall path needs is unavailable.
    ///
    /// **#2075.** "The generation" was the active panel's, and reporting one
    /// generation's health as search's health is the same lying-rollup shape
    /// one level down. `find` accepts an explicit `panel_version` for every
    /// version with a code-declared slot contract, and on the live vault three
    /// of those — the outcome-bearing episode, agent-transcript and mcp-usage
    /// corpora — had no persisted generation at all, so every fused query
    /// naming them hard-errored while this subsystem reported `ok`. The
    /// declared-queryable set is now measured against the built set, and every
    /// generation's own state is published in
    /// `calyx_search_generation_panels` rather than summarised into a verdict
    /// that can hide it.
    fn calyx_search_generation_health(&self) -> SubsystemHealth {
        let status = match self.m3_state.try_lock() {
            Ok(state) => state.calyx_search_generation_status(),
            Err(error) => return state_lock_unavailable_health("M3", error),
        };
        let Some(status) = status else {
            return SubsystemHealth {
                status: "disabled".to_owned(),
                detail: Some(
                    "no storage handle is open, so no vault search generation exists to report"
                        .to_owned(),
                ),
                ..SubsystemHealth::default()
            };
        };
        let status = match status {
            Ok(status) => status,
            Err(error) => {
                return SubsystemHealth {
                    status: "error".to_owned(),
                    detail: Some(format!(
                        "reading the persisted search generation state failed, so whether recall                          can serve is unknown: {error}"
                    )),
                    ..SubsystemHealth::default()
                };
            }
        };
        // The live status is read on the cheap path, which does not scan for the
        // changed-key delta — so on its own it can only report
        // `built_delta_unmeasured`. The derived-state maintainer measures the
        // delta on every tick, so its last measurement is folded in here.
        //
        // This matters because the delta, not the sequence lag, is the quantity
        // the query-time limit is enforced on. Classifying on the lag reported
        // `built` on a generation whose delta was 17,785 keys against a limit of
        // 8,192, while every query failed closed (#1891).
        let derived_state = synapse_storage::derived_state::derived_state_readback();
        let measured = derived_state
            .last_search_state_after
            .as_ref()
            .filter(|after| after.panel_version == status.panel_version);
        let measured_delta = measured.and_then(|after| after.delta_changed_keys);
        let measured_at = measured.and_then(|after| after.delta_measured_at_unix_ms);
        // Where the count came from, not only how large it is (#1901). A delta
        // measured over every panel's `Base` churn could not distinguish a
        // genuinely stale generation from a bystander charged for another
        // panel's ingest, so the composition travels with the number.
        let measured_composition = measured.and_then(|after| after.delta_composition.clone());
        // The remediation travels with the state it explains. Overriding one
        // without the other left `state=built status=ok` carrying "this read did
        // not measure the changed-key delta, so whether a query can reconcile
        // the generation is unknown" — a payload contradicting itself, which is
        // the same lying-surface shape in miniature.
        let (state, remediation) = match (status.state.as_str(), measured_delta) {
            ("built_delta_unmeasured", Some(keys)) if keys > status.max_reconciled_delta_keys => (
                "lagging".to_owned(),
                format!(
                    "the derived-state maintainer measured {keys} changed keys against the bounded                      delta-reconciliation limit {}, so queries fail closed with                      CALYX_SEARCH_DELTA_REBASE_REQUIRED until the generation is rebuilt.",
                    status.max_reconciled_delta_keys
                ),
            ),
            ("built_delta_unmeasured", Some(keys)) => (
                "built".to_owned(),
                format!(
                    "none; the derived-state maintainer measured {keys} changed keys against the                      limit {}",
                    status.max_reconciled_delta_keys
                ),
            ),
            _ => (status.state.clone(), status.remediation.clone()),
        };
        // `built` is the only healthy state: every other value means recall
        // cannot serve, is expected to fail closed, or is unverified, and
        // reporting any of those as `ok` is the lying surface #1891/#1907 were
        // about.
        //
        // But `error` is not the only alternative to `ok`, and collapsing them
        // was its own lying surface (#1914). Before the derived-state maintainer's
        // first tick there is no measurement to fold in, so the state is still
        // `built_delta_unmeasured` — not because anything is wrong, but because
        // nothing has been measured yet. Calling that `error` made `health.ok`
        // false for the first ~5.5 minutes of EVERY daemon start, which is
        // exactly when a gate or operator is most likely to be watching, and
        // trains them to discount the flag.
        //
        // The distinction is load bearing:
        //   error    — measured, and recall cannot serve. Act now.
        //   starting — not measured yet. Nothing is known to be wrong. Wait a tick.
        //
        // `starting` matches what `calyx_derived_state` and `calyx_lens_coverage`
        // already report for their own pre-first-tick state, so the three agree
        // instead of one of them flipping the global flag.
        //
        // Deliberately narrow: this is ONLY the never-measured case. A stale
        // measurement, a missing manifest, an unreadable panel state and a
        // `lagging` generation are all measured facts about a generation that
        // cannot serve, and every one of them stays `error`.
        let never_measured = measured_delta.is_none() && state == "built_delta_unmeasured";
        // #1938: the active panel being healthy says nothing about the other
        // published generations, and until this it was the only thing reported.
        // A generation that cannot be maintained, or one whose maintenance
        // failed, means some corpus is heading for — or already past — the bound
        // at which its every query fails closed. That is not an `ok` vault
        // merely because the manifest happens to point somewhere else.
        let sweep = derived_state.last_search_sweep.as_ref();
        let sweep_unmaintainable = sweep.map_or(0, |sweep| {
            sweep
                .generations
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.disposition,
                        synapse_storage::search_sweep::GenerationDisposition::UnmaintainableNoContract
                    )
                })
                .count() as u64
        });
        // #1972: a closed superseded version of a live panel is *reclaimable*,
        // not unknown. Counting it as unmaintainable put a permanent floor under
        // this subsystem — the live vault carried `unmaintainable=1` for a
        // superseded timeline generation continuously, so `degraded` taught an
        // operator nothing and the next genuinely-unknown generation would have
        // been invisible against that background. It is still reported, in its
        // own field, with the exact op that clears it.
        let sweep_retirable = sweep.map_or(0, |sweep| {
            sweep
                .generations
                .iter()
                .filter(|entry| entry.disposition.is_retirable())
                .count() as u64
        });
        let sweep_failed = sweep.map_or(0, |sweep| {
            sweep
                .generations
                .iter()
                .filter(|entry| entry.disposition.is_failure())
                .count() as u64
        });
        let sweep_lagging = sweep.map_or(0, |sweep| {
            sweep
                .generations
                .iter()
                .filter(|entry| entry.keys_to_bound() == Some(0))
                .count() as u64
        });
        // #2075: every count above measures generations that EXIST. A
        // generation that was never built has no directory, so a disk census
        // cannot see it — which is how the three outcome-bearing corpora came
        // to have no search index at all while this subsystem reported `ok`
        // from the active panel's healthy generation, and `find
        // panel_version=…` hard-errored SYNAPSE_CALYX_FIND_INDEX_STALE on every
        // one of them. The comparison that answers "can fused recall serve" is
        // the declared-queryable set against the built set, and this is it.
        let unbuilt_declared: Vec<u32> = sweep.map_or_else(Vec::new, |sweep| {
            sweep.unbuilt_declared_queryable_panel_versions()
        });
        // The three sweep conditions are NOT the same severity, and collapsing
        // them was wrong (measured on the live daemon: one stranded generation
        // pinned `health.ok=false` permanently).
        //
        //   failed   — maintenance itself errored. Something that should be
        //              running is not, and the generation drifts from here on.
        //   lagging  — a generation is already past its bound, so every query
        //              against it is failing right now.
        //   unmaintainable — a generation is published for a panel version with
        //              no code-declared contract. Permanent, benign, and
        //              operator-owned: nothing is running that could fix it, no
        //              live query is degraded by it, and it will never change on
        //              its own. `error` here is an alarm that can never be
        //              cleared by the daemon, which is exactly the flag-fatigue
        //              #1914 fixed for the pre-first-tick case. It is `degraded`,
        //              reported in its own field with its named action.
        //
        //   declared-queryable but unbuilt — a panel the tool contract says a
        //              caller may query has no persisted generation, so every
        //              fused find naming it is failing closed right now. It is
        //              `degraded` rather than `error` for one reason only: the
        //              sweep now enrolls these panels for an initial build, so
        //              the condition clears itself on the next tick and an
        //              `error` here would be an alarm for work already
        //              scheduled. It can never be `ok` (#2075) — that is the
        //              whole defect.
        //
        // The two `degraded` causes share an arm because they share a verdict,
        // not because they are the same condition: which one fired is in
        // `calyx_search_generations_unbuilt_declared_queryable_panel_versions`
        // / `..._unmaintainable` and in the remediation below. The unbuilt test
        // is first in the disjunction and deliberately NOT gated on
        // `state == "built"`: the active generation's state says nothing about
        // a panel that has no generation at all, and letting it gate this is
        // the substitution that caused the bug.
        let health_status = if sweep_failed > 0 || sweep_lagging > 0 {
            "error"
        } else if !unbuilt_declared.is_empty() || (state == "built" && sweep_unmaintainable > 0) {
            "degraded"
        } else if state == "built" {
            "ok"
        } else if never_measured {
            "starting"
        } else {
            "error"
        };
        // The remediation must explain the status that is actually being
        // reported. A `degraded` caused by an unbuilt declared-queryable panel
        // carrying the active generation's "none; …" remediation would be the
        // same self-contradicting payload the state/remediation pairing above
        // exists to prevent (#2075).
        let remediation_for_field = if unbuilt_declared.is_empty() {
            remediation.clone()
        } else {
            format!(
                "panel version(s) {unbuilt_declared:?} are declared queryable but have no \
                 persisted search generation, so every fused find naming one fails closed with \
                 SYNAPSE_CALYX_FIND_INDEX_STALE; the unattended sweep enrolls them for an initial \
                 build, so wait one derived-state tick, or force one now with storage \
                 operation=search_rebuild expected_panel_version=<version>. The active \
                 generation's own state is unaffected: {remediation}"
            )
        };
        let slot_summary = if status.slots.is_empty() {
            "none".to_owned()
        } else {
            status
                .slots
                .iter()
                .map(|slot| {
                    format!(
                        "{}:{}/{}/rows={}",
                        slot.slot, slot.lane, slot.kind, slot.len
                    )
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        let closest =
            sweep.and_then(synapse_storage::search_sweep::SearchGenerationSweep::closest_to_bound);
        SubsystemHealth {
            calyx_search_generations_total: sweep.map(|sweep| sweep.generations.len() as u64),
            calyx_search_generations_maintained: sweep.map(|sweep| {
                sweep
                    .generations
                    .iter()
                    .filter(|entry| {
                        matches!(
                            entry.disposition,
                            synapse_storage::search_sweep::GenerationDisposition::Maintained(_)
                        )
                    })
                    .count() as u64
            }),
            calyx_search_generations_unmaintainable: sweep.map(|_| sweep_unmaintainable),
            calyx_search_generations_retirable: sweep.map(|_| sweep_retirable),
            calyx_search_generations_retirable_panel_versions: sweep.map(|sweep| {
                sweep
                    .generations
                    .iter()
                    .filter(|entry| entry.disposition.is_retirable())
                    .map(|entry| entry.panel_version)
                    .collect()
            }),
            calyx_search_generations_failed: sweep.map(|_| sweep_failed),
            calyx_search_generations_closest_panel_version: closest.map(|(panel, _)| panel),
            calyx_search_generations_closest_keys_to_bound: closest.map(|(_, keys)| keys),
            calyx_search_generations_detail: sweep
                .map(synapse_storage::search_sweep::SearchGenerationSweep::summary_line),
            calyx_search_generations_swept_at_unix_ms: derived_state.last_search_sweep_unix_ms,
            calyx_search_generations_declared_queryable: sweep
                .map(|sweep| sweep.declared_queryable_panel_versions.len() as u64),
            calyx_search_generations_unbuilt_declared_queryable: sweep
                .map(|_| unbuilt_declared.len() as u64),
            calyx_search_generations_unbuilt_declared_queryable_panel_versions: sweep
                .map(|_| unbuilt_declared.clone()),
            calyx_search_generation_panels: sweep.map(|sweep| {
                sweep
                    .generations
                    .iter()
                    .map(|entry| synapse_core::types::CalyxSearchGenerationPanel {
                        panel_version: entry.panel_version,
                        is_active_panel: entry.is_active_panel,
                        is_declared_queryable: entry.is_declared_queryable,
                        manifest_present: entry.manifest_present(),
                        disposition: entry.disposition.as_str().to_owned(),
                        keys_to_bound: entry.keys_to_bound(),
                    })
                    .collect()
            }),
            status: health_status.to_owned(),
            detail: Some(format!(
                "state={} panel_version={:?} manifest_present={} built_at_seq={:?}                  vault_latest_seq={} seq_lag={:?} delta_changed_keys={:?} delta_measured_at_unix_ms={:?}                  delta_composition={} max_reconciled_delta_keys={} rows_covered={:?}                  dense_lanes={} sparse_lanes={} age_ms={:?} rebuild_required={} slots=[{}]                  manifest_path={} panel_state_error={} remediation={}",
                state,
                status.panel_version,
                status.manifest_present,
                status.built_at_seq,
                status.vault_latest_seq,
                status.seq_lag,
                measured_delta,
                measured_at,
                measured_composition.as_deref().unwrap_or("not_measured"),
                status.max_reconciled_delta_keys,
                status.rows_covered,
                status.dense_slot_count,
                status.sparse_slot_count,
                status.age_ms,
                status.rebuild_required.as_deref().unwrap_or("none"),
                slot_summary,
                status.manifest_path.as_deref().unwrap_or("none"),
                status.panel_state_error.as_deref().unwrap_or("none"),
                remediation,
            )),
            calyx_search_generation_state: Some(state),
            calyx_search_generation_delta_changed_keys: measured_delta,
            calyx_search_generation_delta_composition: measured_composition,
            calyx_search_generation_delta_measured_at_unix_ms: measured_at,
            calyx_search_generation_panel_version: status.panel_version,
            calyx_search_generation_manifest_path: status.manifest_path,
            calyx_search_generation_manifest_present: Some(status.manifest_present),
            calyx_search_generation_built_at_seq: status.built_at_seq,
            calyx_search_generation_seq_lag: status.seq_lag,
            calyx_search_generation_max_reconciled_delta_keys: Some(
                status.max_reconciled_delta_keys,
            ),
            calyx_search_generation_rows_covered: status.rows_covered,
            calyx_search_generation_dense_slot_count: Some(status.dense_slot_count),
            calyx_search_generation_sparse_slot_count: Some(status.sparse_slot_count),
            calyx_search_generation_age_ms: status.age_ms,
            calyx_search_generation_rebuild_required: status.rebuild_required,
            calyx_search_generation_remediation: Some(remediation_for_field),
            ..SubsystemHealth::default()
        }
    }

    /// Reports whether the unattended derived-state maintainer is running and
    /// what it last decided (issues #1891, #1894).
    ///
    /// This reads a published readback, never the vault: the measurement itself
    /// happens on the maintenance tick, off the request path, so `health` stays
    /// a cheap read no matter how large the corpus grows.
    ///
    /// A maintainer that has silently stopped is exactly as dangerous as the
    /// expired generation #1891 found — the generation looks `built` right up
    /// until it does not — so a pass that has never run, or whose last run
    /// failed, reports `error` rather than staying quiet.
    fn calyx_derived_state_health() -> SubsystemHealth {
        let readback = synapse_storage::derived_state::derived_state_readback();
        let never_ran = readback.last_run_unix_ms.is_none();
        // --- The verdict is the last COMPLETED tick's, not a lifetime counter's
        // (#2080) ---
        //
        // `failure_total > 0 && last_success is None` made the first failing tick
        // of a process a permanent `error` with no path back, on a subsystem
        // whose whole job is to run again in five minutes. Worse, `failure_total`
        // counted sub-passes then, so a single broken component made the
        // condition unreachable-to-clear as well as immediate. The maintainer now
        // publishes what its last completed tick decided, and that is what is
        // read: a tick where every sub-pass ran clean clears the subsystem, and a
        // tick with a failing sub-pass reds it, on the tick — which is what an
        // operator means by "is it broken now".
        //
        // `None` after a run means the only thing that has happened is a *skip*.
        // That is not `ok`: a maintainer that only ever skips maintains nothing,
        // and it used to report `ok` because a skip touches no failure counter.
        let status = match (never_ran, readback.last_tick_failed) {
            // Not yet an error at boot: the first tick is a cadence away. It is
            // reported as `starting` so a maintainer that never arrives is still
            // distinguishable from one that is merely young.
            (true, _) => "starting",
            (false, Some(true)) => "error",
            (false, Some(false)) => "ok",
            (false, None) => "error",
        };
        SubsystemHealth {
            status: status.to_owned(),
            detail: Some(format!(
                "attempts={} success={} failure={} skipped={} subpass_failures={} advisories={} \
                 last_tick_failed={:?} last_tick_subpass_failures={:?} last_run_unix_ms={:?} \
                 last_success_unix_ms={:?} last_search_action={} last_search_reason={} \
                 last_search_elapsed_ms={:?} refresh_delta_keys_threshold={} \
                 min_rebuild_interval_ms={} last_failure_code={} last_failure_detail={} \
                 last_advisory_code={} last_advisory_detail={} last_skip={}",
                readback.attempts_total,
                readback.success_total,
                readback.failure_total,
                readback.skipped_total,
                readback.subpass_failures_total,
                readback.advisories_total,
                readback.last_tick_failed,
                readback.last_tick_subpass_failures,
                readback.last_run_unix_ms,
                readback.last_success_unix_ms,
                readback.last_search_action.as_deref().unwrap_or("none"),
                readback.last_search_reason.as_deref().unwrap_or("none"),
                readback.last_search_elapsed_ms,
                readback.refresh_delta_keys_threshold,
                readback.min_rebuild_interval_ms,
                readback.last_failure_code.as_deref().unwrap_or("none"),
                readback.last_failure_detail.as_deref().unwrap_or("none"),
                readback.last_advisory_code.as_deref().unwrap_or("none"),
                readback.last_advisory_detail.as_deref().unwrap_or("none"),
                readback.last_skip_code.as_deref().unwrap_or("none"),
            )),
            calyx_derived_state_success_total: Some(readback.success_total),
            calyx_derived_state_skipped_total: Some(readback.skipped_total),
            calyx_derived_state_subpass_failures_total: Some(readback.subpass_failures_total),
            calyx_derived_state_advisories_total: Some(readback.advisories_total),
            calyx_derived_state_last_tick_failed: readback.last_tick_failed,
            calyx_derived_state_last_tick_subpass_failures: Some(
                readback.last_tick_subpass_failures.clone(),
            ),
            calyx_derived_state_last_advisory_code: readback.last_advisory_code.clone(),
            calyx_derived_state_last_advisory_detail: readback.last_advisory_detail.clone(),
            calyx_derived_state_last_search_action: readback.last_search_action,
            calyx_derived_state_last_search_reason: readback.last_search_reason,
            calyx_derived_state_last_run_unix_ms: readback.last_run_unix_ms,
            calyx_derived_state_last_success_unix_ms: readback.last_success_unix_ms,
            calyx_derived_state_attempts_total: Some(readback.attempts_total),
            calyx_derived_state_failure_total: Some(readback.failure_total),
            calyx_derived_state_last_failure_code: readback.last_failure_code,
            calyx_derived_state_last_failure_detail: readback.last_failure_detail,
            calyx_derived_state_refresh_delta_keys_threshold: Some(
                readback.refresh_delta_keys_threshold,
            ),
            calyx_derived_state_last_delta_changed_keys: readback
                .last_search_state_after
                .as_ref()
                .and_then(|status| status.delta_changed_keys),
            ..SubsystemHealth::default()
        }
    }

    /// Raises panel coverage and grounding as named deficiencies (#1927 ask 4,
    /// #1920 asks 1/4).
    ///
    /// The measured failure this exists to stop: `syn-agent-transcript-v1`
    /// bumped to a new panel version and the active generation was left holding
    /// **389 of 22,999** source rows, while `health` reported `ok` for as long as
    /// it stayed that way. Every surface scoped to the active panel — `bits`,
    /// `sufficiency`, `kernel`, and grounded anchor writes — was operating on
    /// 1.7% of the corpus, and the only way to notice was to run
    /// `hygiene grounding_gap` against a guessed version number and compare it by
    /// hand to `storage corpus_histogram`. That is the same shape as #1907, where
    /// a readback said fine while recall was zero, and it has the same fix:
    /// measure the thing, do not assume it.
    ///
    /// `error` rather than `degraded` when a panel is below the coverage floor,
    /// deliberately. A panel measuring a fraction of its corpus is not a slow
    /// subsystem — it is a subsystem returning confident answers about a corpus
    /// it cannot see.
    ///
    /// Reads the published census; never measures one on the request path.
    fn calyx_panel_coverage_health() -> SubsystemHealth {
        let readback = synapse_storage::derived_state::derived_state_readback();
        let Some(report) = readback.last_panel_coverage else {
            return SubsystemHealth {
                status: "starting".to_owned(),
                detail: Some(
                    "the derived-state maintainer has not completed a panel-coverage census yet; \
                     coverage is measured on its periodic tick, not on this request"
                        .to_owned(),
                ),
                ..SubsystemHealth::default()
            };
        };

        let min_fraction = report
            .panels
            .iter()
            .filter_map(|panel| panel.coverage_fraction)
            .fold(f32::INFINITY, f32::min);
        let min_fraction = if min_fraction.is_finite() {
            Some(min_fraction)
        } else {
            None
        };

        // Three independent reasons to fail, kept separate so the detail names
        // which one fired rather than leaving the reader to infer it.
        let mut reasons: Vec<String> = Vec::new();
        if report.decode_failures > 0 {
            reasons.push(format!(
                "{} Base rows would not decode ({}), so every count below is over a subset",
                report.decode_failures,
                report
                    .first_decode_failure
                    .as_deref()
                    .unwrap_or("<no detail>")
            ));
        }
        if !report.coverage_deficient_panels.is_empty() {
            reasons.push(format!(
                "panels below the {:.2} coverage floor: {:?}{}",
                report.coverage_floor,
                report.coverage_deficient_panels,
                if report.unbackfillable_deficient_panels.is_empty() {
                    String::new()
                } else {
                    format!(
                        " (NO re-measure path exists for {:?}; the maintainer cannot repair these)",
                        report.unbackfillable_deficient_panels
                    )
                }
            ));
        }
        if !report.unknown_panel_versions.is_empty() {
            reasons.push(format!(
                "Base holds panel generations no catalog entry claims: {:?}",
                report.unknown_panel_versions
            ));
        }
        // #1980. A stranded panel WAS grounded and the version bump lost it, so
        // every bits/sufficiency/kernel result over it silently became
        // undefined while coverage stayed at 1.0. That is a defect, not a gap,
        // and it is loud here because it is invisible everywhere else.
        if !report.anchors_stranded_panels.is_empty() {
            reasons.push(format!(
                "panels whose anchors a version bump stranded on a superseded generation: {:?};                  the grounded outcomes still exist one generation back and the active generation                  cannot ground anything until the backfill carries them across",
                report.anchors_stranded_panels
            ));
        }

        let status = if reasons.is_empty() { "ok" } else { "error" };

        SubsystemHealth {
            status: status.to_owned(),
            detail: Some(format!(
                "{}base_cf_rows={} records_total={} superseded_records={} \
                 grounding_deficient_panels={:?} no_outcome_axis_panels={:?}                  anchors_stranded_panels={:?} \
                 records_exceed_source_panels={:?} \
                 backfill={} reason={} panel={} pages={} inserted={} \
                 anchored={} elapsed_ms={} targets_owed={} targets_attempted={} \
                 targets_skipped={:?} anchor_debt_quarantined={} quarantined_identities={:?} \
                 panels=[{}]",
                if reasons.is_empty() {
                    String::new()
                } else {
                    format!("{}; ", reasons.join("; "))
                },
                report.base_cf_rows,
                report.records_total,
                report.superseded_records_total,
                report.grounding_deficient_panels,
                report.no_outcome_axis_panels,
                report.anchors_stranded_panels,
                report.records_exceed_source_panels,
                readback.last_backfill_action.as_deref().unwrap_or("<none>"),
                readback.last_backfill_reason.as_deref().unwrap_or("<none>"),
                readback.last_backfill_panel.as_deref().unwrap_or("<none>"),
                readback.last_backfill_pages.unwrap_or(0),
                readback.last_backfill_inserted_rows.unwrap_or(0),
                readback.last_backfill_outcome_anchored_rows.unwrap_or(0),
                readback.last_backfill_elapsed_ms.unwrap_or(0),
                readback.last_backfill_targets_owed,
                readback.last_backfill_targets_attempted,
                readback.last_backfill_targets_skipped,
                readback.last_anchor_debt_quarantined_total,
                readback.last_anchor_debt_quarantined,
                report.summary_line(),
            )),
            calyx_panel_coverage_panels: Some(report.panels.len() as u64),
            calyx_panel_coverage_deficient_panels: Some(
                report.coverage_deficient_panels.len() as u64
            ),
            calyx_panel_coverage_unbackfillable_panels: Some(
                report.unbackfillable_deficient_panels.len() as u64,
            ),
            calyx_panel_grounding_deficient_panels: Some(
                report.grounding_deficient_panels.len() as u64
            ),
            calyx_panel_no_outcome_axis_panels: Some(report.no_outcome_axis_panels.len() as u64),
            calyx_panel_no_outcome_axis_panel_names: Some(report.no_outcome_axis_panels.clone()),
            calyx_panel_anchors_stranded_panels: Some(report.anchors_stranded_panels.len() as u64),
            calyx_panel_anchors_stranded_panel_names: Some(report.anchors_stranded_panels.clone()),
            calyx_panel_coverage_min_fraction: min_fraction,
            calyx_panel_coverage_floor: Some(report.coverage_floor),
            calyx_panel_superseded_records: Some(report.superseded_records_total as u64),
            calyx_panel_base_cf_rows: Some(report.base_cf_rows as u64),
            calyx_panel_census_decode_failures: Some(report.decode_failures as u64),
            calyx_panel_coverage_measured_at_unix_ms: report.measured_at_unix_ms,
            calyx_panel_backfill_action: readback.last_backfill_action,
            calyx_panel_backfill_reason: readback.last_backfill_reason,
            calyx_panel_backfill_panel: readback.last_backfill_panel,
            calyx_panel_backfill_pages: readback.last_backfill_pages,
            calyx_panel_backfill_inserted_rows: readback.last_backfill_inserted_rows,
            calyx_panel_backfill_outcome_anchored_rows: readback
                .last_backfill_outcome_anchored_rows,
            calyx_panel_backfill_elapsed_ms: readback.last_backfill_elapsed_ms,
            calyx_panel_backfill_targets_owed: Some(readback.last_backfill_targets_owed),
            calyx_panel_backfill_targets_attempted: Some(readback.last_backfill_targets_attempted),
            calyx_panel_backfill_targets_skipped: Some(readback.last_backfill_targets_skipped),
            calyx_panel_anchor_debt_quarantined_total: Some(
                readback.last_anchor_debt_quarantined_total,
            ),
            calyx_panel_anchor_debt_quarantined_identities: Some(
                readback.last_anchor_debt_quarantined,
            ),
            ..SubsystemHealth::default()
        }
    }

    /// Raises panel lens coverage as a named deficiency (issue #1894, ask 2).
    ///
    /// `abundance` already computed the alarm — `blind_spot_records = 1740` out
    /// of 1,745 constellations — and no surface raised it, so the condition was
    /// visible only to whoever ran an intelligence pass by hand and knew what
    /// the zero meant. A panel carrying fewer than two co-present lenses makes
    /// weave, bits, redundancy and kernel all vacuously zero, which is a
    /// capability outage reported as a clean-looking number.
    fn calyx_lens_coverage_health() -> SubsystemHealth {
        let readback = synapse_storage::derived_state::derived_state_readback();
        let Some(coverage) = readback.last_lens_coverage else {
            return SubsystemHealth {
                status: "starting".to_owned(),
                detail: Some(
                    "the derived-state maintainer has not completed a lens-coverage pass yet; \
                     coverage is measured on its periodic tick, not on this request"
                        .to_owned(),
                ),
                ..SubsystemHealth::default()
            };
        };
        let records_measured: usize = coverage
            .panels
            .iter()
            .map(|panel| panel.records_measured)
            .sum();
        let blind_spot_records: usize = coverage
            .panels
            .iter()
            .map(|panel| panel.blind_spot_records)
            .sum();
        // A degenerate lane is not a coverage deficiency: the rows are there and
        // the lens measured them. It is a lane that cannot rank, which is worth
        // reporting without claiming the subsystem is broken (#1970).
        let status = if !coverage.deficient_panels.is_empty() {
            "error"
        } else if coverage.degenerate_lanes.is_empty() {
            "ok"
        } else {
            "degraded"
        };
        let degenerate_lane_keys: Vec<String> = coverage
            .degenerate_lanes
            .iter()
            .map(|lane| format!("{}:{}", lane.panel_version, lane.slot))
            .collect();
        SubsystemHealth {
            status: status.to_owned(),
            detail: Some(format!(
                "panels_measured={} deficient_panels={:?} blind_spot_ceiling={} \
                 sample_records_per_panel={} measured_at_unix_ms={:?} distribution_findings={:?} panels=[{}]",
                coverage.panels.len(),
                coverage.deficient_panels,
                coverage.blind_spot_ceiling,
                coverage.max_records_per_panel,
                coverage.measured_at_unix_ms,
                degenerate_lane_keys,
                coverage
                    .panels
                    .iter()
                    .map(|panel| format!(
                        "{}:n_lenses={} measurable_slots={:?} unusable_slots={:?} measured={}/{} \
                         blind_spot_records={} ({:.4})",
                        panel.panel_version,
                        panel.n_lenses,
                        panel.measurable_slots,
                        panel
                            .slot_states
                            .iter()
                            .filter(|state| !state.measurable)
                            .map(|state| state.slot)
                            .collect::<Vec<_>>(),
                        panel.records_measured,
                        panel.records_scanned,
                        panel.blind_spot_records,
                        panel.blind_spot_fraction
                    ))
                    .collect::<Vec<_>>()
                    .join(" ")
            )),
            calyx_lens_coverage_panels_measured: Some(coverage.panels.len() as u64),
            calyx_lens_coverage_deficient_panels: Some(coverage.deficient_panels.len() as u64),
            calyx_lens_coverage_blind_spot_records: Some(blind_spot_records as u64),
            calyx_lens_coverage_records_measured: Some(records_measured as u64),
            calyx_lens_coverage_measured_at_unix_ms: coverage.measured_at_unix_ms,
            calyx_lens_degenerate_lanes: Some(coverage.degenerate_lanes.len() as u64),
            calyx_lens_degenerate_lane_keys: (!degenerate_lane_keys.is_empty())
                .then_some(degenerate_lane_keys),
            ..SubsystemHealth::default()
        }
    }

    fn oracle_readiness_health(&self) -> SubsystemHealth {
        let readback = match self.m3_state.try_lock() {
            Ok(state) => state.oracle_readiness(),
            Err(error) => return state_lock_unavailable_health("M3", error),
        };
        match readback {
            None => SubsystemHealth {
                status: "disabled".to_owned(),
                detail: Some("the Calyx vault is not open, so Oracle readiness has no source of truth".to_owned()),
                ..SubsystemHealth::default()
            },
            Some(Ok(None)) => SubsystemHealth {
                status: "unmeasured".to_owned(),
                detail: Some("no persisted action readiness snapshot exists; run storage intelligence oracle_readiness".to_owned()),
                ..SubsystemHealth::default()
            },
            Some(Ok(Some(snapshot))) => {
                let ready = snapshot
                    .pointer("/report/overall")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                SubsystemHealth {
                    status: if ready { "ok" } else { "not_ready" }.to_owned(),
                    detail: Some("persisted six-tier action-domain Oracle readiness predicate".to_owned()),
                    oracle_readiness: Some(snapshot),
                    ..SubsystemHealth::default()
                }
            }
            Some(Err(error)) => SubsystemHealth {
                status: "error".to_owned(),
                detail: Some(format!("read persisted Oracle readiness snapshot: {error}")),
                ..SubsystemHealth::default()
            },
        }
    }

    /// Whether the assist surface can currently compose a next action (#2068
    /// clause 5).
    ///
    /// The `Arc<Db>` is cloned out and the M3 lock released **before** the
    /// point-read, so a health call never holds the state lock across storage
    /// I/O — the readback itself is two `CF_KV` point-reads and no composition.
    fn assist_next_action_health(&self) -> SubsystemHealth {
        let db = match self.m3_state.try_lock() {
            Ok(state) => state.db.clone(),
            // #2087: a busy M3 lock is contention, not failure — the
            // composition half is process-static behind its own lock and is
            // still published, typed `busy` (never `health.ok`-fatal). A
            // POISONED lock keeps failing closed as `error`: that is a panic
            // somewhere under M3, not an in-flight borrow.
            Err(error @ std::sync::TryLockError::Poisoned(_)) => {
                return state_lock_unavailable_health("M3", error);
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                return crate::m3::suggestions::next_action_health_m3_lock_busy();
            }
        };
        crate::m3::suggestions::next_action_health(db.as_ref())
    }

    fn calyx_vault_health(&self) -> SubsystemHealth {
        let status = match self.m3_state.try_lock() {
            Ok(state) => match state.calyx_vault_status() {
                Ok(status) => status,
                Err(error) => {
                    return SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some(format!(
                            "live Calyx vault status read failed closed: code={} source_code={} error={} remediation={}",
                            error.code,
                            error.source_code.unwrap_or("none"),
                            error.message,
                            error.remediation
                        )),
                        ..SubsystemHealth::default()
                    };
                }
            },
            Err(error) => return state_lock_unavailable_health("M3", error),
        };
        let health_status = if !status.enabled {
            "disabled"
        } else if status.last_error_code.is_some() {
            "error"
        } else if status.open {
            "ok"
        } else {
            "starting"
        };
        let tuning = status.tuning;
        let math_backend = status.math_backend;
        let anneal = status.anneal.as_ref();
        let gpu_reservation_snapshot = math_backend
            .as_ref()
            .and_then(|math| math.host_reservation.as_ref());
        let gpu_reservation = math_backend.as_ref().and_then(|math| {
            let reservation_id = math.host_reservation_id.as_deref()?;
            math.host_reservation
                .as_ref()?
                .reservations
                .iter()
                .find(|row| row.reservation_id == reservation_id)
        });
        let tuning_knobs = tuning
            .as_ref()
            .map_or_else(Vec::new, calyx_tuning_knob_report);
        let inert_tuning_knob_count = tuning_knobs
            .iter()
            .filter(|knob| !knob.enforcement.is_load_bearing())
            .count();
        let inert_tuning_knob_names = tuning_knobs
            .iter()
            .filter(|knob| !knob.enforcement.is_load_bearing())
            .map(|knob| {
                format!(
                    "{}={}[{}]",
                    knob.knob,
                    knob.configured_value,
                    knob.enforcement.as_str()
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let row_guard_sites = status
            .row_guard_census
            .iter()
            .map(|entry| CalyxRowGuardSiteStatus {
                site: entry.site.clone(),
                holds: entry.holds,
                total_held_us: entry.total_held_us,
                max_held_us: entry.max_held_us,
                mean_held_us: entry.mean_held_us,
                over_budget_holds: entry.over_budget_holds,
                starved_holds: entry.starved_holds,
            })
            .collect::<Vec<_>>();
        let row_guard_sites_exercised = row_guard_sites
            .iter()
            .filter(|entry| entry.holds > 0)
            .count();
        let row_guard_holds_total = row_guard_sites.iter().map(|entry| entry.holds).sum::<u64>();
        let row_guard_over_budget_total = row_guard_sites
            .iter()
            .map(|entry| entry.over_budget_holds)
            .sum::<u64>();
        let row_guard_starved_total = row_guard_sites
            .iter()
            .map(|entry| entry.starved_holds)
            .sum::<u64>();
        SubsystemHealth {
            status: health_status.to_owned(),
            detail: Some(format!(
                "enabled={} phase={} open={} vault_dir={} vault_id={} latest_seq={:?} last_recovered_seq={:?} torn_tail={} last_error_code={} last_calyx_error_code={} clock_mode={} math={} anneal_live_artifact={} anneal_artifact_bytes={} anneal_rollback_rows={} anneal_recent_changes={} anneal_budget_warning={} tuning_knobs_total={} tuning_knobs_inert={} inert_tuning_knobs=[{}] remediation={}",
                status.enabled,
                status.phase,
                status.open,
                status
                    .vault_dir
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), |path| path.display().to_string()),
                status.vault_id.as_deref().unwrap_or("none"),
                status.latest_seq,
                status.last_recovered_seq,
                status.torn_tail.as_deref().unwrap_or("none"),
                status.last_error_code.as_deref().unwrap_or("none"),
                status.last_calyx_error_code.as_deref().unwrap_or("none"),
                tuning
                    .as_ref()
                    .map_or("none", |config| config.clock_mode.as_str()),
                math_backend
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), |math| math.detail()),
                anneal.map_or("none", |status| status.live_artifact_sha256.as_str()),
                anneal.map_or(0, |status| status.live_artifact_bytes),
                anneal.map_or(0, |status| status.rollback_rows),
                anneal.map_or(0, |status| status.recent_changes.len()),
                anneal
                    .and_then(|status| status.budget.warning_code.as_deref())
                    .unwrap_or("none"),
                tuning_knobs.len(),
                inert_tuning_knob_count,
                inert_tuning_knob_names,
                status.remediation.as_deref().unwrap_or("none")
            )),
            calyx_vault_open: Some(status.open),
            calyx_vault_phase: Some(status.phase),
            calyx_vault_path: status.vault_dir.map(|path| path.display().to_string()),
            calyx_vault_identity_path: status.identity_path.map(|path| path.display().to_string()),
            calyx_machine_salt_path: status
                .machine_salt_path
                .map(|path| path.display().to_string()),
            calyx_vault_lock_path: status.lock_path.map(|path| path.display().to_string()),
            calyx_vault_pid_path: status.pid_path.map(|path| path.display().to_string()),
            calyx_vault_id: status.vault_id,
            calyx_vault_latest_seq: status.latest_seq,
            calyx_vault_last_recovered_seq: status.last_recovered_seq,
            calyx_vault_torn_tail: status.torn_tail,
            calyx_vault_last_error_code: status.last_error_code,
            calyx_vault_last_calyx_error_code: status.last_calyx_error_code,
            calyx_vault_last_error: status.last_error,
            calyx_vault_remediation: status.remediation,
            calyx_tuning_knobs: tuning_knobs,
            calyx_inert_tuning_knob_count: Some(inert_tuning_knob_count),
            calyx_row_guard_sites: row_guard_sites,
            calyx_row_guard_sites_exercised: Some(row_guard_sites_exercised),
            calyx_row_guard_holds_total: Some(row_guard_holds_total),
            calyx_row_guard_over_budget_total: Some(row_guard_over_budget_total),
            calyx_row_guard_starved_total: Some(row_guard_starved_total),
            calyx_vram_budget_bytes: tuning.as_ref().map(|config| config.vram_budget_bytes),
            calyx_vram_budget_enforced: math_backend
                .as_ref()
                .map(|math| math.vram_dispatch.is_some()),
            calyx_vram_dispatch_soft_cap_bytes: math_backend
                .as_ref()
                .and_then(|math| math.vram_dispatch.as_ref())
                .map(|dispatch| dispatch.soft_cap_bytes),
            calyx_vram_dispatch_allocated_bytes: math_backend
                .as_ref()
                .and_then(|math| math.vram_dispatch.as_ref())
                .map(|dispatch| dispatch.allocated_bytes),
            calyx_vram_dispatch_serving_allocated_bytes: math_backend
                .as_ref()
                .and_then(|math| math.vram_dispatch.as_ref())
                .map(|dispatch| dispatch.serving_allocated_bytes),
            calyx_vram_dispatch_anneal_allocated_bytes: math_backend
                .as_ref()
                .and_then(|math| math.vram_dispatch.as_ref())
                .map(|dispatch| dispatch.anneal_allocated_bytes),
            calyx_vram_dispatch_device_free_bytes: math_backend
                .as_ref()
                .and_then(|math| math.vram_dispatch.as_ref())
                .map(|dispatch| dispatch.device_free_bytes),
            calyx_gpu_reservation_basis: math_backend
                .as_ref()
                .and_then(|math| math.host_reservation_basis.clone()),
            calyx_gpu_reservation_state_path: gpu_reservation_snapshot
                .map(|snapshot| snapshot.state_path.clone()),
            calyx_gpu_reservation_state_sha256: gpu_reservation_snapshot
                .map(|snapshot| snapshot.state_sha256.clone()),
            calyx_gpu_reservation_device_index: gpu_reservation_snapshot
                .map(|snapshot| snapshot.device_index),
            calyx_gpu_reservation_device_uuid: gpu_reservation_snapshot
                .map(|snapshot| snapshot.device_uuid.clone()),
            calyx_gpu_reservation_device_name: gpu_reservation_snapshot
                .map(|snapshot| snapshot.device_name.clone()),
            calyx_gpu_reservation_device_total_mib: gpu_reservation_snapshot
                .map(|snapshot| snapshot.device_total_mib),
            calyx_gpu_reservation_host_cap_mib: gpu_reservation_snapshot
                .map(|snapshot| snapshot.host_cap_mib),
            calyx_gpu_reservation_required_free_mib: gpu_reservation_snapshot
                .map(|snapshot| snapshot.required_free_mib),
            calyx_gpu_reservation_headroom_mib: gpu_reservation_snapshot
                .map(|snapshot| snapshot.headroom_mib),
            calyx_gpu_reservation_last_physical_free_mib: gpu_reservation_snapshot
                .map(|snapshot| snapshot.last_physical_free_mib),
            calyx_gpu_reservation_reserved_mib: gpu_reservation_snapshot
                .map(|snapshot| snapshot.reserved_mib),
            calyx_gpu_reservation_available_mib: gpu_reservation_snapshot
                .map(|snapshot| snapshot.available_reservation_mib),
            calyx_gpu_reservation_admitted_total: gpu_reservation_snapshot
                .map(|snapshot| snapshot.admitted_total),
            calyx_gpu_reservation_rejected_total: gpu_reservation_snapshot
                .map(|snapshot| snapshot.rejected_total),
            calyx_gpu_reservation_stale_reaped_total: gpu_reservation_snapshot
                .map(|snapshot| snapshot.stale_reaped_total),
            calyx_gpu_reservation_id: gpu_reservation
                .map(|reservation| reservation.reservation_id.clone()),
            calyx_gpu_reservation_owner: gpu_reservation
                .map(|reservation| reservation.owner.clone()),
            calyx_gpu_reservation_job_id: gpu_reservation
                .map(|reservation| reservation.job_id.clone()),
            calyx_gpu_reservation_command: gpu_reservation
                .map(|reservation| reservation.command.clone()),
            calyx_gpu_reservation_pid: gpu_reservation.map(|reservation| reservation.pid),
            calyx_gpu_reservation_requested_mib: gpu_reservation
                .map(|reservation| reservation.requested_mib),
            calyx_gpu_reservation_acquired_unix_ms: gpu_reservation
                .map(|reservation| reservation.acquired_unix_ms.to_string()),
            calyx_gpu_reservation_lease_file: gpu_reservation
                .map(|reservation| reservation.lease_file.clone()),
            calyx_gpu_reservation_last_rejection: gpu_reservation_snapshot
                .and_then(|snapshot| snapshot.last_rejection.as_ref())
                .map(|rejection| {
                    format!(
                        "owner={} job_id={} command={} pid={} requested_mib={} at_unix_ms={} reason={}",
                        rejection.owner,
                        rejection.job_id,
                        rejection.command,
                        rejection.pid,
                        rejection.requested_mib,
                        rejection.at_unix_ms,
                        rejection.reason
                    )
                }),
            calyx_gpu_runtime_readback_code: math_backend
                .as_ref()
                .and_then(|math| math.runtime_readback_code.clone()),
            calyx_gpu_runtime_readback_error: math_backend
                .as_ref()
                .and_then(|math| math.runtime_readback_error.clone()),
            calyx_math_backend: math_backend
                .as_ref()
                .map(|math| math.selected_backend.clone()),
            calyx_math_backend_requested: math_backend
                .as_ref()
                .map(|math| math.requested_backend.as_str().to_owned())
                .or_else(|| {
                    tuning
                        .as_ref()
                        .map(|config| config.math_backend.as_str().to_owned())
                }),
            calyx_math_cuda_compiled: math_backend.as_ref().map(|math| math.cuda_compiled),
            calyx_registry_embedding_runtimes_compiled: Some(
                calyx_registry::EMBEDDING_RUNTIMES_COMPILED,
            ),
            calyx_ward_model_lenses_compiled: Some(calyx_ward::MODEL_LENSES_COMPILED),
            calyx_math_device_name: math_backend.as_ref().map(|math| math.device_name.clone()),
            calyx_math_device_vram_mib: math_backend.as_ref().and_then(|math| math.device_vram_mib),
            calyx_math_device_avx512: math_backend.as_ref().map(|math| math.device_avx512),
            calyx_math_cpu_avx512_available: math_backend
                .as_ref()
                .map(|math| math.cpu_avx512_available),
            calyx_math_cpu_simd_path: math_backend.as_ref().map(|math| math.cpu_simd_path.clone()),
            calyx_math_fallback_code: math_backend
                .as_ref()
                .and_then(|math| math.fallback_code.clone()),
            calyx_math_fallback_source_code: math_backend
                .as_ref()
                .and_then(|math| math.fallback_source_code.clone()),
            calyx_math_fallback_error: math_backend
                .as_ref()
                .and_then(|math| math.fallback_error.clone()),
            calyx_math_probe_status: math_backend.as_ref().map(|math| math.probe.status.clone()),
            calyx_math_probe_detail: math_backend.as_ref().map(|math| math.probe.detail.clone()),
            calyx_math_probe_tolerance: math_backend.as_ref().map(|math| math.probe.tolerance),
            calyx_math_probe_dot: math_backend.as_ref().map(|math| math.probe.dot.clone()),
            calyx_math_probe_cosine: math_backend.as_ref().map(|math| math.probe.cosine.clone()),
            calyx_math_probe_l2_squared: math_backend
                .as_ref()
                .map(|math| math.probe.l2_squared.clone()),
            calyx_math_probe_topk: math_backend.as_ref().map(|math| {
                math.probe
                    .topk
                    .iter()
                    .map(|entry| CalyxMathProbeTopKEntry {
                        index: entry.index,
                        score: entry.score,
                    })
                    .collect()
            }),
            calyx_clock_mode: tuning
                .as_ref()
                .map(|config| config.clock_mode.as_str().to_owned()),
            calyx_fixed_clock_unix_ms: tuning
                .as_ref()
                .and_then(|config| config.fixed_clock_unix_ms),
            calyx_rng_seed: tuning.as_ref().map(|config| config.rng_seed),
            ..SubsystemHealth::default()
        }
    }

    /// Makes the Calyx hot-path boundary externally observable (#1686).
    ///
    /// The doctrine is that latency-critical loops consume only frozen, lowered
    /// artifacts and never issue a live Calyx call. Everything an operator needs
    /// to *check* that claim — without attaching a debugger — is here:
    ///
    /// * `violations_total` must read `0`. It is an always-on counter, not a
    ///   `debug_assert!`, precisely because `debug_assert!` is compiled out of
    ///   optimized builds by default, so a release acceptance run would
    ///   otherwise be measuring nothing.
    /// * `artifact_file_sha256` is the SHA-256 of the whole published file and
    ///   is directly comparable with `Get-FileHash`;
    ///   `artifact_content_sha256` is the payload fingerprint the envelope
    ///   records and that the consumer re-verifies on every refresh.
    /// * `artifact_hot_reads_total` proves the tick is reading the artifact, and
    ///   `publish_*` proves the maintenance pass is producing it.
    ///
    /// The subsystem reports `error` on any violation, on a failing publisher,
    /// or on a tick pinned to the fail-closed defaults while a vault is open —
    /// each of those means the boundary is not delivering what it claims.
    fn calyx_hot_path_health(&self) -> SubsystemHealth {
        let violations_total = synapse_reflex::hot_path::violations_total();
        let last_violation = synapse_reflex::hot_path::last_violation();
        let publish = synapse_storage::maintenance::lowering_publish_readback();
        let readback = match self.m3_state.try_lock() {
            Ok(state) => lowered_feed_readback(state.reflex_runtime.as_ref()),
            Err(error) => return state_lock_unavailable_health("M3", error),
        };
        let feed = readback.snapshot();
        // An unavailable feed must name its condition or say that it cannot.
        // See `feed_unavailability`.
        let (feed_unavailable_code, feed_unavailable_reason) = feed_unavailability(&readback);

        let mut boundary = synapse_core::CalyxHotPathBoundaryHealth {
            tick_thread_tagged: Some(synapse_reflex::hot_path::tick_thread_tagged()),
            hot_ticks_total: Some(synapse_reflex::hot_path::hot_ticks_total()),
            violations_total: Some(violations_total),
            // Only an observed violation gets a violation code. A constant label
            // beside `violations_total = 0` would announce an event that never
            // happened.
            violation_code: violation_code_for(last_violation.as_ref()),
            last_violation_operation: last_violation
                .as_ref()
                .map(|violation| violation.operation.clone()),
            last_violation_unix_ms: last_violation
                .as_ref()
                .map(|violation| violation.at_unix_ms),
            scheduler_started: Some(feed.is_some()),
            feed_unavailable_code,
            feed_unavailable_reason: feed_unavailable_reason.clone(),
            publish_attempts_total: Some(publish.attempts_total),
            publish_success_total: Some(publish.success_total),
            publish_failure_total: Some(publish.failure_total),
            publish_skipped_total: Some(publish.skipped_total),
            publish_last_success_unix_ms: publish.last_success_unix_ms,
            publish_last_content_sha256: publish.last_content_sha256.clone(),
            publish_last_error_code: publish.last_error_code.clone(),
            publish_last_error: publish.last_error.clone(),
            ..synapse_core::CalyxHotPathBoundaryHealth::default()
        };
        if let Some(feed) = feed {
            boundary.artifact_state = Some(feed.state.to_owned());
            boundary.artifact_safe_default_code = feed.safe_default_code.clone();
            boundary.artifact_safe_default_detail = feed.safe_default_detail.clone();
            boundary.artifact_safe_default_remediation = feed.safe_default_remediation.clone();
            boundary.artifact_content_sha256 = feed.content_sha256.clone();
            boundary.artifact_generation = feed.generation;
            boundary.artifact_source_ledger_seq = feed.source_ledger_seq;
            boundary.artifact_vault_id = feed.vault_id.clone();
            boundary.artifact_produced_at_unix_ms = feed.produced_at_unix_ms;
            boundary.artifact_staleness_bound_ms = feed.staleness_bound_ms;
            boundary.artifact_hot_reads_total = Some(feed.hot_reads_total);
            boundary.artifact_refreshes_total = Some(feed.refreshes_total);
            boundary.artifact_fresh_refreshes_total = Some(feed.fresh_refreshes_total);
            boundary.artifact_safe_default_refreshes_total =
                Some(feed.safe_default_refreshes_total);
            boundary.refresher_running = Some(feed.refresher_running);
            boundary.refresher_interval_ms = Some(feed.refresher_interval_ms);
            boundary.artifact_path = feed
                .artifact_path
                .as_ref()
                .map(|path| path.display().to_string());
        }
        // The published file is hashed here, in health, so `Get-FileHash` on the
        // same path is a direct comparison an operator can make by hand.
        let artifact_path = feed
            .and_then(|feed| feed.artifact_path.clone())
            .or_else(|| publish.last_path.clone());
        if let Some(path) = artifact_path.as_ref() {
            boundary.artifact_path = Some(path.display().to_string());
            match std::fs::read(path) {
                Ok(bytes) => {
                    boundary.artifact_file_bytes = u64::try_from(bytes.len()).ok();
                    let mut hasher = Sha256::new();
                    hasher.update(&bytes);
                    boundary.artifact_file_sha256 = Some(hex_lower(&hasher.finalize()));
                }
                Err(error) => {
                    boundary.artifact_file_read_error = Some(format!(
                        "read published lowered artifact {}: {error}",
                        path.display()
                    ));
                }
            }
        }

        let mut reasons = Vec::new();
        if violations_total > 0 {
            reasons.push(format!(
                "{} live-Calyx-from-hot-path violations ({violations_total} total, last={})",
                synapse_reflex::HOT_PATH_BOUNDARY_VIOLATION_CODE,
                last_violation
                    .as_ref()
                    .map_or("unknown", |violation| violation.operation.as_str())
            ));
        }
        if publish.failure_total > 0 {
            reasons.push(format!(
                "guard-threshold lowering publisher has {} failures (last failure {} at unix_ms={}: {})",
                publish.failure_total,
                publish
                    .last_failure_code
                    .as_deref()
                    .unwrap_or("<not recorded: failure predates #1889 retention>"),
                publish
                    .last_failure_unix_ms
                    .map_or_else(|| "<not recorded>".to_owned(), |ms| ms.to_string()),
                publish
                    .last_failure_detail
                    .as_deref()
                    .unwrap_or("<not recorded: failure predates #1889 retention>")
            ));
        }
        // A tick on the fail-closed defaults is only a *defect* once a publish
        // has actually succeeded: that combination means the producer wrote an
        // artifact the consumer refuses, which is real drift. Before the first
        // successful publish the tick is legitimately on the documented safe
        // defaults, and calling that an error would cry wolf on every fresh
        // start for a whole maintenance cadence.
        let pending_first_publish =
            feed.is_some_and(|feed| feed.state != "fresh") && publish.success_total == 0;
        if let Some(feed) = feed
            && feed.state != "fresh"
            && publish.success_total > 0
        {
            reasons.push(format!(
                "a lowered guard-threshold artifact has been published but the reflex tick \
                 refuses it and runs on the fail-closed defaults ({}: {})",
                feed.safe_default_code.as_deref().unwrap_or("unknown"),
                feed.safe_default_detail.as_deref().unwrap_or("unknown")
            ));
        }
        // `no_tick_thread` is deliberately not `initializing`: nothing resolves
        // it on its own. There is no tick until a reflex is registered, and an
        // operator reading "initializing" would wait for a state that never
        // arrives.
        let status = match () {
            () if !reasons.is_empty() => "error",
            () if matches!(readback, LoweredFeedReadback::SchedulerNotStarted) => "no_tick_thread",
            () if feed.is_none() => "initializing",
            () if pending_first_publish => "pending_lowering",
            () => "ok",
        };
        let detail = if reasons.is_empty() {
            feed.map_or_else(
                || {
                    // `feed_unavailability` guarantees a reason whenever `feed`
                    // is None, which is the only branch that reaches here.
                    feed_unavailable_reason.unwrap_or_else(|| {
                        "the lowered-artifact feed is unavailable for an unrecorded reason"
                            .to_owned()
                    })
                },
                |feed| {
                    format!(
                        "violations_total={violations_total} artifact_state={} \
                         content_sha256={} hot_reads_total={} publish_success_total={} \
                         publish_last_error_code={}",
                        feed.state,
                        feed.content_sha256.as_deref().unwrap_or("none"),
                        feed.hot_reads_total,
                        publish.success_total,
                        publish.last_error_code.as_deref().unwrap_or("none")
                    )
                },
            )
        } else {
            reasons.join("; ")
        };
        SubsystemHealth {
            status: status.to_owned(),
            detail: Some(detail),
            calyx_hot_path: Some(boundary),
            ..SubsystemHealth::default()
        }
    }

    /// Surfaces durable shell-job recovery obligations that startup could not
    /// discharge (#1858).
    ///
    /// Startup no longer refuses to run on an unprovable per-record
    /// disposition, because refusing bought no safety and permanently bricked
    /// the daemon. This subsystem is what keeps that from becoming silent: an
    /// outstanding obligation reports `status = "error"`, which drives the
    /// whole health payload's `ok` to false until it is discharged.
    fn shell_job_recovery_health() -> SubsystemHealth {
        match crate::m4::read_shell_job_recovery_obligations() {
            Ok(None) => SubsystemHealth {
                status: "ok".to_owned(),
                detail: Some("no outstanding durable shell-job recovery obligations".to_owned()),
                ..SubsystemHealth::default()
            },
            Ok(Some(ledger)) => {
                let outstanding = ledger.get("outstanding").cloned().unwrap_or(Value::Null);
                SubsystemHealth {
                    status: "error".to_owned(),
                    detail: Some(format!(
                        "durable shell-job recovery obligations are outstanding and retried on every daemon start: outstanding={outstanding} first_observed_at={} last_observed_at={}",
                        ledger
                            .get("first_observed_at")
                            .and_then(Value::as_str)
                            .unwrap_or("<unknown>"),
                        ledger
                            .get("last_observed_at")
                            .and_then(Value::as_str)
                            .unwrap_or("<unknown>"),
                    )),
                    ..SubsystemHealth::default()
                }
            }
            Err(error) => SubsystemHealth {
                status: "error".to_owned(),
                detail: Some(format!(
                    "durable shell-job recovery obligations ledger could not be read: {}",
                    error.message
                )),
                ..SubsystemHealth::default()
            },
        }
    }

    fn public_tool_registry_health(&self) -> SubsystemHealth {
        match self.public_tool_registry_snapshot() {
            Ok(snapshot) => {
                let missing_count = snapshot.registered_tools_missing.len();
                let status = if missing_count == 0 {
                    "ok"
                } else {
                    "pending_facades"
                };
                SubsystemHealth {
                    status: status.to_owned(),
                    detail: Some(format!(
                        "source_of_truth={} public_tool_count={} max_public_tool_count={} implementation_tool_count={} registered_tools_present={} registered_tools_missing={}",
                        snapshot.source_of_truth,
                        snapshot.public_tool_count,
                        snapshot.max_public_tool_count,
                        snapshot.implementation_tool_count,
                        snapshot.registered_tools_present.len(),
                        missing_count
                    )),
                    ..SubsystemHealth::default()
                }
            }
            Err(error) => SubsystemHealth {
                status: "error".to_owned(),
                detail: Some(format!("{error:?}")),
                ..SubsystemHealth::default()
            },
        }
    }

    /// #1886: the facade contract is a hand-maintained list. Reporting a hash of
    /// it as a passing check is exactly the defect this subsystem used to have,
    /// so `ok` now requires the live-schema parity gate to have compared every
    /// facade against its served `operation` enum and agreed in both
    /// directions. Anything less reports `error` and names the divergence.
    fn facade_contract_health(&self) -> SubsystemHealth {
        match self.facade_contract_snapshot() {
            Ok(snapshot) => {
                let invalid_count = snapshot.missing_contract_tool_names.len()
                    + snapshot.unknown_contract_tool_names.len()
                    + snapshot.duplicate_contract_tool_names.len()
                    + snapshot.duplicate_operation_names.len()
                    + snapshot.invalid_contract_reasons.len();
                let parity = &snapshot.schema_parity;
                let status = if invalid_count == 0 && parity.verified {
                    "ok"
                } else {
                    "error"
                };
                SubsystemHealth {
                    status: status.to_owned(),
                    detail: Some(format!(
                        "source_of_truth={} public_tool_count={} contract_tool_count={} operation_count={} mutating_operation_count={} invalid_count={} contract_sha256={} schema_parity: {}",
                        snapshot.source_of_truth,
                        snapshot.public_tool_count,
                        snapshot.contract_tool_count,
                        snapshot.operation_count,
                        snapshot.mutating_operation_count,
                        invalid_count,
                        snapshot.facade_contract_sha256,
                        parity.health_detail()
                    )),
                    ..SubsystemHealth::default()
                }
            }
            Err(error) => SubsystemHealth {
                status: "error".to_owned(),
                detail: Some(format!("{error:?}")),
                ..SubsystemHealth::default()
            },
        }
    }

    /// Reports how the OS is scheduling this daemon, and whether the daemon's
    /// own QoS assertion landed (#1910).
    ///
    /// `status` is `degraded`, not `error`, when the assertion failed: a daemon
    /// running at the wrong priority answers every request correctly, just more
    /// slowly at the tail. Calling that `error` would make `health.ok` false and
    /// fail deploy gates over a performance property, which overstates it. What
    /// matters is that it stops being *invisible*.
    fn process_qos_health() -> SubsystemHealth {
        let Some(report) = crate::server::process_qos_report() else {
            return SubsystemHealth {
                status: "unmeasured".to_owned(),
                detail: Some(
                    "the startup QoS assertion did not run in this process, so the daemon's priority class \
                     and power-throttling state are unknown; this is expected only for an in-process service \
                     that never went through daemon startup"
                        .to_owned(),
                ),
                ..SubsystemHealth::default()
            };
        };

        let status = if report.failure_code.is_some() {
            "degraded"
        } else {
            "ok"
        };
        // Say the thing that is actionable, not just the state. A daemon that had
        // to raise itself is working, but its launcher is still wrong at the
        // source and that is worth repairing.
        let detail = report.failure_detail.clone().or_else(|| {
            report.priority_raised.then(|| {
                format!(
                    "the daemon was launched at {} and raised itself to {}; the launch path is still handing \
                     down a background priority class, which the daemon can only correct after startup — \
                     rerun scripts/synapse-setup.ps1 to converge the scheduled task onto Priority 5 \
                     (NORMAL_PRIORITY_CLASS) so the whole chain starts correctly",
                    report.priority_class_before, report.priority_class_after
                )
            })
        });

        SubsystemHealth {
            status: status.to_owned(),
            detail,
            process_qos: Some(synapse_core::ProcessQosHealth {
                priority_class_before: Some(report.priority_class_before.to_owned()),
                priority_class_after: Some(report.priority_class_after.to_owned()),
                priority_raised: Some(report.priority_raised),
                power_throttling_control_mask: Some(report.power_throttling_control_mask),
                power_throttling_state_mask: Some(report.power_throttling_state_mask),
                execution_speed_throttling_disabled: Some(
                    report.execution_speed_throttling_disabled,
                ),
                failure_code: report.failure_code.map(str::to_owned),
            }),
            ..SubsystemHealth::default()
        }
    }

    fn daemon_drain_health(&self) -> SubsystemHealth {
        let snapshot = self.drain_state_handle().snapshot();
        let status = if snapshot.state_error.is_some() {
            "error"
        } else if snapshot.draining {
            "draining"
        } else {
            "ok"
        };
        let detail = if let Some(error) = snapshot.state_error {
            error
        } else if snapshot.draining {
            format!(
                "reason_code={} source={} started_at_unix_ms={}",
                snapshot.reason_code.unwrap_or("unknown"),
                snapshot.source.unwrap_or("unknown"),
                snapshot.started_at_unix_ms.unwrap_or_default()
            )
        } else {
            "daemon accepting work".to_owned()
        };
        SubsystemHealth {
            status: status.to_owned(),
            detail: Some(detail),
            ..SubsystemHealth::default()
        }
    }

    /// State of the dedicated grounded-usage writer (#1936).
    ///
    /// The queue is the daemon's evidence backlog: every tool call's outcome
    /// passes through it on the way into the corpus. Reporting it here means
    /// the backlog can be read on demand rather than only when a threshold log
    /// fires — and a `failed` count above zero is a corpus that is missing
    /// observations, which is a grounding fault and is reported as `error`
    /// rather than folded into a healthy verdict.
    fn usage_writer_health(&self) -> SubsystemHealth {
        let writer = self.usage_writer();
        let depth = writer.depth();
        let committed = writer.committed();
        let failed = writer.failed();
        let status = if failed > 0 {
            "error"
        } else if writer.is_backlogged() {
            "backlog"
        } else {
            "ok"
        };
        SubsystemHealth {
            status: status.to_owned(),
            detail: Some(format!(
                "queue_depth={depth} committed={committed} failed={failed} source_of_truth={}",
                super::mcp_usage::MCP_USAGE_SOURCE_OF_TRUTH
            )),
            // Also typed, because compact health nulls `detail` and the
            // backlog must stay readable in the mode callers actually use.
            usage_writer: Some(synapse_core::UsageWriterHealth {
                queue_depth: depth,
                committed,
                failed,
            }),
            ..SubsystemHealth::default()
        }
    }

    fn tool_surface_fingerprint(&self, session_id: Option<&str>) -> ToolSurfaceFingerprint {
        if session_id.is_none() {
            return self.immutable_tool_surface_fingerprint();
        }
        let tools = match self.health_tool_surface(session_id) {
            Ok(tools) => tools,
            Err(error) => {
                tracing::error!(
                    code = "MCP_TOOL_SURFACE_HEALTH_READ_FAILED",
                    session_id,
                    error = ?error,
                    "failed to resolve MCP health tool surface"
                );
                return ToolSurfaceFingerprint {
                    names: Vec::new(),
                    sha256: "TOOL_SURFACE_HEALTH_READ_FAILED".to_owned(),
                    error: Some(format!(
                        "failed to resolve MCP health tool surface: {error}"
                    )),
                };
            }
        };
        tool_surface_fingerprint_for_tools(tools)
    }

    /// Report the *exact* tool surface the client is served.
    ///
    /// Health's `tool_names`/`tool_count`/`tool_surface_sha256` must mirror what
    /// `tools/list` actually returns for the same session, or health lies about
    /// the surface. The served surface (see `ServerHandler::list_tools`) is
    /// `tools_for_session_profile(session_id)` for every session — including the
    /// unscoped stdio/admin case where `session_id` is `None` and the full
    /// break-glass surface (raw `act_*` primitives such as `act_run_shell_status`
    /// included) is served. Deriving health from that single source of truth
    /// makes the two surfaces identical by construction, closing the drift class
    /// where a hand-maintained parallel list (previously a `public_tool_names`
    /// filter for the `None` case) silently diverged from what was served
    /// (issue #1612).
    fn health_tool_surface(&self, session_id: Option<&str>) -> Result<Vec<Tool>, ErrorData> {
        self.tools_for_session_profile(session_id)
    }

    fn storage_health(&self) -> SubsystemHealth {
        match self.m3_state.try_lock() {
            Ok(state) => {
                let db_path = state
                    .db_path
                    .as_ref()
                    .map(|path| path.display().to_string());
                let storage_backend = state
                    .db
                    .as_ref()
                    .map_or(state.storage_backend.as_str(), |db| db.backend_name())
                    .to_owned();
                let maintenance = state.storage_maintenance_readback();
                if let Some(error) = &state.storage_last_error {
                    let mut health = SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some(error.clone()),
                        db_path,
                        storage_backend: Some(storage_backend),
                        ..SubsystemHealth::default()
                    };
                    apply_storage_maintenance_fields(&mut health, &maintenance);
                    return health;
                }
                let Some(runtime) = &state.reflex_runtime else {
                    if state.db.is_some() {
                        let maintenance_error = storage_maintenance_error(&maintenance);
                        let maintenance_unsupported = maintenance.unsupported_reason.clone();
                        let maintenance_active = storage_maintenance_active(&maintenance);
                        let cf_sizes_skipped_reason =
                            (storage_backend == "calyx").then(calyx_health_cf_sizes_skipped_reason);
                        let cf_sizes = if cf_sizes_skipped_reason.is_some() {
                            None
                        } else {
                            state.db.as_ref().and_then(|db| {
                                db.cf_live_data_size_estimates()
                                    .ok()
                                    .map(|(sizes, _)| sizes)
                            })
                        };
                        let mut health = SubsystemHealth {
                            status: if maintenance_error.is_some() {
                                "error".to_owned()
                            } else if maintenance_unsupported.is_some() {
                                "maintenance_unsupported".to_owned()
                            } else if maintenance_active {
                                "maintenance".to_owned()
                            } else {
                                "ok".to_owned()
                            },
                            detail: Some(match (
                                maintenance_error,
                                maintenance_unsupported,
                                maintenance_active,
                            ) {
                                (Some(error), _, _) => format!(
                                    "storage opened at daemon startup (reflex runtime idle); maintenance unhealthy: {error}"
                                ),
                                (None, Some(reason), _) => format!(
                                    "storage opened at daemon startup (reflex runtime idle); maintenance unsupported for this backend: {reason}"
                                ),
                                (None, None, true) => "storage opened at daemon startup (reflex runtime idle); maintenance tick active; health skipped scan-bound CF size readback".to_owned(),
                                (None, None, false) => "storage opened at daemon startup (reflex runtime idle); maintenance tasks running and pressure probe observed".to_owned(),
                            }),
                            db_path,
                            storage_backend: Some(storage_backend),
                            schema_version: Some(synapse_core::SCHEMA_VERSION),
                            cf_sizes,
                            storage_cf_sizes_skipped_reason: cf_sizes_skipped_reason,
                            ..SubsystemHealth::default()
                        };
                        apply_storage_maintenance_fields(&mut health, &maintenance);
                        return health;
                    }
                    let mut health = SubsystemHealth {
                        status: "initializing".to_owned(),
                        detail: Some("storage opens on first reflex tool call".to_owned()),
                        db_path,
                        storage_backend: Some(storage_backend),
                        ..SubsystemHealth::default()
                    };
                    apply_storage_maintenance_fields(&mut health, &maintenance);
                    return health;
                };
                match runtime.try_lock() {
                    Ok(runtime) => {
                        let runtime_backend = runtime.storage_backend_name().to_owned();
                        let runtime_db_path = runtime.storage_path().display().to_string();
                        let runtime_schema_version = runtime.schema_version();
                        let maintenance_error = storage_maintenance_error(&maintenance);
                        let maintenance_unsupported = maintenance.unsupported_reason.clone();
                        let maintenance_active = storage_maintenance_active(&maintenance);
                        let cf_sizes_skipped_reason =
                            (runtime_backend == "calyx").then(calyx_health_cf_sizes_skipped_reason);
                        let cf_sizes = if cf_sizes_skipped_reason.is_some() {
                            None
                        } else {
                            match runtime.storage_cf_live_data_size_estimates() {
                                Ok((sizes, _warnings)) => Some(sizes),
                                Err(error) => {
                                    let mut health = SubsystemHealth {
                                        status: "error".to_owned(),
                                        detail: Some(error.to_string()),
                                        db_path: Some(runtime_db_path),
                                        storage_backend: Some(runtime_backend),
                                        schema_version: Some(runtime_schema_version),
                                        ..SubsystemHealth::default()
                                    };
                                    apply_storage_maintenance_fields(&mut health, &maintenance);
                                    return health;
                                }
                            }
                        };
                        let mut health = SubsystemHealth {
                            status: if maintenance_error.is_some() {
                                "error".to_owned()
                            } else if maintenance_unsupported.is_some() {
                                "maintenance_unsupported".to_owned()
                            } else if maintenance_active {
                                "maintenance".to_owned()
                            } else {
                                storage_pressure_status(runtime.storage_pressure_level())
                            },
                            detail: Some(match (
                                maintenance_error,
                                maintenance_unsupported,
                                maintenance_active,
                            ) {
                                (Some(error), _, _) => format!(
                                    "storage runtime initialized; maintenance unhealthy: {error}"
                                ),
                                (None, Some(reason), _) => format!(
                                    "storage runtime initialized; maintenance unsupported for this backend: {reason}"
                                ),
                                (None, None, true) => "storage runtime initialized; maintenance tick active; health skipped scan-bound CF size readback".to_owned(),
                                (None, None, false) => "storage runtime initialized; cf_sizes use backend metrics; maintenance tasks running and pressure probe observed".to_owned(),
                            }),
                            db_path: Some(runtime_db_path),
                            storage_backend: Some(runtime_backend),
                            schema_version: Some(runtime_schema_version),
                            cf_sizes,
                            storage_cf_sizes_skipped_reason: cf_sizes_skipped_reason,
                            ..SubsystemHealth::default()
                        };
                        apply_storage_maintenance_fields(&mut health, &maintenance);
                        health
                    }
                    Err(error) => match error {
                        TryLockError::WouldBlock => {
                            let maintenance_error = storage_maintenance_error(&maintenance);
                            let maintenance_unsupported = maintenance.unsupported_reason.clone();
                            let maintenance_active = storage_maintenance_active(&maintenance);
                            let cf_sizes_skipped_reason = (storage_backend == "calyx")
                                .then(calyx_health_cf_sizes_skipped_reason);
                            let mut health = SubsystemHealth {
                                    status: if maintenance_error.is_some() {
                                        "error".to_owned()
                                    } else if maintenance_unsupported.is_some() {
                                        "maintenance_unsupported".to_owned()
                                    } else if maintenance_active {
                                        "maintenance".to_owned()
                                    } else {
                                        maintenance
                                            .pressure_probe
                                            .last_level
                                            .map(storage_pressure_status)
                                            .unwrap_or_else(|| "ok".to_owned())
                                    },
                                    detail: Some(match (
                                        maintenance_error,
                                        maintenance_unsupported,
                                        maintenance_active,
                                    ) {
                                        (Some(error), _, _) => format!(
                                            "storage runtime lock busy; daemon storage handle readback says maintenance unhealthy: {error}"
                                        ),
                                        (None, Some(reason), _) => format!(
                                            "storage runtime lock busy; daemon storage handle readback says maintenance unsupported for this backend: {reason}"
                                        ),
                                        (None, None, true) => "storage runtime lock busy; daemon storage handle is open and maintenance tick active; health skipped scan-bound CF size readback".to_owned(),
                                        (None, None, false) => "storage runtime lock busy; daemon storage handle is open, maintenance tasks are running, and pressure probe was observed".to_owned(),
                                    }),
                                    db_path,
                                    storage_backend: Some(storage_backend),
                                    schema_version: Some(synapse_core::SCHEMA_VERSION),
                                    cf_sizes: None,
                                    storage_cf_sizes_skipped_reason: cf_sizes_skipped_reason,
                                    ..SubsystemHealth::default()
                                };
                            apply_storage_maintenance_fields(&mut health, &maintenance);
                            health
                        }
                        TryLockError::Poisoned(poisoned) => {
                            let mut health = runtime_lock_unavailable_health(
                                "reflex storage",
                                TryLockError::Poisoned(poisoned),
                            );
                            health.db_path = db_path;
                            health.storage_backend = Some(storage_backend);
                            apply_storage_maintenance_fields(&mut health, &maintenance);
                            health
                        }
                    },
                }
            }
            Err(error) => state_lock_unavailable_health("M3", error),
        }
    }

    fn reflex_health(&self) -> SubsystemHealth {
        match self.m3_state.try_lock() {
            Ok(state) => {
                if let Some(error) = &state.reflex_last_error {
                    return SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some(error.clone()),
                        ..SubsystemHealth::default()
                    };
                }
                if state.reflex_disabled {
                    return SubsystemHealth {
                        status: "disabled".to_owned(),
                        detail: Some("reflex runtime disabled by operator".to_owned()),
                        active_count: Some(0),
                        ..SubsystemHealth::default()
                    };
                }
                let Some(runtime) = &state.reflex_runtime else {
                    return SubsystemHealth {
                        status: "initializing".to_owned(),
                        detail: Some("reflex runtime starts on first reflex tool call".to_owned()),
                        active_count: Some(0),
                        recursion_clamps_total: Some(0),
                        audit_timestamp_invalid_total: Some(0),
                        ..SubsystemHealth::default()
                    };
                };
                match runtime.try_lock() {
                    Ok(runtime) => {
                        let audit_timestamp_invalid_total = runtime.audit_timestamp_invalid_total();
                        let audit_queue = runtime.audit_queue_snapshot().unwrap_or_default();
                        match runtime.recursion_clamps_total() {
                            Ok(recursion_clamps_total) => SubsystemHealth {
                                status: if audit_timestamp_invalid_total > 0
                                    || audit_queue.terminal_failed > 0
                                {
                                    "error".to_owned()
                                } else if audit_queue.terminal_pending > 0 {
                                    "pending_terminal_commit".to_owned()
                                } else if runtime.degraded_latency() {
                                    "degraded_latency".to_owned()
                                } else {
                                    "ok".to_owned()
                                },
                                detail: Some(if audit_timestamp_invalid_total > 0 {
                                    format!(
                                        "{audit_timestamp_invalid_total} reflex audit timestamp(s) were rejected; inspect REFLEX_AUDIT_TIMESTAMP_INVALID logs and repair the system clock/timestamp source before restarting"
                                    )
                                } else if audit_queue.terminal_failed > 0 {
                                    format!(
                                        "{} reflex terminal lifecycle transition(s) failed closed; last_reflex_id={} last_intent_id={} phase={} detail={}; remediation=keep the reflex non-dispatchable, repair the named queue/storage/readback failure, and restart so durable terminal-intent recovery can finish",
                                        audit_queue.terminal_failed,
                                        audit_queue
                                            .terminal_failure_reflex_id
                                            .as_deref()
                                            .unwrap_or("unknown"),
                                        audit_queue
                                            .terminal_failure_intent_id
                                            .as_deref()
                                            .unwrap_or("unknown"),
                                        audit_queue
                                            .terminal_failure_phase
                                            .as_deref()
                                            .unwrap_or("unknown"),
                                        audit_queue
                                            .terminal_failure_detail
                                            .as_deref()
                                            .unwrap_or("unknown"),
                                    )
                                } else if audit_queue.terminal_pending > 0 {
                                    format!(
                                        "{} reflex terminal lifecycle transition(s) are non-dispatchable and awaiting durable prepare/completion acknowledgement",
                                        audit_queue.terminal_pending
                                    )
                                } else {
                                    "reflex runtime initialized".to_owned()
                                }),
                                active_count: Some(runtime.active_count()),
                                sample_count: Some(runtime.sample_count()),
                                sample_limit: Some(runtime.sample_limit()),
                                last_tick_jitter_us: runtime.last_tick_jitter_us(),
                                p99_tick_jitter_us: runtime.p99_tick_jitter_us(),
                                late_tick_count: Some(runtime.late_tick_count()),
                                deadline_miss_streak: runtime.deadline_miss_streak(),
                                deadline_miss_audit_after: Some(
                                    runtime.deadline_miss_audit_after(),
                                ),
                                severe_deadline_miss_after_us: Some(
                                    runtime.severe_deadline_miss_after_us(),
                                ),
                                degraded_tick_count: Some(runtime.degraded_tick_count()),
                                recursion_clamps_total: Some(recursion_clamps_total),
                                audit_timestamp_invalid_total: Some(audit_timestamp_invalid_total),
                                terminal_lifecycle_pending: Some(audit_queue.terminal_pending),
                                terminal_lifecycle_prepared_total: Some(
                                    audit_queue.terminal_prepared,
                                ),
                                terminal_lifecycle_committed_total: Some(
                                    audit_queue.terminal_committed,
                                ),
                                terminal_lifecycle_failed_total: Some(audit_queue.terminal_failed),
                                terminal_lifecycle_failure_reflex_id: audit_queue
                                    .terminal_failure_reflex_id,
                                terminal_lifecycle_failure_intent_id: audit_queue
                                    .terminal_failure_intent_id,
                                terminal_lifecycle_failure_phase: audit_queue
                                    .terminal_failure_phase,
                                terminal_lifecycle_failure_detail: audit_queue
                                    .terminal_failure_detail,
                                ..SubsystemHealth::default()
                            },
                            Err(error) => SubsystemHealth {
                                status: "error".to_owned(),
                                detail: Some(error.to_string()),
                                active_count: Some(runtime.active_count()),
                                sample_count: Some(runtime.sample_count()),
                                sample_limit: Some(runtime.sample_limit()),
                                last_tick_jitter_us: runtime.last_tick_jitter_us(),
                                p99_tick_jitter_us: runtime.p99_tick_jitter_us(),
                                late_tick_count: Some(runtime.late_tick_count()),
                                deadline_miss_streak: runtime.deadline_miss_streak(),
                                deadline_miss_audit_after: Some(
                                    runtime.deadline_miss_audit_after(),
                                ),
                                severe_deadline_miss_after_us: Some(
                                    runtime.severe_deadline_miss_after_us(),
                                ),
                                degraded_tick_count: Some(runtime.degraded_tick_count()),
                                audit_timestamp_invalid_total: Some(audit_timestamp_invalid_total),
                                terminal_lifecycle_pending: Some(audit_queue.terminal_pending),
                                terminal_lifecycle_prepared_total: Some(
                                    audit_queue.terminal_prepared,
                                ),
                                terminal_lifecycle_committed_total: Some(
                                    audit_queue.terminal_committed,
                                ),
                                terminal_lifecycle_failed_total: Some(audit_queue.terminal_failed),
                                terminal_lifecycle_failure_reflex_id: audit_queue
                                    .terminal_failure_reflex_id,
                                terminal_lifecycle_failure_intent_id: audit_queue
                                    .terminal_failure_intent_id,
                                terminal_lifecycle_failure_phase: audit_queue
                                    .terminal_failure_phase,
                                terminal_lifecycle_failure_detail: audit_queue
                                    .terminal_failure_detail,
                                ..SubsystemHealth::default()
                            },
                        }
                    }
                    Err(error) => runtime_lock_unavailable_health("reflex", error),
                }
            }
            Err(error) => state_lock_unavailable_health("M3", error),
        }
    }

    fn profile_health(&self) -> SubsystemHealth {
        match self.m3_state.try_lock() {
            Ok(state) => {
                if let Some(error) = &state.profile_last_error {
                    return SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some(error.clone()),
                        ..SubsystemHealth::default()
                    };
                }
                state.profile_runtime.as_ref().map_or_else(
                    || SubsystemHealth {
                        status: "initializing".to_owned(),
                        detail: Some(
                            "profile runtime initializes on first profile tool call".to_owned(),
                        ),
                        ..SubsystemHealth::default()
                    },
                    |runtime| {
                        let active_profile_id = runtime.active_profile_id();
                        let profiles = runtime.list(true);
                        let last_reload_at = runtime.last_reload_at();
                        match (active_profile_id, profiles, last_reload_at) {
                            (Ok(active_profile_id), Ok(profiles), Ok(last_reload_at)) => {
                                SubsystemHealth {
                                    status: "ok".to_owned(),
                                    detail: Some(format!(
                                        "profile_dir={}",
                                        runtime.profile_dir().display()
                                    )),
                                    active_profile_id,
                                    profile_count: Some(profiles.len()),
                                    last_reload_at,
                                    ..SubsystemHealth::default()
                                }
                            }
                            (active_profile_id, profiles, last_reload_at) => {
                                let detail = active_profile_id
                                    .err()
                                    .map(|error| error.to_string())
                                    .or_else(|| profiles.err().map(|error| error.to_string()))
                                    .or_else(|| last_reload_at.err().map(|error| error.to_string()))
                                    .unwrap_or_else(|| "profile runtime error".to_owned());
                                SubsystemHealth {
                                    status: "error".to_owned(),
                                    detail: Some(detail),
                                    ..SubsystemHealth::default()
                                }
                            }
                        }
                    },
                )
            }
            Err(error) => state_lock_unavailable_health("M3", error),
        }
    }

    fn perception_health(&self) -> SubsystemHealth {
        match self.m1_state.try_lock() {
            Ok(state) => {
                // #2054: `status` is the readiness of the M1 perception runtime
                // (a11y + capture), which is exactly what the deploy gate reads.
                // Whether a detector runs is a capability, not a readiness, and
                // it gets its own field: reporting the bundled model's presence
                // under `status="ok"` let readers claim neural perception ran
                // when the active profile requested none.
                let bundle = crate::m1::detection_bundle_readback();
                let mut detection = perception_detection_health(&state);
                let bundled_blob = match &bundle {
                    Ok(bundle) => {
                        detection.bundled_model_id = Some(bundle.model_id.to_owned());
                        detection.bundled_provider = Some(bundle.provider.to_owned());
                        detection.bundled_materialized = Some(bundle.materialized);
                        detection.bundled_materialized_verified =
                            Some(bundle.materialized_verified);
                        detection.bundled_materialized_path =
                            Some(bundle.materialized_path.clone());
                        format!(
                            "bundled_detection_provider={} bundled_detection_model={} model_source=executable_bundle bundled_materialized={} bundled_materialized_verified={} bundled_materialized_path={}",
                            bundle.provider,
                            bundle.model_id,
                            bundle.materialized,
                            bundle.materialized_verified,
                            bundle.materialized_path
                        )
                    }
                    Err((code, detail)) => {
                        format!("bundled_detection_readback_failed {code}: {detail}")
                    }
                };
                let detection_detail = format!(
                    "detection_inference_configured={} detection_capability={} detection_model={} detection_max_detections={} detection_config_source={} detection_config_applied_unix_ms={}{}",
                    detection.inference_configured,
                    detection.status,
                    detection.configured_model_id.as_deref().unwrap_or("<none>"),
                    detection.max_detections,
                    detection.config_source,
                    detection.config_applied_unix_ms,
                    detection
                        .remediation
                        .as_deref()
                        .map_or_else(String::new, |remediation| format!(
                            " detection_remediation=\"{remediation}\""
                        ))
                );
                // #2069: `misconfigured` degrades the ROLLUP, asymmetrically
                // with `not_configured`. A profile that requests no detector is
                // a healthy runtime (capability absent by choice); a profile
                // that names an unregistered detector guarantees every
                // pixel-bearing observe hard-errors, and a deploy gate reading
                // only the rollup must see that.
                //
                // #2086, decided: `not_configured` rolls up `ok` EVEN WHEN
                // `mode_admits_detection=true`. "Admits" means the mode allows
                // detection-bearing observes, not that the deployment requires
                // them — requiring a detector is a configuration statement, and
                // making its absence red would hold `health.ok` false forever
                // on every host whose operator chose not to wire one. The
                // contract that keeps this honest: detection-bearing observes
                // refuse loudly with `DETECTION_MODEL_NOT_LOADED` (#2074,
                // live-verified), the nested `perception_detection` field stays
                // truthful, and the arm below names the absence in the rollup
                // detail instead of folding it into a generic "initialized".
                let (status, prefix) = match (&bundle, detection.status.as_str()) {
                    (Err(_), _) => ("error", "perception detector backend probe failed"),
                    (Ok(_), "misconfigured") => (
                        "misconfigured",
                        "perception runtime initialized, but the configured detector cannot load and every detection-bearing observe will fail",
                    ),
                    (Ok(_), "not_configured") => (
                        "ok",
                        "perception runtime initialized; no detector configured (capability absent by operator choice — detection-bearing observes refuse with DETECTION_MODEL_NOT_LOADED)",
                    ),
                    (Ok(_), _) => ("ok", "perception runtime initialized"),
                };
                SubsystemHealth {
                    status: status.to_owned(),
                    detail: Some(format!("{prefix}; {detection_detail}; {bundled_blob}")),
                    perception_mode: Some(state.perception_mode),
                    capture_config: Some(state.active_capture_config.clone()),
                    capture_runtime: Some(state.capture_runtime_readback()),
                    perception_detection: Some(detection),
                    ..SubsystemHealth::default()
                }
            }
            Err(error) => state_lock_unavailable_health("M1", error),
        }
    }

    fn action_health(&self) -> SubsystemHealth {
        match self.m2_state.try_lock() {
            Ok(state) => match state.backend_resolution_readback() {
                Ok((source, policy)) => {
                    let emitter_available = state.emitter_available();
                    let operator_hotkey = synapse_action::operator_hotkey_status().label();
                    let allow_shell = if self.m4_config.allow_shell_any() {
                        "any".to_owned()
                    } else {
                        self.m4_config.allow_shell_count().to_string()
                    };
                    let allow_launch = if self.m4_config.allow_launch_any() {
                        "any".to_owned()
                    } else {
                        self.m4_config.allow_launch_count().to_string()
                    };
                    let lease = synapse_action::lease::status();
                    let lease_detail = lease.owner_session_id.as_deref().map_or_else(
                        || "input_lease_held=false".to_owned(),
                        |owner| {
                            format!(
                                "input_lease_held=true input_lease_owner={owner} input_lease_expires_in_ms={}",
                                lease.expires_in_ms.unwrap_or(0)
                            )
                        },
                    );
                    let search_tools = self.m4_config.shell_search_tool_readback();
                    // #2082 finding E: the synthetic-hold watchdog's bound
                    // selection (30 s vs 300 s track) and tracked-hold state are
                    // otherwise unobservable without tripping the watchdog.
                    let watchdog =
                        synapse_action::synthetic_input::synthetic_hold_watchdog_status().label();
                    SubsystemHealth {
                        status: if emitter_available { "ok" } else { "error" }.to_owned(),
                        detail: Some(format!(
                            "emitter_available={} recording_enabled={} operator_hotkey={} allow_shell_patterns={} allow_launch_patterns={} {} {} {}",
                            emitter_available,
                            state.recording_enabled(),
                            operator_hotkey,
                            allow_shell,
                            allow_launch,
                            lease_detail,
                            search_tools,
                            watchdog
                        )),
                        backend_resolution: Some(backend_resolution_health(source, policy)),
                        run_shell_inline_await_limit_ms: Some(
                            self.m4_config.run_shell_inline_await_limit_ms(),
                        ),
                        run_shell_inline_client_call_budget_ms: Some(
                            self.m4_config.run_shell_inline_client_call_budget_ms(),
                        ),
                        run_shell_durable_default_timeout_ms: Some(
                            self.m4_config.run_shell_durable_default_timeout_ms(),
                        ),
                        run_shell_durable_max_timeout_ms: Some(
                            self.m4_config.run_shell_durable_max_timeout_ms(),
                        ),
                        ..SubsystemHealth::default()
                    }
                }
                Err(error) => SubsystemHealth {
                    status: "error".to_owned(),
                    detail: Some(error),
                    ..SubsystemHealth::default()
                },
            },
            Err(error) => state_lock_unavailable_health("M2", error),
        }
    }

    fn audio_health(&self) -> SubsystemHealth {
        match self.m3_state.try_lock() {
            Ok(state) => {
                // Physical fact about this binary, independent of whether audio
                // is switched on: is the optional STT model actually packaged?
                // (#1863)
                let stt_availability = stt_model_availability();
                if let Some(error) = &state.audio_last_error {
                    return SubsystemHealth {
                        status: "error".to_owned(),
                        detail: Some(error.clone()),
                        stt_model_available: Some(stt_availability.is_none()),
                        stt_model_unavailable_reason: stt_availability,
                        ..SubsystemHealth::default()
                    };
                }
                if !state.enable_audio {
                    return SubsystemHealth {
                        status: "disabled".to_owned(),
                        detail: Some("audio is disabled; start with --enable-audio".to_owned()),
                        ring_buffer_seconds: Some(synapse_audio::DEFAULT_RING_SECONDS),
                        stt_model_loaded: Some(false),
                        stt_model_available: Some(stt_availability.is_none()),
                        stt_model_unavailable_reason: stt_availability,
                        ..SubsystemHealth::default()
                    };
                }
                // Audio is enabled but this build cannot transcribe. Reporting
                // "ok" here would be a silent capability lie, so it is called
                // out as degraded with the exact acquisition remediation.
                if let Some(reason) = stt_availability {
                    return SubsystemHealth {
                        status: "degraded".to_owned(),
                        detail: Some(format!(
                            "audio is enabled but speech-to-text is unavailable in this build: {reason}"
                        )),
                        ring_buffer_seconds: Some(synapse_audio::DEFAULT_RING_SECONDS),
                        stt_model_loaded: Some(false),
                        stt_model_available: Some(false),
                        stt_model_unavailable_reason: Some(reason),
                        ..SubsystemHealth::default()
                    };
                }
                let Some(runtime) = &state.audio_runtime else {
                    return SubsystemHealth {
                        status: "initializing".to_owned(),
                        detail: Some(
                            "audio runtime initializes on buffered audio or transcription requests"
                                .to_owned(),
                        ),
                        ring_buffer_seconds: Some(synapse_audio::DEFAULT_RING_SECONDS),
                        stt_model_loaded: Some(false),
                        stt_model_available: Some(true),
                        ..SubsystemHealth::default()
                    };
                };
                let loopback_status = runtime.loopback_status();
                let stt_readback = match runtime.stt_backend_readback() {
                    Ok(readback) => readback,
                    Err(error) => {
                        return SubsystemHealth {
                            status: "error".to_owned(),
                            detail: Some(format!("STT backend readback failed: {error}")),
                            stt_model_available: Some(true),
                            ..SubsystemHealth::default()
                        };
                    }
                };
                let status = if loopback_status.last_error_code.is_some() {
                    "error"
                } else {
                    "ok"
                };
                SubsystemHealth {
                    status: status.to_owned(),
                    detail: Some(loopback_status.last_error_code.map_or_else(
                        || {
                            if loopback_status.running {
                                "audio loopback running".to_owned()
                            } else {
                                "audio runtime initialized; loopback disabled".to_owned()
                            }
                        },
                        |code| format!("audio loopback error: {code}"),
                    )),
                    ring_buffer_seconds: Some(runtime.config().ring_seconds),
                    stt_model_loaded: Some(runtime.stt_model_loaded()),
                    stt_backend_policy: Some(stt_readback.policy),
                    stt_selected_backend: stt_readback
                        .selected_backend
                        .map(|backend| format!("{backend:?}")),
                    stt_device_memory_policy: stt_readback.device_memory_policy,
                    stt_gpu_reservation_id: stt_readback.gpu_reservation_id,
                    stt_gpu_reservation_mib: stt_readback.gpu_reservation_mib,
                    stt_fallback_code: stt_readback.fallback_code,
                    stt_fallback_detail: stt_readback.fallback_detail,
                    stt_model_available: Some(true),
                    ..SubsystemHealth::default()
                }
            }
            Err(error) => state_lock_unavailable_health("M3", error),
        }
    }

    fn http_health(
        &self,
        active_sessions: Option<usize>,
        active_sessions_error: Option<String>,
    ) -> SubsystemHealth {
        match self.m3_state.try_lock() {
            Ok(state) => {
                let session_detail = active_sessions_error
                    .as_deref()
                    .map_or_else(String::new, |error| {
                        format!(" active_session_readback_error={error}")
                    });
                if state.shutdown_reason == "http" {
                    let diagnostics = crate::http::http_transport_diagnostics_detail();
                    SubsystemHealth {
                        status: if active_sessions_error.is_some() {
                            "error".to_owned()
                        } else {
                            "ok".to_owned()
                        },
                        detail: Some(format!(
                            "HTTP transport initialized; {diagnostics}{session_detail}"
                        )),
                        bind_addr: Some(state.bind.clone()),
                        active_sessions,
                        sse_subscribers: Some(state.sse_state.active_subscription_count()),
                        ..SubsystemHealth::default()
                    }
                } else {
                    SubsystemHealth {
                        status: if active_sessions_error.is_some() {
                            "error".to_owned()
                        } else {
                            "disabled".to_owned()
                        },
                        detail: Some(format!(
                            "HTTP transport disabled in stdio mode{session_detail}"
                        )),
                        bind_addr: Some(state.bind.clone()),
                        active_sessions: Some(0),
                        sse_subscribers: Some(state.sse_state.active_subscription_count()),
                        ..SubsystemHealth::default()
                    }
                }
            }
            Err(error) => state_lock_unavailable_health("M3", error),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ToolSurfaceFingerprint {
    pub(crate) names: Vec<String>,
    pub(crate) sha256: String,
    pub(crate) error: Option<String>,
}

pub(crate) fn tool_surface_fingerprint_for_tools(mut tools: Vec<Tool>) -> ToolSurfaceFingerprint {
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let names = tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect::<Vec<_>>();
    let canonical = serde_json::json!({
        "mcp_surface": "tools/list",
        "tools": tools,
    });
    let bytes = match canonical_json_bytes(canonical) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(
                code = "MCP_TOOL_SURFACE_FINGERPRINT_SERIALIZE_FAILED",
                %error,
                "sanitized MCP tool surface failed to serialize for health fingerprinting"
            );
            return ToolSurfaceFingerprint {
                names,
                sha256: "TOOL_SURFACE_FINGERPRINT_ERROR".to_owned(),
                error: Some(format!(
                    "sanitized MCP tool surface failed to serialize for health fingerprinting: {error}"
                )),
            };
        }
    };
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    ToolSurfaceFingerprint {
        names,
        sha256: hex_lower(&hasher.finalize()),
        error: None,
    }
}

/// Apply the requested detail verbosity to the assembled subsystem map.
///
/// `Compact` (the default) drops every verbose per-subsystem `detail` string
/// but leaves the structured verdict fields (status, counts, schema versions,
/// timeouts, ...) untouched, so the health conclusion is unchanged — only the
/// human-readable prose is trimmed. `Full` preserves the `detail` strings.
///
/// The `chrome_bridge` subsystem's only structured information historically
/// lived inside its concatenated `detail` blob, so in both modes we parse that
/// blob into the typed `ChromeBridgeDetail`. `Full` keeps the original blob and
/// the fully-parsed struct; `Compact` keeps only the verdict-critical fields
/// and drops the blob.
fn apply_health_detail(subsystems: &mut BTreeMap<String, SubsystemHealth>, detail: HealthDetail) {
    for (name, subsystem) in subsystems.iter_mut() {
        if name == "chrome_bridge" {
            let parsed = subsystem.detail.as_deref().map(parse_chrome_bridge_detail);
            match detail {
                HealthDetail::Full => {
                    subsystem.chrome_bridge = parsed;
                }
                HealthDetail::Compact => {
                    subsystem.chrome_bridge = parsed.map(compact_chrome_bridge_detail);
                    subsystem.detail = None;
                }
            }
        } else if detail == HealthDetail::Compact {
            subsystem.detail = None;
        }
    }
}

/// Parse the `chrome_bridge` `detail` blob (`key=value` tokens joined by
/// spaces, with trailing free-text guidance) into the typed
/// `ChromeBridgeDetail`.
///
/// Only whitespace-free `key=value` tokens are consumed, so the trailing
/// human-readable guidance strings (which contain spaces) are ignored rather
/// than corrupting fields. Full-mode responses retain the original `detail`
/// string, so no information is lost even for fields this parser does not
/// surface.
fn parse_chrome_bridge_detail(detail: &str) -> ChromeBridgeDetail {
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    for token in detail.split_whitespace() {
        if let Some((key, value)) = token.split_once('=') {
            if !key.is_empty()
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            {
                // First occurrence wins; blob keys are unique.
                fields.entry(key).or_insert(value);
            }
        }
    }
    let bool_field = |key: &str| {
        fields.get(key).and_then(|value| match *value {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
    };
    let u64_field = |key: &str| {
        fields
            .get(key)
            .and_then(|value| (*value).parse::<u64>().ok())
    };
    let string_field = |key: &str| fields.get(key).map(|value| (*value).to_owned());
    let tab_control_available = bool_field("tab_control_available");
    let reason = if tab_control_available == Some(false) && !fields.contains_key("active_host_id") {
        string_field("reason")
    } else {
        None
    };
    ChromeBridgeDetail {
        tab_control_available,
        extension_stale: bool_field("extension_stale"),
        extension_stale_reasons: string_field("extension_stale_reasons"),
        reason,
        host_count: u64_field("host_count"),
        queued_count: u64_field("queued_count"),
        pending_count: u64_field("pending_count"),
        extension_id: string_field("extension_id"),
        expected_extension_id: string_field("expected_extension_id"),
        extension_version: string_field("extension_version"),
        transport: string_field("transport"),
        endpoint: string_field("endpoint"),
    }
}

/// Reduce a fully-parsed `ChromeBridgeDetail` to the verdict-critical fields
/// retained in compact health responses. The verbose identity/version/endpoint
/// fields are dropped (they remain available via `detail=full`).
fn compact_chrome_bridge_detail(detail: ChromeBridgeDetail) -> ChromeBridgeDetail {
    ChromeBridgeDetail {
        tab_control_available: detail.tab_control_available,
        extension_stale: detail.extension_stale,
        extension_stale_reasons: detail.extension_stale_reasons,
        reason: detail.reason,
        host_count: detail.host_count,
        queued_count: detail.queued_count,
        pending_count: detail.pending_count,
        extension_id: None,
        expected_extension_id: None,
        extension_version: None,
        transport: None,
        endpoint: None,
    }
}

fn canonical_json_bytes(value: Value) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&canonical_json_value(value))
}

fn canonical_json_value(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(canonical_json_value).collect()),
        Value::Object(map) => {
            let mut entries = map.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            let mut ordered = Map::new();
            for (key, child) in entries {
                ordered.insert(key, canonical_json_value(child));
            }
            Value::Object(ordered)
        }
        scalar => scalar,
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

/// Report whether neural detection actually runs, from the same gate the
/// observe path uses (#2054).
///
/// The observe path decides per call with `input.mode_override`, which a caller
/// may set for one request; this reports the daemon's standing configuration,
/// which is what a health reader is asking about.
fn perception_detection_health(state: &crate::m1::M1State) -> PerceptionDetectionHealth {
    let config = &state.detection_config;
    let gate = crate::m1::detection_inference_gate(config);
    let configured_model_id = config.model_id.clone();
    let configured_model_registered = configured_model_id
        .as_deref()
        .map(|id| synapse_models::registered_model(id).is_some());
    // #2064: the status is the gate's own verdict label, not a two-valued
    // reduction of it. A profile naming an unloadable detector reported
    // `configured` here while every pixel-bearing observe failed, with
    // `configured_model_registered: false` as the only tell.
    let (status, reason_code, remediation) = gate.map_or_else(
        || ("configured".to_owned(), None, None),
        |fault| {
            (
                fault.kind.status().to_owned(),
                Some(fault.reason_code),
                Some(fault.remediation),
            )
        },
    );
    PerceptionDetectionHealth {
        inference_configured: status == "configured",
        status,
        reason_code,
        remediation,
        configured_model_id,
        configured_model_registered,
        max_detections: config.max_detections,
        confidence_threshold: config.confidence_threshold,
        perception_mode: state.perception_mode,
        // Mirrors the mode gate in `populate_detection_from_state`: only the
        // two pixel-bearing modes reach detection at all. `Auto` does not.
        mode_admits_detection: matches!(
            state.perception_mode,
            PerceptionMode::PixelOnly | PerceptionMode::Hybrid
        ),
        config_source: state.detection_config_source.clone(),
        config_applied_unix_ms: state.detection_config_applied_unix_ms,
        bundled_model_id: None,
        bundled_provider: None,
        bundled_materialized: None,
        bundled_materialized_verified: None,
        bundled_materialized_path: None,
    }
}

fn backend_resolution_health(
    source: String,
    policy: BackendResolutionPolicy,
) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("source".to_owned(), source),
        (
            "default_backend".to_owned(),
            backend_config_name(policy.default_backend).to_owned(),
        ),
        (
            "keyboard_default".to_owned(),
            backend_config_name(policy.keyboard_default).to_owned(),
        ),
        (
            "mouse_default".to_owned(),
            backend_config_name(policy.mouse_default).to_owned(),
        ),
        (
            "pad_default".to_owned(),
            backend_config_name(policy.pad_default).to_owned(),
        ),
        (
            "keyboard_auto".to_owned(),
            policy.keyboard_auto_backend().as_str().to_owned(),
        ),
        (
            "mouse_auto".to_owned(),
            policy.mouse_auto_backend().as_str().to_owned(),
        ),
        (
            "pad_auto".to_owned(),
            policy.pad_auto_backend().as_str().to_owned(),
        ),
        (
            "release_all_auto".to_owned(),
            policy.release_all_auto_backend().as_str().to_owned(),
        ),
    ])
}

const fn backend_config_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Software => "software",
        Backend::Vigem => "vigem",
        Backend::Hardware => "hardware",
        Backend::Auto => "auto",
    }
}
