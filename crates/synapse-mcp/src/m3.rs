pub(crate) mod a11y_events;
pub mod activity_recorder;
pub mod approvals;
pub mod armed_routines;
pub mod audio;
pub mod audit_export;
pub mod audit_retention;
pub mod demo_recording;
pub mod episodes;
pub(crate) mod grounding;
pub mod hygiene;
pub mod intent;
pub mod intent_events;
pub mod interaction_cadence;
pub mod local_models;
pub mod permissions;
pub mod plan;
pub mod plan_execution;
pub mod profile;
pub mod profile_authoring;
pub mod profile_quality;
pub mod profile_registry;
pub mod reflex;
pub mod replay;
pub mod routine_miner_job;
pub mod routines;
pub mod storage;
pub mod subscribe;
pub mod suggestions;
pub mod timeline;
pub mod timeline_control;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use synapse_action::ActionHandle;
use synapse_audio::{AudioConfig, AudioError, AudioRuntime, DEFAULT_RING_SECONDS};
use synapse_core::{Event, SCHEMA_VERSION, SessionId, StoredProfileHistoryEntry};
use synapse_profiles::{ProfileError, ProfileRuntime, bundled_profiles_dir};
use synapse_reflex::{
    DEFAULT_MAX_SUBSCRIPTIONS_NONZERO, EventBus, ReflexError, ReflexRuntime,
    install_action_combo_scheduler,
};
use synapse_storage::{
    Db, GcTask, GcTaskReadback, PressureProbeReadback, PressureTask, StorageBackendKind,
    StorageError,
};
use tokio_util::sync::CancellationToken;

use self::a11y_events::A11yEventBridge;
use self::activity_recorder::{ActivityRecorder, RecorderConfig};
use self::demo_recording::DemoRecordControl;
use self::permissions::{PermissionGrants, configured_grants_from_parts};
use self::timeline_control::RecorderControl;
use crate::http::sse::SseState;

const DB_ENV: &str = "SYNAPSE_DB";
const STORAGE_BACKEND_ENV: &str = "SYNAPSE_STORAGE_BACKEND";
const PROFILE_DIR_ENV: &str = "SYNAPSE_PROFILE_DIR";
const REFLEX_DISABLED_ENV: &str = "SYNAPSE_REFLEX_DISABLED";
const REFLEX_FORCE_DEGRADED_ENV: &str = "SYNAPSE_REFLEX_FORCE_DEGRADED";
const STORAGE_PRESSURE_FREE_BYTES_SAMPLE_ENV: &str = "SYNAPSE_STORAGE_PRESSURE_FREE_BYTES_SAMPLE";
const CALYX_VAULT_ENV: &str = "SYNAPSE_CALYX_VAULT";
const CALYX_VAULT_DIR_ENV: &str = "SYNAPSE_CALYX_VAULT_DIR";
const CALYX_CONFIG_ENV: &str = "SYNAPSE_CALYX_CONFIG";
const ENABLE_AUDIO_ENV: &str = "SYNAPSE_ENABLE_AUDIO";
const ALLOW_UNKNOWN_PROFILE_ENV: &str = "SYNAPSE_ALLOW_UNKNOWN_PROFILE";
const ALLOWED_PERMISSIONS_ENV: &str = "SYNAPSE_MCP_ALLOWED_PERMISSIONS";
const BIND_ENV: &str = "SYNAPSE_BIND";
const BEARER_TOKEN_ENV: &str = "SYNAPSE_BEARER_TOKEN";
const AUDIO_LOOPBACK_ENV: &str = "SYNAPSE_AUDIO_LOOPBACK";
const MAX_SUBSCRIPTIONS_ENV: &str = "SYNAPSE_MAX_SUBSCRIPTIONS";
const DEFAULT_BIND: &str = "127.0.0.1:7700";
pub type SharedM3State = Arc<Mutex<M3State>>;

#[derive(Clone, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "config mirrors independent operator startup gates and fail-closed toggles"
)]
pub struct M3ServiceConfig {
    pub db_path: Option<PathBuf>,
    pub storage_backend: StorageBackendKind,
    pub profile_dir: Option<PathBuf>,
    pub reflex_disabled: bool,
    pub bind: String,
    pub bearer_token: Option<String>,
    pub max_subscriptions: NonZeroUsize,
    pub enable_audio: bool,
    pub allow_unknown_profile: bool,
    pub allowed_permissions: Option<String>,
    pub reflex_force_degraded: bool,
    pub storage_pressure_free_bytes_sample: Option<u64>,
    pub calyx_vault: bool,
    pub calyx_vault_dir: Option<PathBuf>,
    pub calyx_config_path: Option<PathBuf>,
}

impl M3ServiceConfig {
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "constructor mirrors parsed CLI/config fields without hiding startup gates"
    )]
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "constructor mirrors parsed CLI/config booleans exactly"
    )]
    pub fn from_cli_parts(
        db_path: Option<PathBuf>,
        storage_backend: StorageBackendKind,
        profile_dir: Option<PathBuf>,
        reflex_disabled: bool,
        bind: String,
        max_subscriptions: NonZeroUsize,
        enable_audio: bool,
        allow_unknown_profile: bool,
        allowed_permissions: Option<String>,
        reflex_force_degraded: bool,
        storage_pressure_free_bytes_sample: Option<u64>,
    ) -> Self {
        Self {
            db_path,
            storage_backend,
            profile_dir,
            reflex_disabled,
            bind,
            bearer_token: std::env::var(BEARER_TOKEN_ENV).ok(),
            max_subscriptions,
            enable_audio,
            allow_unknown_profile,
            allowed_permissions,
            reflex_force_degraded,
            storage_pressure_free_bytes_sample,
            calyx_vault: true,
            calyx_vault_dir: None,
            calyx_config_path: None,
        }
    }

    pub fn from_env() -> Result<Self> {
        let storage_backend_raw = std::env::var(STORAGE_BACKEND_ENV).ok();
        let reflex_disabled_raw = std::env::var(REFLEX_DISABLED_ENV).ok();
        let reflex_force_degraded_raw = std::env::var(REFLEX_FORCE_DEGRADED_ENV).ok();
        let storage_pressure_free_bytes_sample_raw =
            std::env::var(STORAGE_PRESSURE_FREE_BYTES_SAMPLE_ENV).ok();
        let enable_audio_raw = std::env::var(ENABLE_AUDIO_ENV).ok();
        let allow_unknown_profile_raw = std::env::var(ALLOW_UNKNOWN_PROFILE_ENV).ok();
        let calyx_vault_raw = std::env::var(CALYX_VAULT_ENV).ok();
        let max_subscriptions_raw = std::env::var(MAX_SUBSCRIPTIONS_ENV).ok();
        Ok(Self {
            db_path: std::env::var_os(DB_ENV).map(PathBuf::from),
            storage_backend: storage_backend_raw.as_deref().map_or_else(
                || Ok(StorageBackendKind::default()),
                StorageBackendKind::parse_config,
            )?,
            profile_dir: std::env::var_os(PROFILE_DIR_ENV).map(PathBuf::from),
            reflex_disabled: parse_bool_env(REFLEX_DISABLED_ENV, reflex_disabled_raw.as_deref())?,
            enable_audio: parse_bool_env(ENABLE_AUDIO_ENV, enable_audio_raw.as_deref())?,
            // Permissive by default: unset means unknown/unprofiled foreground
            // apps are actionable. Explicit `0`/`false` restores fail-closed.
            allow_unknown_profile: allow_unknown_profile_raw
                .as_deref()
                .map_or(Ok(true), |raw| {
                    parse_bool_env(ALLOW_UNKNOWN_PROFILE_ENV, Some(raw))
                })?,
            reflex_force_degraded: parse_bool_env(
                REFLEX_FORCE_DEGRADED_ENV,
                reflex_force_degraded_raw.as_deref(),
            )?,
            storage_pressure_free_bytes_sample: parse_optional_u64_env(
                STORAGE_PRESSURE_FREE_BYTES_SAMPLE_ENV,
                storage_pressure_free_bytes_sample_raw.as_deref(),
            )?,
            calyx_vault: calyx_vault_raw
                .as_deref()
                .map_or(Ok(true), |raw| parse_bool_env(CALYX_VAULT_ENV, Some(raw)))?,
            calyx_vault_dir: std::env::var_os(CALYX_VAULT_DIR_ENV).map(PathBuf::from),
            calyx_config_path: std::env::var_os(CALYX_CONFIG_ENV).map(PathBuf::from),
            bind: std::env::var(BIND_ENV).unwrap_or_else(|_| DEFAULT_BIND.to_owned()),
            bearer_token: std::env::var(BEARER_TOKEN_ENV).ok(),
            max_subscriptions: parse_max_subscriptions_env(max_subscriptions_raw.as_deref())?,
            allowed_permissions: std::env::var(ALLOWED_PERMISSIONS_ENV).ok(),
        })
    }
}

/// Bounded maximum lifetime of a runtime reality-write opt-in overlay (#1559).
/// The overlay is a break-glass escalation, so it self-expires quickly; a fresh
/// grant is required after this window regardless of daemon uptime.
pub const REALITY_WRITE_GRANT_MAX_TTL: Duration = Duration::from_mins(15);

/// In-memory, scoped, reversible, audited reality-write opt-in overlay (#1559).
///
/// After #1539 made the stock M3 grant read-only, `full_capability` no longer
/// yields reality-write capability and there was no usable opt-in. This overlay
/// is that opt-in: when active (non-expired) it satisfies EXACTLY the
/// reality-write permission set (`READ_STORAGE`/`WRITE_STORAGE`/`READ_EVENTS`)
/// and is consulted only by explicit reality-write enforcement paths — it never
/// widens authority for unrelated storage registries, secrets, or generic M3
/// permissions. Expiry is authoritative from the monotonic `Instant` (mirrors
/// the input-lease module), immune to wall-clock changes; `granted_at`/`expires_at`
/// are wall-clock copies for human readback.
#[derive(Clone, Debug)]
pub struct RealityWriteGrant {
    granted_by: String,
    reason: String,
    granted_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    granted_at_instant: Instant,
    ttl: Duration,
}

/// Serializable, `Instant`-free copy of an active [`RealityWriteGrant`] for tool
/// responses and status readback.
#[derive(Clone, Debug)]
pub struct RealityWriteGrantSnapshot {
    pub granted_by: String,
    pub reason: String,
    pub granted_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub remaining_ms: u64,
}

impl RealityWriteGrant {
    #[must_use]
    pub fn new(granted_by: String, reason: String) -> Self {
        let ttl = REALITY_WRITE_GRANT_MAX_TTL;
        let granted_at = Utc::now();
        let expires_at = granted_at
            + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::seconds(900));
        Self {
            granted_by,
            reason,
            granted_at,
            expires_at,
            granted_at_instant: Instant::now(),
            ttl,
        }
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.granted_at_instant.elapsed() < self.ttl
    }

    #[must_use]
    pub fn remaining_ms(&self) -> u64 {
        self.ttl
            .checked_sub(self.granted_at_instant.elapsed())
            .map_or(0, |remaining| {
                u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX)
            })
    }

    #[must_use]
    pub fn snapshot(&self) -> RealityWriteGrantSnapshot {
        RealityWriteGrantSnapshot {
            granted_by: self.granted_by.clone(),
            reason: self.reason.clone(),
            granted_at: self.granted_at,
            expires_at: self.expires_at,
            remaining_ms: self.remaining_ms(),
        }
    }
}

#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "runtime state keeps independent startup gates explicit for health readback"
)]
pub struct M3State {
    pub db_path: Option<PathBuf>,
    pub storage_backend: StorageBackendKind,
    pub profile_dir: Option<PathBuf>,
    pub reflex_disabled: bool,
    pub bind: String,
    pub bearer_token: Option<String>,
    pub shutdown_cancel: CancellationToken,
    pub shutdown_reason: &'static str,
    pub connection_closed_cancel: Option<CancellationToken>,
    pub permission_grants: PermissionGrants,
    /// Where the startup `permission_grants` came from (#1559): an explicit
    /// operator config (`SYNAPSE_MCP_ALLOWED_PERMISSIONS` env / `--allowed-permissions`
    /// CLI) or the fail-closed read-only default. Reported in status and denial
    /// remediation so `full_capability` is never mistaken for a write grant.
    pub permission_grants_source: &'static str,
    /// Runtime reality-write opt-in overlay (#1559). `None` restores the
    /// fail-closed default; `Some` (while non-expired) satisfies the reality-write
    /// permission set for reality tools only. See [`RealityWriteGrant`].
    pub reality_write_grant: Option<RealityWriteGrant>,
    pub enable_audio: bool,
    pub allow_unknown_profile: bool,
    pub reflex_force_degraded: bool,
    pub storage_pressure_free_bytes_sample: Option<u64>,
    pub calyx_vault_enabled: bool,
    pub calyx_vault_config: Option<synapse_calyx::SynapseCalyxConfig>,
    pub calyx_vault_status: synapse_calyx::SynapseCalyxVaultStatus,
    /// Shared storage handle. Opened once (eagerly at daemon startup, or lazily
    /// on first reflex use) and reused by the reflex runtime so there is never
    /// a second open of the same path within this process.
    pub db: Option<Arc<Db>>,
    pub storage_gc_task: Option<GcTask>,
    /// Checkpoint-only maintenance task (2026-07-23 cold-start fix): bounds
    /// the crash-stranded WAL tail to ~30s of commits.
    pub storage_checkpoint_task: Option<GcTask>,
    pub storage_pressure_task: Option<PressureTask>,
    pub storage_last_error: Option<String>,
    pub storage_maintenance_unsupported: Option<String>,
    pub reflex_last_error: Option<String>,
    pub profile_last_error: Option<String>,
    pub audio_last_error: Option<String>,
    pub profile_runtime: Option<Arc<ProfileRuntime>>,
    pub sse_state: SseState,
    pub reflex_runtime: Option<Arc<Mutex<ReflexRuntime>>>,
    pub file_jsonl_tail_watchers: BTreeMap<String, reflex::FileJsonlTailWatcher>,
    pub a11y_event_bridge: Option<A11yEventBridge>,
    pub activity_recorder: Option<Arc<ActivityRecorder>>,
    pub recorder_control: Option<Arc<RecorderControl>>,
    pub demo_record_control: Option<Arc<DemoRecordControl>>,
    pub audio_runtime: Option<Arc<AudioRuntime>>,
    pub audit_session: Option<AuditSessionState>,
    pub mcp_audit_sessions: BTreeMap<SessionId, AuditSessionState>,
    /// Shared intent state machine (#855), advanced by both the periodic intent
    /// detector and the `intent_detect_tick` tool so transitions have one
    /// source of truth. Cheap `Arc` clone hands a handle to either driver.
    pub intent_tracker: intent_events::SharedIntentTracker,
}

#[derive(Clone, Debug, Default)]
pub struct StorageMaintenanceReadback {
    pub maintenance_supported: bool,
    pub unsupported_reason: Option<String>,
    pub gc_task_running: bool,
    pub checkpoint_task_running: bool,
    pub pressure_task_running: bool,
    pub gc_task: GcTaskReadback,
    pub checkpoint_task: GcTaskReadback,
    pub pressure_probe: PressureProbeReadback,
}

#[derive(Clone, Debug)]
pub struct StorageMaintenanceShutdownReadback {
    pub reason: &'static str,
    pub gc_owner_present: bool,
    pub checkpoint_owner_present: bool,
    pub pressure_owner_present: bool,
    pub owners_quiescent: bool,
    pub failures: Vec<String>,
}

impl StorageMaintenanceShutdownReadback {
    #[must_use]
    pub const fn owners_quiescent(&self) -> bool {
        self.owners_quiescent
    }

    #[must_use]
    pub fn owner_count(&self) -> usize {
        usize::from(self.gc_owner_present)
            + usize::from(self.checkpoint_owner_present)
            + usize::from(self.pressure_owner_present)
    }

    pub fn verdict(&self) -> Result<()> {
        let owner_count = self.owner_count();
        if !self.owners_quiescent || !self.failures.is_empty() {
            bail!(
                "storage maintenance shutdown failed before vault close: reason={} owner_count={} owners_quiescent={} failures={:?}",
                self.reason,
                owner_count,
                self.owners_quiescent,
                self.failures
            );
        }
        Ok(())
    }
}

/// Stops every periodic storage producer and joins its exact task owner before
/// the Calyx vault may be flushed or closed.
///
/// The task handles are removed atomically under the M3 state lock, then joined
/// without retaining that lock. Each task's shutdown method waits through any
/// already-started `spawn_blocking` closure, which Tokio cannot abort.
pub async fn shutdown_storage_maintenance_tasks(
    state: &SharedM3State,
    reason: &'static str,
) -> StorageMaintenanceShutdownReadback {
    let (gc_task, checkpoint_task, pressure_task) = match state.lock() {
        Ok(mut state) => (
            state.storage_gc_task.take(),
            state.storage_checkpoint_task.take(),
            state.storage_pressure_task.take(),
        ),
        Err(poisoned) => {
            let detail = format!(
                "M3 service state lock poisoned while taking storage-maintenance owners: {poisoned}"
            );
            drop(poisoned);
            let readback = StorageMaintenanceShutdownReadback {
                reason,
                gc_owner_present: false,
                checkpoint_owner_present: false,
                pressure_owner_present: false,
                owners_quiescent: false,
                failures: vec![detail],
            };
            tracing::error!(
                code = "STORAGE_MAINTENANCE_SHUTDOWN_UNPROVEN",
                readback = ?readback,
                "could not take periodic storage-maintenance owners before vault close"
            );
            return readback;
        }
    };

    let gc_owner_present = gc_task.is_some();
    let checkpoint_owner_present = checkpoint_task.is_some();
    let pressure_owner_present = pressure_task.is_some();
    let gc_shutdown = async move {
        match gc_task {
            Some(task) => task.shutdown("garbage_collection").await.err(),
            None => None,
        }
    };
    let checkpoint_shutdown = async move {
        match checkpoint_task {
            Some(task) => task.shutdown("checkpoint").await.err(),
            None => None,
        }
    };
    let pressure_shutdown = async move {
        match pressure_task {
            Some(task) => task.shutdown().await.err(),
            None => None,
        }
    };
    let (gc_error, checkpoint_error, pressure_error) =
        tokio::join!(gc_shutdown, checkpoint_shutdown, pressure_shutdown);
    let failures = [
        gc_error.map(|error| format!("garbage_collection: {error}")),
        checkpoint_error.map(|error| format!("checkpoint: {error}")),
        pressure_error.map(|error| format!("disk_pressure: {error}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    // Every present JoinHandle was awaited above. A JoinError is a failed task,
    // but is still terminal and therefore cannot mutate the vault after close.
    let readback = StorageMaintenanceShutdownReadback {
        reason,
        gc_owner_present,
        checkpoint_owner_present,
        pressure_owner_present,
        owners_quiescent: true,
        failures,
    };
    tracing::info!(
        code = "STORAGE_MAINTENANCE_SHUTDOWN_READBACK",
        owner_count = readback.owner_count(),
        readback = ?readback,
        "readback=periodic_storage_task_owners edge=shutdown before_vault_close"
    );
    readback
}

#[derive(Clone, Debug)]
pub struct AuditSessionState {
    pub session_id: SessionId,
    pub started_at: DateTime<Utc>,
    pub profile_history: Vec<StoredProfileHistoryEntry>,
}

pub fn shared_m3_state_from_env() -> Result<SharedM3State> {
    Ok(Arc::new(Mutex::new(M3State::from_config(
        M3ServiceConfig::from_env()?,
    )?)))
}

pub fn shared_m3_state_from_config_with_shutdown_reason_and_sse_state(
    config: M3ServiceConfig,
    shutdown_cancel: CancellationToken,
    shutdown_reason: &'static str,
    connection_closed_cancel: Option<CancellationToken>,
    sse_state: SseState,
) -> Result<SharedM3State> {
    Ok(Arc::new(Mutex::new(
        M3State::from_config_with_shutdown_reason_and_sse_state(
            config,
            shutdown_cancel,
            shutdown_reason,
            connection_closed_cancel,
            sse_state,
        )?,
    )))
}

pub(crate) fn record_calyx_vault_status_event(
    status: &synapse_calyx::SynapseCalyxVaultStatus,
    event_status: &'static str,
) -> Result<u64> {
    let detail = serde_json::to_value(status).context("serialize Calyx vault status event")?;
    crate::daemon_lifecycle::record_context_event(crate::daemon_lifecycle::ContextEvent {
        event_kind: "calyx_vault_lifecycle",
        tool: "calyx_vault",
        status: event_status,
        mcp_session_id: None,
        foreground: None,
        foreground_read_error: None,
        detail,
    })
    .context("record Calyx vault lifecycle status event")
}

pub(crate) fn record_calyx_vault_close_event(
    readback: &synapse_calyx::SynapseCalyxVaultCloseReadback,
    event_status: &'static str,
) -> Result<u64> {
    let detail = serde_json::to_value(readback).context("serialize Calyx vault close event")?;
    crate::daemon_lifecycle::record_context_event(crate::daemon_lifecycle::ContextEvent {
        event_kind: "calyx_vault_lifecycle",
        tool: "calyx_vault",
        status: event_status,
        mcp_session_id: None,
        foreground: None,
        foreground_read_error: None,
        detail,
    })
    .context("record Calyx vault lifecycle close event")
}

impl M3State {
    pub fn from_config(config: M3ServiceConfig) -> Result<Self> {
        let sse_state = SseState::with_max_subscriptions(config.max_subscriptions);
        Self::from_config_with_shutdown_reason_and_sse_state(
            config,
            CancellationToken::new(),
            "shutdown",
            None,
            sse_state,
        )
    }

    pub fn from_config_with_shutdown_reason_and_sse_state(
        config: M3ServiceConfig,
        shutdown_cancel: CancellationToken,
        shutdown_reason: &'static str,
        connection_closed_cancel: Option<CancellationToken>,
        sse_state: SseState,
    ) -> Result<Self> {
        Self::from_parts_with_sse_state(
            config.db_path,
            config.storage_backend,
            config.profile_dir,
            Some(bool_env_value(config.reflex_disabled)),
            config.bearer_token,
            Some(config.bind),
            Some(bool_env_value(config.enable_audio)),
            Some(bool_env_value(config.allow_unknown_profile)),
            config.allowed_permissions.as_deref(),
            Some(bool_env_value(config.reflex_force_degraded)),
            config.storage_pressure_free_bytes_sample,
            config.calyx_vault,
            config.calyx_vault_dir,
            config.calyx_config_path,
            shutdown_cancel,
            shutdown_reason,
            connection_closed_cancel,
            sse_state,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_parts_with_sse_state(
        db_path: Option<PathBuf>,
        storage_backend: StorageBackendKind,
        profile_dir: Option<PathBuf>,
        reflex_disabled: Option<&str>,
        bearer_token: Option<String>,
        bind: Option<String>,
        enable_audio: Option<&str>,
        allow_unknown_profile: Option<&str>,
        allowed_permissions: Option<&str>,
        reflex_force_degraded: Option<&str>,
        storage_pressure_free_bytes_sample: Option<u64>,
        calyx_vault_enabled: bool,
        calyx_vault_dir: Option<PathBuf>,
        calyx_config_path: Option<PathBuf>,
        shutdown_cancel: CancellationToken,
        shutdown_reason: &'static str,
        connection_closed_cancel: Option<CancellationToken>,
        sse_state: SseState,
    ) -> Result<Self> {
        let enable_audio = parse_bool_env(ENABLE_AUDIO_ENV, enable_audio)?;
        let allow_unknown_profile =
            parse_bool_env(ALLOW_UNKNOWN_PROFILE_ENV, allow_unknown_profile)?;
        let reflex_force_degraded =
            parse_bool_env(REFLEX_FORCE_DEGRADED_ENV, reflex_force_degraded)?;
        let permission_grants = configured_grants_from_parts(allowed_permissions, enable_audio)?;
        if !calyx_vault_enabled {
            anyhow::bail!(
                "SYNAPSE_CALYX_STORAGE_VAULT_REQUIRED: the sole supported storage backend is Calyx, so disabling its vault would disable storage; remove --no-calyx-vault/SYNAPSE_CALYX_VAULT=false"
            );
        }
        let storage_vault_dir = db_path.clone().unwrap_or_else(default_db_path);
        if let Some(requested) = calyx_vault_dir.as_ref() {
            let requested = std::path::absolute(requested)?;
            let storage = std::path::absolute(&storage_vault_dir)?;
            if requested != storage {
                anyhow::bail!(
                    "SYNAPSE_CALYX_VAULT_PATH_CONFLICT: configured Calyx vault {} differs from the sole storage vault {}; set SYNAPSE_CALYX_VAULT_DIR/--calyx-vault-dir to the DB path or remove it",
                    requested.display(),
                    storage.display()
                );
            }
        }
        let calyx_vault_config = Some(
            synapse_calyx::SynapseCalyxConfig::from_optional_vault_dir_and_config_path(
                Some(storage_vault_dir),
                calyx_config_path,
            )?,
        );
        let calyx_vault_status = calyx_vault_config
            .as_ref()
            .map_or_else(synapse_calyx::SynapseCalyxVaultStatus::disabled, |config| {
                synapse_calyx::SynapseCalyxVaultStatus::not_opened(Some(config))
            });
        // #1559: record the config source so status/denial remediation can state
        // exactly how to opt into reality-write, and never imply that an absent
        // WRITE_STORAGE is present.
        let permission_grants_source = if allowed_permissions.is_some() {
            "explicit operator config (SYNAPSE_MCP_ALLOWED_PERMISSIONS env / --allowed-permissions CLI)"
        } else {
            "fail-closed read-only default (#1539; no SYNAPSE_MCP_ALLOWED_PERMISSIONS / --allowed-permissions set)"
        };
        Ok(Self {
            db_path,
            storage_backend,
            profile_dir,
            reflex_disabled: parse_bool_env(REFLEX_DISABLED_ENV, reflex_disabled)?,
            bind: bind
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| DEFAULT_BIND.to_owned()),
            bearer_token: bearer_token.filter(|value| !value.is_empty()),
            shutdown_cancel,
            shutdown_reason,
            connection_closed_cancel,
            permission_grants,
            permission_grants_source,
            reality_write_grant: None,
            enable_audio,
            allow_unknown_profile,
            reflex_force_degraded,
            storage_pressure_free_bytes_sample,
            calyx_vault_enabled,
            calyx_vault_config,
            calyx_vault_status,
            db: None,
            storage_gc_task: None,
            storage_checkpoint_task: None,
            storage_pressure_task: None,
            storage_last_error: None,
            storage_maintenance_unsupported: None,
            reflex_last_error: None,
            profile_last_error: None,
            audio_last_error: None,
            profile_runtime: None,
            sse_state,
            reflex_runtime: None,
            file_jsonl_tail_watchers: BTreeMap::new(),
            a11y_event_bridge: None,
            activity_recorder: None,
            recorder_control: None,
            demo_record_control: None,
            audio_runtime: None,
            audit_session: None,
            mcp_audit_sessions: BTreeMap::new(),
            intent_tracker: intent_events::new_shared_tracker(),
        })
    }

    #[must_use]
    pub const fn scaffold_ready(&self) -> bool {
        !self.bind.is_empty()
    }

    /// Hands out a cheap `Arc` clone of the shared intent tracker (#855) so the
    /// periodic detector and the `intent_detect_tick` tool advance one state
    /// machine.
    #[must_use]
    pub fn intent_tracker(&self) -> intent_events::SharedIntentTracker {
        Arc::clone(&self.intent_tracker)
    }

    /// True when a non-expired runtime reality-write overlay is active (#1559).
    /// Clears the overlay in place when it has lapsed so an expired grant leaves
    /// NO residue and the fail-closed default is restored automatically.
    pub fn reality_write_grant_active(&mut self) -> bool {
        match self.reality_write_grant.as_ref() {
            Some(grant) if grant.is_active() => true,
            Some(_expired) => {
                self.reality_write_grant = None;
                false
            }
            None => false,
        }
    }

    /// Installs (or replaces) the runtime reality-write overlay with a fresh
    /// bounded TTL (#1559). Returns a serializable snapshot for audit/readback.
    pub fn grant_reality_write(
        &mut self,
        granted_by: String,
        reason: String,
    ) -> RealityWriteGrantSnapshot {
        let grant = RealityWriteGrant::new(granted_by, reason);
        let snapshot = grant.snapshot();
        self.reality_write_grant = Some(grant);
        snapshot
    }

    /// Clears any runtime reality-write overlay (#1559), restoring the
    /// fail-closed default with no residue. Returns the snapshot of the overlay
    /// that was active at revoke time, or `None` when nothing active was present.
    pub fn revoke_reality_write(&mut self) -> Option<RealityWriteGrantSnapshot> {
        self.reality_write_grant
            .take()
            .filter(RealityWriteGrant::is_active)
            .map(|grant| grant.snapshot())
    }

    /// Read-only snapshot of an active overlay for status readback (#1559).
    /// Does not mutate; an expired overlay reports as `None`.
    #[must_use]
    pub fn reality_write_grant_snapshot(&self) -> Option<RealityWriteGrantSnapshot> {
        self.reality_write_grant
            .as_ref()
            .filter(|grant| grant.is_active())
            .map(RealityWriteGrant::snapshot)
    }

    pub fn ensure_profile_runtime(
        &mut self,
    ) -> std::result::Result<Arc<ProfileRuntime>, ProfileError> {
        if let Some(runtime) = &self.profile_runtime {
            return Ok(Arc::clone(runtime));
        }

        let profile_dir = self
            .profile_dir
            .clone()
            .unwrap_or_else(bundled_profiles_dir);
        let runtime = match ProfileRuntime::spawn(profile_dir) {
            Ok(runtime) => Arc::new(runtime),
            Err(error) => {
                self.profile_last_error = Some(error.to_string());
                return Err(error);
            }
        };
        self.profile_last_error = None;
        self.profile_runtime = Some(Arc::clone(&runtime));
        Ok(runtime)
    }

    /// Open the shared storage handle once and cache it; subsequent callers
    /// (including the reflex runtime) reuse the same handle, so the path is
    /// never opened twice within this process. Called eagerly at daemon startup
    /// for fail-fast lock/schema detection, and lazily otherwise.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`synapse_storage::StorageError`] (lock held by
    /// another process, schema mismatch, etc.) and records it in
    /// `storage_last_error`.
    pub fn ensure_storage(
        &mut self,
    ) -> std::result::Result<Arc<Db>, synapse_storage::StorageError> {
        if let Some(db) = &self.db {
            return Ok(Arc::clone(db));
        }
        let db_path = self.db_path.clone().unwrap_or_else(default_db_path);
        let Some(calyx_config) = self.calyx_vault_config.clone() else {
            let error = StorageError::OpenFailed {
                path: db_path,
                detail: "SYNAPSE_CALYX_CONFIG_MISSING: the sole Calyx storage backend has no configuration resolved from CLI/environment; restart with a valid storage/Calyx configuration".to_owned(),
            };
            self.storage_last_error = Some(error.to_string());
            return Err(error);
        };
        match Db::open_with_resolved_calyx_config(
            &db_path,
            SCHEMA_VERSION,
            self.storage_backend,
            calyx_config,
        ) {
            Ok(db) => {
                let db = Arc::new(db);
                self.db = Some(Arc::clone(&db));
                self.storage_last_error = None;
                Ok(db)
            }
            Err(error) => {
                self.storage_last_error = Some(error.to_string());
                Err(error)
            }
        }
    }

    pub fn ensure_storage_maintenance_tasks(
        &mut self,
    ) -> std::result::Result<(), synapse_storage::StorageError> {
        let db = self.ensure_storage()?;
        // Hot-path boundary (#1686). The admitted maintenance pass also lowers
        // the guard-threshold hot set into the frozen artifact the reflex tick
        // consumes, and it reads the vault through this handle. Register before
        // the first pass can run so no pass is skipped for want of a source.
        synapse_storage::maintenance::register_lowering_source(&db);
        self.storage_maintenance_unsupported = None;
        if self.storage_pressure_task.is_none() {
            let pressure_result = if let Some(free_bytes) = self.storage_pressure_free_bytes_sample
            {
                db.run_pressure_check_with_free_bytes_sample(free_bytes)
            } else {
                db.run_pressure_check_once()
            };
            if let Err(error) = pressure_result {
                self.storage_last_error = Some(format!("storage pressure startup probe: {error}"));
                return Err(error);
            }
            match db.spawn_pressure_task() {
                Ok(task) => {
                    self.storage_pressure_task = Some(task);
                }
                Err(error) => {
                    self.storage_last_error = Some(format!("storage pressure task start: {error}"));
                    return Err(error);
                }
            }
        }
        if self.storage_gc_task.is_none() {
            match db.spawn_gc_task() {
                Ok(task) => {
                    self.storage_gc_task = Some(task);
                }
                Err(error) => {
                    self.storage_last_error = Some(format!("storage GC task start: {error}"));
                    return Err(error);
                }
            }
        }
        if self.storage_checkpoint_task.is_none() {
            match db.spawn_checkpoint_task() {
                Ok(task) => {
                    self.storage_checkpoint_task = Some(task);
                }
                Err(error) => {
                    self.storage_last_error =
                        Some(format!("storage checkpoint task start: {error}"));
                    return Err(error);
                }
            }
        }
        self.storage_last_error = None;
        Ok(())
    }

    pub fn ensure_calyx_vault(
        &mut self,
    ) -> std::result::Result<synapse_calyx::SynapseCalyxVaultStatus, synapse_calyx::SynapseCalyxError>
    {
        if !self.calyx_vault_enabled {
            self.calyx_vault_status = synapse_calyx::SynapseCalyxVaultStatus::disabled();
            return Ok(self.calyx_vault_status.clone());
        }
        let Some(config) = self.calyx_vault_config.as_ref() else {
            let error = synapse_calyx::SynapseCalyxError::new(
                "SYNAPSE_CALYX_CONFIG_MISSING",
                "Calyx vault is enabled but no resolved vault configuration is present",
                "restart with the Calyx storage DB path configured as the sole vault path",
            );
            self.calyx_vault_status =
                synapse_calyx::SynapseCalyxVaultStatus::error(None, "error", &error);
            return Err(error);
        };
        let config = config.clone();
        let status = self
            .ensure_storage()
            .map_err(|error| storage_owned_calyx_error("open sole Calyx storage vault", &error))?
            .calyx_vault_status()
            .map_err(|error| {
                storage_owned_calyx_error("read sole Calyx storage vault status", &error)
            });
        match status {
            Ok(status) => {
                self.calyx_vault_status = status.clone();
                Ok(status)
            }
            Err(error) => {
                self.calyx_vault_status =
                    synapse_calyx::SynapseCalyxVaultStatus::error(Some(&config), "error", &error);
                Err(error)
            }
        }
    }

    pub fn close_calyx_vault_for_shutdown(
        &mut self,
        reason: &'static str,
        expected_open: bool,
    ) -> std::result::Result<
        synapse_calyx::SynapseCalyxVaultCloseReadback,
        synapse_calyx::SynapseCalyxError,
    > {
        if !self.calyx_vault_enabled {
            self.calyx_vault_status = synapse_calyx::SynapseCalyxVaultStatus::disabled();
            return Ok(synapse_calyx::SynapseCalyxVaultCloseReadback::disabled(
                reason,
            ));
        }
        let Some(db) = self.db.as_ref() else {
            if expected_open {
                let error = synapse_calyx::SynapseCalyxError::new(
                    "SYNAPSE_CALYX_VAULT_MISSING_AT_SHUTDOWN",
                    "the sole Calyx storage vault was expected during daemon shutdown but storage was not open",
                    "inspect startup logs; the daemon must open the storage-owned vault before serving and retain the Db owner until shutdown",
                );
                self.calyx_vault_status = synapse_calyx::SynapseCalyxVaultStatus::error(
                    self.calyx_vault_config.as_ref(),
                    "error",
                    &error,
                );
                return Err(error);
            }
            return Ok(synapse_calyx::SynapseCalyxVaultCloseReadback::not_open(
                reason,
                self.calyx_vault_config.as_ref(),
            ));
        };
        match db.close_calyx_vault(reason).map_err(|error| {
            storage_owned_calyx_error("flush and close sole Calyx storage vault", &error)
        }) {
            Ok(readback) => {
                self.calyx_vault_status = synapse_calyx::SynapseCalyxVaultStatus {
                    enabled: true,
                    phase: "closed".to_owned(),
                    open: false,
                    latest_seq: readback.latest_seq,
                    vault_dir: readback.vault_dir.clone(),
                    lock_path: readback.lock_path.clone(),
                    pid_path: readback.pid_path.clone(),
                    ..self.calyx_vault_status.clone()
                };
                Ok(readback)
            }
            Err(error) => {
                self.calyx_vault_status = synapse_calyx::SynapseCalyxVaultStatus::error(
                    self.calyx_vault_config.as_ref(),
                    "error",
                    &error,
                );
                Err(error)
            }
        }
    }

    #[must_use]
    pub fn calyx_vault_status(&self) -> synapse_calyx::SynapseCalyxVaultStatus {
        self.db
            .as_ref()
            .and_then(|db| db.calyx_vault_status().ok())
            .unwrap_or_else(|| self.calyx_vault_status.clone())
    }

    #[must_use]
    pub fn storage_maintenance_readback(&self) -> StorageMaintenanceReadback {
        let gc_task = self
            .storage_gc_task
            .as_ref()
            .map(GcTask::readback)
            .unwrap_or_default();
        let checkpoint_task = self
            .storage_checkpoint_task
            .as_ref()
            .map(GcTask::readback)
            .unwrap_or_default();
        let pressure_task_running = self
            .storage_pressure_task
            .as_ref()
            .is_some_and(PressureTask::running);
        let pressure_probe = self
            .db
            .as_ref()
            .map_or_else(PressureProbeReadback::default, |db| {
                db.pressure_probe_readback()
                    .unwrap_or_else(|error| PressureProbeReadback {
                        last_error: Some(error.to_string()),
                        ..PressureProbeReadback::default()
                    })
            });
        StorageMaintenanceReadback {
            maintenance_supported: self.storage_maintenance_unsupported.is_none(),
            unsupported_reason: self.storage_maintenance_unsupported.clone(),
            gc_task_running: gc_task.running,
            checkpoint_task_running: checkpoint_task.running,
            pressure_task_running,
            gc_task,
            checkpoint_task,
            pressure_probe,
        }
    }

    pub fn ensure_reflex_runtime(
        &mut self,
        action_handle: ActionHandle,
        event_bus: EventBus,
    ) -> Result<Arc<Mutex<ReflexRuntime>>> {
        if let Some(runtime) = &self.reflex_runtime {
            return Ok(Arc::clone(runtime));
        }
        if self.reflex_disabled {
            bail!(ReflexError::DisabledByOperator {
                detail: "SYNAPSE_REFLEX_DISABLED is set".to_owned(),
            });
        }

        self.ensure_storage_maintenance_tasks()?;
        let db = self.ensure_storage()?;
        let scheduler_config = synapse_reflex::SchedulerConfig {
            force_degraded: self.reflex_force_degraded,
            ..synapse_reflex::SchedulerConfig::default()
        };
        let runtime = match ReflexRuntime::spawn_with_config(
            db,
            action_handle,
            event_bus,
            scheduler_config,
        ) {
            Ok(runtime) => Arc::new(Mutex::new(runtime)),
            Err(error) => {
                self.reflex_last_error = Some(error.to_string());
                return Err(error.into());
            }
        };
        install_action_combo_scheduler(&runtime)
            .context("install action combo scheduler bridge for reflex runtime")?;
        self.reflex_last_error = None;
        self.reflex_runtime = Some(Arc::clone(&runtime));
        Ok(runtime)
    }

    pub fn ensure_a11y_event_bridge(
        &mut self,
        event_bus: EventBus,
    ) -> synapse_a11y::A11yResult<()> {
        if self.a11y_event_bridge.is_some() {
            return Ok(());
        }
        let bridge = A11yEventBridge::start(event_bus, self.activity_recorder.clone())?;
        self.a11y_event_bridge = Some(bridge);
        Ok(())
    }

    /// Starts the always-on operator-activity recorder (#837) and the WinEvent
    /// bridge that feeds it. Called eagerly at daemon startup, before any tool
    /// call can lazily start a recorder-less bridge.
    ///
    /// # Errors
    ///
    /// Returns an error when storage cannot open, when the recorder cannot
    /// probe its idle source or write its `session_start` row, when the
    /// WinEvent bridge cannot start, or when a bridge already exists without
    /// the recorder attached (a startup-ordering bug that would silently drop
    /// every timeline row).
    pub fn ensure_activity_recorder(&mut self, event_bus: EventBus) -> Result<()> {
        if self.activity_recorder.is_some() {
            return Ok(());
        }
        if self.a11y_event_bridge.is_some() {
            bail!(
                "a11y event bridge started before the activity recorder; \
                 timeline rows would be silently dropped (startup-ordering bug)"
            );
        }
        let db = self
            .ensure_storage()
            .context("open storage for the activity recorder")?;
        let config = RecorderConfig::from_env()?;
        let control = self
            .ensure_recorder_control()
            .context("hydrate recorder control state for the activity recorder")?;
        let demo_control = self
            .ensure_demo_record_control()
            .context("hydrate demo record control state for the activity recorder")?;
        // WinEvent subscription is the last fallible activation step. Complete
        // it before spawning recorder tasks so a hook-start failure cannot
        // force synchronous code to drop-and-detach live Tokio owners.
        let prepared_bridge = A11yEventBridge::prepare()
            .context("prepare WinEvent bridge for the activity recorder")?;
        let recorder = Arc::new(ActivityRecorder::spawn(
            db,
            config,
            control,
            demo_control,
            event_bus.clone(),
        )?);
        let bridge = A11yEventBridge::start_prepared(
            prepared_bridge,
            event_bus,
            Some(Arc::clone(&recorder)),
        );
        self.activity_recorder = Some(Arc::clone(&recorder));
        self.a11y_event_bridge = Some(bridge);
        Ok(())
    }

    /// Hydrates (once) the shared pause/exclusion gate for the timeline
    /// recorder and the `timeline_pause`/`timeline_resume`/
    /// `timeline_exclusions` tools (#843). The persisted `CF_KV` control row
    /// is the durable source of truth; this is its in-process cache.
    ///
    /// # Errors
    ///
    /// Returns an error when storage cannot open, when the env exclusion
    /// baseline is malformed, or when the persisted control row is corrupt.
    pub fn ensure_recorder_control(&mut self) -> Result<Arc<RecorderControl>> {
        if let Some(control) = &self.recorder_control {
            return Ok(Arc::clone(control));
        }
        let db = self
            .ensure_storage()
            .context("open storage for timeline recorder control state")?;
        let control = Arc::new(RecorderControl::hydrate(&db)?);
        self.recorder_control = Some(Arc::clone(&control));
        Ok(control)
    }

    /// Hydrates the shared explicitly-armed demo recorder state (#844). The
    /// persisted `CF_KV` row is the source of truth for whether a demo is
    /// active; the activity recorder receives this handle so UIA events are
    /// captured by the existing WinEvent bridge without installing another
    /// process-global hook.
    ///
    /// # Errors
    ///
    /// Returns an error when storage cannot open or the persisted demo control
    /// row is corrupt.
    pub fn ensure_demo_record_control(&mut self) -> Result<Arc<DemoRecordControl>> {
        if let Some(control) = &self.demo_record_control {
            return Ok(Arc::clone(control));
        }
        let db = self
            .ensure_storage()
            .context("open storage for demo record control state")?;
        let control = Arc::new(DemoRecordControl::hydrate(db)?);
        self.demo_record_control = Some(Arc::clone(&control));
        Ok(control)
    }

    pub fn ensure_audio_runtime(&mut self) -> std::result::Result<Arc<AudioRuntime>, AudioError> {
        if let Some(runtime) = &self.audio_runtime {
            return Ok(Arc::clone(runtime));
        }

        let start_loopback = audio_loopback_enabled()?;
        let config = AudioConfig {
            ring_seconds: DEFAULT_RING_SECONDS,
            start_loopback,
            detectors_enabled: start_loopback,
            stt_model_path: None,
        };
        let runtime = match AudioRuntime::spawn_with_event_sink(
            config,
            audio_event_sink(self.sse_state.event_bus()),
        ) {
            Ok(runtime) => Arc::new(runtime),
            Err(error) => {
                self.audio_last_error = Some(error.to_string());
                return Err(error);
            }
        };
        self.audio_last_error = None;
        self.audio_runtime = Some(Arc::clone(&runtime));
        Ok(runtime)
    }
}

fn audio_event_sink(event_bus: EventBus) -> synapse_audio::AudioEventSink {
    Arc::new(move |event: Event| {
        let seq = event.seq;
        let kind = event.kind.clone();
        let source = event.source;
        let report = event_bus.publish(event);
        tracing::debug!(
            code = "AUDIO_EVENT_PUBLISHED",
            seq,
            kind = %kind,
            source = ?source,
            matched = report.matched,
            queued = report.queued,
            dropped = report.dropped,
            "audio detector event published to event bus"
        );
    })
}

#[must_use]
pub fn default_db_path() -> PathBuf {
    local_synapse_dir().join("db")
}

#[must_use]
pub fn default_daemon_db_path() -> PathBuf {
    local_synapse_dir().join("db-daemon")
}

fn local_synapse_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("synapse")
}
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct M3ToolStub {
    pub name: &'static str,
}

impl M3ToolStub {
    #[must_use]
    pub const fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[must_use]
pub const fn m3_tool_stubs() -> [M3ToolStub; 58] {
    [
        subscribe::subscribe(),
        subscribe::subscribe_cancel(),
        reflex::reflex_register(),
        reflex::reflex_cancel(),
        reflex::reflex_list(),
        reflex::reflex_history(),
        profile::profile_list(),
        profile::profile_activate(),
        profile_authoring::profile_authoring_generate(),
        profile_authoring::profile_authoring_list(),
        profile_authoring::profile_authoring_inspect(),
        profile_authoring::profile_authoring_decide(),
        profile_authoring::profile_authoring_export(),
        profile_authoring::routine_automate(),
        profile_quality::profile_quality_refresh(),
        profile_registry::profile_registry_query(),
        profile_registry::profile_registry_install(),
        profile_registry::profile_registry_disable(),
        profile_registry::profile_registry_export(),
        profile_registry::profile_registry_import(),
        profile_registry::profile_registry_rollback(),
        profile_registry::audit_intelligence_query(),
        audit_export::audit_export_bundle(),
        replay::replay_record(),
        demo_recording::demo_record_start(),
        demo_recording::demo_record_stop(),
        audio::audio_tail(),
        audio::audio_transcribe(),
        approvals::approval_request(),
        approvals::approval_list(),
        approvals::approval_decide(),
        hygiene::hygiene_scan_text(),
        hygiene::hygiene_scan_storage(),
        hygiene::hygiene_flags(),
        local_models::local_model_register(),
        local_models::local_model_list(),
        local_models::local_model_update(),
        local_models::local_model_remove(),
        local_models::local_model_probe(),
        storage::storage_inspect(),
        storage::storage_put_probe_rows(),
        storage::storage_gc_once(),
        storage::storage_pressure_sample(),
        timeline::timeline_search(),
        timeline::timeline_purge(),
        episodes::episode_segment(),
        episodes::episode_list(),
        episodes::episode_get(),
        routines::routine_mine(),
        routines::routine_list(),
        routines::routine_inspect(),
        routines::routine_update(),
        armed_routines::armed_routine_tick(),
        timeline_control::timeline_pause(),
        timeline_control::timeline_resume(),
        timeline_control::timeline_exclusions(),
        intent::intent_current(),
        intent_events::intent_detect_tick(),
    ]
}

fn storage_owned_calyx_error(
    operation: &str,
    error: &impl std::fmt::Display,
) -> synapse_calyx::SynapseCalyxError {
    synapse_calyx::SynapseCalyxError::new(
        "SYNAPSE_CALYX_STORAGE_VAULT_FAILED",
        format!("{operation}: {error}"),
        "inspect the storage-owned Calyx vault lock/PID, manifest, WAL, and the preceding structured storage error; repair that one vault and retry",
    )
}

fn parse_bool_env(name: &str, value: Option<&str>) -> Result<bool> {
    match value {
        None | Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(value) if value.eq_ignore_ascii_case("true") => Ok(true),
        Some(value) if value.eq_ignore_ascii_case("false") => Ok(false),
        Some(value) => bail!("{name} must be one of 1, 0, true, or false; got {value:?}"),
    }
}

fn parse_max_subscriptions_env(value: Option<&str>) -> Result<NonZeroUsize> {
    let Some(value) = value else {
        return Ok(DEFAULT_MAX_SUBSCRIPTIONS_NONZERO);
    };
    parse_max_subscriptions_value(MAX_SUBSCRIPTIONS_ENV, value)
}

fn parse_optional_u64_env(name: &str, value: Option<&str>) -> Result<Option<u64>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    value
        .parse::<u64>()
        .map(Some)
        .with_context(|| format!("{name} must be an unsigned integer; got {value:?}"))
}

fn parse_max_subscriptions_value(name: &str, value: &str) -> Result<NonZeroUsize> {
    let trimmed = value.trim();
    let Ok(parsed) = trimmed.parse::<usize>() else {
        bail!("{name} must be a positive integer; got {value:?}");
    };
    let Some(nonzero) = NonZeroUsize::new(parsed) else {
        bail!("{name} must be >= 1; got {value:?}");
    };
    Ok(nonzero)
}

const fn bool_env_value(value: bool) -> &'static str {
    if value { "1" } else { "0" }
}

fn audio_loopback_enabled() -> std::result::Result<bool, AudioError> {
    match std::env::var(AUDIO_LOOPBACK_ENV).ok().as_deref() {
        None | Some("1") => Ok(true),
        Some("0") => Ok(false),
        Some(value) if value.eq_ignore_ascii_case("true") => Ok(true),
        Some(value) if value.eq_ignore_ascii_case("false") => Ok(false),
        Some(value) => Err(AudioError::LoopbackInitFailed {
            detail: format!(
                "{AUDIO_LOOPBACK_ENV} must be one of 1, 0, true, or false; got {value:?}"
            ),
        }),
    }
}
