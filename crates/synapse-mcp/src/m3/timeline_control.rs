//! Recorder controls for the operator-activity timeline (#843, epic #829).
//!
//! Three operator freedoms over the always-on recorder (#837), mirroring the
//! Windows Recall control surface (pause with optional auto-resume, per-app
//! filter list, bulk delete):
//!
//! - `timeline_pause` / `timeline_resume` — a global gate over every recorder
//!   feed. Durable truth lives in one `CF_KV` row (`timeline/control/v1`),
//!   written with the pressure bypass (pausing must work under disk pressure
//!   precisely because it reduces future writes) and flushed before the
//!   in-memory gate flips, so the state survives a daemon restart by
//!   construction. Optional `duration_ms` arms an auto-resume deadline —
//!   Recall's "pause until tomorrow" lesson: an indefinite silent pause is a
//!   data-loss footgun.
//! - per-process exclusions — events from excluded executables are never
//!   written. The effective set is the immutable env baseline
//!   (`SYNAPSE_TIMELINE_EXCLUDE`) union the runtime list mutated by the
//!   `timeline_exclusions` tool. Enforcement happens at the recorder write
//!   choke point (ADR §6: storage has no notion of exclusion).
//!
//! The in-memory [`RecorderControl`] is a cache of the persisted row for the
//! hot write path; every mutation persists first and flips the cache after,
//! and hydration failures are hard errors (a recorder that cannot read its
//! own pause state must not record).

use std::collections::BTreeSet;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use rmcp::{
    ErrorData,
    schemars::{self, JsonSchema},
};
use serde::{Deserialize, Serialize};
use synapse_core::error_codes;
use synapse_storage::{Db, cf};

use crate::m1::mcp_error;

use super::{
    M3ToolStub, SharedM3State,
    activity_recorder::ActivityRecorder,
    permissions::{Permission, RequiredPermissions, required},
};

/// Comma-separated executable names excluded from recording at daemon start.
pub const TIMELINE_EXCLUDE_ENV: &str = "SYNAPSE_TIMELINE_EXCLUDE";
/// `CF_KV` key of the persisted control row.
pub const TIMELINE_CONTROL_KEY: &[u8] = b"timeline/control/v1";
/// Persisted control-row schema version.
pub const TIMELINE_CONTROL_VERSION: u32 = 1;
/// Upper bound for one exclusion entry, in bytes.
const MAX_EXCLUSION_ENTRY_BYTES: usize = 256;
/// Upper bound for the runtime exclusion list.
const MAX_RUNTIME_EXCLUSIONS: usize = 256;

/// Durable control state as persisted in `CF_KV`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedControlState {
    pub version: u32,
    pub paused: bool,
    /// Auto-resume deadline; `None` pauses indefinitely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_until_ns: Option<u64>,
    pub runtime_exclusions: Vec<String>,
    pub changed_at_ns: u64,
    /// MCP session id (or daemon edge) that last changed the state.
    pub changed_by: String,
}

impl PersistedControlState {
    #[must_use]
    pub fn initial() -> Self {
        Self {
            version: TIMELINE_CONTROL_VERSION,
            paused: false,
            paused_until_ns: None,
            runtime_exclusions: Vec::new(),
            changed_at_ns: 0,
            changed_by: "initial".to_owned(),
        }
    }
}

/// Why the recorder suppressed a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuppressReason {
    Paused,
    ExcludedApp,
}

/// Shared pause/exclusion gate. One per storage handle; the recorder checks
/// it on every row write, the MCP tools mutate it.
#[derive(Debug)]
pub struct RecorderControl {
    paused: AtomicBool,
    /// Auto-resume deadline in epoch ns; 0 means no deadline.
    paused_until_ns: AtomicU64,
    /// Lowercased executable names from `SYNAPSE_TIMELINE_EXCLUDE`; immutable
    /// for the daemon lifetime so an operator config cannot be silently
    /// edited away at runtime.
    env_exclusions: BTreeSet<String>,
    /// Lowercased executable names mutable via `timeline_exclusions`.
    runtime_exclusions: Mutex<BTreeSet<String>>,
    /// Serializes read-modify-write mutations against the persisted row.
    mutation: Mutex<()>,
}

impl RecorderControl {
    /// Hydrates the control gate from the persisted `CF_KV` row and the env
    /// baseline.
    ///
    /// # Errors
    ///
    /// Returns an error when the env baseline is malformed or when the
    /// persisted control row exists but cannot be decoded — a recorder that
    /// cannot read its own pause state must refuse to start rather than
    /// record against the operator's wishes.
    pub fn hydrate(db: &Db) -> Result<Self> {
        let env_exclusions = parse_env_exclusions(std::env::var(TIMELINE_EXCLUDE_ENV).ok())?;
        let state = load_persisted(db)?;
        let runtime_exclusions = state
            .runtime_exclusions
            .iter()
            .map(|entry| validate_exclusion_entry(entry))
            .collect::<Result<BTreeSet<_>>>()
            .context("persisted timeline control row holds an invalid exclusion entry")?;
        Ok(Self {
            paused: AtomicBool::new(state.paused),
            paused_until_ns: AtomicU64::new(state.paused_until_ns.unwrap_or(0)),
            env_exclusions,
            runtime_exclusions: Mutex::new(runtime_exclusions),
            mutation: Mutex::new(()),
        })
    }

    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Auto-resume deadline, when armed.
    #[must_use]
    pub fn paused_until_ns(&self) -> Option<u64> {
        match self.paused_until_ns.load(Ordering::SeqCst) {
            0 => None,
            deadline => Some(deadline),
        }
    }

    /// True when the recorder is paused with an expired auto-resume deadline.
    #[must_use]
    pub fn auto_resume_due(&self, now_ns: u64) -> bool {
        self.is_paused()
            && self
                .paused_until_ns()
                .is_some_and(|deadline| now_ns >= deadline)
    }

    /// The write-path gate: every recorder row consults this before hitting
    /// storage.
    #[must_use]
    pub fn suppress_reason(&self, app: Option<&str>) -> Option<SuppressReason> {
        if self.is_paused() {
            return Some(SuppressReason::Paused);
        }
        let app = app?;
        let app_lower = app.to_lowercase();
        if self.env_exclusions.contains(&app_lower) || self.runtime_excluded(&app_lower) {
            return Some(SuppressReason::ExcludedApp);
        }
        None
    }

    fn runtime_excluded(&self, app_lower: &str) -> bool {
        lock_unpoisoned(&self.runtime_exclusions).contains(app_lower)
    }

    #[must_use]
    pub fn env_exclusions(&self) -> Vec<String> {
        self.env_exclusions.iter().cloned().collect()
    }

    #[must_use]
    pub fn runtime_exclusions(&self) -> Vec<String> {
        lock_unpoisoned(&self.runtime_exclusions)
            .iter()
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn effective_exclusions(&self) -> Vec<String> {
        let mut all = self.env_exclusions.clone();
        all.extend(lock_unpoisoned(&self.runtime_exclusions).iter().cloned());
        all.into_iter().collect()
    }

    /// Persists `paused = true` (durable first), then flips the gate.
    ///
    /// # Errors
    ///
    /// Returns an error when the durable write fails; the gate is left
    /// unchanged so the tool reports exactly what did not happen.
    pub fn persist_pause(
        &self,
        db: &Db,
        paused_until_ns: Option<u64>,
        now_ns: u64,
        changed_by: &str,
    ) -> Result<PersistedControlState> {
        let _mutation = lock_unpoisoned(&self.mutation);
        let state = PersistedControlState {
            version: TIMELINE_CONTROL_VERSION,
            paused: true,
            paused_until_ns,
            runtime_exclusions: self.runtime_exclusions(),
            changed_at_ns: now_ns,
            changed_by: changed_by.to_owned(),
        };
        persist(db, &state)?;
        self.paused_until_ns
            .store(paused_until_ns.unwrap_or(0), Ordering::SeqCst);
        self.paused.store(true, Ordering::SeqCst);
        Ok(state)
    }

    /// Persists `paused = false` (durable first), then opens the gate.
    ///
    /// # Errors
    ///
    /// Returns an error when the durable write fails; the gate stays paused.
    pub fn persist_resume(
        &self,
        db: &Db,
        now_ns: u64,
        changed_by: &str,
    ) -> Result<PersistedControlState> {
        let _mutation = lock_unpoisoned(&self.mutation);
        let state = PersistedControlState {
            version: TIMELINE_CONTROL_VERSION,
            paused: false,
            paused_until_ns: None,
            runtime_exclusions: self.runtime_exclusions(),
            changed_at_ns: now_ns,
            changed_by: changed_by.to_owned(),
        };
        persist(db, &state)?;
        self.paused_until_ns.store(0, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        Ok(state)
    }

    /// Applies `add`/`remove` to the runtime exclusion list, persisting the
    /// new list before the in-memory set changes.
    ///
    /// # Errors
    ///
    /// Returns an error when an entry is invalid (empty, path-like, or
    /// oversized), when the list would exceed its cap, when `remove` names an
    /// env-baseline entry (immutable at runtime by design), or when the
    /// durable write fails.
    pub fn persist_exclusion_update(
        &self,
        db: &Db,
        add: &[String],
        remove: &[String],
        now_ns: u64,
        changed_by: &str,
    ) -> Result<ExclusionUpdate> {
        let _mutation = lock_unpoisoned(&self.mutation);
        let mut next = lock_unpoisoned(&self.runtime_exclusions).clone();
        let mut added = Vec::new();
        let mut removed = Vec::new();
        for entry in add {
            let normalized = validate_exclusion_entry(entry)?;
            if next.insert(normalized.clone()) {
                added.push(normalized);
            }
        }
        for entry in remove {
            let normalized = validate_exclusion_entry(entry)?;
            if self.env_exclusions.contains(&normalized) {
                bail!(
                    "exclusion {normalized:?} comes from {TIMELINE_EXCLUDE_ENV} and cannot be \
                     removed at runtime; change the environment configuration instead"
                );
            }
            if next.remove(&normalized) {
                removed.push(normalized);
            }
        }
        if next.len() > MAX_RUNTIME_EXCLUSIONS {
            bail!(
                "runtime exclusion list would hold {} entries; the cap is {MAX_RUNTIME_EXCLUSIONS}",
                next.len()
            );
        }
        let state = PersistedControlState {
            version: TIMELINE_CONTROL_VERSION,
            paused: self.is_paused(),
            paused_until_ns: self.paused_until_ns(),
            runtime_exclusions: next.iter().cloned().collect(),
            changed_at_ns: now_ns,
            changed_by: changed_by.to_owned(),
        };
        persist(db, &state)?;
        *lock_unpoisoned(&self.runtime_exclusions) = next;
        Ok(ExclusionUpdate { added, removed })
    }
}

#[derive(Clone, Debug)]
pub struct ExclusionUpdate {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

fn load_persisted(db: &Db) -> Result<PersistedControlState> {
    let rows = db
        .scan_cf_prefix(cf::CF_KV, TIMELINE_CONTROL_KEY)
        .context("read timeline control row from CF_KV")?;
    let Some((key, value)) = rows
        .into_iter()
        .find(|(key, _value)| key.as_slice() == TIMELINE_CONTROL_KEY)
    else {
        return Ok(PersistedControlState::initial());
    };
    let state: PersistedControlState = serde_json::from_slice(&value).with_context(|| {
        format!(
            "decode timeline control row (CF_KV key {:?}); the row is corrupt — \
             inspect and delete it to reset recorder controls",
            String::from_utf8_lossy(&key)
        )
    })?;
    if state.version != TIMELINE_CONTROL_VERSION {
        bail!(
            "timeline control row version {} is not the supported version {TIMELINE_CONTROL_VERSION}",
            state.version
        );
    }
    Ok(state)
}

/// Durable write of the control row: pressure bypass (pause/exclusion must
/// work under disk pressure — they reduce retained state) plus an explicit
/// flush, because the batcher acks `put_batch` on enqueue and a control row
/// that evaporates on crash would silently resume recording.
fn persist(db: &Db, state: &PersistedControlState) -> Result<()> {
    let encoded = serde_json::to_vec(state).context("encode timeline control row")?;
    db.put_batch_pressure_bypass(cf::CF_KV, [(TIMELINE_CONTROL_KEY.to_vec(), encoded)])
        .context("write timeline control row to CF_KV")?;
    db.flush().context("flush timeline control row")?;
    tracing::info!(
        code = "TIMELINE_CONTROL_PERSISTED",
        paused = state.paused,
        paused_until_ns = state.paused_until_ns,
        runtime_exclusions = state.runtime_exclusions.len(),
        changed_by = %state.changed_by,
        "timeline recorder control state persisted"
    );
    Ok(())
}

fn parse_env_exclusions(raw: Option<String>) -> Result<BTreeSet<String>> {
    let Some(raw) = raw else {
        return Ok(BTreeSet::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            validate_exclusion_entry(entry).with_context(|| {
                format!("{TIMELINE_EXCLUDE_ENV} holds an invalid exclusion entry {entry:?}")
            })
        })
        .collect()
}

/// Exclusion entries are bare executable names (`chrome.exe`), matching the
/// recorder's `process_name` field — the same shape Windows Recall's app
/// filter list uses. Paths are rejected so a typo cannot silently exclude
/// nothing.
fn validate_exclusion_entry(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("exclusion entries must not be empty");
    }
    if trimmed.len() > MAX_EXCLUSION_ENTRY_BYTES {
        bail!(
            "exclusion entry is {} bytes; the cap is {MAX_EXCLUSION_ENTRY_BYTES}",
            trimmed.len()
        );
    }
    if trimmed.contains(['\\', '/', ',']) {
        bail!(
            "exclusion entry {trimmed:?} must be a bare executable name like \"chrome.exe\", \
             not a path or list"
        );
    }
    Ok(trimmed.to_lowercase())
}

fn lock_unpoisoned<'a, T>(mutex: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// MCP parameter/response types for the control tools.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelinePauseParams {
    /// Auto-resume after this many milliseconds; omit to pause until an
    /// explicit `timeline_resume`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelinePauseResponse {
    pub paused: bool,
    pub was_paused: bool,
    /// Epoch-ns auto-resume deadline, when `duration_ms` was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_until_ns: Option<u64>,
    /// Whether a live recorder enforced the change in this process (the HTTP
    /// daemon). `false` means only the durable state changed — no recorder
    /// holds this database, so nothing was being written anyway.
    pub recorder_live: bool,
    /// Whether the `session_end { edge: "pause" }` boundary row was written.
    pub boundary_row_written: bool,
    pub persisted: bool,
    pub changed_at_ns: u64,
    /// Live recorder counters (events suppressed while paused / rows
    /// suppressed by exclusion); absent when no recorder runs here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppressed_paused_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppressed_excluded_total: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineResumeParams {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineResumeResponse {
    pub paused: bool,
    pub was_paused: bool,
    pub recorder_live: bool,
    /// Whether the `session_start { edge: "resume" }` boundary row was
    /// written and flushed — the resume-time write-path proof.
    pub boundary_row_written: bool,
    pub persisted: bool,
    pub changed_at_ns: u64,
    /// Live recorder counters (events suppressed while paused / rows
    /// suppressed by exclusion); absent when no recorder runs here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppressed_paused_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppressed_excluded_total: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineExclusionsParams {
    /// Executable names to add to the runtime exclusion list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add: Option<Vec<String>>,
    /// Executable names to remove from the runtime exclusion list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove: Option<Vec<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TimelineExclusionsResponse {
    /// Immutable baseline from `SYNAPSE_TIMELINE_EXCLUDE`.
    pub env_exclusions: Vec<String>,
    /// Runtime list persisted in `CF_KV`.
    pub runtime_exclusions: Vec<String>,
    /// Union the recorder enforces.
    pub effective_exclusions: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub recorder_live: bool,
    pub persisted: bool,
    /// Live recorder counters (events suppressed while paused / rows
    /// suppressed by exclusion); absent when no recorder runs here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppressed_paused_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppressed_excluded_total: Option<u64>,
}

#[must_use]
pub const fn timeline_pause() -> M3ToolStub {
    M3ToolStub::new("timeline_pause")
}

#[must_use]
pub const fn timeline_resume() -> M3ToolStub {
    M3ToolStub::new("timeline_resume")
}

#[must_use]
pub const fn timeline_exclusions() -> M3ToolStub {
    M3ToolStub::new("timeline_exclusions")
}

#[must_use]
pub fn required_permissions_pause(_params: &TimelinePauseParams) -> RequiredPermissions {
    required([Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_resume(_params: &TimelineResumeParams) -> RequiredPermissions {
    required([Permission::WriteStorage])
}

#[must_use]
pub fn required_permissions_exclusions(params: &TimelineExclusionsParams) -> RequiredPermissions {
    if params.add.as_deref().unwrap_or_default().is_empty()
        && params.remove.as_deref().unwrap_or_default().is_empty()
    {
        required([Permission::ReadStorage])
    } else {
        required([Permission::ReadStorage, Permission::WriteStorage])
    }
}

/// Storage handle, control gate, and (when this process is the daemon) the
/// live recorder, for one control-tool invocation.
type ControlContext = (Arc<Db>, Arc<RecorderControl>, Option<Arc<ActivityRecorder>>);

fn control_context(m3_state: &SharedM3State) -> Result<ControlContext, ErrorData> {
    let mut guard = m3_state.lock().map_err(|_poisoned| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "M3 service state lock poisoned",
        )
    })?;
    let db = guard.ensure_storage().map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("open storage for timeline recorder controls: {error}"),
        )
    })?;
    let control = guard
        .ensure_recorder_control()
        .map_err(|error| mcp_error(error_codes::TOOL_INTERNAL_ERROR, format!("{error:#}")))?;
    let recorder = guard.activity_recorder.clone();
    Ok((db, control, recorder))
}

/// Read-only handle to the recorder control gate, for status tools (`#842`
/// `timeline_stats`). Reuses [`control_context`] so the pause/exclusion state a
/// status read reports is the exact same gate the recorder write-path consults
/// — no second copy that could drift.
pub fn recorder_control_handle(
    m3_state: &SharedM3State,
) -> Result<Arc<RecorderControl>, ErrorData> {
    let (_db, control, _recorder) = control_context(m3_state)?;
    Ok(control)
}

fn internal(error: &anyhow::Error) -> ErrorData {
    mcp_error(error_codes::TOOL_INTERNAL_ERROR, format!("{error:#}"))
}

fn now_ts_ns() -> u64 {
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(i64::MAX);
    u64::try_from(nanos).unwrap_or(0)
}

pub fn pause_timeline(
    m3_state: &SharedM3State,
    params: &TimelinePauseParams,
    by_session: &str,
) -> Result<TimelinePauseResponse, ErrorData> {
    if params.duration_ms == Some(0) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "timeline_pause duration_ms must be >= 1 when provided",
        ));
    }
    let (db, control, recorder) = control_context(m3_state)?;
    let now_ns = now_ts_ns();
    let paused_until_ns = params
        .duration_ms
        .map(|duration_ms| now_ns.saturating_add(duration_ms.saturating_mul(1_000_000)));
    let recorder_live = recorder.is_some();
    let suppressed = recorder
        .as_ref()
        .map(|recorder| recorder.suppressed_counters());
    let (was_paused, boundary_row_written, state) = match recorder {
        Some(recorder) => {
            let outcome = recorder
                .pause(paused_until_ns, by_session)
                .map_err(|error| internal(&error))?;
            (
                outcome.was_paused,
                outcome.boundary_row_written,
                outcome.state,
            )
        }
        None => {
            // No live recorder holds this database, so nothing is being
            // written anyway; only the durable state changes and the next
            // recorder startup honors it.
            let was_paused = control.is_paused();
            let state = control
                .persist_pause(&db, paused_until_ns, now_ns, by_session)
                .map_err(|error| internal(&error))?;
            (was_paused, false, state)
        }
    };
    Ok(TimelinePauseResponse {
        paused: true,
        was_paused,
        paused_until_ns: state.paused_until_ns,
        recorder_live,
        boundary_row_written,
        persisted: true,
        changed_at_ns: state.changed_at_ns,
        suppressed_paused_total: suppressed.map(|(paused, _excluded)| paused),
        suppressed_excluded_total: suppressed.map(|(_paused, excluded)| excluded),
    })
}

pub fn resume_timeline(
    m3_state: &SharedM3State,
    _params: &TimelineResumeParams,
    by_session: &str,
) -> Result<TimelineResumeResponse, ErrorData> {
    let (db, control, recorder) = control_context(m3_state)?;
    let recorder_live = recorder.is_some();
    let suppressed = recorder
        .as_ref()
        .map(|recorder| recorder.suppressed_counters());
    let (was_paused, boundary_row_written, state) = match recorder {
        Some(recorder) => {
            let outcome = recorder
                .resume(by_session)
                .map_err(|error| internal(&error))?;
            (
                outcome.was_paused,
                outcome.boundary_row_written,
                outcome.state,
            )
        }
        None => {
            let was_paused = control.is_paused();
            let state = control
                .persist_resume(&db, now_ts_ns(), by_session)
                .map_err(|error| internal(&error))?;
            (was_paused, false, state)
        }
    };
    Ok(TimelineResumeResponse {
        paused: false,
        was_paused,
        recorder_live,
        boundary_row_written,
        persisted: true,
        changed_at_ns: state.changed_at_ns,
        suppressed_paused_total: suppressed.map(|(paused, _excluded)| paused),
        suppressed_excluded_total: suppressed.map(|(_paused, excluded)| excluded),
    })
}

pub fn update_timeline_exclusions(
    m3_state: &SharedM3State,
    params: &TimelineExclusionsParams,
    by_session: &str,
) -> Result<TimelineExclusionsResponse, ErrorData> {
    let add = params.add.clone().unwrap_or_default();
    let remove = params.remove.clone().unwrap_or_default();
    // Parameter problems are TOOL_PARAMS_INVALID, decided before anything
    // durable can change.
    for entry in add.iter().chain(remove.iter()) {
        validate_exclusion_entry(entry)
            .map_err(|error| mcp_error(error_codes::TOOL_PARAMS_INVALID, format!("{error:#}")))?;
    }
    let (db, control, recorder) = control_context(m3_state)?;
    for entry in &remove {
        let normalized = validate_exclusion_entry(entry)
            .map_err(|error| mcp_error(error_codes::TOOL_PARAMS_INVALID, format!("{error:#}")))?;
        if control.env_exclusions.contains(&normalized) {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "exclusion {normalized:?} comes from {TIMELINE_EXCLUDE_ENV} and cannot be \
                     removed at runtime; change the environment configuration instead"
                ),
            ));
        }
    }
    let (added, removed, persisted) = if add.is_empty() && remove.is_empty() {
        (Vec::new(), Vec::new(), false)
    } else {
        let update = control
            .persist_exclusion_update(&db, &add, &remove, now_ts_ns(), by_session)
            .map_err(|error| internal(&error))?;
        (update.added, update.removed, true)
    };
    let suppressed = recorder
        .as_ref()
        .map(|recorder| recorder.suppressed_counters());
    Ok(TimelineExclusionsResponse {
        env_exclusions: control.env_exclusions(),
        runtime_exclusions: control.runtime_exclusions(),
        effective_exclusions: control.effective_exclusions(),
        added,
        removed,
        recorder_live: recorder.is_some(),
        persisted,
        suppressed_paused_total: suppressed.map(|(paused, _excluded)| paused),
        suppressed_excluded_total: suppressed.map(|(_paused, excluded)| excluded),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let db = Db::open(dir.path(), synapse_core::SCHEMA_VERSION)
            .unwrap_or_else(|error| panic!("open temp db: {error}"));
        (dir, db)
    }

    #[test]
    fn hydrate_defaults_to_recording_with_no_row() {
        let (_dir, db) = temp_db();
        let control = RecorderControl::hydrate(&db).unwrap_or_else(|error| panic!("{error:#}"));
        assert!(!control.is_paused());
        assert!(control.effective_exclusions().is_empty());
        assert_eq!(control.suppress_reason(Some("notepad.exe")), None);
    }

    #[test]
    fn pause_persists_and_survives_rehydration() {
        let (_dir, db) = temp_db();
        let control = RecorderControl::hydrate(&db).unwrap_or_else(|error| panic!("{error:#}"));
        control
            .persist_pause(&db, Some(42), 7, "session-a")
            .unwrap_or_else(|error| panic!("{error:#}"));
        assert_eq!(
            control.suppress_reason(Some("notepad.exe")),
            Some(SuppressReason::Paused)
        );
        // Source of truth: a fresh hydration from the same db must be paused.
        let rehydrated = RecorderControl::hydrate(&db).unwrap_or_else(|error| panic!("{error:#}"));
        assert!(rehydrated.is_paused());
        assert_eq!(rehydrated.paused_until_ns(), Some(42));
        assert!(rehydrated.auto_resume_due(42));
        assert!(!rehydrated.auto_resume_due(41));
        rehydrated
            .persist_resume(&db, 9, "session-a")
            .unwrap_or_else(|error| panic!("{error:#}"));
        let after_resume =
            RecorderControl::hydrate(&db).unwrap_or_else(|error| panic!("{error:#}"));
        assert!(!after_resume.is_paused());
        assert_eq!(after_resume.paused_until_ns(), None);
    }

    #[test]
    fn exclusions_union_env_and_runtime_case_insensitively() {
        let (_dir, db) = temp_db();
        let env = parse_env_exclusions(Some("KeePass.exe, signal.exe".to_owned()))
            .unwrap_or_else(|error| panic!("{error:#}"));
        let control = RecorderControl {
            paused: AtomicBool::new(false),
            paused_until_ns: AtomicU64::new(0),
            env_exclusions: env,
            runtime_exclusions: Mutex::new(BTreeSet::new()),
            mutation: Mutex::new(()),
        };
        let update = control
            .persist_exclusion_update(&db, &["Chrome.EXE".to_owned()], &[], 1, "session-b")
            .unwrap_or_else(|error| panic!("{error:#}"));
        assert_eq!(update.added, vec!["chrome.exe".to_owned()]);
        assert_eq!(
            control.suppress_reason(Some("CHROME.exe")),
            Some(SuppressReason::ExcludedApp)
        );
        assert_eq!(
            control.suppress_reason(Some("keepass.EXE")),
            Some(SuppressReason::ExcludedApp)
        );
        assert_eq!(control.suppress_reason(Some("code.exe")), None);
        assert_eq!(control.suppress_reason(None), None);
        // Env entries are immutable at runtime.
        let env_remove =
            control.persist_exclusion_update(&db, &[], &["keepass.exe".to_owned()], 2, "session-b");
        assert!(env_remove.is_err(), "env baseline removal must be refused");
        // Runtime removal works and persists.
        control
            .persist_exclusion_update(&db, &[], &["chrome.exe".to_owned()], 3, "session-b")
            .unwrap_or_else(|error| panic!("{error:#}"));
        assert_eq!(control.suppress_reason(Some("chrome.exe")), None);
        let persisted = load_persisted(&db).unwrap_or_else(|error| panic!("{error:#}"));
        assert!(persisted.runtime_exclusions.is_empty());
    }

    #[test]
    fn invalid_entries_are_rejected() {
        assert!(validate_exclusion_entry("").is_err(), "empty");
        assert!(validate_exclusion_entry("   ").is_err(), "blank");
        assert!(
            validate_exclusion_entry(r"C:\tools\app.exe").is_err(),
            "path"
        );
        assert!(validate_exclusion_entry("a/b.exe").is_err(), "separator");
        assert!(
            validate_exclusion_entry(&"x".repeat(257)).is_err(),
            "oversized"
        );
        assert_eq!(
            validate_exclusion_entry(" Notepad.EXE ").unwrap_or_else(|error| panic!("{error:#}")),
            "notepad.exe"
        );
    }

    #[test]
    fn corrupt_control_row_is_a_hard_error() {
        let (_dir, db) = temp_db();
        db.put_batch_pressure_bypass(
            cf::CF_KV,
            [(TIMELINE_CONTROL_KEY.to_vec(), b"not-json".to_vec())],
        )
        .unwrap_or_else(|error| panic!("{error}"));
        db.flush().unwrap_or_else(|error| panic!("{error}"));
        let error = RecorderControl::hydrate(&db).expect_err("corrupt row must refuse hydration");
        assert!(
            format!("{error:#}").contains("decode timeline control row"),
            "error must name the corrupt row: {error:#}"
        );
    }
}
