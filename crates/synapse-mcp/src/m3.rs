mod a11y_events;
pub mod activity_recorder;
pub mod approvals;
pub mod audio;
pub mod audit_export;
pub mod audit_retention;
pub mod episodes;
pub mod hygiene;
pub mod intent;
pub mod interaction_cadence;
pub mod local_models;
pub mod permissions;
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
#[cfg(test)]
mod tests;
pub mod timeline;
pub mod timeline_control;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use synapse_action::ActionHandle;
use synapse_audio::{AudioConfig, AudioError, AudioRuntime, DEFAULT_RING_SECONDS};
use synapse_core::{Event, SCHEMA_VERSION, SessionId, StoredProfileHistoryEntry};
use synapse_profiles::{ProfileError, ProfileRuntime, bundled_profiles_dir};
use synapse_reflex::{
    DEFAULT_MAX_SUBSCRIPTIONS_NONZERO, EventBus, ReflexError, ReflexRuntime,
    install_action_combo_scheduler,
};
use synapse_storage::Db;
use tokio_util::sync::CancellationToken;

use self::a11y_events::A11yEventBridge;
use self::activity_recorder::{ActivityRecorder, RecorderConfig};
use self::permissions::{PermissionGrants, configured_grants_from_parts};
use self::timeline_control::RecorderControl;
use crate::http::sse::SseState;

const DB_ENV: &str = "SYNAPSE_DB";
const PROFILE_DIR_ENV: &str = "SYNAPSE_PROFILE_DIR";
const REFLEX_DISABLED_ENV: &str = "SYNAPSE_REFLEX_DISABLED";
const REFLEX_FORCE_DEGRADED_ENV: &str = "SYNAPSE_REFLEX_FORCE_DEGRADED";
const STORAGE_PRESSURE_FREE_BYTES_SAMPLE_ENV: &str = "SYNAPSE_STORAGE_PRESSURE_FREE_BYTES_SAMPLE";
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
        }
    }

    pub fn from_env() -> Result<Self> {
        let reflex_disabled_raw = std::env::var(REFLEX_DISABLED_ENV).ok();
        let reflex_force_degraded_raw = std::env::var(REFLEX_FORCE_DEGRADED_ENV).ok();
        let storage_pressure_free_bytes_sample_raw =
            std::env::var(STORAGE_PRESSURE_FREE_BYTES_SAMPLE_ENV).ok();
        let enable_audio_raw = std::env::var(ENABLE_AUDIO_ENV).ok();
        let allow_unknown_profile_raw = std::env::var(ALLOW_UNKNOWN_PROFILE_ENV).ok();
        let max_subscriptions_raw = std::env::var(MAX_SUBSCRIPTIONS_ENV).ok();
        Ok(Self {
            db_path: std::env::var_os(DB_ENV).map(PathBuf::from),
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
            bind: std::env::var(BIND_ENV).unwrap_or_else(|_| DEFAULT_BIND.to_owned()),
            bearer_token: std::env::var(BEARER_TOKEN_ENV).ok(),
            max_subscriptions: parse_max_subscriptions_env(max_subscriptions_raw.as_deref())?,
            allowed_permissions: std::env::var(ALLOWED_PERMISSIONS_ENV).ok(),
        })
    }
}

#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "runtime state keeps independent startup gates explicit for health readback"
)]
pub struct M3State {
    pub db_path: Option<PathBuf>,
    pub profile_dir: Option<PathBuf>,
    pub reflex_disabled: bool,
    pub bind: String,
    pub bearer_token: Option<String>,
    pub shutdown_cancel: CancellationToken,
    pub shutdown_reason: &'static str,
    pub connection_closed_cancel: Option<CancellationToken>,
    pub permission_grants: PermissionGrants,
    pub enable_audio: bool,
    pub allow_unknown_profile: bool,
    pub reflex_force_degraded: bool,
    pub storage_pressure_free_bytes_sample: Option<u64>,
    /// Shared RocksDB handle. Opened once (eagerly at daemon startup, or lazily
    /// on first reflex use) and reused by the reflex runtime so there is never
    /// a second open of the same path within this process.
    pub db: Option<Arc<Db>>,
    pub storage_last_error: Option<String>,
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
    pub audio_runtime: Option<Arc<AudioRuntime>>,
    pub audit_session: Option<AuditSessionState>,
    pub mcp_audit_sessions: BTreeMap<SessionId, AuditSessionState>,
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
            config.profile_dir,
            Some(bool_env_value(config.reflex_disabled)),
            config.bearer_token,
            Some(config.bind),
            Some(bool_env_value(config.enable_audio)),
            Some(bool_env_value(config.allow_unknown_profile)),
            config.allowed_permissions.as_deref(),
            Some(bool_env_value(config.reflex_force_degraded)),
            config.storage_pressure_free_bytes_sample,
            shutdown_cancel,
            shutdown_reason,
            connection_closed_cancel,
            sse_state,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_parts_with_sse_state(
        db_path: Option<PathBuf>,
        profile_dir: Option<PathBuf>,
        reflex_disabled: Option<&str>,
        bearer_token: Option<String>,
        bind: Option<String>,
        enable_audio: Option<&str>,
        allow_unknown_profile: Option<&str>,
        allowed_permissions: Option<&str>,
        reflex_force_degraded: Option<&str>,
        storage_pressure_free_bytes_sample: Option<u64>,
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
        Ok(Self {
            db_path,
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
            enable_audio,
            allow_unknown_profile,
            reflex_force_degraded,
            storage_pressure_free_bytes_sample,
            db: None,
            storage_last_error: None,
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
            audio_runtime: None,
            audit_session: None,
            mcp_audit_sessions: BTreeMap::new(),
        })
    }

    #[must_use]
    pub const fn scaffold_ready(&self) -> bool {
        !self.bind.is_empty()
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

    /// Open the shared RocksDB handle once and cache it; subsequent callers
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
        match Db::open(&db_path, SCHEMA_VERSION) {
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

        let db = self.ensure_storage()?;
        if let Some(free_bytes) = self.storage_pressure_free_bytes_sample
            && let Err(error) = db.run_pressure_check_with_free_bytes_sample(free_bytes)
        {
            self.storage_last_error = Some(error.to_string());
            return Err(error.into());
        }
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
        let recorder = Arc::new(ActivityRecorder::spawn(
            db,
            config,
            control,
            event_bus.clone(),
        )?);
        self.activity_recorder = Some(Arc::clone(&recorder));
        if let Err(error) = self.ensure_a11y_event_bridge(event_bus) {
            self.activity_recorder = None;
            return Err(error).context("start WinEvent bridge for the activity recorder");
        }
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
    std::env::var_os("LOCALAPPDATA")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("synapse")
        .join("db")
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
pub const fn m3_tool_stubs() -> [M3ToolStub; 53] {
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
        timeline_control::timeline_pause(),
        timeline_control::timeline_resume(),
        timeline_control::timeline_exclusions(),
        intent::intent_current(),
    ]
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
