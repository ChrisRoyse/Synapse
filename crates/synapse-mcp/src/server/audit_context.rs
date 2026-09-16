use std::{
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Utc};
use rmcp::ErrorData;
use serde_json::{Value, json};
use synapse_core::{
    EventSource, ForegroundContext, Observation, Profile, ProfileBackends, ProfileId,
    SCHEMA_VERSION, SessionId, StoredAppContext, StoredAuditContext, StoredBackendPolicy,
    StoredEvent, StoredObservation, StoredProfileHistoryEntry, StoredSession, error_codes,
    new_session_id,
};

use super::SynapseService;
use crate::{m1::mcp_error, m3::AuditSessionState};

static EVENT_AUDIT_SEQ: AtomicU32 = AtomicU32::new(0);
static OBSERVATION_AUDIT_SEQ: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Debug)]
struct ProfileAuditInfo {
    profile: Profile,
    schema_version: u32,
}

#[derive(Clone, Debug)]
struct ObservationAuditSessionSnapshot {
    session_id: SessionId,
    started_at: DateTime<Utc>,
    profile_history: Vec<StoredProfileHistoryEntry>,
}

impl SynapseService {
    pub(super) fn persist_profile_activation_success(
        &self,
        profile_id: &ProfileId,
        changed: bool,
    ) -> Result<(), ErrorData> {
        let session_id = self.ensure_audit_session_started()?;
        let profile_info = self.profile_audit_info(profile_id)?;
        let audit_context = self.audit_context_for_profile(&profile_info, Some(session_id.clone()));
        let activated_at = Utc::now();
        let mut state = self.m3_state.lock().map_err(|_err| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned while updating audit session",
            )
        })?;
        let (session_started_at, profile_history) = {
            let audit_session = state.audit_session.as_mut().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "audit session disappeared while persisting profile activation",
                )
            })?;
            let should_append = changed
                || audit_session
                    .profile_history
                    .last()
                    .is_none_or(|last| last.profile_id != *profile_id);
            if should_append {
                audit_session
                    .profile_history
                    .push(StoredProfileHistoryEntry {
                        profile_id: profile_id.clone(),
                        profile_version: Some(profile_info.profile.version.clone()),
                        profile_schema_version: Some(profile_info.schema_version),
                        activated_at,
                        reason: "profile_activate".to_owned(),
                    });
            }
            (
                audit_session.started_at,
                audit_session.profile_history.clone(),
            )
        };
        drop(state);
        let session = StoredSession {
            schema_version: SCHEMA_VERSION,
            session_id: session_id.clone(),
            started_at: session_started_at,
            ended_at: None,
            transport: "mcp".to_owned(),
            client: Some("synapse-mcp".to_owned()),
            mode: profile_info.profile.mode,
            active_profile: Some(profile_id.clone()),
            audit_context: Some(audit_context.clone()),
            profile_history,
            redacted: false,
            redactions: Vec::new(),
        };

        self.write_session_row(&session)?;
        self.write_event_row(&StoredEvent {
            schema_version: SCHEMA_VERSION,
            event_id: format!("profile-activation-{}", event_id_suffix()),
            ts_ns: now_ts_ns(),
            session_id: Some(session_id),
            audit_context: Some(audit_context.clone()),
            source: EventSource::System,
            kind: "profile.activated".to_owned(),
            data: json!({
                "profile_id": profile_id,
                "profile_version": profile_info.profile.version,
                "profile_schema_version": profile_info.schema_version,
                "changed": changed,
                "reason": "profile_activate",
            }),
            window_id: None,
            element_id: None,
            redacted: false,
            redactions: Vec::new(),
        })?;
        self.set_reflex_runtime_audit_context(Some(audit_context))?;
        Ok(())
    }

    pub(super) fn persist_profile_activation_denied(
        &self,
        profile_id: &ProfileId,
        error: &ErrorData,
    ) {
        let audit_context = self.audit_context_for_denied_profile(profile_id);
        let event = StoredEvent {
            schema_version: SCHEMA_VERSION,
            event_id: format!("profile-activation-denied-{}", event_id_suffix()),
            ts_ns: now_ts_ns(),
            session_id: audit_context.session_id.clone(),
            audit_context: Some(audit_context),
            source: EventSource::System,
            kind: "profile.activation_denied".to_owned(),
            data: json!({
                "profile_id": profile_id,
                "error_code": error_data_code(error),
                "message": error.message.to_string(),
                "data": error.data.clone(),
            }),
            window_id: None,
            element_id: None,
            redacted: false,
            redactions: Vec::new(),
        };
        if let Err(write_error) = self.write_event_row(&event) {
            tracing::warn!(
                code = "PROFILE_ACTIVATION_AUDIT_WRITE_FAILED",
                profile_id = %profile_id,
                audit_error = %write_error,
                "profile activation denial audit write failed"
            );
        }
    }

    pub(super) fn persist_observation(
        &self,
        observation: &Observation,
        reason: &'static str,
    ) -> Result<(), ErrorData> {
        self.persist_observation_for_mcp_session(observation, reason, None)
    }

    pub(super) fn persist_observation_for_mcp_session(
        &self,
        observation: &Observation,
        reason: &'static str,
        mcp_session_id: Option<&str>,
    ) -> Result<(), ErrorData> {
        let (ts_ns, seq) = next_observation_key_parts();
        let observation_id = format!("observe-{ts_ns:020}-{seq:010}");
        let profile_info = match observation.foreground.profile_id.as_deref() {
            Some(profile_id) => Some(self.profile_audit_info(profile_id)?),
            None => None,
        };
        let audit_session = self.observation_audit_session_snapshot(
            mcp_session_id,
            profile_info.as_ref(),
            observation.at,
            reason,
        )?;
        let session_id = audit_session.session_id.clone();
        let audit_context = profile_info.as_ref().map_or_else(
            || StoredAuditContext {
                session_id: Some(session_id.clone()),
                profile_id: None,
                profile_version: None,
                profile_schema_version: None,
                backend_policy: None,
                app_context: Some(app_context_for_foreground(None, &observation.foreground)),
            },
            |info| {
                audit_context_for_profile_and_foreground(
                    info,
                    Some(session_id.clone()),
                    &observation.foreground,
                )
            },
        );

        let session = StoredSession {
            schema_version: SCHEMA_VERSION,
            session_id: session_id.clone(),
            started_at: audit_session.started_at,
            ended_at: None,
            transport: "mcp".to_owned(),
            client: Some("synapse-mcp".to_owned()),
            mode: observation.mode,
            active_profile: observation.foreground.profile_id.clone(),
            audit_context: Some(audit_context.clone()),
            profile_history: audit_session.profile_history,
            redacted: false,
            redactions: Vec::new(),
        };
        self.write_session_row(&session)?;

        let stored = stored_observation(observation, &observation_id, ts_ns, &session_id, reason);
        self.write_observation_row(ts_ns, seq, &stored)?;
        let source_key = observation_key(ts_ns, seq);
        {
            let runtime = self.reflex_runtime()?;
            let runtime = runtime.lock().map_err(|_error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "reflex runtime lock poisoned while scanning observation hygiene",
                )
            })?;
            crate::m3::hygiene::scan_and_persist_observation(&runtime, &stored, &source_key)?;
        }

        let event = observation_recorded_event(
            observation,
            &observation_id,
            ts_ns,
            session_id.clone(),
            audit_context,
            reason,
        );
        self.write_event_row(&event)?;
        tracing::info!(
            code = "OBSERVATION_AUDIT_RECORDED",
            ts_ns,
            seq,
            observation_id,
            session_id = %session_id,
            mcp_session_id = mcp_session_id.unwrap_or("<global>"),
            "observation audit row written"
        );
        Ok(())
    }

    pub(super) fn current_action_audit_context(&self) -> Result<StoredAuditContext, ErrorData> {
        let session_id = self.current_audit_session_id()?;
        let profile_id = self
            .profile_runtime()
            .ok()
            .and_then(|runtime| runtime.active_profile_id().ok().flatten());
        match profile_id {
            Some(profile_id) => {
                let info = self.profile_audit_info(&profile_id)?;
                Ok(self.audit_context_for_profile(&info, session_id))
            }
            None => Ok(StoredAuditContext {
                session_id,
                profile_id: None,
                profile_version: None,
                profile_schema_version: None,
                backend_policy: None,
                app_context: self.current_app_context(None),
            }),
        }
    }

    pub(super) fn refresh_reflex_audit_context(&self) -> Result<(), ErrorData> {
        let context = self.current_action_audit_context()?;
        self.set_reflex_runtime_audit_context(Some(context))
    }

    pub(super) fn current_audit_foreground(&self) -> Result<ForegroundContext, ErrorData> {
        {
            let state = self.m1_state()?;
            if state.force_observe_internal {
                return Err(mcp_error(
                    error_codes::OBSERVE_INTERNAL,
                    "forced observe internal error",
                ));
            }
            if state.force_no_perception {
                return Err(mcp_error(
                    error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE,
                    "no perception source is available",
                ));
            }
            if state.force_no_foreground {
                // Reproduce the real GetForegroundWindow-returned-null condition
                // deterministically (#1061), matching synapse_a11y's NoForeground
                // error exactly so the action gate sees an identical signal.
                return Err(mcp_error(
                    error_codes::A11Y_NO_FOREGROUND,
                    "GetForegroundWindow returned null",
                ));
            }
            if let Some(input) = &state.synthetic {
                return Ok(input.foreground.clone());
            }
        }
        synapse_a11y::current_foreground_context()
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    fn ensure_audit_session_started(&self) -> Result<SessionId, ErrorData> {
        let mut state = self.m3_state.lock().map_err(|_err| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned while starting audit session",
            )
        })?;
        if state.audit_session.is_none() {
            state.audit_session = Some(AuditSessionState {
                session_id: new_session_id(),
                started_at: Utc::now(),
                profile_history: Vec::new(),
            });
        }
        state
            .audit_session
            .as_ref()
            .map(|session| session.session_id.clone())
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "audit session did not initialize",
                )
            })
    }

    fn observation_audit_session_snapshot(
        &self,
        mcp_session_id: Option<&str>,
        profile_info: Option<&ProfileAuditInfo>,
        observed_at: DateTime<Utc>,
        reason: &'static str,
    ) -> Result<ObservationAuditSessionSnapshot, ErrorData> {
        let mut state = self.m3_state.lock().map_err(|_err| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned while updating observation audit session",
            )
        })?;
        let audit_session = match mcp_session_id {
            Some(session_id) => {
                let session_id = session_id.trim();
                if session_id.is_empty() {
                    return Err(mcp_error(
                        error_codes::HTTP_SESSION_INVALID,
                        "Mcp-Session-Id for observation audit is empty",
                    ));
                }
                state
                    .mcp_audit_sessions
                    .entry(session_id.to_owned())
                    .or_insert_with(|| AuditSessionState {
                        session_id: session_id.to_owned(),
                        started_at: Utc::now(),
                        profile_history: Vec::new(),
                    })
            }
            None => {
                if state.audit_session.is_none() {
                    state.audit_session = Some(AuditSessionState {
                        session_id: new_session_id(),
                        started_at: Utc::now(),
                        profile_history: Vec::new(),
                    });
                }
                state.audit_session.as_mut().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "audit session did not initialize",
                    )
                })?
            }
        };
        if let Some(info) = profile_info {
            let should_append = audit_session
                .profile_history
                .last()
                .is_none_or(|last| last.profile_id != info.profile.id);
            if should_append {
                audit_session
                    .profile_history
                    .push(StoredProfileHistoryEntry {
                        profile_id: info.profile.id.clone(),
                        profile_version: Some(info.profile.version.clone()),
                        profile_schema_version: Some(info.schema_version),
                        activated_at: observed_at,
                        reason: reason.to_owned(),
                    });
            }
        }
        Ok(ObservationAuditSessionSnapshot {
            session_id: audit_session.session_id.clone(),
            started_at: audit_session.started_at,
            profile_history: audit_session.profile_history.clone(),
        })
    }

    fn current_audit_session_id(&self) -> Result<Option<SessionId>, ErrorData> {
        self.m3_state
            .lock()
            .map(|state| {
                state
                    .audit_session
                    .as_ref()
                    .map(|session| session.session_id.clone())
            })
            .map_err(|_err| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "M3 service state lock poisoned while reading audit session",
                )
            })
    }

    fn profile_audit_info(&self, profile_id: &str) -> Result<ProfileAuditInfo, ErrorData> {
        let runtime = self.profile_runtime()?;
        let profile = runtime
            .profile(profile_id)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::PROFILE_NOT_FOUND,
                    format!("profile {profile_id} was not found"),
                )
            })?;
        let schema_version = runtime
            .list(true)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?
            .into_iter()
            .find(|status| status.id == profile_id)
            .map_or(SCHEMA_VERSION, |status| status.schema_version);
        Ok(ProfileAuditInfo {
            profile,
            schema_version,
        })
    }

    fn audit_context_for_profile(
        &self,
        info: &ProfileAuditInfo,
        session_id: Option<SessionId>,
    ) -> StoredAuditContext {
        let app_context = self
            .current_audit_foreground()
            .ok()
            .map(|foreground| app_context_for_foreground(Some(&info.profile), &foreground))
            .or_else(|| Some(app_context_from_metadata(Some(&info.profile))));
        StoredAuditContext {
            session_id,
            profile_id: Some(info.profile.id.clone()),
            profile_version: Some(info.profile.version.clone()),
            profile_schema_version: Some(info.schema_version),
            backend_policy: Some(backend_policy(info.profile.backends)),
            app_context,
        }
    }

    fn audit_context_for_denied_profile(&self, profile_id: &ProfileId) -> StoredAuditContext {
        StoredAuditContext {
            session_id: self.current_audit_session_id().ok().flatten(),
            profile_id: Some(profile_id.clone()),
            profile_version: None,
            profile_schema_version: None,
            backend_policy: None,
            app_context: self.current_app_context(None),
        }
    }

    fn current_app_context(&self, profile: Option<&Profile>) -> Option<StoredAppContext> {
        self.current_audit_foreground()
            .ok()
            .map(|foreground| app_context_for_foreground(profile, &foreground))
            .or_else(|| profile.map(|profile| app_context_from_metadata(Some(profile))))
    }

    fn write_session_row(&self, session: &StoredSession) -> Result<(), ErrorData> {
        let encoded = synapse_storage::encode_json(session).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("session audit row encode failed: {error}"),
            )
        })?;
        let runtime = self.reflex_runtime()?;
        let runtime = runtime.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "reflex runtime lock poisoned while writing session audit",
            )
        })?;
        runtime
            .storage_put_session_rows(vec![(session_key(&session.session_id), encoded)])
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    fn write_event_row(&self, event: &StoredEvent) -> Result<(), ErrorData> {
        let encoded = synapse_storage::encode_json(event).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("event audit row encode failed: {error}"),
            )
        })?;
        let runtime = self.reflex_runtime()?;
        let runtime = runtime.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "reflex runtime lock poisoned while writing event audit",
            )
        })?;
        runtime
            .storage_put_event_rows(vec![(event_key(event.ts_ns), encoded)])
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    fn write_observation_row(
        &self,
        ts_ns: u64,
        seq: u32,
        observation: &StoredObservation,
    ) -> Result<(), ErrorData> {
        let encoded = synapse_storage::encode_json(observation).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("observation audit row encode failed: {error}"),
            )
        })?;
        let runtime = self.reflex_runtime()?;
        let runtime = runtime.lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "reflex runtime lock poisoned while writing observation audit",
            )
        })?;
        runtime
            .storage_put_observation_rows(vec![(observation_key(ts_ns, seq), encoded)])
            .map_err(|error| mcp_error(error.code(), error.to_string()))
    }

    fn set_reflex_runtime_audit_context(
        &self,
        context: Option<StoredAuditContext>,
    ) -> Result<(), ErrorData> {
        let runtime = self.reflex_runtime()?;
        runtime
            .lock()
            .map_err(|_error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "reflex runtime lock poisoned while setting audit context",
                )
            })?
            .set_audit_context(context);
        Ok(())
    }
}

fn app_context_for_foreground(
    profile: Option<&Profile>,
    foreground: &synapse_core::ForegroundContext,
) -> StoredAppContext {
    let mut context = app_context_from_metadata(profile);
    context.process_name = Some(foreground.process_name.clone());
    context.process_path = Some(foreground.process_path.clone());
    context.window_title = Some(foreground.window_title.clone());
    context
}

fn audit_context_for_profile_and_foreground(
    info: &ProfileAuditInfo,
    session_id: Option<SessionId>,
    foreground: &synapse_core::ForegroundContext,
) -> StoredAuditContext {
    StoredAuditContext {
        session_id,
        profile_id: Some(info.profile.id.clone()),
        profile_version: Some(info.profile.version.clone()),
        profile_schema_version: Some(info.schema_version),
        backend_policy: Some(backend_policy(info.profile.backends)),
        app_context: Some(app_context_for_foreground(Some(&info.profile), foreground)),
    }
}

fn stored_observation(
    observation: &Observation,
    observation_id: &str,
    ts_ns: u64,
    session_id: &SessionId,
    reason: &'static str,
) -> StoredObservation {
    StoredObservation {
        schema_version: SCHEMA_VERSION,
        observation_id: observation_id.to_owned(),
        ts_ns,
        session_id: Some(session_id.clone()),
        mode: observation.mode,
        foreground: observation.foreground.clone(),
        focused: observation.focused.clone(),
        elements: observation.elements.clone(),
        entities: observation.entities.clone(),
        hud: observation.hud.clone(),
        audio: observation.audio.clone(),
        recent_events: observation.recent_events.clone(),
        clipboard_summary: observation.clipboard_summary.clone(),
        fs_recent: observation.fs_recent.clone(),
        diagnostics: observation.diagnostics.clone(),
        reason: reason.to_owned(),
        redacted: false,
        redactions: Vec::new(),
    }
}

fn observation_recorded_event(
    observation: &Observation,
    observation_id: &str,
    ts_ns: u64,
    session_id: SessionId,
    audit_context: StoredAuditContext,
    reason: &'static str,
) -> StoredEvent {
    StoredEvent {
        schema_version: SCHEMA_VERSION,
        event_id: format!("observation-recorded-{}", event_id_suffix()),
        ts_ns,
        session_id: Some(session_id),
        audit_context: Some(audit_context),
        source: EventSource::Perception,
        kind: "perception.observed".to_owned(),
        data: json!({
            "observation_id": observation_id,
            "reason": reason,
            "profile_id": observation.foreground.profile_id,
            "process_name": observation.foreground.process_name,
            "hud_field_count": observation.hud.by_name.len(),
            "hud_error_count": observation.hud.errors.len(),
            "hud_fields": observation.hud.by_name.keys().cloned().collect::<Vec<_>>(),
            "hud_error_fields": observation.hud.errors.keys().cloned().collect::<Vec<_>>(),
            "entity_count": observation.entities.len(),
            "element_count": observation.elements.len(),
            "capture_status": observation.diagnostics.capture_status,
            "detection_status": observation.diagnostics.detection_status,
            "a11y_status": observation.diagnostics.a11y_status,
        }),
        window_id: Some(observation.foreground.hwnd),
        element_id: observation
            .focused
            .as_ref()
            .map(|focused| focused.element_id.clone()),
        redacted: false,
        redactions: Vec::new(),
    }
}

fn app_context_from_metadata(profile: Option<&Profile>) -> StoredAppContext {
    let metadata = profile.map(|profile| &profile.metadata);
    StoredAppContext {
        process_name: None,
        process_path: None,
        window_title: None,
        target_id: metadata.and_then(|value| {
            value
                .get("benchmark_id")
                .or_else(|| value.get("registry.family"))
                .cloned()
        }),
        gameid: metadata.and_then(|value| value.get("benchmark_world_gameid").cloned()),
        world_name: metadata.and_then(|value| value.get("benchmark_world_name").cloned()),
        world_path: metadata.and_then(|value| value.get("launch.world").cloned()),
        log_path: metadata.and_then(|value| value.get("launch.logfile").cloned()),
    }
}

const fn backend_policy(backends: ProfileBackends) -> StoredBackendPolicy {
    StoredBackendPolicy {
        default: backends.default,
        keyboard_default: backends.keyboard_default,
        mouse_default: backends.mouse_default,
        pad_default: backends.pad_default,
    }
}

fn session_key(session_id: &str) -> Vec<u8> {
    format!("session/v1/{session_id}").into_bytes()
}

fn event_key(ts_ns: u64) -> Vec<u8> {
    let seq = EVENT_AUDIT_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut key = Vec::with_capacity(12);
    key.extend_from_slice(&ts_ns.to_be_bytes());
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

fn observation_key(ts_ns: u64, seq: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(12);
    key.extend_from_slice(&ts_ns.to_be_bytes());
    key.extend_from_slice(&seq.to_be_bytes());
    key
}

fn next_observation_key_parts() -> (u64, u32) {
    let ts_ns = now_ts_ns();
    let seq = OBSERVATION_AUDIT_SEQ.fetch_add(1, Ordering::Relaxed);
    (ts_ns, seq)
}

fn event_id_suffix() -> String {
    format!(
        "{}-{}",
        now_ts_ns(),
        EVENT_AUDIT_SEQ.load(Ordering::Relaxed)
    )
}

fn now_ts_ns() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    u64::try_from(nanos).unwrap_or(u64::MAX)
}

fn error_data_code(error: &ErrorData) -> Option<&str> {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
}
