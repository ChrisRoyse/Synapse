//! Explicitly armed demonstration recording for profile authoring (#844).
//!
//! The ambient timeline intentionally avoids raw input content. Demo recording
//! is a separate, operator-armed mode: while active, the existing WinEvent
//! bridge writes high-fidelity UIA event rows as `TimelineKind::DemoMarker`
//! records and `demo_record_stop` exports those rows as replay JSONL consumable
//! by `profile_authoring_generate`.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU32, Ordering},
    },
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use rmcp::{
    ErrorData,
    schemars::{self, JsonSchema},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use synapse_a11y::{AccessibleEvent, AccessibleEventKind};
use synapse_core::{
    ProfileId, error_codes,
    types::{TIMELINE_RECORD_VERSION, TimelineActor, TimelineKind, TimelineRecord},
};
use synapse_storage::{
    Db, cf,
    timeline::{decode_timeline_key, timeline_key, timeline_scan_start},
};

use crate::m1::mcp_error;

use super::{
    M3ToolStub, SharedM3State,
    permissions::{Permission, RequiredPermissions, normalize_replay_path, replay_root, required},
};

const DEMO_RECORD_KEY: &[u8] = b"timeline/demo-record/v1";
const DEMO_RECORD_VERSION: u32 = 1;
const DEFAULT_DEMO_DURATION_MS: u64 = 600_000;
const MAX_DEMO_DURATION_MS: u64 = 3_600_000;
const MAX_LABEL_BYTES: usize = 256;
const DEMO_TIMELINE_SEQ_BASE: u32 = 0x8000_0000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DemoRecordActiveState {
    pub demo_id: String,
    pub profile_id: ProfileId,
    pub started_at_ns: u64,
    pub expires_at_ns: u64,
    pub replay_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub started_by: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedDemoRecordState {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<DemoRecordActiveState>,
    changed_at_ns: u64,
    changed_by: String,
}

impl PersistedDemoRecordState {
    fn inactive(changed_at_ns: u64, changed_by: &str) -> Self {
        Self {
            version: DEMO_RECORD_VERSION,
            active: None,
            changed_at_ns,
            changed_by: changed_by.to_owned(),
        }
    }
}

#[derive(Debug)]
pub struct DemoRecordControl {
    db: Arc<Db>,
    active: Mutex<Option<DemoRecordActiveState>>,
    seq: AtomicU32,
    last_values: Mutex<BTreeMap<String, String>>,
}

impl DemoRecordControl {
    /// Hydrates the currently armed demo session from `CF_KV`.
    ///
    /// # Errors
    ///
    /// Returns an error when the persisted row is corrupt or has an
    /// unsupported schema version.
    pub fn hydrate(db: Arc<Db>) -> Result<Self> {
        let state = load_persisted(&db)?;
        Ok(Self {
            db,
            active: Mutex::new(state.active),
            seq: AtomicU32::new(DEMO_TIMELINE_SEQ_BASE),
            last_values: Mutex::new(BTreeMap::new()),
        })
    }

    #[cfg(test)]
    #[must_use]
    pub fn active_snapshot(&self) -> Option<DemoRecordActiveState> {
        lock_unpoisoned(&self.active).clone()
    }

    #[must_use]
    pub fn status_snapshot(&self, now_ns: u64) -> DemoRecordStatusResponse {
        let stored_active = lock_unpoisoned(&self.active).clone();
        let active = stored_active
            .clone()
            .filter(|active| now_ns < active.expires_at_ns);
        let expires_in_ms = active
            .as_ref()
            .map(|active| active.expires_at_ns.saturating_sub(now_ns) / 1_000_000);
        DemoRecordStatusResponse {
            schema_version: 1,
            source_of_truth: "CF_KV timeline/demo-record/v1 hydrated DemoRecordControl",
            armed: active.is_some(),
            active,
            expires_in_ms,
            expired_active_row: stored_active
                .as_ref()
                .is_some_and(|active| now_ns >= active.expires_at_ns),
            now_ns,
        }
    }

    /// Records one UIA event if a demo is armed and not expired.
    ///
    /// This is deliberately best-effort and non-panicking because it is called
    /// from the shared WinEvent bridge. Failures are structured logs; tool
    /// calls and `timeline_search` provide the source-of-truth readback.
    pub fn record_accessible_event(&self, event: &AccessibleEvent) {
        let now_ns = now_ts_ns();
        let Some(active) = self.active_for_event(now_ns) else {
            return;
        };
        let foreground = event_foreground_context(event.window_id);
        let process_name = foreground
            .as_ref()
            .map(|context| context.process_name.clone())
            .filter(|value| !value.trim().is_empty());
        let element_key = event
            .element_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("hwnd:{:x}:event:{}", event.window_id, event.seq));
        let previous_value = event.value.as_ref().and_then(|value| {
            let mut values = lock_unpoisoned(&self.last_values);
            values.insert(element_key.clone(), value.clone())
        });
        let payload = demo_event_payload(
            &active,
            event,
            foreground.as_ref(),
            previous_value,
            process_name.as_deref(),
        );
        if let Err(error) =
            self.write_marker(now_ns, TimelineActor::Human, process_name, payload, false)
        {
            tracing::error!(
                code = "DEMO_RECORD_EVENT_WRITE_FAILED",
                demo_id = %active.demo_id,
                detail = %format!("{error:#}"),
                "failed to persist demo recording UIA event"
            );
        }
    }

    fn active_for_event(&self, now_ns: u64) -> Option<DemoRecordActiveState> {
        let mut guard = lock_unpoisoned(&self.active);
        let active = guard.clone()?;
        if now_ns < active.expires_at_ns {
            return Some(active);
        }
        if let Err(error) = self.expire_locked(&mut guard, &active, now_ns, "auto_expire") {
            tracing::error!(
                code = "DEMO_RECORD_AUTO_EXPIRE_FAILED",
                demo_id = %active.demo_id,
                detail = %format!("{error:#}"),
                "demo recording auto-expiry failed"
            );
        }
        None
    }

    fn expire_locked(
        &self,
        guard: &mut Option<DemoRecordActiveState>,
        active: &DemoRecordActiveState,
        now_ns: u64,
        changed_by: &str,
    ) -> Result<()> {
        self.write_marker(
            now_ns,
            TimelineActor::Human,
            None,
            demo_edge_payload(active, "expired", now_ns, changed_by),
            true,
        )
        .context("write demo_record expired marker")?;
        self.persist_active(None, now_ns, changed_by)
            .context("persist inactive demo state after expiry")?;
        *guard = None;
        lock_unpoisoned(&self.last_values).clear();
        tracing::info!(
            code = "DEMO_RECORD_EXPIRED",
            demo_id = %active.demo_id,
            profile_id = %active.profile_id,
            "demo recording auto-expired"
        );
        Ok(())
    }

    fn write_marker(
        &self,
        ts_ns: u64,
        actor: TimelineActor,
        app: Option<String>,
        payload: Value,
        flush: bool,
    ) -> Result<()> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let record = TimelineRecord {
            record_version: TIMELINE_RECORD_VERSION,
            ts_ns,
            kind: TimelineKind::DemoMarker,
            actor,
            app,
            payload,
        };
        let key = timeline_key(ts_ns, seq);
        let value = serde_json::to_vec(&record).context("encode demo marker timeline row")?;
        self.db
            .put_batch(cf::CF_TIMELINE, [(key, value)])
            .context("write demo marker timeline row")?;
        if flush {
            self.db.flush().context("flush demo marker timeline row")?;
        }
        Ok(())
    }

    fn persist_active(
        &self,
        active: Option<DemoRecordActiveState>,
        changed_at_ns: u64,
        changed_by: &str,
    ) -> Result<()> {
        let row = PersistedDemoRecordState {
            version: DEMO_RECORD_VERSION,
            active,
            changed_at_ns,
            changed_by: changed_by.to_owned(),
        };
        let encoded = serde_json::to_vec(&row).context("encode demo record control row")?;
        self.db
            .put_batch_pressure_bypass(cf::CF_KV, [(DEMO_RECORD_KEY.to_vec(), encoded)])
            .context("write demo record control row")?;
        self.db.flush().context("flush demo record control row")?;
        Ok(())
    }

    fn start(
        &self,
        params: &DemoRecordStartParams,
        by_session: &str,
        recorder_live: bool,
    ) -> Result<DemoRecordStartResponse, ErrorData> {
        validate_profile_id(&params.profile_id)?;
        validate_duration(params.duration_ms)?;
        let label = normalize_label(params.label.as_deref())?;
        let now_ns = now_ts_ns();
        let mut guard = lock_unpoisoned(&self.active);
        if let Some(active) = guard.clone() {
            if now_ns >= active.expires_at_ns {
                self.expire_locked(&mut guard, &active, now_ns, by_session)
                    .map_err(internal)?;
            } else {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "demo recording {} is already active for profile {}; stop it before starting another",
                        active.demo_id, active.profile_id
                    ),
                ));
            }
        }
        let demo_id = generated_demo_id(&params.profile_id, now_ns, by_session);
        let replay_path = resolve_demo_replay_path(params.path.as_deref(), &demo_id)?;
        let replay_path = replay_path.display().to_string();
        let active = DemoRecordActiveState {
            demo_id,
            profile_id: params.profile_id.clone(),
            started_at_ns: now_ns,
            expires_at_ns: now_ns.saturating_add(params.duration_ms.saturating_mul(1_000_000)),
            replay_path,
            label,
            started_by: by_session.to_owned(),
        };
        self.persist_active(Some(active.clone()), now_ns, by_session)
            .map_err(internal)?;
        *guard = Some(active.clone());
        lock_unpoisoned(&self.last_values).clear();
        self.write_marker(
            now_ns,
            TimelineActor::Human,
            None,
            demo_edge_payload(&active, "start", now_ns, by_session),
            true,
        )
        .map_err(internal)?;
        Ok(DemoRecordStartResponse {
            demo_id: active.demo_id,
            profile_id: active.profile_id,
            started_at_ns: active.started_at_ns,
            expires_at_ns: active.expires_at_ns,
            duration_ms: params.duration_ms,
            replay_path: active.replay_path,
            label: active.label,
            recorder_live,
            persisted: true,
            marker_row_written: true,
        })
    }

    fn stop(
        &self,
        params: &DemoRecordStopParams,
        by_session: &str,
    ) -> Result<DemoRecordStopResponse, ErrorData> {
        let now_ns = now_ts_ns();
        let mut guard = lock_unpoisoned(&self.active);
        let Some(active) = guard.clone() else {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "demo_record_stop found no active demo recording",
            ));
        };
        if let Some(requested) = params.demo_id.as_deref().map(str::trim)
            && !requested.is_empty()
            && requested != active.demo_id
        {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "demo_record_stop requested demo_id {requested:?}, but active demo is {:?}",
                    active.demo_id
                ),
            ));
        }
        let expired = now_ns >= active.expires_at_ns;
        let edge = if expired { "expired" } else { "stop" };
        self.write_marker(
            now_ns,
            TimelineActor::Human,
            None,
            demo_edge_payload(&active, edge, now_ns, by_session),
            true,
        )
        .map_err(internal)?;
        let export = export_demo_replay(&self.db, &active, now_ns).map_err(internal)?;
        self.persist_active(None, now_ns, by_session)
            .map_err(internal)?;
        *guard = None;
        lock_unpoisoned(&self.last_values).clear();
        Ok(DemoRecordStopResponse {
            demo_id: active.demo_id,
            profile_id: active.profile_id,
            started_at_ns: active.started_at_ns,
            stopped_at_ns: now_ns,
            expired,
            replay_path: active.replay_path,
            records_written: export.records_written,
            event_rows_exported: export.event_rows_exported,
            bytes: export.bytes,
            source_cf_name: cf::CF_TIMELINE.to_owned(),
            source_row_count: export.source_row_count,
            marker_row_written: true,
            cleared_active_state: true,
        })
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DemoRecordStartParams {
    pub profile_id: ProfileId,
    /// Auto-expire after this many milliseconds. Default is 10 minutes.
    #[serde(default = "default_demo_duration_ms")]
    #[schemars(default = "default_demo_duration_ms", range(min = 1, max = 3600000))]
    pub duration_ms: u64,
    /// Optional replay JSONL path under the Synapse replay root. Omit for
    /// `demo-recordings/<demo_id>.jsonl`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DemoRecordStopParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demo_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DemoRecordStartResponse {
    pub demo_id: String,
    pub profile_id: ProfileId,
    pub started_at_ns: u64,
    pub expires_at_ns: u64,
    pub duration_ms: u64,
    pub replay_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub recorder_live: bool,
    pub persisted: bool,
    pub marker_row_written: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DemoRecordStopResponse {
    pub demo_id: String,
    pub profile_id: ProfileId,
    pub started_at_ns: u64,
    pub stopped_at_ns: u64,
    pub expired: bool,
    pub replay_path: String,
    pub records_written: u64,
    pub event_rows_exported: u64,
    pub bytes: u64,
    pub source_cf_name: String,
    pub source_row_count: u64,
    pub marker_row_written: bool,
    pub cleared_active_state: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DemoRecordStatusResponse {
    pub schema_version: u32,
    pub source_of_truth: &'static str,
    pub armed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<DemoRecordActiveState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_ms: Option<u64>,
    pub expired_active_row: bool,
    pub now_ns: u64,
}

#[must_use]
pub const fn demo_record_start() -> M3ToolStub {
    M3ToolStub::new("demo_record_start")
}

#[must_use]
pub const fn demo_record_stop() -> M3ToolStub {
    M3ToolStub::new("demo_record_stop")
}

#[must_use]
pub fn required_permissions_start(_params: &DemoRecordStartParams) -> RequiredPermissions {
    required([
        Permission::ReadStorage,
        Permission::WriteStorage,
        Permission::WriteReplay,
    ])
}

#[must_use]
pub fn required_permissions_stop(_params: &DemoRecordStopParams) -> RequiredPermissions {
    required([
        Permission::ReadStorage,
        Permission::WriteStorage,
        Permission::WriteReplay,
    ])
}

type DemoContext = (Arc<DemoRecordControl>, bool);

fn demo_context(m3_state: &SharedM3State) -> Result<DemoContext, ErrorData> {
    let mut guard = m3_state.lock().map_err(|_poisoned| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "M3 service state lock poisoned",
        )
    })?;
    let control = guard
        .ensure_demo_record_control()
        .map_err(|error| mcp_error(error_codes::TOOL_INTERNAL_ERROR, format!("{error:#}")))?;
    let recorder_live = guard.activity_recorder.is_some();
    Ok((control, recorder_live))
}

pub fn start_demo_recording(
    m3_state: &SharedM3State,
    params: &DemoRecordStartParams,
    by_session: &str,
) -> Result<DemoRecordStartResponse, ErrorData> {
    let (control, recorder_live) = demo_context(m3_state)?;
    control.start(params, by_session, recorder_live)
}

pub fn stop_demo_recording(
    m3_state: &SharedM3State,
    params: &DemoRecordStopParams,
    by_session: &str,
) -> Result<DemoRecordStopResponse, ErrorData> {
    let (control, _recorder_live) = demo_context(m3_state)?;
    control.stop(params, by_session)
}

pub fn demo_record_status_snapshot(
    m3_state: &SharedM3State,
) -> Result<DemoRecordStatusResponse, ErrorData> {
    let mut guard = m3_state.lock().map_err(|_poisoned| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "M3 service state lock poisoned",
        )
    })?;
    let control = guard
        .ensure_demo_record_control()
        .map_err(|error| mcp_error(error_codes::TOOL_INTERNAL_ERROR, format!("{error:#}")))?;
    Ok(control.status_snapshot(now_ts_ns()))
}

fn load_persisted(db: &Db) -> Result<PersistedDemoRecordState> {
    let rows = db
        .scan_cf_prefix(cf::CF_KV, DEMO_RECORD_KEY)
        .context("read demo record control row from CF_KV")?;
    let Some((_key, value)) = rows
        .into_iter()
        .find(|(key, _value)| key.as_slice() == DEMO_RECORD_KEY)
    else {
        return Ok(PersistedDemoRecordState::inactive(0, "initial"));
    };
    let state: PersistedDemoRecordState =
        serde_json::from_slice(&value).context("decode demo record control row")?;
    if state.version != DEMO_RECORD_VERSION {
        bail!(
            "demo record control row version {} is not supported; expected {DEMO_RECORD_VERSION}",
            state.version
        );
    }
    Ok(state)
}

fn demo_edge_payload(
    active: &DemoRecordActiveState,
    edge: &str,
    ts_ns: u64,
    by_session: &str,
) -> Value {
    json!({
        "edge": edge,
        "demo_id": active.demo_id,
        "profile_id": active.profile_id,
        "started_at_ns": active.started_at_ns,
        "expires_at_ns": active.expires_at_ns,
        "replay_path": active.replay_path,
        "label": active.label,
        "by_session": by_session,
        "ts_ns": ts_ns,
        "profile_authoring": {
            "metadata": {
                "demo_recording": "true",
                "demo_id": active.demo_id.as_str(),
            },
        },
    })
}

fn demo_event_payload(
    active: &DemoRecordActiveState,
    event: &AccessibleEvent,
    foreground: Option<&synapse_core::ForegroundContext>,
    previous_value: Option<String>,
    process_name: Option<&str>,
) -> Value {
    let event_kind = event_kind_name(event.kind);
    let profile_authoring = match process_name {
        Some(process_name) => json!({
            "matches": { "exe": [process_name] },
            "metadata": {
                "demo_recording": "true",
                "demo_id": active.demo_id.as_str(),
                "uia_event_kind": event_kind,
            },
        }),
        None => json!({
            "metadata": {
                "demo_recording": "true",
                "demo_id": active.demo_id.as_str(),
                "uia_event_kind": event_kind,
            },
        }),
    };
    json!({
        "edge": "event",
        "demo_id": active.demo_id,
        "profile_id": active.profile_id,
        "started_at_ns": active.started_at_ns,
        "expires_at_ns": active.expires_at_ns,
        "replay_path": active.replay_path,
        "label": active.label,
        "foreground": foreground.map(|context| json!({
            "profile_id": active.profile_id,
            "hwnd": context.hwnd,
            "pid": context.pid,
            "process_name": context.process_name,
            "process_path": context.process_path,
            "window_title": context.window_title,
        })),
        "uia": {
            "event_kind": event_kind,
            "win_event_seq": event.seq,
            "win_event_at_ms": event.at_ms,
            "window_id": event.window_id,
            "element_id": event.element_id.as_ref().map(ToString::to_string),
            "name": event.name,
            "value": event.value,
            "previous_value": previous_value,
            "control_pattern": inferred_control_pattern(event.kind),
        },
        "profile_authoring": profile_authoring,
    })
}

fn event_foreground_context(window_id: i64) -> Option<synapse_core::ForegroundContext> {
    synapse_a11y::foreground_context(window_id)
        .ok()
        .or_else(|| synapse_a11y::current_foreground_context().ok())
}

const fn event_kind_name(kind: AccessibleEventKind) -> &'static str {
    match kind {
        AccessibleEventKind::ForegroundChanged => "foreground_changed",
        AccessibleEventKind::FocusChanged => "focus_changed",
        AccessibleEventKind::ValueChanged => "value_changed",
        AccessibleEventKind::NameChanged => "name_changed",
        AccessibleEventKind::ElementAppeared => "element_appeared",
        AccessibleEventKind::ElementDisappeared => "element_disappeared",
        AccessibleEventKind::SelectionChanged => "selection_changed",
        AccessibleEventKind::MenuStart => "menu_start",
        AccessibleEventKind::MenuEnd => "menu_end",
        AccessibleEventKind::Alert => "alert",
    }
}

const fn inferred_control_pattern(kind: AccessibleEventKind) -> &'static str {
    match kind {
        AccessibleEventKind::ValueChanged => "uia_value_pattern_or_native_value",
        AccessibleEventKind::SelectionChanged => "uia_selection_pattern",
        AccessibleEventKind::FocusChanged => "uia_focus",
        AccessibleEventKind::NameChanged => "uia_name_property",
        AccessibleEventKind::ForegroundChanged => "win_event_foreground",
        AccessibleEventKind::ElementAppeared => "uia_element_create",
        AccessibleEventKind::ElementDisappeared => "uia_element_destroy",
        AccessibleEventKind::MenuStart => "win_event_menu_start",
        AccessibleEventKind::MenuEnd => "win_event_menu_end",
        AccessibleEventKind::Alert => "win_event_alert",
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExportSummary {
    records_written: u64,
    event_rows_exported: u64,
    bytes: u64,
    source_row_count: u64,
}

fn export_demo_replay(
    db: &Db,
    active: &DemoRecordActiveState,
    end_ts_ns: u64,
) -> Result<ExportSummary> {
    let path = PathBuf::from(&active.replay_path);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create demo replay parent {}", parent.display()))?;
    }
    let file = File::create(&path)
        .with_context(|| format!("create demo replay bundle {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    let rows = db
        .scan_cf_from(
            cf::CF_TIMELINE,
            &timeline_scan_start(active.started_at_ns),
            usize::MAX,
        )
        .context("scan CF_TIMELINE for demo replay export")?
        .0;
    let mut records_written = 0_u64;
    let mut event_rows_exported = 0_u64;
    let mut source_row_count = 0_u64;
    for (key, value) in rows {
        let (ts_ns, _seq) = decode_timeline_key(&key).context("decode CF_TIMELINE key")?;
        if ts_ns > end_ts_ns {
            break;
        }
        source_row_count = source_row_count.saturating_add(1);
        let record: TimelineRecord =
            serde_json::from_slice(&value).context("decode CF_TIMELINE record")?;
        if record.kind != TimelineKind::DemoMarker {
            continue;
        }
        if record.payload.get("demo_id").and_then(Value::as_str) != Some(&active.demo_id) {
            continue;
        }
        if record.payload.get("edge").and_then(Value::as_str) == Some("event") {
            event_rows_exported = event_rows_exported.saturating_add(1);
        }
        let replay_record = replay_record_from_timeline(active, &key, &record);
        let line = serde_json::to_vec(&replay_record).context("encode demo replay JSONL row")?;
        writer
            .write_all(&line)
            .context("write demo replay JSONL row")?;
        writer
            .write_all(b"\n")
            .context("write demo replay newline")?;
        records_written = records_written.saturating_add(1);
    }
    writer.flush().context("flush demo replay JSONL")?;
    let bytes = fs::metadata(&path)
        .with_context(|| format!("read demo replay metadata {}", path.display()))?
        .len();
    Ok(ExportSummary {
        records_written,
        event_rows_exported,
        bytes,
        source_row_count,
    })
}

fn replay_record_from_timeline(
    active: &DemoRecordActiveState,
    key: &[u8],
    record: &TimelineRecord,
) -> Value {
    let fallback_foreground = json!({ "profile_id": active.profile_id });
    json!({
        "record": {
            "type": "demo_record",
            "demo_id": active.demo_id,
            "profile_id": active.profile_id,
            "timeline_key_hex": hex_encode(key),
            "timeline_ts_ns": record.ts_ns,
            "timeline_kind": "demo_marker",
            "edge": record.payload.get("edge").cloned().unwrap_or(Value::Null),
            "foreground": record
                .payload
                .get("foreground")
                .cloned()
                .unwrap_or(fallback_foreground),
            "uia": record.payload.get("uia").cloned().unwrap_or(Value::Null),
            "profile_authoring": record
                .payload
                .get("profile_authoring")
                .cloned()
                .unwrap_or_else(|| json!({
                    "metadata": {
                        "demo_recording": "true",
                        "demo_id": active.demo_id.as_str(),
                    },
                })),
            "demo": {
                "started_at_ns": active.started_at_ns,
                "expires_at_ns": active.expires_at_ns,
                "label": active.label,
            },
        }
    })
}

fn validate_profile_id(profile_id: &str) -> Result<(), ErrorData> {
    let trimmed = profile_id.trim();
    if trimmed.is_empty() || trimmed.len() > 160 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "demo_record_start profile_id must be 1..=160 non-blank characters",
        ));
    }
    Ok(())
}

fn validate_duration(duration_ms: u64) -> Result<(), ErrorData> {
    if duration_ms == 0 || duration_ms > MAX_DEMO_DURATION_MS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("demo_record_start duration_ms must be between 1 and {MAX_DEMO_DURATION_MS}"),
        ));
    }
    Ok(())
}

fn normalize_label(label: Option<&str>) -> Result<Option<String>, ErrorData> {
    let Some(label) = label.map(str::trim).filter(|label| !label.is_empty()) else {
        return Ok(None);
    };
    if label.len() > MAX_LABEL_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "demo_record_start label is {} bytes; cap is {MAX_LABEL_BYTES}",
                label.len()
            ),
        ));
    }
    Ok(Some(label.to_owned()))
}

fn resolve_demo_replay_path(path: Option<&str>, demo_id: &str) -> Result<PathBuf, ErrorData> {
    let default_path = format!("demo-recordings/{demo_id}.jsonl");
    normalize_replay_path(&replay_root(), path.or(Some(default_path.as_str())))
}

fn generated_demo_id(profile_id: &str, now_ns: u64, by_session: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(profile_id.as_bytes());
    hasher.update(now_ns.to_le_bytes());
    hasher.update(by_session.as_bytes());
    let digest = hasher.finalize();
    let suffix = hex_encode(&digest[..8]);
    format!("demo.{}.{}", sanitize_id_component(profile_id), suffix)
}

fn sanitize_id_component(value: &str) -> String {
    let mut output = String::new();
    for ch in value.chars().take(64) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "profile".to_owned()
    } else {
        output
    }
}

fn default_demo_duration_ms() -> u64 {
    DEFAULT_DEMO_DURATION_MS
}

fn internal(error: anyhow::Error) -> ErrorData {
    mcp_error(error_codes::TOOL_INTERNAL_ERROR, format!("{error:#}"))
}

fn now_ts_ns() -> u64 {
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
    u64::try_from(nanos).unwrap_or(0)
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(test)]
mod tests {
    use std::{path::Path, thread, time::Duration};

    use synapse_core::element_id;

    use super::*;

    fn temp_control() -> (tempfile::TempDir, Arc<DemoRecordControl>) {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let db = Arc::new(
            Db::open(dir.path(), synapse_core::SCHEMA_VERSION)
                .unwrap_or_else(|error| panic!("open temp db: {error}")),
        );
        let control = Arc::new(
            DemoRecordControl::hydrate(db).unwrap_or_else(|error| panic!("hydrate: {error:#}")),
        );
        (dir, control)
    }

    fn event(seq: u64, kind: AccessibleEventKind, value: Option<&str>) -> AccessibleEvent {
        AccessibleEvent {
            seq,
            at_ms: seq.saturating_mul(10),
            window_id: 0x42,
            element_id: Some(element_id(0x42, "0000002a")),
            kind,
            name: Some("Document".to_owned()),
            value: value.map(str::to_owned),
        }
    }

    #[test]
    fn demo_record_start_stop_exports_profile_authoring_replay() {
        let (_dir, control) = temp_control();
        let params = DemoRecordStartParams {
            profile_id: "notepad".to_owned(),
            duration_ms: 60_000,
            path: Some("demo-recordings/test-demo.jsonl".to_owned()),
            label: Some("five step notepad demo".to_owned()),
        };
        let started = control
            .start(&params, "test-session", true)
            .unwrap_or_else(|error| panic!("start: {error:?}"));
        assert_eq!(started.profile_id, "notepad");
        assert!(Path::new(&started.replay_path).starts_with(replay_root()));

        control.record_accessible_event(&event(1, AccessibleEventKind::FocusChanged, None));
        control.record_accessible_event(&event(2, AccessibleEventKind::ValueChanged, Some("h")));
        control.record_accessible_event(&event(3, AccessibleEventKind::ValueChanged, Some("hi")));

        let stopped = control
            .stop(&DemoRecordStopParams::default(), "test-session")
            .unwrap_or_else(|error| panic!("stop: {error:?}"));
        assert_eq!(stopped.demo_id, started.demo_id);
        assert_eq!(stopped.profile_id, "notepad");
        assert_eq!(stopped.event_rows_exported, 3);
        assert!(stopped.records_written >= 5, "start + 3 events + stop");
        assert!(stopped.bytes > 0);
        assert!(control.active_snapshot().is_none());

        let text = fs::read_to_string(&stopped.replay_path)
            .unwrap_or_else(|error| panic!("read replay: {error}"));
        println!(
            "readback=demo_record edge=stop_export path={} bytes={} rows={}",
            stopped.replay_path, stopped.bytes, stopped.records_written
        );
        assert!(text.contains("\"profile_id\":\"notepad\""));
        assert!(text.contains("\"type\":\"demo_record\""));
        assert!(text.contains("\"demo_recording\":\"true\""));
        assert!(text.contains("\"previous_value\":\"h\""));
    }

    #[test]
    fn demo_record_auto_expires_before_late_event() {
        let (_dir, control) = temp_control();
        let params = DemoRecordStartParams {
            profile_id: "notepad".to_owned(),
            duration_ms: 1,
            path: Some("demo-recordings/expired-demo.jsonl".to_owned()),
            label: None,
        };
        let started = control
            .start(&params, "test-session", true)
            .unwrap_or_else(|error| panic!("start: {error:?}"));
        thread::sleep(Duration::from_millis(5));
        control.record_accessible_event(&event(1, AccessibleEventKind::FocusChanged, None));
        assert!(control.active_snapshot().is_none());
        let rows = control
            .db
            .scan_cf(cf::CF_TIMELINE)
            .unwrap_or_else(|error| panic!("scan timeline: {error}"));
        let expired = rows.iter().any(|(_key, value)| {
            let record: TimelineRecord = serde_json::from_slice(value).expect("timeline row");
            record.kind == TimelineKind::DemoMarker
                && record.payload.get("demo_id").and_then(Value::as_str)
                    == Some(started.demo_id.as_str())
                && record.payload.get("edge").and_then(Value::as_str) == Some("expired")
        });
        println!(
            "readback=demo_record edge=auto_expire demo_id={} rows={}",
            started.demo_id,
            rows.len()
        );
        assert!(expired, "expired marker must be written");
    }

    #[test]
    fn demo_record_status_reports_expired_rows_as_not_armed() {
        let (_dir, control) = temp_control();
        let params = DemoRecordStartParams {
            profile_id: "notepad".to_owned(),
            duration_ms: 1,
            path: Some("demo-recordings/status-expired-demo.jsonl".to_owned()),
            label: None,
        };
        let started = control
            .start(&params, "test-session", true)
            .unwrap_or_else(|error| panic!("start: {error:?}"));

        let armed = control.status_snapshot(started.started_at_ns.saturating_add(1));
        assert!(armed.armed);
        assert_eq!(
            armed.active.as_ref().map(|active| active.demo_id.as_str()),
            Some(started.demo_id.as_str())
        );

        let expired = control.status_snapshot(started.expires_at_ns);
        assert!(!expired.armed);
        assert!(expired.active.is_none());
        assert!(expired.expired_active_row);
    }

    #[test]
    fn demo_record_rejects_invalid_duration() {
        let (_dir, control) = temp_control();
        let params = DemoRecordStartParams {
            profile_id: "notepad".to_owned(),
            duration_ms: MAX_DEMO_DURATION_MS + 1,
            path: None,
            label: None,
        };
        let error = control
            .start(&params, "test-session", true)
            .expect_err("oversized duration must fail");
        let code = error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str);
        assert_eq!(code, Some(error_codes::TOOL_PARAMS_INVALID));
    }
}
