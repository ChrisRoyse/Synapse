use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use rmcp::model::ErrorCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use synapse_core::error_codes;

#[cfg(windows)]
use std::os::windows::{ffi::OsStrExt, process::CommandExt};
#[cfg(windows)]
use windows::{
    Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
    core::PCWSTR,
};

use crate::{
    m4,
    server::{
        ErrorData,
        operational_facades::{
            SETUP_SOT,
            types::{
                FileReadback, SetupHostTransitionAction, SetupHostTransitionGuardReadback,
                SetupHostTransitionGuardSpec, SetupHostTransitionIntentReadback,
                SetupHostTransitionKind, SetupHostTransitionOverride, SetupHostTransitionParams,
                SetupHostTransitionResponse,
            },
        },
    },
};

const STATE_SCHEMA: &str = "synapse_host_transition_guards/v1";
const PREFLIGHT_SCHEMA: &str = "synapse_host_transition_preflight/v1";
const OVERRIDE_SCHEMA: &str = "synapse_host_transition_override/v1";
const INTENT_SCHEMA: &str = "synapse_host_transition_intent/v1";
const CONFIG_FILE: &str = "guards.json";
const PENDING_INTENT_FILE: &str = "pending-intent.json";
const PREFLIGHT_TTL_MS: u64 = 5 * 60 * 1_000;
const MAX_GUARDS: usize = 32;
const MAX_WSL_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_LEASE_BYTES: usize = 1024 * 1024;
const MAX_EVENT_LOG_BYTES: usize = 8 * 1024 * 1024;
const OVERRIDE_CONFIRMATION: &str = "I ACCEPT DESTRUCTIVE HOST TRANSITION";
const CONFIGURE_CONFIRMATION: &str = "REPLACE HOST TRANSITION GUARDS";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GuardConfig {
    schema: String,
    guards: Vec<SetupHostTransitionGuardSpec>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PreflightRecord {
    schema: String,
    preflight_id: String,
    transition: SetupHostTransitionKind,
    reason: String,
    created_unix_ms: u64,
    expires_unix_ms: u64,
    host_boot_id: String,
    safety_digest: String,
    authorized: bool,
    durable_job_ids: Vec<String>,
    active_guard_ids: Vec<String>,
    override_id: Option<String>,
    consumed_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct OverrideRecord {
    schema: String,
    override_id: String,
    preflight_id: String,
    transition: SetupHostTransitionKind,
    reason: String,
    recorded_unix_ms: u64,
    host_boot_id: String,
    accepted_job_ids: Vec<String>,
    accepted_guard_checkpoint_sha256: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IntentRecord {
    schema: String,
    intent_id: String,
    preflight_id: String,
    transition: SetupHostTransitionKind,
    reason: String,
    requested_unix_ms: u64,
    prior_host_boot_id: String,
    status: String,
    shutdown_comment: String,
    event_1074_record_id: Option<u64>,
    event_1074_sha256: Option<String>,
    reconciled_host_boot_id: Option<String>,
    reconciled_unix_ms: Option<u64>,
    trigger_error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LslocksDocument {
    locks: Vec<LslocksRow>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LslocksRow {
    pid: Option<u32>,
    command: Option<String>,
    #[serde(rename = "type")]
    lock_type: String,
    mode: String,
    path: String,
}

pub(super) fn handle(
    params: SetupHostTransitionParams,
) -> Result<SetupHostTransitionResponse, ErrorData> {
    validate_action_shape(&params)?;
    tracing::info!(
        code = "SETUP_HOST_TRANSITION_INVOCATION",
        action = params.action.as_str(),
        transition = params.transition.map(SetupHostTransitionKind::as_str),
        "setup.host_transition invocation"
    );
    match params.action {
        SetupHostTransitionAction::Status => status_response(params.action),
        SetupHostTransitionAction::Configure => configure(params),
        SetupHostTransitionAction::Preflight => preflight(params),
        SetupHostTransitionAction::Execute => execute(params),
    }
}

#[cfg(windows)]
pub(crate) fn reconcile_pending_intent_on_startup() -> Result<(), ErrorData> {
    let state_root = state_root()?;
    let current_host_boot_id = m4::read_host_boot_identity()?;
    let _ = reconcile_pending_intent(&state_root, &current_host_boot_id)?;
    Ok(())
}

fn status_response(
    action: SetupHostTransitionAction,
) -> Result<SetupHostTransitionResponse, ErrorData> {
    let state_root = state_root()?;
    let current_host_boot_id = m4::read_host_boot_identity()?;
    let intent = reconcile_pending_intent(&state_root, &current_host_boot_id)?.map(intent_readback);
    let config = read_guard_config(&state_root)?;
    let durable_jobs = m4::shell_job_host_transition_snapshot()?;
    let guards = probe_guards(&config.guards)?;
    let safety_digest = safety_digest(&current_host_boot_id, &durable_jobs, &guards)?;
    let authorized =
        durable_jobs.live_jobs.is_empty() && guards.iter().all(|guard| !guard.lease_active);
    Ok(SetupHostTransitionResponse {
        action,
        source_of_truth: host_transition_sot(&state_root),
        state_root: state_root.display().to_string(),
        current_host_boot_id,
        guard_config_file: file_readback(state_root.join(CONFIG_FILE)),
        durable_jobs,
        guards,
        authorized,
        safety_digest,
        preflight_id: None,
        preflight_path: None,
        override_path: None,
        intent,
    })
}

fn configure(params: SetupHostTransitionParams) -> Result<SetupHostTransitionResponse, ErrorData> {
    let state_root = state_root()?;
    let guards = params
        .guards
        .ok_or_else(|| invalid_params("configure requires guards", "host_transition.guards"))?;
    validate_guard_specs(&guards)?;
    let config = GuardConfig {
        schema: STATE_SCHEMA.to_owned(),
        guards,
    };
    let path = state_root.join(CONFIG_FILE);
    write_verified_json(&path, &config, "guard_config")?;
    let readback = read_guard_config(&state_root)?;
    if readback.guards.len() != config.guards.len()
        || safety_sha256(&readback)? != safety_sha256(&config)?
    {
        return Err(host_error(
            "HOST_TRANSITION_GUARD_CONFIG_READBACK_MISMATCH",
            "guard_config",
            format!(
                "host-transition guard configuration did not match its separate readback at {}",
                path.display()
            ),
            "inspect the exact guard config bytes and filesystem replacement semantics; host transition remains refused",
        ));
    }
    tracing::info!(
        code = "SETUP_HOST_TRANSITION_GUARDS_CONFIGURED",
        path = %path.display(),
        guard_count = config.guards.len(),
        sha256 = %safety_sha256(&config)?,
        "readback=host_transition_guard_config after=atomic_replace"
    );
    status_response(SetupHostTransitionAction::Configure)
}

fn preflight(params: SetupHostTransitionParams) -> Result<SetupHostTransitionResponse, ErrorData> {
    let state_root = state_root()?;
    let transition = params.transition.ok_or_else(|| {
        invalid_params(
            "preflight requires transition",
            "host_transition.transition",
        )
    })?;
    let reason = validated_reason(params.reason.as_deref())?;
    let now = now_unix_ms()?;
    let preflight_id = format!("pf-{}-{}", now, uuid::Uuid::new_v4().simple());
    let current_host_boot_id = m4::read_host_boot_identity()?;
    let config = read_guard_config(&state_root)?;
    let durable_jobs = m4::shell_job_host_transition_snapshot()?;
    let guards = probe_guards(&config.guards)?;
    let safety_digest = safety_digest(&current_host_boot_id, &durable_jobs, &guards)?;
    let durable_job_ids = durable_jobs
        .live_jobs
        .iter()
        .map(|job| job.job_id.clone())
        .collect::<Vec<_>>();
    let active_guard_ids = guards
        .iter()
        .filter(|guard| guard.lease_active)
        .map(|guard| guard.id.clone())
        .collect::<Vec<_>>();
    let has_blockers = !durable_job_ids.is_empty() || !active_guard_ids.is_empty();
    let mut override_id = None;
    let mut override_path = None;

    if has_blockers {
        let Some(acceptance) = params.override_acceptance.as_ref() else {
            let record = PreflightRecord {
                schema: PREFLIGHT_SCHEMA.to_owned(),
                preflight_id: preflight_id.clone(),
                transition,
                reason,
                created_unix_ms: now,
                expires_unix_ms: now.saturating_add(PREFLIGHT_TTL_MS),
                host_boot_id: current_host_boot_id,
                safety_digest,
                authorized: false,
                durable_job_ids: durable_job_ids.clone(),
                active_guard_ids: active_guard_ids.clone(),
                override_id: None,
                consumed_unix_ms: None,
            };
            let path = preflight_path(&state_root, &preflight_id);
            write_verified_json(&path, &record, "blocked_preflight")?;
            tracing::error!(
                code = "SETUP_HOST_TRANSITION_PREFLIGHT_REFUSED_LIVE_WORK",
                preflight_id = %preflight_id,
                transition = transition.as_str(),
                durable_job_ids = ?durable_job_ids,
                active_guard_ids = ?active_guard_ids,
                preflight_path = %path.display(),
                "planned host transition refused because protected work is live"
            );
            return Err(blocked_error(
                &record,
                &path,
                &durable_jobs,
                &guards,
                "protected durable jobs or production leases are live; wait for terminal/released state, or supply the explicit checkpoint-bound destructive override",
            ));
        };
        let (id, path) = validate_and_record_override(
            &state_root,
            &preflight_id,
            transition,
            &current_host_boot_id,
            &durable_job_ids,
            &active_guard_ids,
            &guards,
            acceptance,
        )?;
        override_id = Some(id);
        override_path = Some(path.display().to_string());
    } else if params.override_acceptance.is_some() {
        return Err(invalid_params(
            "destructive override was supplied even though no protected work is live",
            "host_transition.override_acceptance",
        ));
    }

    let record = PreflightRecord {
        schema: PREFLIGHT_SCHEMA.to_owned(),
        preflight_id: preflight_id.clone(),
        transition,
        reason,
        created_unix_ms: now,
        expires_unix_ms: now.saturating_add(PREFLIGHT_TTL_MS),
        host_boot_id: current_host_boot_id.clone(),
        safety_digest: safety_digest.clone(),
        authorized: true,
        durable_job_ids,
        active_guard_ids,
        override_id,
        consumed_unix_ms: None,
    };
    let path = preflight_path(&state_root, &preflight_id);
    write_verified_json(&path, &record, "authorized_preflight")?;
    tracing::info!(
        code = "SETUP_HOST_TRANSITION_PREFLIGHT_AUTHORIZED",
        preflight_id = %preflight_id,
        transition = transition.as_str(),
        host_boot_id = %current_host_boot_id,
        safety_digest = %safety_digest,
        preflight_path = %path.display(),
        override_path = ?override_path,
        "readback=host_transition_preflight after=physical_safety_snapshot"
    );
    Ok(SetupHostTransitionResponse {
        action: SetupHostTransitionAction::Preflight,
        source_of_truth: host_transition_sot(&state_root),
        state_root: state_root.display().to_string(),
        current_host_boot_id,
        guard_config_file: file_readback(state_root.join(CONFIG_FILE)),
        durable_jobs,
        guards,
        authorized: true,
        safety_digest,
        preflight_id: Some(preflight_id),
        preflight_path: Some(path.display().to_string()),
        override_path,
        intent: reconcile_pending_intent(&state_root, record.host_boot_id.as_str())?
            .map(intent_readback),
    })
}

fn execute(params: SetupHostTransitionParams) -> Result<SetupHostTransitionResponse, ErrorData> {
    let state_root = state_root()?;
    let transition = params.transition.ok_or_else(|| {
        invalid_params("execute requires transition", "host_transition.transition")
    })?;
    let reason = validated_reason(params.reason.as_deref())?;
    let preflight_id = params.preflight_id.as_deref().ok_or_else(|| {
        invalid_params(
            "execute requires preflight_id",
            "host_transition.preflight_id",
        )
    })?;
    validate_identifier(preflight_id, "preflight_id")?;
    let expected_confirmation = match transition {
        SetupHostTransitionKind::Restart => "EXECUTE PLANNED RESTART",
        SetupHostTransitionKind::Poweroff => "EXECUTE PLANNED POWEROFF",
    };
    if params.confirmation.as_deref() != Some(expected_confirmation) {
        return Err(invalid_params(
            format!("execute confirmation must equal {expected_confirmation:?}"),
            "host_transition.confirmation",
        ));
    }
    let preflight_file = preflight_path(&state_root, preflight_id);
    let mut record: PreflightRecord = read_required_json(&preflight_file, "preflight")?;
    if record.schema != PREFLIGHT_SCHEMA
        || record.preflight_id != preflight_id
        || !record.authorized
        || record.consumed_unix_ms.is_some()
    {
        return Err(host_error(
            "HOST_TRANSITION_PREFLIGHT_NOT_EXECUTABLE",
            "preflight",
            format!(
                "host-transition preflight {} is unsupported, refused, mismatched, or already consumed",
                preflight_file.display()
            ),
            "run a fresh setup.host_transition action=preflight and use its exact transition, reason, and preflight_id",
        ));
    }
    if record.transition != transition || record.reason != reason {
        return Err(invalid_params(
            "execute transition/reason do not exactly match the persisted preflight",
            "host_transition.preflight_id",
        ));
    }
    let now = now_unix_ms()?;
    if now > record.expires_unix_ms {
        return Err(host_error(
            "HOST_TRANSITION_PREFLIGHT_EXPIRED",
            "preflight",
            format!(
                "host-transition preflight {preflight_id} expired at {} (now={now})",
                record.expires_unix_ms
            ),
            "run a fresh preflight so all jobs, leases, checkpoints, and boot identity are read again",
        ));
    }
    let current_host_boot_id = m4::read_host_boot_identity()?;
    if current_host_boot_id != record.host_boot_id {
        return Err(host_error(
            "HOST_TRANSITION_PREFLIGHT_BOOT_CHANGED",
            "kernel_boot_identity",
            format!(
                "host boot identity changed after preflight: preflight={} current={current_host_boot_id}",
                record.host_boot_id
            ),
            "run a fresh preflight on the current boot; the prior authorization cannot be replayed",
        ));
    }
    let config = read_guard_config(&state_root)?;
    let durable_jobs = m4::shell_job_host_transition_snapshot()?;
    let guards = probe_guards(&config.guards)?;
    let current_digest = safety_digest(&current_host_boot_id, &durable_jobs, &guards)?;
    if current_digest != record.safety_digest {
        return Err(ErrorData::new(
            ErrorCode(-32099),
            "host-transition safety state changed after preflight; refusing execution",
            Some(json!({
                "code": error_codes::TOOL_INTERNAL_ERROR,
                "detail_code": "HOST_TRANSITION_SAFETY_STATE_CHANGED",
                "source_of_truth": SETUP_SOT,
                "preflight_path": preflight_file,
                "preflight_safety_digest": record.safety_digest,
                "current_safety_digest": current_digest,
                "durable_jobs": durable_jobs,
                "guards": guards,
                "remediation": "inspect the changed job/lease/checkpoint state and run a fresh preflight",
            })),
        ));
    }
    let intent_id = format!("intent-{}-{}", now, uuid::Uuid::new_v4().simple());
    let shutdown_comment = format!(
        "Synapse planned {} intent={} {}",
        transition.as_str(),
        intent_id,
        reason
    );
    let intent_path = state_root.join("intents").join(format!("{intent_id}.json"));
    let pending_path = state_root.join(PENDING_INTENT_FILE);
    let mut intent = IntentRecord {
        schema: INTENT_SCHEMA.to_owned(),
        intent_id: intent_id.clone(),
        preflight_id: preflight_id.to_owned(),
        transition,
        reason,
        requested_unix_ms: now,
        prior_host_boot_id: current_host_boot_id.clone(),
        status: "persisted_before_trigger".to_owned(),
        shutdown_comment: shutdown_comment.clone(),
        event_1074_record_id: None,
        event_1074_sha256: None,
        reconciled_host_boot_id: None,
        reconciled_unix_ms: None,
        trigger_error: None,
    };
    write_verified_json(&intent_path, &intent, "intent_archive")?;
    write_verified_json(&pending_path, &intent, "pending_intent")?;
    record.consumed_unix_ms = Some(now);
    write_verified_json(&preflight_file, &record, "consumed_preflight")?;

    match trigger_shutdown(transition, &shutdown_comment) {
        Ok(()) => {
            intent.status = "request_accepted".to_owned();
            write_verified_json(&intent_path, &intent, "accepted_intent_archive")?;
            write_verified_json(&pending_path, &intent, "accepted_pending_intent")?;
        }
        Err(error) => {
            intent.status = "trigger_failed".to_owned();
            intent.trigger_error = Some(error.message.to_string());
            write_verified_json(&intent_path, &intent, "failed_intent_archive")?;
            write_verified_json(&pending_path, &intent, "failed_pending_intent")?;
            return Err(error);
        }
    }
    tracing::warn!(
        code = "SETUP_HOST_TRANSITION_REQUEST_ACCEPTED",
        intent_id = %intent_id,
        transition = transition.as_str(),
        host_boot_id = %current_host_boot_id,
        intent_path = %intent_path.display(),
        pending_intent_path = %pending_path.display(),
        "Windows accepted the planned host-transition request; acceptance is not completion and must be reconciled after boot against Event 1074 plus BootIdentifier"
    );
    Ok(SetupHostTransitionResponse {
        action: SetupHostTransitionAction::Execute,
        source_of_truth: host_transition_sot(&state_root),
        state_root: state_root.display().to_string(),
        current_host_boot_id: current_host_boot_id.clone(),
        guard_config_file: file_readback(state_root.join(CONFIG_FILE)),
        durable_jobs,
        guards,
        authorized: true,
        safety_digest: current_digest,
        preflight_id: Some(preflight_id.to_owned()),
        preflight_path: Some(preflight_file.display().to_string()),
        override_path: record.override_id.as_ref().map(|id| {
            state_root
                .join("overrides")
                .join(format!("{id}.json"))
                .display()
                .to_string()
        }),
        intent: Some(intent_readback_at(
            intent,
            intent_path,
            current_host_boot_id,
        )),
    })
}

fn validate_action_shape(params: &SetupHostTransitionParams) -> Result<(), ErrorData> {
    let wrong = match params.action {
        SetupHostTransitionAction::Status => {
            params.transition.is_some()
                || params.reason.is_some()
                || params.preflight_id.is_some()
                || params.confirmation.is_some()
                || params.guards.is_some()
                || params.override_acceptance.is_some()
        }
        SetupHostTransitionAction::Configure => {
            params.transition.is_some()
                || params.reason.is_some()
                || params.preflight_id.is_some()
                || params.override_acceptance.is_some()
                || params.guards.is_none()
                || params.confirmation.as_deref() != Some(CONFIGURE_CONFIRMATION)
        }
        SetupHostTransitionAction::Preflight => {
            params.transition.is_none()
                || params.reason.is_none()
                || params.preflight_id.is_some()
                || params.confirmation.is_some()
                || params.guards.is_some()
        }
        SetupHostTransitionAction::Execute => {
            params.transition.is_none()
                || params.reason.is_none()
                || params.preflight_id.is_none()
                || params.confirmation.is_none()
                || params.guards.is_some()
                || params.override_acceptance.is_some()
        }
    };
    if wrong {
        return Err(invalid_params(
            format!(
                "host_transition action={} has missing or extra fields",
                params.action.as_str()
            ),
            "host_transition",
        ));
    }
    Ok(())
}

fn validate_guard_specs(guards: &[SetupHostTransitionGuardSpec]) -> Result<(), ErrorData> {
    if guards.len() > MAX_GUARDS {
        return Err(invalid_params(
            format!("guards exceeds maximum {MAX_GUARDS}"),
            "host_transition.guards",
        ));
    }
    let mut ids = BTreeSet::new();
    for guard in guards {
        validate_identifier(&guard.id, "guard.id")?;
        validate_identifier(&guard.distribution, "guard.distribution")?;
        validate_linux_absolute_path(&guard.lease_path, "guard.lease_path")?;
        validate_linux_absolute_path(&guard.checkpoint_path, "guard.checkpoint_path")?;
        validate_identifier(&guard.lease_active_phase, "guard.lease_active_phase")?;
        validate_identifier(
            &guard.checkpoint_complete_field,
            "guard.checkpoint_complete_field",
        )?;
        if !ids.insert(guard.id.clone()) {
            return Err(invalid_params(
                format!("duplicate host-transition guard id {}", guard.id),
                "host_transition.guards",
            ));
        }
    }
    Ok(())
}

fn validate_identifier(value: &str, field: &'static str) -> Result<(), ErrorData> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid_params(
            format!("{field} must be 1..=128 ASCII alphanumeric/._- bytes"),
            field,
        ));
    }
    Ok(())
}

fn validate_linux_absolute_path(value: &str, field: &'static str) -> Result<(), ErrorData> {
    if !value.starts_with('/')
        || value.len() > 1_024
        || value
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(invalid_params(
            format!(
                "{field} must be an absolute Linux path of at most 1024 bytes with no control characters"
            ),
            field,
        ));
    }
    Ok(())
}

fn validated_reason(reason: Option<&str>) -> Result<String, ErrorData> {
    let Some(reason) = reason.map(str::trim).filter(|reason| !reason.is_empty()) else {
        return Err(invalid_params(
            "host transition reason must not be empty",
            "host_transition.reason",
        ));
    };
    if reason.len() > 240 || reason.chars().any(char::is_control) {
        return Err(invalid_params(
            "host transition reason must be at most 240 bytes and contain no control characters",
            "host_transition.reason",
        ));
    }
    Ok(reason.to_owned())
}

fn validate_and_record_override(
    state_root: &Path,
    preflight_id: &str,
    transition: SetupHostTransitionKind,
    host_boot_id: &str,
    durable_job_ids: &[String],
    active_guard_ids: &[String],
    guards: &[SetupHostTransitionGuardReadback],
    acceptance: &SetupHostTransitionOverride,
) -> Result<(String, PathBuf), ErrorData> {
    if acceptance.confirmation != OVERRIDE_CONFIRMATION {
        return Err(invalid_params(
            format!("override confirmation must equal {OVERRIDE_CONFIRMATION:?}"),
            "host_transition.override_acceptance.confirmation",
        ));
    }
    let reason = validated_reason(Some(&acceptance.reason))?;
    let accepted_jobs = acceptance
        .accepted_job_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let required_jobs = durable_job_ids.iter().cloned().collect::<BTreeSet<_>>();
    if accepted_jobs != required_jobs {
        return Err(invalid_params(
            "override accepted_job_ids must exactly equal every live durable job id",
            "host_transition.override_acceptance.accepted_job_ids",
        ));
    }
    let mut supplied = BTreeMap::new();
    for guard in &acceptance.guard_acceptances {
        validate_identifier(&guard.guard_id, "override.guard_id")?;
        validate_sha256(&guard.checkpoint_sha256)?;
        if supplied
            .insert(guard.guard_id.clone(), guard.checkpoint_sha256.clone())
            .is_some()
        {
            return Err(invalid_params(
                format!("duplicate override guard acceptance {}", guard.guard_id),
                "host_transition.override_acceptance.guard_acceptances",
            ));
        }
    }
    let required_guards = active_guard_ids.iter().cloned().collect::<BTreeSet<_>>();
    if supplied.keys().cloned().collect::<BTreeSet<_>>() != required_guards {
        return Err(invalid_params(
            "override guard_acceptances must exactly equal every active configured guard",
            "host_transition.override_acceptance.guard_acceptances",
        ));
    }
    for guard_id in active_guard_ids {
        let guard = guards
            .iter()
            .find(|guard| guard.id == *guard_id)
            .ok_or_else(|| {
                host_error(
                    "HOST_TRANSITION_OVERRIDE_GUARD_READBACK_MISSING",
                    "guard_readback",
                    format!("active guard {guard_id} has no physical readback"),
                    "repair the guard inventory mismatch and retry preflight; host transition remains refused",
                )
            })?;
        if !guard.checkpoint_complete {
            return Err(host_error(
                "HOST_TRANSITION_OVERRIDE_CHECKPOINT_INCOMPLETE",
                "checkpoint",
                format!(
                    "destructive override refused: guard {} checkpoint {} has {}=false",
                    guard.id, guard.checkpoint_path, guard.checkpoint_complete_field
                ),
                "wait for the application to atomically publish a complete checkpoint, then read and accept its new SHA-256",
            ));
        }
        if supplied.get(guard_id) != Some(&guard.checkpoint_sha256) {
            return Err(host_error(
                "HOST_TRANSITION_OVERRIDE_CHECKPOINT_DIGEST_MISMATCH",
                "checkpoint",
                format!(
                    "destructive override refused: guard {} checkpoint digest does not match its separate physical readback",
                    guard.id
                ),
                "read the exact checkpoint bytes, confirm semantic completeness, and supply the returned checkpoint_sha256",
            ));
        }
    }
    let now = now_unix_ms()?;
    let override_id = format!("override-{}-{}", now, uuid::Uuid::new_v4().simple());
    let record = OverrideRecord {
        schema: OVERRIDE_SCHEMA.to_owned(),
        override_id: override_id.clone(),
        preflight_id: preflight_id.to_owned(),
        transition,
        reason,
        recorded_unix_ms: now,
        host_boot_id: host_boot_id.to_owned(),
        accepted_job_ids: acceptance.accepted_job_ids.clone(),
        accepted_guard_checkpoint_sha256: supplied,
    };
    let path = state_root
        .join("overrides")
        .join(format!("{override_id}.json"));
    write_verified_json(&path, &record, "destructive_override")?;
    tracing::warn!(
        code = "SETUP_HOST_TRANSITION_DESTRUCTIVE_OVERRIDE_ACCEPTED",
        override_id = %override_id,
        preflight_id,
        transition = transition.as_str(),
        path = %path.display(),
        accepted_job_ids = ?record.accepted_job_ids,
        accepted_guard_ids = ?record.accepted_guard_checkpoint_sha256.keys().collect::<Vec<_>>(),
        "readback=host_transition_override after=checkpoint_digest_acceptance"
    );
    Ok((override_id, path))
}

fn probe_guards(
    specs: &[SetupHostTransitionGuardSpec],
) -> Result<Vec<SetupHostTransitionGuardReadback>, ErrorData> {
    validate_guard_specs(specs)?;
    let mut readbacks = Vec::with_capacity(specs.len());
    for spec in specs {
        readbacks.push(probe_guard(spec)?);
    }
    readbacks.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(readbacks)
}

fn probe_guard(
    spec: &SetupHostTransitionGuardSpec,
) -> Result<SetupHostTransitionGuardReadback, ErrorData> {
    let lslocks_bytes = run_wsl(
        &spec.distribution,
        &[
            "/usr/bin/lslocks",
            "--json",
            "--notruncate",
            "--output",
            "PID,COMMAND,TYPE,MODE,PATH",
        ],
        "kernel_lock_table",
    )?;
    if lslocks_bytes.len() > MAX_LEASE_BYTES {
        return Err(host_error(
            "HOST_TRANSITION_LOCK_TABLE_TOO_LARGE",
            "kernel_lock_table",
            format!(
                "WSL lock-table readback exceeded {MAX_LEASE_BYTES} bytes for distribution {}",
                spec.distribution
            ),
            "inspect the distribution lock table and retry; host transition remains refused",
        ));
    }
    let document: LslocksDocument = serde_json::from_slice(&lslocks_bytes).map_err(|error| {
        host_error(
            "HOST_TRANSITION_LOCK_TABLE_INVALID",
            "kernel_lock_table",
            format!(
                "WSL lslocks JSON was invalid for distribution {}: {error}",
                spec.distribution
            ),
            "repair util-linux lslocks JSON output and retry; host transition remains refused",
        )
    })?;
    let matching = document
        .locks
        .iter()
        .filter(|row| row.path == spec.lease_path)
        .collect::<Vec<_>>();
    if matching.len() > 1 {
        return Err(host_error(
            "HOST_TRANSITION_DUPLICATE_KERNEL_LOCK_ROWS",
            "kernel_lock_table",
            format!(
                "WSL lock table returned multiple rows for configured lease {}",
                spec.lease_path
            ),
            "inspect the exact lease inode/owners and repair the ambiguous lock contract; host transition remains refused",
        ));
    }
    let kernel_owner = matching.first().copied();
    if let Some(row) = kernel_owner {
        if row.lock_type != "FLOCK" || row.mode != "WRITE" || row.pid.is_none() {
            return Err(host_error(
                "HOST_TRANSITION_KERNEL_LOCK_SHAPE_INVALID",
                "kernel_lock_table",
                format!(
                    "configured lease {} is present with unexpected type={} mode={} pid={:?}",
                    spec.lease_path, row.lock_type, row.mode, row.pid
                ),
                "repair the production lease to an exclusive FLOCK WRITE owner; host transition remains refused",
            ));
        }
    }
    let lease_bytes = read_wsl_file(
        &spec.distribution,
        &spec.lease_path,
        MAX_LEASE_BYTES,
        "lease_record",
    )?;
    let lease_json: Value = serde_json::from_slice(&lease_bytes).map_err(|error| {
        host_error(
            "HOST_TRANSITION_LEASE_JSON_INVALID",
            "lease_record",
            format!(
                "configured lease record {} is invalid JSON: {error}",
                spec.lease_path
            ),
            "repair the exact lease record and retry; host transition remains refused",
        )
    })?;
    let lease_object = lease_json.as_object().ok_or_else(|| {
        host_error(
            "HOST_TRANSITION_LEASE_NOT_OBJECT",
            "lease_record",
            format!(
                "configured lease record {} is not a JSON object",
                spec.lease_path
            ),
            "repair the exact lease record and retry; host transition remains refused",
        )
    })?;
    let phase = lease_object
        .get("phase")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            host_error(
                "HOST_TRANSITION_LEASE_PHASE_MISSING",
                "lease_record",
                format!(
                    "configured lease record {} has no string phase",
                    spec.lease_path
                ),
                "repair the lease contract to persist its phase; host transition remains refused",
            )
        })?
        .to_owned();
    let released_present = match lease_object.get("released_unix_ns") {
        Some(Value::Null) | None => false,
        Some(Value::Number(_)) => true,
        Some(_) => {
            return Err(host_error(
                "HOST_TRANSITION_LEASE_RELEASE_INVALID",
                "lease_record",
                format!(
                    "configured lease record {} has a non-null, non-numeric released_unix_ns",
                    spec.lease_path
                ),
                "repair the lease contract release field; host transition remains refused",
            ));
        }
    };
    let metadata_active = phase == spec.lease_active_phase && !released_present;
    if kernel_owner.is_some() && !metadata_active {
        return Err(host_error(
            "HOST_TRANSITION_LEASE_KERNEL_METADATA_CONTRADICTION",
            "lease_record",
            format!(
                "kernel lock {} is held but its lease metadata says phase={} released={released_present}",
                spec.lease_path, phase
            ),
            "reconcile the production lease kernel owner and atomic metadata before retrying; host transition remains refused",
        ));
    }
    let checkpoint_bytes = read_wsl_file(
        &spec.distribution,
        &spec.checkpoint_path,
        MAX_WSL_FILE_BYTES,
        "checkpoint",
    )?;
    let checkpoint_json: Value = serde_json::from_slice(&checkpoint_bytes).map_err(|error| {
        host_error(
            "HOST_TRANSITION_CHECKPOINT_JSON_INVALID",
            "checkpoint",
            format!(
                "configured checkpoint {} is invalid JSON: {error}",
                spec.checkpoint_path
            ),
            "repair the exact checkpoint bytes and retry; host transition remains refused",
        )
    })?;
    let checkpoint_complete = checkpoint_json
        .as_object()
        .and_then(|object| object.get(&spec.checkpoint_complete_field))
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            host_error(
                "HOST_TRANSITION_CHECKPOINT_COMPLETE_MISSING",
                "checkpoint",
                format!(
                    "configured checkpoint {} has no boolean field {}",
                    spec.checkpoint_path, spec.checkpoint_complete_field
                ),
                "repair the checkpoint contract to atomically publish a boolean completeness field; host transition remains refused",
            )
        })?;
    Ok(SetupHostTransitionGuardReadback {
        id: spec.id.clone(),
        distribution: spec.distribution.clone(),
        lease_path: spec.lease_path.clone(),
        // Stale admitted metadata after a controller crash is still protected
        // work even when the kernel owner is gone. It requires explicit lease
        // reconciliation rather than optimistic shutdown authorization.
        lease_active: kernel_owner.is_some() || metadata_active,
        lease_owner_pid: kernel_owner.and_then(|row| row.pid),
        lease_owner_command: kernel_owner.and_then(|row| row.command.clone()),
        lease_phase: phase,
        lease_released_unix_ns_present: released_present,
        lease_sha256: sha256_label(&lease_bytes),
        checkpoint_path: spec.checkpoint_path.clone(),
        checkpoint_complete_field: spec.checkpoint_complete_field.clone(),
        checkpoint_complete,
        checkpoint_len_bytes: checkpoint_bytes.len() as u64,
        checkpoint_sha256: sha256_label(&checkpoint_bytes),
    })
}

fn read_wsl_file(
    distribution: &str,
    path: &str,
    max_bytes: usize,
    source_id: &'static str,
) -> Result<Vec<u8>, ErrorData> {
    let limit = max_bytes.saturating_add(1).to_string();
    let bytes = run_wsl(
        distribution,
        &["/usr/bin/head", "--bytes", &limit, "--", path],
        source_id,
    )?;
    if bytes.len() > max_bytes {
        return Err(host_error(
            "HOST_TRANSITION_WSL_FILE_TOO_LARGE",
            source_id,
            format!(
                "configured WSL Source of Truth {path} exceeds the {max_bytes}-byte safety limit"
            ),
            "reduce the machine-readable lease/checkpoint record below the documented limit or narrow the contract; host transition remains refused",
        ));
    }
    Ok(bytes)
}

fn run_wsl(
    distribution: &str,
    args: &[&str],
    source_id: &'static str,
) -> Result<Vec<u8>, ErrorData> {
    #[cfg(windows)]
    {
        let system_root = std::env::var_os("SystemRoot").ok_or_else(|| {
            host_error(
                "HOST_TRANSITION_SYSTEMROOT_MISSING",
                source_id,
                "SystemRoot is missing while resolving wsl.exe".to_owned(),
                "repair the daemon Windows environment and restart synapse-mcp; host transition remains refused",
            )
        })?;
        let wsl = PathBuf::from(system_root).join("System32").join("wsl.exe");
        if !wsl.is_file() {
            return Err(host_error(
                "HOST_TRANSITION_WSL_MISSING",
                source_id,
                format!("required WSL executable is missing at {}", wsl.display()),
                "repair the Windows WSL installation at the named path and retry; host transition remains refused",
            ));
        }
        let output = Command::new(&wsl)
            .arg("--distribution")
            .arg(distribution)
            .arg("--exec")
            .args(args)
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| {
                host_error(
                    "HOST_TRANSITION_WSL_SPAWN_FAILED",
                    source_id,
                    format!(
                        "could not spawn WSL Source-of-Truth read for distribution {distribution}: {error}"
                    ),
                    "repair wsl.exe/distribution process launch and retry; host transition remains refused",
                )
            })?;
        if !output.status.success() {
            return Err(host_error(
                "HOST_TRANSITION_WSL_READ_FAILED",
                source_id,
                format!(
                    "WSL Source-of-Truth read failed for distribution {} with exit={:?}: {}",
                    distribution,
                    output.status.code(),
                    bounded_stderr(&output.stderr)
                ),
                "repair the exact WSL path, permissions, or required /usr/bin utility named by the command and retry; host transition remains refused",
            ));
        }
        Ok(output.stdout)
    }
    #[cfg(not(windows))]
    {
        let _ = (distribution, args);
        Err(host_error(
            "HOST_TRANSITION_WSL_UNSUPPORTED_HOST",
            source_id,
            "configured WSL lease guard requires a Windows host".to_owned(),
            "run the configured host transition on the Windows host that owns the WSL distribution",
        ))
    }
}

fn trigger_shutdown(transition: SetupHostTransitionKind, comment: &str) -> Result<(), ErrorData> {
    #[cfg(windows)]
    {
        let system_root = std::env::var_os("SystemRoot").ok_or_else(|| {
            host_error(
                "HOST_TRANSITION_SYSTEMROOT_MISSING",
                "shutdown_trigger",
                "SystemRoot is missing while resolving shutdown.exe".to_owned(),
                "repair the daemon Windows environment and run a fresh preflight",
            )
        })?;
        let shutdown = PathBuf::from(system_root)
            .join("System32")
            .join("shutdown.exe");
        if !shutdown.is_file() {
            return Err(host_error(
                "HOST_TRANSITION_SHUTDOWN_EXE_MISSING",
                "shutdown_trigger",
                format!(
                    "required Windows shutdown executable is missing at {}",
                    shutdown.display()
                ),
                "repair the Windows system executable and run a fresh preflight",
            ));
        }
        let mode = match transition {
            SetupHostTransitionKind::Restart => "/r",
            SetupHostTransitionKind::Poweroff => "/s",
        };
        let output = Command::new(&shutdown)
            .args([mode, "/t", "30", "/d", "p:4:1", "/c", comment])
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| {
                host_error(
                    "HOST_TRANSITION_SHUTDOWN_SPAWN_FAILED",
                    "shutdown_trigger",
                    format!("could not spawn {}: {error}", shutdown.display()),
                    "inspect Windows shutdown privileges/path and run a fresh preflight",
                )
            })?;
        if !output.status.success() {
            return Err(host_error(
                "HOST_TRANSITION_SHUTDOWN_REJECTED",
                "shutdown_trigger",
                format!(
                    "Windows shutdown.exe rejected the planned {} request with exit={:?}: {}",
                    transition.as_str(),
                    output.status.code(),
                    bounded_stderr(&output.stderr)
                ),
                "inspect shutdown privilege/policy and run a fresh preflight; the persisted intent records trigger_failed",
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (transition, comment);
        Err(host_error(
            "HOST_TRANSITION_SHUTDOWN_UNSUPPORTED_HOST",
            "shutdown_trigger",
            "planned host transitions are implemented only for the configured Windows host"
                .to_owned(),
            "run the transition on the configured Windows Synapse host",
        ))
    }
}

fn reconcile_pending_intent(
    state_root: &Path,
    current_host_boot_id: &str,
) -> Result<Option<(IntentRecord, PathBuf)>, ErrorData> {
    let path = state_root.join(PENDING_INTENT_FILE);
    let Some(mut intent): Option<IntentRecord> = read_optional_json(&path, "pending_intent")?
    else {
        return Ok(None);
    };
    if intent.schema != INTENT_SCHEMA {
        return Err(host_error(
            "HOST_TRANSITION_INTENT_SCHEMA_UNSUPPORTED",
            "pending_intent",
            format!(
                "pending host-transition intent {} has unsupported schema {}",
                path.display(),
                intent.schema
            ),
            "inspect/migrate the exact intent record; host transition remains refused",
        ));
    }
    if intent.prior_host_boot_id == current_host_boot_id
        || matches!(
            intent.status.as_str(),
            "reconciled_after_boot" | "trigger_failed"
        )
    {
        return Ok(Some((intent, path)));
    }
    let (record_id, event_sha256) = find_event_1074(&intent.intent_id)?;
    intent.status = "reconciled_after_boot".to_owned();
    intent.event_1074_record_id = Some(record_id);
    intent.event_1074_sha256 = Some(event_sha256);
    intent.reconciled_host_boot_id = Some(current_host_boot_id.to_owned());
    intent.reconciled_unix_ms = Some(now_unix_ms()?);
    write_verified_json(&path, &intent, "reconciled_pending_intent")?;
    let archive = state_root
        .join("intents")
        .join(format!("{}.json", intent.intent_id));
    write_verified_json(&archive, &intent, "reconciled_intent_archive")?;
    tracing::info!(
        code = "SETUP_HOST_TRANSITION_RECONCILED_AFTER_BOOT",
        intent_id = %intent.intent_id,
        prior_host_boot_id = %intent.prior_host_boot_id,
        current_host_boot_id,
        event_1074_record_id = record_id,
        event_1074_sha256 = ?intent.event_1074_sha256,
        intent_path = %path.display(),
        "readback=host_transition_intent after=BootIdentifier_plus_System_Event_1074"
    );
    Ok(Some((intent, path)))
}

fn find_event_1074(intent_id: &str) -> Result<(u64, String), ErrorData> {
    #[cfg(windows)]
    {
        let system_root = std::env::var_os("SystemRoot").ok_or_else(|| {
            host_error(
                "HOST_TRANSITION_SYSTEMROOT_MISSING",
                "event_1074",
                "SystemRoot is missing while resolving wevtutil.exe".to_owned(),
                "repair the daemon Windows environment and retry intent reconciliation",
            )
        })?;
        let wevtutil = PathBuf::from(system_root)
            .join("System32")
            .join("wevtutil.exe");
        if !wevtutil.is_file() {
            return Err(host_error(
                "HOST_TRANSITION_WEVTUTIL_MISSING",
                "event_1074",
                format!(
                    "required Windows event-log reader is missing at {}",
                    wevtutil.display()
                ),
                "repair the Windows event-log utility and retry intent reconciliation",
            ));
        }
        let output = Command::new(&wevtutil)
            .args([
                "qe",
                "System",
                "/q:*[System[(EventID=1074)]]",
                "/rd:true",
                "/c:64",
                "/f:xml",
            ])
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|error| {
                host_error(
                    "HOST_TRANSITION_EVENT_QUERY_SPAWN_FAILED",
                    "event_1074",
                    format!("could not query Windows System Event 1074: {error}"),
                    "repair Windows Event Log access and retry intent reconciliation",
                )
            })?;
        if !output.status.success() {
            return Err(host_error(
                "HOST_TRANSITION_EVENT_QUERY_FAILED",
                "event_1074",
                format!(
                    "Windows Event 1074 query failed with exit={:?}: {}",
                    output.status.code(),
                    bounded_stderr(&output.stderr)
                ),
                "repair Windows Event Log access and retry intent reconciliation",
            ));
        }
        if output.stdout.len() > MAX_EVENT_LOG_BYTES {
            return Err(host_error(
                "HOST_TRANSITION_EVENT_QUERY_TOO_LARGE",
                "event_1074",
                format!("Windows Event 1074 query exceeded {MAX_EVENT_LOG_BYTES} bytes"),
                "inspect System log query cardinality and retry with a healthy event-log service",
            ));
        }
        let xml = String::from_utf8(output.stdout).map_err(|error| {
            host_error(
                "HOST_TRANSITION_EVENT_QUERY_NOT_UTF8",
                "event_1074",
                format!("Windows Event 1074 XML was not UTF-8: {error}"),
                "repair Windows event-log XML output and retry reconciliation",
            )
        })?;
        let marker = format!("intent={intent_id}");
        let marker_index = xml.find(&marker).ok_or_else(|| {
            host_error(
                "HOST_TRANSITION_EVENT_1074_NOT_FOUND",
                "event_1074",
                format!(
                    "no recent Windows System Event 1074 contains planned host-transition {marker}"
                ),
                "inspect the persisted intent and System/User32 Event 1074 records; do not claim the planned transition completed until the exact intent id is present",
            )
        })?;
        let event_start = xml[..marker_index].rfind("<Event ").ok_or_else(|| {
            host_error(
                "HOST_TRANSITION_EVENT_1074_XML_BOUNDARY_MISSING",
                "event_1074",
                "matching Event 1074 has no opening Event XML boundary".to_owned(),
                "inspect/repair Windows event-log XML and retry reconciliation",
            )
        })?;
        let relative_end = xml[marker_index..].find("</Event>").ok_or_else(|| {
            host_error(
                "HOST_TRANSITION_EVENT_1074_XML_BOUNDARY_MISSING",
                "event_1074",
                "matching Event 1074 has no closing Event XML boundary".to_owned(),
                "inspect/repair Windows event-log XML and retry reconciliation",
            )
        })?;
        let event_end = marker_index + relative_end + "</Event>".len();
        let event = &xml[event_start..event_end];
        if !event.contains("<EventID>1074</EventID>") {
            return Err(host_error(
                "HOST_TRANSITION_EVENT_ID_MISMATCH",
                "event_1074",
                "matching planned-transition event is not EventID 1074".to_owned(),
                "inspect the Windows System event and retry reconciliation",
            ));
        }
        let record_id = xml_tag_u64(event, "EventRecordID")?;
        Ok((record_id, sha256_label(event.as_bytes())))
    }
    #[cfg(not(windows))]
    {
        let _ = intent_id;
        Err(host_error(
            "HOST_TRANSITION_EVENT_1074_UNSUPPORTED_HOST",
            "event_1074",
            "Windows Event 1074 reconciliation requires the configured Windows host".to_owned(),
            "run intent reconciliation on the Windows host that performed the transition",
        ))
    }
}

fn xml_tag_u64(xml: &str, tag: &'static str) -> Result<u64, ErrorData> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml
        .find(&open)
        .map(|index| index + open.len())
        .ok_or_else(|| {
            host_error(
                "HOST_TRANSITION_EVENT_FIELD_MISSING",
                "event_1074",
                format!("matching Event 1074 has no {tag} field"),
                "inspect the Windows event XML and retry reconciliation",
            )
        })?;
    let end = xml[start..]
        .find(&close)
        .map(|index| start + index)
        .ok_or_else(|| {
            host_error(
                "HOST_TRANSITION_EVENT_FIELD_MISSING",
                "event_1074",
                format!("matching Event 1074 has no closing {tag} field"),
                "inspect the Windows event XML and retry reconciliation",
            )
        })?;
    xml[start..end].parse::<u64>().map_err(|error| {
        host_error(
            "HOST_TRANSITION_EVENT_FIELD_INVALID",
            "event_1074",
            format!("matching Event 1074 {tag} is not u64: {error}"),
            "inspect the Windows event XML and retry reconciliation",
        )
    })
}

fn intent_readback(value: (IntentRecord, PathBuf)) -> SetupHostTransitionIntentReadback {
    let current = value
        .0
        .reconciled_host_boot_id
        .clone()
        .unwrap_or_else(|| value.0.prior_host_boot_id.clone());
    intent_readback_at(value.0, value.1, current)
}

fn intent_readback_at(
    intent: IntentRecord,
    path: PathBuf,
    current_host_boot_id: String,
) -> SetupHostTransitionIntentReadback {
    SetupHostTransitionIntentReadback {
        intent_id: intent.intent_id,
        transition: intent.transition,
        status: intent.status,
        intent_path: path.display().to_string(),
        prior_host_boot_id: intent.prior_host_boot_id,
        current_host_boot_id,
        event_1074_record_id: intent.event_1074_record_id,
        event_1074_sha256: intent.event_1074_sha256,
    }
}

fn safety_digest(
    host_boot_id: &str,
    durable_jobs: &m4::ShellJobHostTransitionSnapshot,
    guards: &[SetupHostTransitionGuardReadback],
) -> Result<String, ErrorData> {
    safety_sha256(&json!({
        "schema": "synapse_host_transition_safety_snapshot/v1",
        "host_boot_id": host_boot_id,
        "durable_jobs": durable_jobs,
        "guards": guards,
    }))
}

fn safety_sha256(value: &impl Serialize) -> Result<String, ErrorData> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        host_error(
            "HOST_TRANSITION_SAFETY_SERIALIZE_FAILED",
            "safety_snapshot",
            format!("could not serialize host-transition safety evidence: {error}"),
            "repair the evidence serialization failure; host transition remains refused",
        )
    })?;
    Ok(sha256_label(&bytes))
}

fn validate_sha256(value: &str) -> Result<(), ErrorData> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(invalid_params(
            "checkpoint_sha256 must use sha256:<64 lowercase hex>",
            "host_transition.override_acceptance.checkpoint_sha256",
        ));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(invalid_params(
            "checkpoint_sha256 must use sha256:<64 lowercase hex>",
            "host_transition.override_acceptance.checkpoint_sha256",
        ));
    }
    Ok(())
}

fn sha256_label(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity("sha256:".len() + digest.len() * 2);
    output.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn read_guard_config(state_root: &Path) -> Result<GuardConfig, ErrorData> {
    let path = state_root.join(CONFIG_FILE);
    let Some(config): Option<GuardConfig> = read_optional_json(&path, "guard_config")? else {
        return Ok(GuardConfig {
            schema: STATE_SCHEMA.to_owned(),
            guards: Vec::new(),
        });
    };
    if config.schema != STATE_SCHEMA {
        return Err(host_error(
            "HOST_TRANSITION_GUARD_CONFIG_SCHEMA_UNSUPPORTED",
            "guard_config",
            format!(
                "guard configuration {} has unsupported schema {}",
                path.display(),
                config.schema
            ),
            "inspect/migrate the exact guard config; host transition remains refused",
        ));
    }
    validate_guard_specs(&config.guards)?;
    Ok(config)
}

fn read_required_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    source_id: &'static str,
) -> Result<T, ErrorData> {
    read_optional_json(path, source_id)?.ok_or_else(|| {
        host_error(
            "HOST_TRANSITION_REQUIRED_RECORD_MISSING",
            source_id,
            format!("required host-transition record is missing: {}", path.display()),
            "run a fresh preflight or repair the exact missing persisted record; host transition remains refused",
        )
    })
}

fn read_optional_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    source_id: &'static str,
) -> Result<Option<T>, ErrorData> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(host_error(
                "HOST_TRANSITION_RECORD_READ_FAILED",
                source_id,
                format!("could not read {}: {error}", path.display()),
                "repair the exact file permissions/filesystem failure; host transition remains refused",
            ));
        }
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        host_error(
            "HOST_TRANSITION_RECORD_INVALID",
            source_id,
            format!("could not decode {}: {error}", path.display()),
            "inspect and repair the exact JSON record; host transition remains refused",
        )
    })
}

fn write_verified_json(
    path: &Path,
    value: &impl Serialize,
    source_id: &'static str,
) -> Result<(), ErrorData> {
    let parent = path.parent().ok_or_else(|| {
        host_error(
            "HOST_TRANSITION_RECORD_PARENT_MISSING",
            source_id,
            format!("host-transition record has no parent: {}", path.display()),
            "repair the internal state path; host transition remains refused",
        )
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        host_error(
            "HOST_TRANSITION_RECORD_DIRECTORY_CREATE_FAILED",
            source_id,
            format!("could not create {}: {error}", parent.display()),
            "repair %LOCALAPPDATA%\\synapse permissions/filesystem state; host transition remains refused",
        )
    })?;
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        host_error(
            "HOST_TRANSITION_RECORD_SERIALIZE_FAILED",
            source_id,
            format!("could not serialize {}: {error}", path.display()),
            "repair the record serialization failure; host transition remains refused",
        )
    })?;
    bytes.push(b'\n');
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("host-transition"),
        uuid::Uuid::new_v4().simple()
    ));
    let write_result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        commit_atomic(&temp, path)
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(host_error(
            "HOST_TRANSITION_RECORD_WRITE_FAILED",
            source_id,
            format!(
                "could not atomically persist host-transition record {}: {error}",
                path.display()
            ),
            "repair the exact filesystem/permissions failure and retry; host transition remains refused",
        ));
    }
    let readback = fs::read(path).map_err(|error| {
        host_error(
            "HOST_TRANSITION_RECORD_READBACK_FAILED",
            source_id,
            format!(
                "host-transition record {} was committed but separate readback failed: {error}",
                path.display()
            ),
            "repair the exact filesystem read failure and inspect the persisted record before retrying",
        )
    })?;
    if readback != bytes {
        return Err(host_error(
            "HOST_TRANSITION_RECORD_READBACK_MISMATCH",
            source_id,
            format!(
                "host-transition record {} bytes differ from the separate post-commit readback",
                path.display()
            ),
            "inspect the filesystem/write interposer; host transition remains refused",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn commit_atomic(temp: &Path, destination: &Path) -> io::Result<()> {
    let temp_wide = temp
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination_wide = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR(temp_wide.as_ptr()),
            PCWSTR(destination_wide.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(|error| io::Error::from_raw_os_error((error.code().0 as u32 & 0xffff) as i32))
    }
}

#[cfg(not(windows))]
fn commit_atomic(temp: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temp, destination)?;
    let parent = destination
        .parent()
        .ok_or_else(|| io::Error::other("host-transition destination has no parent"))?;
    fs::File::open(parent)?.sync_all()
}

fn state_root() -> Result<PathBuf, ErrorData> {
    let local_app_data = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
        host_error(
            "HOST_TRANSITION_LOCALAPPDATA_MISSING",
            "state_root",
            "LOCALAPPDATA is missing from the daemon environment".to_owned(),
            "repair the installed daemon environment and restart synapse-mcp; host transition remains refused",
        )
    })?;
    Ok(PathBuf::from(local_app_data)
        .join("synapse")
        .join("host-transitions"))
}

fn preflight_path(state_root: &Path, preflight_id: &str) -> PathBuf {
    state_root
        .join("preflights")
        .join(format!("{preflight_id}.json"))
}

fn file_readback(path: PathBuf) -> FileReadback {
    match fs::read(&path) {
        Ok(bytes) => FileReadback {
            path: path.display().to_string(),
            exists: true,
            len_bytes: Some(bytes.len() as u64),
            sha256: Some(sha256_label(&bytes)),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => FileReadback {
            path: path.display().to_string(),
            exists: false,
            len_bytes: None,
            sha256: None,
        },
        Err(error) => FileReadback {
            path: format!("{} [read_error={error}]", path.display()),
            exists: true,
            len_bytes: None,
            sha256: None,
        },
    }
}

fn host_transition_sot(state_root: &Path) -> String {
    format!(
        "kernel BootIdentifier + durable shell status files + {} + configured WSL lslocks/lease/checkpoint bytes + Windows System Event 1074",
        state_root.display()
    )
}

fn now_unix_ms() -> Result<u64, ErrorData> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|error| {
            host_error(
                "HOST_TRANSITION_CLOCK_BEFORE_EPOCH",
                "system_clock",
                format!("system clock is before Unix epoch: {error}"),
                "repair the host clock and retry; host transition remains refused",
            )
        })
}

fn bounded_stderr(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(4_096)])
        .trim()
        .to_owned()
}

fn blocked_error(
    record: &PreflightRecord,
    path: &Path,
    durable_jobs: &m4::ShellJobHostTransitionSnapshot,
    guards: &[SetupHostTransitionGuardReadback],
    message: &'static str,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message,
        Some(json!({
            "code": error_codes::TOOL_INTERNAL_ERROR,
            "detail_code": "HOST_TRANSITION_PROTECTED_WORK_LIVE",
            "source_of_truth": SETUP_SOT,
            "preflight_id": record.preflight_id,
            "preflight_path": path,
            "host_boot_id": record.host_boot_id,
            "safety_digest": record.safety_digest,
            "durable_jobs": durable_jobs,
            "guards": guards,
            "remediation": "wait for every named durable job to become terminal and every named lease to release, then run a fresh preflight; override is accepted only with exact job ids plus complete checkpoint digests",
        })),
    )
}

fn invalid_params(message: impl Into<String>, source_id: &'static str) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32602),
        message.into(),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "detail_code": "HOST_TRANSITION_PARAMS_INVALID",
            "source_of_truth": "MCP request parameters",
            "source_id": source_id,
        })),
    )
}

fn host_error(
    detail_code: &'static str,
    source_id: &'static str,
    message: String,
    remediation: &'static str,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message,
        Some(json!({
            "code": error_codes::TOOL_INTERNAL_ERROR,
            "detail_code": detail_code,
            "tool": "setup",
            "operation": "host_transition",
            "source_id": source_id,
            "source_of_truth": SETUP_SOT,
            "remediation": remediation,
        })),
    )
}
