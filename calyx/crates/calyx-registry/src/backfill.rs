//! Durable lazy backfill scheduler state.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use calyx_core::{CalyxError, CxId, LensId, PanelSlotId, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackfillPriority {
    Normal,
    Hot,
    Kernel,
}

impl BackfillPriority {
    const fn rank(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Hot => 1,
            Self::Kernel => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillRequest {
    pub panel_slot: PanelSlotId,
    pub lens_id: LensId,
    pub priority: BackfillPriority,
    pub candidates: Vec<CxId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillConfig {
    pub max_concurrent: usize,
    pub batch_size: usize,
    pub throttle_ms: u64,
}

impl Default for BackfillConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            batch_size: 16,
            throttle_ms: 50,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillBatch {
    pub panel_slot: PanelSlotId,
    pub lens_id: LensId,
    pub candidates: Vec<CxId>,
    pub throttled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillWatermark {
    pub panel_slot: PanelSlotId,
    pub lens_id: LensId,
    pub priority: BackfillPriority,
    pub processed: usize,
    pub pending: usize,
    pub in_flight: usize,
    pub complete: bool,
    pub last_processed: Option<CxId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedScheduler {
    config: BackfillConfig,
    next_allowed_ms: u64,
    requests: BTreeMap<String, RequestState>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RequestState {
    request: BackfillRequest,
    next_index: usize,
    in_flight: Vec<CxId>,
    last_processed: Option<CxId>,
    complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackfillScheduler {
    path: PathBuf,
    state: PersistedScheduler,
}

impl BackfillScheduler {
    pub fn open(path: impl Into<PathBuf>, config: BackfillConfig) -> Result<Self> {
        let path = path.into();
        if path.exists() {
            let bytes = fs::read(&path).map_err(|err| io_error(&path, err))?;
            let mut state: PersistedScheduler = serde_json::from_slice(&bytes)
                .map_err(|err| CalyxError::stale_derived(err.to_string()))?;
            let needs_persist = state.config != config
                || state
                    .requests
                    .values()
                    .any(|request| !request.in_flight.is_empty());
            state.config = config;
            for request in state.requests.values_mut() {
                request.in_flight.clear();
            }
            let scheduler = Self { path, state };
            if needs_persist {
                scheduler.persist()?;
            }
            return Ok(scheduler);
        }
        Ok(Self {
            path,
            state: PersistedScheduler {
                config,
                next_allowed_ms: 0,
                requests: BTreeMap::new(),
            },
        })
    }

    pub fn enqueue(&mut self, request: BackfillRequest) -> Result<()> {
        self.mutate_and_persist(|state| {
            let key = request_key(request.panel_slot, request.lens_id);
            match state.requests.entry(key) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(RequestState {
                        request,
                        next_index: 0,
                        in_flight: Vec::new(),
                        last_processed: None,
                        complete: false,
                    });
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    merge_request(entry.get_mut(), request);
                }
            }
            Ok(())
        })
    }

    pub fn claim_next_batch(&mut self, now_ms: u64) -> Result<Option<BackfillBatch>> {
        if now_ms < self.state.next_allowed_ms {
            return Ok(None);
        }
        if self.active_count() >= self.state.config.max_concurrent.max(1) {
            return Ok(None);
        }
        let Some(key) = self.next_request_key() else {
            return Ok(None);
        };
        let batch_size = self.state.config.batch_size.max(1);
        self.mutate_and_persist(|state| {
            let state = state.requests.get_mut(&key).expect("key selected from map");
            let start = state.next_index;
            let end = (start + batch_size).min(state.request.candidates.len());
            if start >= end {
                state.complete = true;
                return Ok(None);
            }
            state.in_flight = state.request.candidates[start..end].to_vec();
            Ok(Some(BackfillBatch {
                panel_slot: state.request.panel_slot,
                lens_id: state.request.lens_id,
                candidates: state.in_flight.clone(),
                throttled: false,
            }))
        })
    }

    pub fn complete_batch(
        &mut self,
        panel_slot: PanelSlotId,
        lens_id: LensId,
        now_ms: u64,
    ) -> Result<()> {
        self.mutate_and_persist(|state| {
            let key = request_key(panel_slot, lens_id);
            let request = state.requests.get_mut(&key).ok_or_else(|| {
                CalyxError::stale_derived(format!("backfill request {key} missing"))
            })?;
            if request.in_flight.is_empty() {
                return Err(CalyxError::stale_derived(format!(
                    "backfill request {key} has no in-flight batch"
                )));
            }
            request.next_index += request.in_flight.len();
            request.last_processed = request.in_flight.last().copied();
            request.in_flight.clear();
            request.complete = request.next_index >= request.request.candidates.len();
            state.next_allowed_ms = now_ms.saturating_add(state.config.throttle_ms);
            Ok(())
        })
    }

    pub fn watermarks(&self) -> Vec<BackfillWatermark> {
        self.state
            .requests
            .values()
            .map(|state| {
                let total = state.request.candidates.len();
                BackfillWatermark {
                    panel_slot: state.request.panel_slot,
                    lens_id: state.request.lens_id,
                    priority: state.request.priority,
                    processed: state.next_index,
                    pending: total.saturating_sub(state.next_index),
                    in_flight: state.in_flight.len(),
                    complete: state.complete,
                    last_processed: state.last_processed,
                }
            })
            .collect()
    }

    pub fn persist(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|err| io_error(parent, err))?;
        }
        let bytes = serde_json::to_vec_pretty(&self.state)
            .map_err(|err| CalyxError::stale_derived(err.to_string()))?;
        atomic_write(&self.path, &bytes)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn mutate_and_persist<T>(
        &mut self,
        mutate: impl FnOnce(&mut PersistedScheduler) -> Result<T>,
    ) -> Result<T> {
        let before = self.state.clone();
        let output = match mutate(&mut self.state) {
            Ok(output) => output,
            Err(error) => {
                self.state = before;
                return Err(error);
            }
        };
        if let Err(error) = self.persist() {
            self.state = before;
            if let Err(rollback_error) = self.persist() {
                return Err(CalyxError::stale_derived(format!(
                    "scheduler persist failed: {error}; rollback persist failed: {rollback_error}"
                )));
            }
            return Err(error);
        }
        Ok(output)
    }

    fn active_count(&self) -> usize {
        self.state
            .requests
            .values()
            .filter(|state| !state.in_flight.is_empty())
            .count()
    }

    fn next_request_key(&self) -> Option<String> {
        self.state
            .requests
            .iter()
            .filter(|(_, state)| {
                !state.complete
                    && state.in_flight.is_empty()
                    && state.next_index < state.request.candidates.len()
            })
            .max_by_key(|(_, state)| {
                (
                    state.request.priority.rank(),
                    std::cmp::Reverse(state.next_index),
                )
            })
            .map(|(key, _)| key.clone())
    }
}

fn merge_request(existing: &mut RequestState, request: BackfillRequest) {
    if request.priority.rank() > existing.request.priority.rank() {
        existing.request.priority = request.priority;
    }
    let mut seen = existing
        .request
        .candidates
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let mut added = false;
    for candidate in request.candidates {
        if seen.insert(candidate) {
            existing.request.candidates.push(candidate);
            added = true;
        }
    }
    if added {
        existing.complete = false;
    }
}

fn request_key(panel_slot: PanelSlotId, lens_id: LensId) -> String {
    format!("{panel_slot}:{lens_id}")
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = temp_path(path)?;
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .map_err(|err| io_error(&tmp, err))?;
        file.write_all(bytes).map_err(|err| io_error(&tmp, err))?;
        file.sync_all().map_err(|err| io_error(&tmp, err))?;
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(io_error(path, err));
    }
    injected_post_rename_failure(path)?;
    sync_parent(parent)
}

fn temp_path(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            CalyxError::stale_derived(format!("invalid scheduler path {}", path.display()))
        })?;
    Ok(path.with_file_name(format!(".{name}.tmp-{}", std::process::id())))
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<()> {
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|err| io_error(parent, err))
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> Result<()> {
    Ok(())
}

#[cfg(debug_assertions)]
fn injected_post_rename_failure(path: &Path) -> Result<()> {
    let marker = post_rename_failure_marker(path)?;
    if !marker.exists() {
        return Ok(());
    }
    fs::remove_file(&marker).map_err(|err| io_error(&marker, err))?;
    Err(CalyxError::stale_derived(format!(
        "{}: injected post-rename persist failure",
        path.display()
    )))
}

#[cfg(debug_assertions)]
fn post_rename_failure_marker(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            CalyxError::stale_derived(format!("invalid scheduler path {}", path.display()))
        })?;
    Ok(path.with_file_name(format!(".{name}.fail-after-rename-once")))
}

#[cfg(not(debug_assertions))]
fn injected_post_rename_failure(_path: &Path) -> Result<()> {
    Ok(())
}

fn io_error(path: &Path, err: std::io::Error) -> CalyxError {
    CalyxError::stale_derived(format!("{}: {err}", path.display()))
}
