//! Persistent per-target JavaScript dialog capture over CDP (#1097).
//!
//! `Page.javascriptDialogOpening` is a live event. This module keeps a
//! long-lived CDP connection per armed target, records dialog open/close state,
//! and immediately applies a configured default policy so an unhandled dialog
//! cannot silently block page execution.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chromiumoxide::cdp::browser_protocol::page::{
    DialogType, EnableParams as PageEnableParams, EventJavascriptDialogClosed,
    EventJavascriptDialogOpening, FrameId, HandleJavaScriptDialogParams,
};
use chromiumoxide::{Browser, Page};
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::{A11yError, A11yResult};

pub const DEFAULT_DIALOG_BUFFER_CAPACITY: usize = 128;
pub const MAX_DIALOG_BUFFER_CAPACITY: usize = 1000;

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct CdpDialogDurableDrainReadback {
    pub found: usize,
    pub listener_tasks_drained: usize,
    pub handler_tasks_drained: usize,
    pub failures: Vec<String>,
    pub active_after: usize,
}

/// Default action applied when a JavaScript dialog opens and no explicit handler
/// is waiting on it. `Dismiss` is conservative and avoids approving confirms or
/// submitting prompt text accidentally.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CdpDialogDefaultPolicy {
    Accept,
    #[default]
    Dismiss,
    Manual,
}

impl CdpDialogDefaultPolicy {
    const fn accept(self) -> bool {
        matches!(self, Self::Accept)
    }

    const fn auto_action(self) -> Option<CdpDialogAutoAction> {
        match self {
            Self::Accept => Some(CdpDialogAutoAction::Accepted),
            Self::Dismiss => Some(CdpDialogAutoAction::Dismissed),
            Self::Manual => None,
        }
    }
}

/// Action the default policy attempted for a captured dialog.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CdpDialogAutoAction {
    Accepted,
    Dismissed,
}

/// Explicit action for a currently pending JavaScript dialog (#1098).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CdpDialogHandleAction {
    Accept,
    Dismiss,
}

impl CdpDialogHandleAction {
    const fn accept(self) -> bool {
        matches!(self, Self::Accept)
    }
}

/// One JavaScript dialog open/close record for a target.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CdpDialogEntry {
    pub seq: u64,
    pub url: String,
    pub frame_id: String,
    pub dialog_type: String,
    pub message: String,
    pub default_prompt: Option<String>,
    pub has_browser_handler: bool,
    pub opened_at_unix_ms: u64,
    pub pending: bool,
    pub default_policy: CdpDialogDefaultPolicy,
    pub auto_action: Option<CdpDialogAutoAction>,
    pub auto_handled_at_unix_ms: Option<u64>,
    pub auto_handle_error: Option<String>,
    pub manual_action: Option<CdpDialogHandleAction>,
    pub manual_prompt_text: Option<String>,
    pub manual_handled_at_unix_ms: Option<u64>,
    pub manual_handle_error: Option<String>,
    pub closed_at_unix_ms: Option<u64>,
    pub close_result: Option<bool>,
    pub user_input: Option<String>,
}

/// Status returned when dialog capture is armed or inspected.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CdpDialogCaptureStatus {
    pub newly_armed: bool,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub armed_at_unix_ms: u64,
    pub capacity: usize,
    pub default_policy: CdpDialogDefaultPolicy,
    pub pending_dialog: Option<CdpDialogEntry>,
    pub last_dialog: Option<CdpDialogEntry>,
    pub opened_count: u64,
    pub closed_count: u64,
    pub auto_handled_count: u64,
    pub error_count: u64,
}

/// Optional filters for [`dialog_capture_read`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CdpDialogReadFilter {
    pub since_seq: Option<u64>,
    pub max: usize,
}

/// Readback for captured dialog history.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CdpDialogReadResult {
    pub entries: Vec<CdpDialogEntry>,
    pub pending_dialog: Option<CdpDialogEntry>,
    pub next_cursor: u64,
    pub returned: usize,
    pub total_buffered: usize,
    pub dropped: u64,
    pub armed_at_unix_ms: u64,
    pub default_policy: CdpDialogDefaultPolicy,
}

/// Result of explicitly accepting or dismissing a pending dialog.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CdpDialogHandleResult {
    pub cdp_target_id: String,
    pub action: CdpDialogHandleAction,
    pub prompt_text: Option<String>,
    pub handled_at_unix_ms: u64,
    pub dialog: CdpDialogEntry,
}

struct DialogState {
    entries: VecDeque<CdpDialogEntry>,
    capacity: usize,
    next_seq: u64,
    dropped: u64,
    pending_seq: Option<u64>,
    default_policy: CdpDialogDefaultPolicy,
    opened_count: u64,
    closed_count: u64,
    auto_handled_count: u64,
    error_count: u64,
}

impl DialogState {
    fn new(capacity: usize, default_policy: CdpDialogDefaultPolicy) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity.min(64)),
            capacity: capacity.max(1),
            next_seq: 0,
            dropped: 0,
            pending_seq: None,
            default_policy,
            opened_count: 0,
            closed_count: 0,
            auto_handled_count: 0,
            error_count: 0,
        }
    }

    fn push_opening(&mut self, mut entry: CdpDialogEntry) -> u64 {
        entry.seq = self.next_seq;
        entry.pending = true;
        let seq = entry.seq;
        self.next_seq = self.next_seq.saturating_add(1);
        while self.entries.len() >= self.capacity {
            self.entries.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.pending_seq = Some(seq);
        self.opened_count = self.opened_count.saturating_add(1);
        self.entries.push_back(entry);
        seq
    }

    fn mark_auto_handled(&mut self, seq: u64, action: CdpDialogAutoAction) {
        if let Some(entry) = self.entry_mut(seq) {
            entry.auto_action = Some(action);
            entry.auto_handled_at_unix_ms = Some(now_unix_ms());
            entry.auto_handle_error = None;
        }
        self.auto_handled_count = self.auto_handled_count.saturating_add(1);
    }

    fn mark_manual_handled(
        &mut self,
        seq: u64,
        action: CdpDialogHandleAction,
        prompt_text: Option<String>,
        handled_at_unix_ms: u64,
    ) -> Option<CdpDialogEntry> {
        self.pending_seq = None;
        let entry = self.entry_mut(seq)?;
        entry.pending = false;
        entry.manual_action = Some(action);
        entry.manual_prompt_text = prompt_text.clone();
        entry.manual_handled_at_unix_ms = Some(handled_at_unix_ms);
        entry.manual_handle_error = None;
        entry.closed_at_unix_ms = Some(handled_at_unix_ms);
        entry.close_result = Some(action.accept());
        entry.user_input = if action.accept() {
            Some(
                prompt_text
                    .or_else(|| entry.default_prompt.clone())
                    .unwrap_or_default(),
            )
        } else {
            Some(String::new())
        };
        let updated = entry.clone();
        self.closed_count = self.closed_count.saturating_add(1);
        Some(updated)
    }

    fn mark_manual_error(
        &mut self,
        seq: u64,
        action: CdpDialogHandleAction,
        prompt_text: Option<String>,
        error: String,
    ) {
        if let Some(entry) = self.entry_mut(seq) {
            entry.manual_action = Some(action);
            entry.manual_prompt_text = prompt_text;
            entry.manual_handle_error = Some(error);
        }
        self.error_count = self.error_count.saturating_add(1);
    }

    fn mark_auto_error(&mut self, seq: u64, action: CdpDialogAutoAction, error: String) {
        if let Some(entry) = self.entry_mut(seq) {
            entry.auto_action = Some(action);
            entry.auto_handle_error = Some(error);
        }
        self.error_count = self.error_count.saturating_add(1);
    }

    fn mark_closed(&mut self, event: &EventJavascriptDialogClosed) {
        let Some(seq) = self.pending_seq.take() else {
            return;
        };
        if let Some(entry) = self.entry_mut(seq) {
            entry.pending = false;
            entry.closed_at_unix_ms = Some(now_unix_ms());
            entry.close_result = Some(event.result);
            entry.user_input = Some(event.user_input.clone());
        }
        self.closed_count = self.closed_count.saturating_add(1);
    }

    const fn cursor(&self) -> u64 {
        self.next_seq
    }

    fn pending_dialog(&self) -> Option<CdpDialogEntry> {
        let seq = self.pending_seq?;
        self.entries.iter().find(|entry| entry.seq == seq).cloned()
    }

    fn last_dialog(&self) -> Option<CdpDialogEntry> {
        self.entries.back().cloned()
    }

    fn entry_mut(&mut self, seq: u64) -> Option<&mut CdpDialogEntry> {
        self.entries.iter_mut().find(|entry| entry.seq == seq)
    }
}

struct DialogCaptureSlot {
    state: Arc<Mutex<DialogState>>,
    endpoint: String,
    armed_at_unix_ms: u64,
    capacity: usize,
    page: Page,
    _browser: Browser,
    handler_task: JoinHandle<()>,
    listener_task: JoinHandle<()>,
}

impl Drop for DialogCaptureSlot {
    fn drop(&mut self) {
        self.handler_task.abort();
        self.listener_task.abort();
    }
}

#[derive(Default)]
struct DialogCaptureRegistry {
    slots: Mutex<HashMap<String, Arc<DialogCaptureSlot>>>,
}

fn registry() -> &'static DialogCaptureRegistry {
    static REGISTRY: OnceLock<DialogCaptureRegistry> = OnceLock::new();
    REGISTRY.get_or_init(DialogCaptureRegistry::default)
}

/// Arms (or re-arms) persistent dialog capture for `target_id`.
///
/// If a live capture already exists, its default policy is updated and the
/// existing connection is reused.
pub async fn dialog_capture_ensure(
    endpoint: &str,
    target_id: &str,
    default_policy: CdpDialogDefaultPolicy,
    capacity: usize,
) -> A11yResult<CdpDialogCaptureStatus> {
    let endpoint = endpoint.to_owned();
    let target_id = target_id.to_owned();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result =
            dialog_capture_ensure_owned(&endpoint, &target_id, default_policy, capacity).await;
        let _ = result_tx.send(result);
    });
    result_rx.await.map_err(|_| A11yError::CdpAttachFailed {
        detail: "owned dialog policy install task terminated before publishing a verdict"
            .to_owned(),
    })?
}

async fn dialog_capture_ensure_owned(
    endpoint: &str,
    target_id: &str,
    default_policy: CdpDialogDefaultPolicy,
    capacity: usize,
) -> A11yResult<CdpDialogCaptureStatus> {
    let _operation_guard = crate::cdp_network::durable_browser_mutation_operation_guard().await;
    if !crate::cdp_network::durable_browser_mutation_owners_enabled() {
        return Err(A11yError::CdpAttachFailed {
            detail: "durable browser mutation owners are disabled by operator panic; refusing dialog capture/policy install".to_owned(),
        });
    }
    let target_id = target_id.trim();
    if target_id.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "dialog capture target id must not be empty".to_owned(),
        });
    }

    if let Some(slot) = lookup_live(target_id) {
        if let Ok(mut state) = slot.state.lock() {
            state.default_policy = default_policy;
        }
        return status_from_slot(&slot, target_id, false);
    }

    let capacity = capacity.clamp(1, MAX_DIALOG_BUFFER_CAPACITY);
    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("dialog capture connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let armed = async {
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        page.execute(PageEnableParams::default())
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Page.enable for dialog capture: {err}"),
            })?;
        let openings = page
            .event_listener::<EventJavascriptDialogOpening>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Page.javascriptDialogOpening: {err}"),
            })?;
        let closings = page
            .event_listener::<EventJavascriptDialogClosed>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Page.javascriptDialogClosed: {err}"),
            })?;
        Ok::<_, A11yError>((page, openings, closings))
    }
    .await;

    let (page, mut openings, mut closings) = match armed {
        Ok(armed) => armed,
        Err(err) => {
            handler_task.abort();
            return Err(err);
        }
    };

    let state = Arc::new(Mutex::new(DialogState::new(capacity, default_policy)));
    let pump_state = Arc::clone(&state);
    let listener_page = page.clone();
    let slot_page = page.clone();
    let listener_task = tokio::spawn(async move {
        let _page = page;
        loop {
            if !crate::cdp_network::durable_browser_mutation_owners_enabled() {
                break;
            }
            tokio::select! {
                Some(event) = openings.next() => {
                    let event = event.as_ref();
                    let (seq, policy) = push_opening(&pump_state, event);
                    let Some(action) = policy.auto_action() else {
                        continue;
                    };
                    if !crate::cdp_network::durable_browser_mutation_owners_enabled() {
                        break;
                    }
                    match dialog_handle_params(policy, event.default_prompt.as_deref()) {
                        Ok(params) => {
                            match listener_page.execute(params).await {
                                Ok(_) => mark_auto_handled(&pump_state, seq, action),
                                Err(error) => mark_auto_error(
                                    &pump_state,
                                    seq,
                                    action,
                                    format!("Page.handleJavaScriptDialog default policy failed: {error}"),
                                ),
                            }
                        }
                        Err(error) => {
                            mark_auto_error(&pump_state, seq, action, error.to_string());
                        }
                    }
                }
                Some(event) = closings.next() => {
                    apply_closed(&pump_state, event.as_ref());
                }
                else => break,
            }
        }
    });

    let armed_at_unix_ms = now_unix_ms();
    let slot = Arc::new(DialogCaptureSlot {
        state,
        endpoint: endpoint.to_owned(),
        armed_at_unix_ms,
        capacity,
        page: slot_page,
        _browser: browser,
        handler_task,
        listener_task,
    });
    if let Ok(mut slots) = registry().slots.lock() {
        if !crate::cdp_network::durable_browser_mutation_owners_enabled() {
            slot.listener_task.abort();
            slot.handler_task.abort();
            return Err(A11yError::CdpAttachFailed {
                detail: "operator panic crossed during dialog capture registration".to_owned(),
            });
        }
        if let Some(existing) = slots.get(target_id)
            && !existing.listener_task.is_finished()
        {
            if let Ok(mut state) = existing.state.lock() {
                state.default_policy = default_policy;
            }
            return status_from_slot(existing, target_id, false);
        }
        slots.insert(target_id.to_owned(), Arc::clone(&slot));
    }
    status_from_slot(&slot, target_id, true)
}

/// Compatibility alias for callers using the issue's CDP-prefixed naming.
pub async fn cdp_dialog_capture_start(
    endpoint: &str,
    target_id: &str,
    default_policy: CdpDialogDefaultPolicy,
    capacity: usize,
) -> A11yResult<CdpDialogCaptureStatus> {
    dialog_capture_ensure(endpoint, target_id, default_policy, capacity).await
}

/// Updates the default auto-policy for a live dialog capture.
///
/// Returns `false` if the target has not been armed or its listener has ended.
#[must_use]
pub fn dialog_capture_set_default_policy(
    target_id: &str,
    default_policy: CdpDialogDefaultPolicy,
) -> bool {
    let Some(slot) = lookup_live(target_id.trim()) else {
        return false;
    };
    match slot.state.lock() {
        Ok(mut state) => {
            state.default_policy = default_policy;
            true
        }
        Err(_) => false,
    }
}

/// Accepts or dismisses the currently pending JavaScript dialog for `target_id`.
///
/// `prompt_text` is only valid for `Accept`; when omitted for a prompt dialog,
/// the dialog's captured `default_prompt` is submitted if one exists.
pub async fn dialog_handle_pending(
    target_id: &str,
    action: CdpDialogHandleAction,
    prompt_text: Option<String>,
) -> A11yResult<CdpDialogHandleResult> {
    let target_id = target_id.trim();
    if target_id.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "dialog handle target id must not be empty".to_owned(),
        });
    }
    if matches!(action, CdpDialogHandleAction::Dismiss) && prompt_text.is_some() {
        return Err(A11yError::CdpAttachFailed {
            detail: "dialog prompt_text is only valid when accepting a dialog".to_owned(),
        });
    }
    validate_prompt_text(prompt_text.as_deref())?;

    let slot = lookup_live(target_id).ok_or_else(|| A11yError::CdpAttachFailed {
        detail: format!("dialog capture is not armed for target {target_id}"),
    })?;
    let pending = {
        let state = slot.state.lock().map_err(|_| A11yError::CdpAttachFailed {
            detail: "dialog capture state lock is poisoned".to_owned(),
        })?;
        state
            .pending_dialog()
            .ok_or_else(|| A11yError::CdpAttachFailed {
                detail: format!("no pending JavaScript dialog for target {target_id}"),
            })?
    };
    let effective_prompt_text = if action.accept() {
        prompt_text
            .clone()
            .or_else(|| pending.default_prompt.clone())
    } else {
        None
    };
    let params = dialog_manual_handle_params(action, effective_prompt_text.as_deref())?;
    let handled_at_unix_ms = now_unix_ms();
    match slot.page.execute(params).await {
        Ok(_) => {
            let mut state = slot.state.lock().map_err(|_| A11yError::CdpAttachFailed {
                detail: "dialog capture state lock is poisoned".to_owned(),
            })?;
            let dialog = state
                .mark_manual_handled(
                    pending.seq,
                    action,
                    effective_prompt_text.clone(),
                    handled_at_unix_ms,
                )
                .ok_or_else(|| A11yError::CdpAttachFailed {
                    detail: format!("pending JavaScript dialog disappeared for target {target_id}"),
                })?;
            Ok(CdpDialogHandleResult {
                cdp_target_id: target_id.to_owned(),
                action,
                prompt_text: effective_prompt_text,
                handled_at_unix_ms,
                dialog,
            })
        }
        Err(error) => {
            if let Ok(mut state) = slot.state.lock() {
                state.mark_manual_error(
                    pending.seq,
                    action,
                    effective_prompt_text.clone(),
                    format!("Page.handleJavaScriptDialog explicit action failed: {error}"),
                );
            }
            Err(A11yError::CdpAxtreeFailed {
                detail: format!("Page.handleJavaScriptDialog for target {target_id}: {error}"),
            })
        }
    }
}

/// Compatibility alias for callers using the issue's CDP-prefixed naming.
pub async fn cdp_dialog_handle_pending(
    target_id: &str,
    action: CdpDialogHandleAction,
    prompt_text: Option<String>,
) -> A11yResult<CdpDialogHandleResult> {
    dialog_handle_pending(target_id, action, prompt_text).await
}

/// Reads a filtered, cursor-delimited dialog history for a target.
#[must_use]
pub fn dialog_capture_read(
    target_id: &str,
    filter: &CdpDialogReadFilter,
) -> Option<CdpDialogReadResult> {
    let slot = lookup_live(target_id.trim())?;
    let state = slot.state.lock().ok()?;
    let max = if filter.max == 0 {
        usize::MAX
    } else {
        filter.max
    };
    let entries: Vec<CdpDialogEntry> = state
        .entries
        .iter()
        .filter(|entry| filter.since_seq.is_none_or(|since| entry.seq >= since))
        .take(max)
        .cloned()
        .collect();
    Some(CdpDialogReadResult {
        returned: entries.len(),
        entries,
        pending_dialog: state.pending_dialog(),
        next_cursor: state.cursor(),
        total_buffered: state.entries.len(),
        dropped: state.dropped,
        armed_at_unix_ms: slot.armed_at_unix_ms,
        default_policy: state.default_policy,
    })
}

/// Returns current dialog capture status for an armed target.
#[must_use]
pub fn dialog_capture_status(target_id: &str) -> Option<CdpDialogCaptureStatus> {
    let target_id = target_id.trim();
    let slot = lookup_live(target_id)?;
    status_from_slot(&slot, target_id, false).ok()
}

/// Stops dialog capture for a target.
#[must_use]
pub fn dialog_capture_stop(target_id: &str) -> bool {
    registry()
        .slots
        .lock()
        .ok()
        .and_then(|mut slots| slots.remove(target_id.trim()))
        .is_some()
}

/// Number of currently live dialog capture slots.
#[must_use]
pub fn dialog_capture_active_count() -> usize {
    dialog_capture_active_count_readback().unwrap_or(usize::MAX)
}

pub fn dialog_capture_active_count_readback() -> Result<usize, String> {
    registry()
        .slots
        .lock()
        .map(|slots| {
            slots
                .values()
                .filter(|slot| !slot.listener_task.is_finished())
                .count()
        })
        .map_err(|_| "dialog capture registry lock is poisoned".to_owned())
}

pub async fn dialog_capture_disable_and_drain_all() -> CdpDialogDurableDrainReadback {
    let slots = match registry().slots.lock() {
        Ok(mut slots) => std::mem::take(&mut *slots),
        Err(_) => {
            return CdpDialogDurableDrainReadback {
                failures: vec!["dialog capture registry lock is poisoned".to_owned()],
                active_after: dialog_capture_active_count(),
                ..Default::default()
            };
        }
    };
    let found = slots.len();
    let mut listener_tasks_drained = 0usize;
    let mut handler_tasks_drained = 0usize;
    let mut failures = Vec::new();
    for (target_id, slot) in slots {
        slot.listener_task.abort();
        let listener_drained = tokio::time::timeout(Duration::from_secs(5), async {
            while !slot.listener_task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        listener_tasks_drained += usize::from(listener_drained);
        if !listener_drained {
            failures.push(format!(
                "dialog listener task did not drain for target {target_id:?}"
            ));
        }
        slot.handler_task.abort();
        let handler_drained = tokio::time::timeout(Duration::from_secs(5), async {
            while !slot.handler_task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok();
        handler_tasks_drained += usize::from(handler_drained);
        if !handler_drained {
            failures.push(format!(
                "dialog handler task did not drain for target {target_id:?}"
            ));
        }
    }
    CdpDialogDurableDrainReadback {
        found,
        listener_tasks_drained,
        handler_tasks_drained,
        failures,
        active_after: dialog_capture_active_count(),
    }
}

fn lookup_live(target_id: &str) -> Option<Arc<DialogCaptureSlot>> {
    let mut slots = registry().slots.lock().ok()?;
    match slots.get(target_id) {
        Some(slot) if !slot.listener_task.is_finished() => Some(Arc::clone(slot)),
        Some(_) => {
            slots.remove(target_id);
            None
        }
        None => None,
    }
}

fn status_from_slot(
    slot: &DialogCaptureSlot,
    target_id: &str,
    newly_armed: bool,
) -> A11yResult<CdpDialogCaptureStatus> {
    let state = slot.state.lock().map_err(|_| A11yError::CdpAttachFailed {
        detail: "dialog capture state lock is poisoned".to_owned(),
    })?;
    Ok(CdpDialogCaptureStatus {
        newly_armed,
        endpoint: slot.endpoint.clone(),
        cdp_target_id: target_id.to_owned(),
        armed_at_unix_ms: slot.armed_at_unix_ms,
        capacity: slot.capacity,
        default_policy: state.default_policy,
        pending_dialog: state.pending_dialog(),
        last_dialog: state.last_dialog(),
        opened_count: state.opened_count,
        closed_count: state.closed_count,
        auto_handled_count: state.auto_handled_count,
        error_count: state.error_count,
    })
}

fn push_opening(
    state: &Arc<Mutex<DialogState>>,
    event: &EventJavascriptDialogOpening,
) -> (u64, CdpDialogDefaultPolicy) {
    let now = now_unix_ms();
    let Ok(mut state) = state.lock() else {
        return (0, CdpDialogDefaultPolicy::Dismiss);
    };
    let policy = state.default_policy;
    let entry = dialog_entry_from_opening(event, policy, now);
    let seq = state.push_opening(entry);
    (seq, policy)
}

fn mark_auto_handled(state: &Arc<Mutex<DialogState>>, seq: u64, action: CdpDialogAutoAction) {
    if let Ok(mut state) = state.lock() {
        state.mark_auto_handled(seq, action);
    }
}

fn mark_auto_error(
    state: &Arc<Mutex<DialogState>>,
    seq: u64,
    action: CdpDialogAutoAction,
    error: String,
) {
    if let Ok(mut state) = state.lock() {
        state.mark_auto_error(seq, action, error);
    }
}

fn apply_closed(state: &Arc<Mutex<DialogState>>, event: &EventJavascriptDialogClosed) {
    if let Ok(mut state) = state.lock() {
        state.mark_closed(event);
    }
}

fn dialog_entry_from_opening(
    event: &EventJavascriptDialogOpening,
    default_policy: CdpDialogDefaultPolicy,
    opened_at_unix_ms: u64,
) -> CdpDialogEntry {
    CdpDialogEntry {
        seq: 0,
        url: event.url.clone(),
        frame_id: frame_id_string(&event.frame_id),
        dialog_type: dialog_type_string(&event.r#type),
        message: event.message.clone(),
        default_prompt: event.default_prompt.clone(),
        has_browser_handler: event.has_browser_handler,
        opened_at_unix_ms,
        pending: true,
        default_policy,
        auto_action: None,
        auto_handled_at_unix_ms: None,
        auto_handle_error: None,
        manual_action: None,
        manual_prompt_text: None,
        manual_handled_at_unix_ms: None,
        manual_handle_error: None,
        closed_at_unix_ms: None,
        close_result: None,
        user_input: None,
    }
}

fn dialog_handle_params(
    policy: CdpDialogDefaultPolicy,
    default_prompt: Option<&str>,
) -> A11yResult<HandleJavaScriptDialogParams> {
    let mut builder = HandleJavaScriptDialogParams::builder().accept(policy.accept());
    if policy.accept()
        && let Some(default_prompt) = default_prompt
    {
        builder = builder.prompt_text(default_prompt.to_owned());
    }
    builder
        .build()
        .map_err(|detail| A11yError::CdpAttachFailed {
            detail: format!("Page.handleJavaScriptDialog params: {detail}"),
        })
}

fn dialog_manual_handle_params(
    action: CdpDialogHandleAction,
    prompt_text: Option<&str>,
) -> A11yResult<HandleJavaScriptDialogParams> {
    let mut builder = HandleJavaScriptDialogParams::builder().accept(action.accept());
    if action.accept()
        && let Some(prompt_text) = prompt_text
    {
        builder = builder.prompt_text(prompt_text.to_owned());
    }
    builder
        .build()
        .map_err(|detail| A11yError::CdpAttachFailed {
            detail: format!("Page.handleJavaScriptDialog params: {detail}"),
        })
}

fn validate_prompt_text(prompt_text: Option<&str>) -> A11yResult<()> {
    if let Some(prompt_text) = prompt_text
        && prompt_text.contains('\0')
    {
        return Err(A11yError::CdpAttachFailed {
            detail: "dialog prompt_text must not contain NUL".to_owned(),
        });
    }
    Ok(())
}

fn dialog_type_string(dialog_type: &DialogType) -> String {
    dialog_type.as_ref().to_owned()
}

fn frame_id_string(frame_id: &FrameId) -> String {
    <FrameId as std::borrow::Borrow<str>>::borrow(frame_id).to_owned()
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().try_into().unwrap_or(u64::MAX))
}
