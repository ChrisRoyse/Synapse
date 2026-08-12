//! Typed CDP helpers for file uploads (#1101-#1103).
//!
//! The normal authenticated Chrome profile is debugger-free. File uploads run
//! only on Synapse-owned raw-CDP automation profiles through
//! `DOM.setFileInputFiles` and
//! `Page.setInterceptFileChooserDialog`/`Page.fileChooserOpened`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, DescribeNodeParams, ResolveNodeParams, SetFileInputFilesParams,
};
use chromiumoxide::cdp::browser_protocol::page::{
    EnableParams as PageEnableParams, EventFileChooserOpened, SetInterceptFileChooserDialogParams,
};
use chromiumoxide::cdp::js_protocol::runtime::{CallFunctionOnParams, EvaluateParams};
use chromiumoxide::{Browser, Page};
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::{A11yError, A11yResult};

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CdpFileChooserEntry {
    pub seq: u64,
    pub frame_id: String,
    pub mode: String,
    pub backend_node_id: Option<i64>,
    pub opened_at_unix_ms: u64,
    pub pending: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CdpFileInputFile {
    pub name: String,
    pub size: u64,
    #[serde(rename = "type")]
    pub file_type: String,
    pub last_modified: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CdpFileInputState {
    pub resolved_by: String,
    pub match_count: u32,
    pub backend_node_id: i64,
    pub tag_name: String,
    pub type_attr: String,
    pub id: String,
    pub name_attr: String,
    pub accept: String,
    pub multiple: bool,
    pub webkitdirectory: bool,
    pub disabled: bool,
    pub file_count: usize,
    pub files: Vec<CdpFileInputFile>,
    pub value: String,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CdpFileChooserRecord {
    pub seq: u64,
    pub frame_id: String,
    pub mode: String,
    pub backend_node_id: Option<i64>,
    pub opened_at_unix_ms: u64,
    pub pending: bool,
    pub handled_at_unix_ms: Option<u64>,
    pub canceled_at_unix_ms: Option<u64>,
    pub requested_file_count: Option<usize>,
    pub file_names: Vec<String>,
    pub input: Option<CdpFileInputState>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CdpFileChooserStatus {
    pub newly_armed: bool,
    pub target_id: String,
    pub armed_at_unix_ms: u64,
    pub pending_chooser: Option<CdpFileChooserRecord>,
    pub entries: Vec<CdpFileChooserRecord>,
    pub next_cursor: u64,
    pub returned: usize,
    pub total_buffered: usize,
    pub dropped: u64,
    pub opened_count: u64,
    pub handled_count: u64,
    pub canceled_count: u64,
    pub error_count: u64,
}

/// Physical teardown/readback for persistent raw-CDP chooser interception.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CdpFileChooserDrainReadback {
    pub found: usize,
    pub intercepts_disabled: usize,
    pub listener_tasks_drained: usize,
    pub handler_tasks_drained: usize,
    pub active_after: usize,
    pub failures: Vec<String>,
}

#[derive(Deserialize)]
struct FileInputValue {
    tag_name: String,
    type_attr: String,
    id: String,
    name_attr: String,
    accept: String,
    multiple: bool,
    webkitdirectory: bool,
    disabled: bool,
    file_count: usize,
    files: Vec<CdpFileInputFile>,
    value: String,
}

pub fn cdp_set_file_input_files_params_for_backend_node(
    backend_node_id: i64,
    files: &[String],
) -> A11yResult<SetFileInputFilesParams> {
    if backend_node_id <= 0 {
        return Err(A11yError::CdpAttachFailed {
            detail: format!("backend_node_id must be positive, got {backend_node_id}"),
        });
    }
    SetFileInputFilesParams::builder()
        .files(files.iter().cloned())
        .backend_node_id(BackendNodeId::new(backend_node_id))
        .build()
        .map_err(|error| A11yError::CdpAttachFailed {
            detail: format!("build DOM.setFileInputFiles params: {error}"),
        })
}

pub async fn cdp_set_file_input_files_by_backend_node(
    page: &Page,
    backend_node_id: i64,
    files: &[String],
) -> A11yResult<()> {
    let params = cdp_set_file_input_files_params_for_backend_node(backend_node_id, files)?;
    page.execute(params)
        .await
        .map_err(|error| A11yError::CdpAxtreeFailed {
            detail: format!("DOM.setFileInputFiles backendNodeId={backend_node_id}: {error}"),
        })?;
    Ok(())
}

pub fn cdp_intercept_file_chooser_params(
    enabled: bool,
    cancel: Option<bool>,
) -> A11yResult<SetInterceptFileChooserDialogParams> {
    let mut builder = SetInterceptFileChooserDialogParams::builder().enabled(enabled);
    if let Some(cancel) = cancel {
        builder = builder.cancel(cancel);
    }
    builder.build().map_err(|error| A11yError::CdpAttachFailed {
        detail: format!("build Page.setInterceptFileChooserDialog params: {error}"),
    })
}

pub async fn cdp_set_intercept_file_chooser(
    page: &Page,
    enabled: bool,
    cancel: Option<bool>,
) -> A11yResult<()> {
    let params = cdp_intercept_file_chooser_params(enabled, cancel)?;
    page.execute(params)
        .await
        .map_err(|error| A11yError::CdpAxtreeFailed {
            detail: format!("Page.setInterceptFileChooserDialog enabled={enabled}: {error}"),
        })?;
    Ok(())
}

#[must_use]
pub fn cdp_file_chooser_entry_from_event(
    event: &EventFileChooserOpened,
    seq: u64,
    opened_at_unix_ms: u64,
) -> CdpFileChooserEntry {
    CdpFileChooserEntry {
        seq,
        frame_id: event.frame_id.as_ref().to_owned(),
        mode: event.mode.as_ref().to_owned(),
        backend_node_id: event.backend_node_id.map(|id| *id.inner()),
        opened_at_unix_ms,
        pending: true,
    }
}

const FILE_INPUT_OBJECT_GROUP: &str = "synapse-file-input-readback";
pub const DEFAULT_FILE_CHOOSER_CAPACITY: usize = 128;
pub const MAX_FILE_CHOOSER_CAPACITY: usize = 1000;

#[derive(Default)]
struct FileChooserState {
    entries: VecDeque<CdpFileChooserRecord>,
    next_seq: u64,
    dropped: u64,
    pending_seq: Option<u64>,
    opened_count: u64,
    handled_count: u64,
    canceled_count: u64,
    error_count: u64,
    capacity: usize,
}

impl FileChooserState {
    fn push(&mut self, event: &EventFileChooserOpened) {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        while self.entries.len() >= self.capacity.max(1) {
            self.entries.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.pending_seq = Some(seq);
        self.opened_count = self.opened_count.saturating_add(1);
        self.entries.push_back(CdpFileChooserRecord {
            seq,
            frame_id: event.frame_id.as_ref().to_owned(),
            mode: event.mode.as_ref().to_owned(),
            backend_node_id: event.backend_node_id.map(|id| *id.inner()),
            opened_at_unix_ms: now_unix_ms(),
            pending: true,
            handled_at_unix_ms: None,
            canceled_at_unix_ms: None,
            requested_file_count: None,
            file_names: Vec::new(),
            input: None,
            error: None,
        });
    }

    fn pending(&self) -> Option<CdpFileChooserRecord> {
        let seq = self.pending_seq?;
        self.entries.iter().find(|entry| entry.seq == seq).cloned()
    }
}

struct FileChooserSlot {
    target_id: String,
    armed_at_unix_ms: u64,
    state: Arc<Mutex<FileChooserState>>,
    page: Page,
    _browser: Browser,
    handler_task: Mutex<Option<JoinHandle<()>>>,
    listener_task: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for FileChooserSlot {
    fn drop(&mut self) {
        if let Ok(task) = self.handler_task.get_mut()
            && let Some(task) = task.take()
        {
            task.abort();
        }
        if let Ok(task) = self.listener_task.get_mut()
            && let Some(task) = task.take()
        {
            task.abort();
        }
    }
}

fn chooser_registry() -> &'static Mutex<HashMap<String, Arc<FileChooserSlot>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<FileChooserSlot>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn chooser_key(endpoint: &str, target_id: &str) -> String {
    format!("{endpoint}\n{target_id}")
}

/// Resolves the exact active element to a backend node on a raw-CDP target.
pub async fn cdp_active_element_backend_node(endpoint: &str, target_id: &str) -> A11yResult<i64> {
    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|error| A11yError::CdpAttachFailed {
                detail: format!("active file input connect {endpoint}: {error}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });
    let result = async {
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        let evaluated = page
            .execute(
                EvaluateParams::builder()
                    .expression("document.activeElement")
                    .object_group(FILE_INPUT_OBJECT_GROUP)
                    .return_by_value(false)
                    .await_promise(false)
                    .build()
                    .map_err(|error| A11yError::CdpAxtreeFailed {
                        detail: format!("active file input evaluate params: {error}"),
                    })?,
            )
            .await
            .map_err(|error| A11yError::CdpAxtreeFailed {
                detail: format!("active file input Runtime.evaluate: {error}"),
            })?
            .result;
        let object_id = evaluated
            .result
            .object_id
            .ok_or_else(|| A11yError::CdpAxtreeFailed {
                detail: "document.activeElement returned no object id".to_owned(),
            })?;
        let described = page
            .execute(DescribeNodeParams::builder().object_id(object_id).build())
            .await
            .map_err(|error| A11yError::CdpAxtreeFailed {
                detail: format!("active file input DOM.describeNode: {error}"),
            })?;
        Ok(*described.node.backend_node_id.inner())
    }
    .await;
    crate::cdp_action::finish_chromiumoxide_handler(
        result,
        handler_task,
        "active file input backend read",
    )
    .await
}

/// Sets or clears files on an exact backend node and independently reads the
/// live `HTMLInputElement.files` state before returning.
pub async fn cdp_set_file_input_files_target(
    endpoint: &str,
    target_id: &str,
    backend_node_id: i64,
    files: &[String],
    resolved_by: &str,
    match_count: u32,
) -> A11yResult<CdpFileInputState> {
    let _operation_guard = crate::cdp_network::durable_browser_mutation_operation_guard().await;
    if !crate::cdp_network::durable_browser_mutation_owners_enabled() {
        return Err(A11yError::CdpAttachFailed {
            detail: "durable browser mutation owners are disabled; refusing raw-CDP file upload"
                .to_owned(),
        });
    }
    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|error| A11yError::CdpAttachFailed {
                detail: format!("file upload connect {endpoint}: {error}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });
    let result = async {
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        cdp_set_file_input_files_by_backend_node(&page, backend_node_id, files).await?;
        read_file_input_state(&page, backend_node_id, resolved_by, match_count).await
    }
    .await;
    crate::cdp_action::finish_chromiumoxide_handler(result, handler_task, "file upload").await
}

/// Arms persistent chooser capture for an exact raw-CDP target.
pub async fn cdp_file_chooser_ensure(
    endpoint: &str,
    target_id: &str,
    capacity: usize,
) -> A11yResult<CdpFileChooserStatus> {
    let _operation_guard = crate::cdp_network::durable_browser_mutation_operation_guard().await;
    if !crate::cdp_network::durable_browser_mutation_owners_enabled() {
        return Err(A11yError::CdpAttachFailed {
            detail: "durable browser mutation owners are disabled; refusing file chooser capture"
                .to_owned(),
        });
    }
    let key = chooser_key(endpoint, target_id);
    if let Some(slot) = chooser_registry()
        .lock()
        .ok()
        .and_then(|registry| registry.get(&key).cloned())
    {
        return chooser_status(&slot, None, usize::MAX, false);
    }
    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|error| A11yError::CdpAttachFailed {
                detail: format!("file chooser connect {endpoint}: {error}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });
    let page = match crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await {
        Ok(page) => page,
        Err(error) => {
            handler_task.abort();
            return Err(error);
        }
    };
    if let Err(error) = page.execute(PageEnableParams::default()).await {
        handler_task.abort();
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!("Page.enable for file chooser capture: {error}"),
        });
    }
    let mut events = match page.event_listener::<EventFileChooserOpened>().await {
        Ok(events) => events,
        Err(error) => {
            handler_task.abort();
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Page.fileChooserOpened: {error}"),
            });
        }
    };
    if let Err(error) = cdp_set_intercept_file_chooser(&page, true, Some(true)).await {
        handler_task.abort();
        return Err(error);
    }
    let state = Arc::new(Mutex::new(FileChooserState {
        capacity: capacity.clamp(1, MAX_FILE_CHOOSER_CAPACITY),
        ..FileChooserState::default()
    }));
    let listener_state = Arc::clone(&state);
    let listener_task = tokio::spawn(async move {
        while let Some(event) = events.next().await {
            if !crate::cdp_network::durable_browser_mutation_owners_enabled() {
                break;
            }
            if let Ok(mut state) = listener_state.lock() {
                state.push(&event);
            }
        }
    });
    let slot = Arc::new(FileChooserSlot {
        target_id: target_id.to_owned(),
        armed_at_unix_ms: now_unix_ms(),
        state,
        page,
        _browser: browser,
        handler_task: Mutex::new(Some(handler_task)),
        listener_task: Mutex::new(Some(listener_task)),
    });
    let mut registry = chooser_registry()
        .lock()
        .map_err(|_| A11yError::CdpAxtreeFailed {
            detail: "file chooser registry lock poisoned".to_owned(),
        })?;
    if let Some(existing) = registry.get(&key).cloned() {
        drop(registry);
        return chooser_status(&existing, None, usize::MAX, false);
    }
    registry.insert(key, Arc::clone(&slot));
    drop(registry);
    chooser_status(&slot, None, usize::MAX, true)
}

/// Reads persistent chooser history without mutation.
pub fn cdp_file_chooser_read(
    endpoint: &str,
    target_id: &str,
    since_seq: Option<u64>,
    limit: usize,
) -> A11yResult<CdpFileChooserStatus> {
    let slot = chooser_slot(endpoint, target_id)?;
    chooser_status(&slot, since_seq, limit, false)
}

/// Sets files on the pending chooser backend node, then reads the real input.
pub async fn cdp_file_chooser_set_pending(
    endpoint: &str,
    target_id: &str,
    files: &[String],
) -> A11yResult<CdpFileChooserRecord> {
    let _operation_guard = crate::cdp_network::durable_browser_mutation_operation_guard().await;
    if !crate::cdp_network::durable_browser_mutation_owners_enabled() {
        return Err(A11yError::CdpAttachFailed {
            detail: "durable browser mutation owners are disabled; refusing pending file chooser mutation"
                .to_owned(),
        });
    }
    let slot = chooser_slot(endpoint, target_id)?;
    let pending = slot
        .state
        .lock()
        .map_err(|_| A11yError::CdpAxtreeFailed {
            detail: "file chooser state lock poisoned".to_owned(),
        })?
        .pending()
        .ok_or_else(|| A11yError::CdpAxtreeFailed {
            detail: "no pending file chooser exists for this target".to_owned(),
        })?;
    let backend_node_id = pending
        .backend_node_id
        .ok_or_else(|| A11yError::CdpAxtreeFailed {
            detail: format!("pending chooser seq {} has no backend node id", pending.seq),
        })?;
    cdp_set_file_input_files_by_backend_node(&slot.page, backend_node_id, files).await?;
    let input = read_file_input_state(&slot.page, backend_node_id, "pending_chooser", 1).await?;
    let file_names = input.files.iter().map(|file| file.name.clone()).collect();
    let mut state = slot.state.lock().map_err(|_| A11yError::CdpAxtreeFailed {
        detail: "file chooser state lock poisoned".to_owned(),
    })?;
    state.pending_seq = None;
    let entry = state
        .entries
        .iter_mut()
        .find(|entry| entry.seq == pending.seq)
        .ok_or_else(|| A11yError::CdpAxtreeFailed {
            detail: "pending chooser disappeared before commit".to_owned(),
        })?;
    entry.pending = false;
    entry.handled_at_unix_ms = Some(now_unix_ms());
    entry.requested_file_count = Some(files.len());
    entry.file_names = file_names;
    entry.input = Some(input);
    let readback = entry.clone();
    state.handled_count = state.handled_count.saturating_add(1);
    Ok(readback)
}

/// Cancels only the pending chooser record for the exact target.
pub async fn cdp_file_chooser_cancel_pending(
    endpoint: &str,
    target_id: &str,
) -> A11yResult<CdpFileChooserRecord> {
    let _operation_guard = crate::cdp_network::durable_browser_mutation_operation_guard().await;
    if !crate::cdp_network::durable_browser_mutation_owners_enabled() {
        return Err(A11yError::CdpAttachFailed {
            detail: "durable browser mutation owners are disabled; refusing pending file chooser cancellation"
                .to_owned(),
        });
    }
    let slot = chooser_slot(endpoint, target_id)?;
    let mut state = slot.state.lock().map_err(|_| A11yError::CdpAxtreeFailed {
        detail: "file chooser state lock poisoned".to_owned(),
    })?;
    let seq = state
        .pending_seq
        .take()
        .ok_or_else(|| A11yError::CdpAxtreeFailed {
            detail: "no pending file chooser exists for this target".to_owned(),
        })?;
    let entry = state
        .entries
        .iter_mut()
        .find(|entry| entry.seq == seq)
        .ok_or_else(|| A11yError::CdpAxtreeFailed {
            detail: "pending chooser disappeared before cancel".to_owned(),
        })?;
    entry.pending = false;
    entry.canceled_at_unix_ms = Some(now_unix_ms());
    let readback = entry.clone();
    state.canceled_count = state.canceled_count.saturating_add(1);
    Ok(readback)
}

/// Independent process-global readback for active chooser intercept owners.
pub fn cdp_file_chooser_active_count_readback() -> Result<usize, String> {
    chooser_registry()
        .lock()
        .map(|registry| registry.len())
        .map_err(|_| "file chooser registry lock is poisoned".to_owned())
}

/// Disables every physical chooser intercept and drains its autonomous tasks.
/// The caller must close the durable-owner gate and hold the shared operation
/// lock before entering this teardown boundary.
pub async fn cdp_file_chooser_disable_and_drain_all() -> CdpFileChooserDrainReadback {
    let mut failures = Vec::new();
    let slots = match chooser_registry().lock() {
        Ok(mut registry) => std::mem::take(&mut *registry),
        Err(_) => {
            return CdpFileChooserDrainReadback {
                found: usize::MAX,
                intercepts_disabled: 0,
                listener_tasks_drained: 0,
                handler_tasks_drained: 0,
                active_after: usize::MAX,
                failures: vec!["file chooser registry lock is poisoned".to_owned()],
            };
        }
    };
    let found = slots.len();
    let mut intercepts_disabled = 0usize;
    let mut listener_tasks_drained = 0usize;
    let mut handler_tasks_drained = 0usize;
    for slot in slots.into_values() {
        if let Err(error) = cdp_set_intercept_file_chooser(&slot.page, false, None).await {
            failures.push(format!(
                "disable file chooser intercept for target {:?}: {error}",
                slot.target_id
            ));
        } else {
            intercepts_disabled = intercepts_disabled.saturating_add(1);
        }
        match take_file_chooser_task(&slot.listener_task, "listener", &slot.target_id) {
            Ok(Some(task)) => {
                if abort_and_drain_file_chooser_task(task).await {
                    listener_tasks_drained = listener_tasks_drained.saturating_add(1);
                } else {
                    failures.push(format!(
                        "file chooser listener task did not drain for target {:?}",
                        slot.target_id
                    ));
                }
            }
            Ok(None) => failures.push(format!(
                "file chooser listener task was already absent for target {:?}",
                slot.target_id
            )),
            Err(error) => failures.push(error),
        }
        match take_file_chooser_task(&slot.handler_task, "handler", &slot.target_id) {
            Ok(Some(task)) => {
                if abort_and_drain_file_chooser_task(task).await {
                    handler_tasks_drained = handler_tasks_drained.saturating_add(1);
                } else {
                    failures.push(format!(
                        "file chooser handler task did not drain for target {:?}",
                        slot.target_id
                    ));
                }
            }
            Ok(None) => failures.push(format!(
                "file chooser handler task was already absent for target {:?}",
                slot.target_id
            )),
            Err(error) => failures.push(error),
        }
    }
    let active_after = match cdp_file_chooser_active_count_readback() {
        Ok(count) => count,
        Err(error) => {
            failures.push(error);
            usize::MAX
        }
    };
    CdpFileChooserDrainReadback {
        found,
        intercepts_disabled,
        listener_tasks_drained,
        handler_tasks_drained,
        active_after,
        failures,
    }
}

fn take_file_chooser_task(
    task: &Mutex<Option<JoinHandle<()>>>,
    kind: &str,
    target_id: &str,
) -> Result<Option<JoinHandle<()>>, String> {
    task.lock()
        .map(|mut task| task.take())
        .map_err(|_| format!("file chooser {kind} task lock poisoned for target {target_id:?}"))
}

async fn abort_and_drain_file_chooser_task(task: JoinHandle<()>) -> bool {
    task.abort();
    match task.await {
        Ok(()) => true,
        Err(error) => error.is_cancelled(),
    }
}

fn chooser_slot(endpoint: &str, target_id: &str) -> A11yResult<Arc<FileChooserSlot>> {
    chooser_registry()
        .lock()
        .map_err(|_| A11yError::CdpAxtreeFailed {
            detail: "file chooser registry lock poisoned".to_owned(),
        })?
        .get(&chooser_key(endpoint, target_id))
        .cloned()
        .ok_or_else(|| A11yError::CdpAxtreeFailed {
            detail: "file chooser capture is not armed for this target; call arm_chooser first"
                .to_owned(),
        })
}

fn chooser_status(
    slot: &FileChooserSlot,
    since_seq: Option<u64>,
    limit: usize,
    newly_armed: bool,
) -> A11yResult<CdpFileChooserStatus> {
    let state = slot.state.lock().map_err(|_| A11yError::CdpAxtreeFailed {
        detail: "file chooser state lock poisoned".to_owned(),
    })?;
    let cursor = since_seq.unwrap_or(0);
    let entries = state
        .entries
        .iter()
        .filter(|entry| entry.seq >= cursor)
        .take(limit.max(1))
        .cloned()
        .collect::<Vec<_>>();
    Ok(CdpFileChooserStatus {
        newly_armed,
        target_id: slot.target_id.clone(),
        armed_at_unix_ms: slot.armed_at_unix_ms,
        pending_chooser: state.pending(),
        returned: entries.len(),
        total_buffered: state.entries.len(),
        entries,
        next_cursor: state.next_seq,
        dropped: state.dropped,
        opened_count: state.opened_count,
        handled_count: state.handled_count,
        canceled_count: state.canceled_count,
        error_count: state.error_count,
    })
}

async fn read_file_input_state(
    page: &Page,
    backend_node_id: i64,
    resolved_by: &str,
    match_count: u32,
) -> A11yResult<CdpFileInputState> {
    let resolved = page
        .execute(
            ResolveNodeParams::builder()
                .backend_node_id(BackendNodeId::new(backend_node_id))
                .object_group(FILE_INPUT_OBJECT_GROUP)
                .build(),
        )
        .await
        .map_err(|error| A11yError::CdpAxtreeFailed {
            detail: format!("resolve file input backendNodeId={backend_node_id}: {error}"),
        })?;
    let object_id =
        resolved
            .object
            .object_id
            .clone()
            .ok_or_else(|| A11yError::CdpAxtreeFailed {
                detail: format!("file input backendNodeId={backend_node_id} returned no object id"),
            })?;
    let declaration = r#"function() {
      if (!(this instanceof HTMLInputElement) || String(this.type).toLowerCase() !== "file") {
        throw new Error(`resolved node is not input[type=file]: ${this && this.tagName}:${this && this.type}`);
      }
      return {
        tag_name: String(this.tagName || ""), type_attr: String(this.type || ""),
        id: String(this.id || ""), name_attr: String(this.name || ""),
        accept: String(this.accept || ""), multiple: Boolean(this.multiple),
        webkitdirectory: Boolean(this.webkitdirectory), disabled: Boolean(this.disabled),
        file_count: this.files ? this.files.length : 0,
        files: Array.from(this.files || []).map(file => ({name:file.name,size:file.size,type:file.type,last_modified:file.lastModified})),
        value: String(this.value || "")
      };
    }"#;
    let evaluated = page
        .execute(
            CallFunctionOnParams::builder()
                .function_declaration(declaration)
                .object_id(object_id)
                .return_by_value(true)
                .await_promise(false)
                .build()
                .map_err(|error| A11yError::CdpAxtreeFailed {
                    detail: format!("file input readback params: {error}"),
                })?,
        )
        .await
        .map_err(|error| A11yError::CdpAxtreeFailed {
            detail: format!("file input Runtime.callFunctionOn: {error}"),
        })?
        .result;
    if let Some(exception) = evaluated.exception_details {
        return Err(A11yError::CdpAxtreeFailed {
            detail: format!("file input readback threw: {exception:?}"),
        });
    }
    let value = evaluated
        .result
        .value
        .ok_or_else(|| A11yError::CdpAxtreeFailed {
            detail: "file input readback returned no JSON value".to_owned(),
        })?;
    let value: FileInputValue =
        serde_json::from_value(value).map_err(|error| A11yError::CdpAxtreeFailed {
            detail: format!("file input JSON readback decode: {error}"),
        })?;
    Ok(CdpFileInputState {
        resolved_by: resolved_by.to_owned(),
        match_count,
        backend_node_id,
        tag_name: value.tag_name,
        type_attr: value.type_attr,
        id: value.id,
        name_attr: value.name_attr,
        accept: value.accept,
        multiple: value.multiple,
        webkitdirectory: value.webkitdirectory,
        disabled: value.disabled,
        file_count: value.file_count,
        files: value.files,
        value: value.value,
    })
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}
