//! Persistent per-target Runtime binding capture over CDP (#1069).
//!
//! `Runtime.bindingCalled` is a live event. This module keeps one long-lived
//! CDP session per target, exposes one or more binding names through that
//! session, and buffers binding payloads for later MCP reads.

#![cfg(windows)]

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use chromiumoxide::cdp::js_protocol::runtime::{
    AddBindingParams, EnableParams as RuntimeEnableParams, EventBindingCalled, RemoveBindingParams,
};
use chromiumoxide::{Browser, Page};
use futures_util::StreamExt as _;
use serde::Serialize;
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::{A11yError, A11yResult};

pub const DEFAULT_BINDING_BUFFER_CAPACITY: usize = 1000;
pub const MAX_BINDING_BUFFER_CAPACITY: usize = 10_000;
const MAX_PAYLOAD_CHARS: usize = 65_536;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CdpBindingCall {
    pub seq: u64,
    pub name: String,
    pub payload: String,
    pub payload_len: usize,
    pub payload_truncated: bool,
    pub payload_json: Option<Value>,
    pub execution_context_id: i64,
    pub timestamp_ms: f64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CdpBindingCaptureStatus {
    pub newly_armed: bool,
    pub binding_newly_added: bool,
    pub binding_removed: bool,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub name: String,
    pub armed_at_unix_ms: f64,
    pub capacity: usize,
    pub binding_active: bool,
    pub active_binding_count: usize,
    pub active_binding_names: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CdpBindingReadFilter {
    pub since_seq: Option<u64>,
    pub max: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CdpBindingReadResult {
    pub calls: Vec<CdpBindingCall>,
    pub next_cursor: u64,
    pub returned: usize,
    pub total_buffered: usize,
    pub dropped: u64,
    pub armed_at_unix_ms: f64,
    pub binding_active: bool,
    pub active_binding_count: usize,
    pub active_binding_names: Vec<String>,
}

struct BindingState {
    entries: VecDeque<CdpBindingCall>,
    capacity: usize,
    next_seq: u64,
    dropped: u64,
    active_names: BTreeSet<String>,
}

impl BindingState {
    fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity.min(256)),
            capacity: capacity.max(1),
            next_seq: 0,
            dropped: 0,
            active_names: BTreeSet::new(),
        }
    }

    fn push(&mut self, mut call: CdpBindingCall) {
        call.seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        while self.entries.len() >= self.capacity {
            self.entries.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.entries.push_back(call);
    }

    fn active_binding_names(&self) -> Vec<String> {
        self.active_names.iter().cloned().collect()
    }
}

struct BindingSlot {
    state: Arc<Mutex<BindingState>>,
    endpoint: String,
    target_id: String,
    armed_at_unix_ms: f64,
    page: Page,
    _browser: Browser,
    handler_task: JoinHandle<()>,
    listener_task: JoinHandle<()>,
}

impl Drop for BindingSlot {
    fn drop(&mut self) {
        self.handler_task.abort();
        self.listener_task.abort();
    }
}

#[derive(Default)]
struct BindingRegistry {
    slots: Mutex<HashMap<String, Arc<BindingSlot>>>,
}

fn registry() -> &'static BindingRegistry {
    static REGISTRY: OnceLock<BindingRegistry> = OnceLock::new();
    REGISTRY.get_or_init(BindingRegistry::default)
}

fn slot_key(endpoint: &str, target_id: &str) -> String {
    format!("{endpoint}\n{target_id}")
}

fn now_unix_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

fn lookup_live(endpoint: &str, target_id: &str) -> Option<Arc<BindingSlot>> {
    let key = slot_key(endpoint, target_id);
    let mut slots = registry().slots.lock().ok()?;
    match slots.get(&key) {
        Some(slot) if !slot.listener_task.is_finished() => Some(Arc::clone(slot)),
        Some(_) => {
            slots.remove(&key);
            None
        }
        None => None,
    }
}

pub async fn binding_capture_add(
    endpoint: &str,
    target_id: &str,
    name: &str,
    execution_context_name: Option<&str>,
    capacity: usize,
) -> A11yResult<CdpBindingCaptureStatus> {
    let target_id = non_empty(target_id, "binding capture target id")?;
    let name = non_empty(name, "binding name")?;
    let capacity = capacity.clamp(1, MAX_BINDING_BUFFER_CAPACITY);

    if let Some(slot) = lookup_live(endpoint, target_id) {
        let binding_newly_added = add_binding_to_slot(&slot, name, execution_context_name).await?;
        return status_from_slot(&slot, name, false, binding_newly_added, false);
    }

    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("binding capture connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let armed = async {
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        page.execute(RuntimeEnableParams::default())
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Runtime.enable for binding capture: {err}"),
            })?;
        let listener = page
            .event_listener::<EventBindingCalled>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Runtime.bindingCalled: {err}"),
            })?;
        Ok::<_, A11yError>((page, listener))
    }
    .await;

    let (page, mut listener) = match armed {
        Ok(result) => result,
        Err(err) => {
            handler_task.abort();
            return Err(err);
        }
    };

    let state = Arc::new(Mutex::new(BindingState::new(capacity)));
    let slot = Arc::new(BindingSlot {
        state: Arc::clone(&state),
        endpoint: endpoint.to_owned(),
        target_id: target_id.to_owned(),
        armed_at_unix_ms: now_unix_ms(),
        page: page.clone(),
        _browser: browser,
        handler_task,
        listener_task: tokio::spawn(async move {
            let _page = page;
            while let Some(event) = listener.next().await {
                push_binding_event(&state, event.as_ref());
            }
        }),
    });

    let binding_newly_added = match add_binding_to_slot(&slot, name, execution_context_name).await {
        Ok(added) => added,
        Err(err) => return Err(err),
    };

    let key = slot_key(endpoint, target_id);
    let racing_existing = {
        let mut slots = registry().slots.lock().ok();
        if let Some(slots) = slots.as_mut() {
            match slots.get(&key) {
                Some(existing) if !existing.listener_task.is_finished() => {
                    Some(Arc::clone(existing))
                }
                Some(_) => {
                    slots.insert(key, Arc::clone(&slot));
                    None
                }
                None => {
                    slots.insert(key, Arc::clone(&slot));
                    None
                }
            }
        } else {
            None
        }
    };
    if let Some(existing) = racing_existing {
        let binding_newly_added =
            add_binding_to_slot(&existing, name, execution_context_name).await?;
        return status_from_slot(&existing, name, false, binding_newly_added, false);
    }
    status_from_slot(&slot, name, true, binding_newly_added, false)
}

pub fn binding_capture_read(
    endpoint: &str,
    target_id: &str,
    name: &str,
    filter: &CdpBindingReadFilter,
) -> Option<CdpBindingReadResult> {
    let slot = lookup_live(endpoint, target_id)?;
    let state = slot.state.lock().ok()?;
    let max = if filter.max == 0 {
        usize::MAX
    } else {
        filter.max
    };
    let total_buffered = state
        .entries
        .iter()
        .filter(|entry| entry.name == name)
        .count();
    let calls: Vec<CdpBindingCall> = state
        .entries
        .iter()
        .filter(|entry| entry.name == name)
        .filter(|entry| filter.since_seq.is_none_or(|since| entry.seq >= since))
        .take(max)
        .cloned()
        .collect();
    let returned = calls.len();
    Some(CdpBindingReadResult {
        calls,
        next_cursor: state.next_seq,
        returned,
        total_buffered,
        dropped: state.dropped,
        armed_at_unix_ms: slot.armed_at_unix_ms,
        binding_active: state.active_names.contains(name),
        active_binding_count: state.active_names.len(),
        active_binding_names: state.active_binding_names(),
    })
}

pub async fn binding_capture_remove(
    endpoint: &str,
    target_id: &str,
    name: &str,
) -> A11yResult<CdpBindingCaptureStatus> {
    let target_id = non_empty(target_id, "binding capture target id")?;
    let name = non_empty(name, "binding name")?;
    let Some(slot) = lookup_live(endpoint, target_id) else {
        return Ok(CdpBindingCaptureStatus {
            newly_armed: false,
            binding_newly_added: false,
            binding_removed: false,
            endpoint: endpoint.to_owned(),
            cdp_target_id: target_id.to_owned(),
            name: name.to_owned(),
            armed_at_unix_ms: 0.0,
            capacity: 0,
            binding_active: false,
            active_binding_count: 0,
            active_binding_names: Vec::new(),
        });
    };

    let was_active = {
        let mut state = slot.state.lock().map_err(|_| {
            A11yError::internal("binding capture state mutex poisoned while removing binding")
        })?;
        state.active_names.remove(name)
    };
    if was_active
        && let Err(err) = slot
            .page
            .execute(RemoveBindingParams::new(name.to_owned()))
            .await
        {
            if let Ok(mut state) = slot.state.lock() {
                state.active_names.insert(name.to_owned());
            }
            return Err(A11yError::CdpAxtreeFailed {
                detail: format!("Runtime.removeBinding({name:?}): {err}"),
            });
        }
    status_from_slot(&slot, name, false, false, was_active)
}

fn non_empty<'a>(value: &'a str, field: &str) -> A11yResult<&'a str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: format!("{field} must not be empty"),
        });
    }
    Ok(trimmed)
}

async fn add_binding_to_slot(
    slot: &BindingSlot,
    name: &str,
    execution_context_name: Option<&str>,
) -> A11yResult<bool> {
    {
        let mut state = slot.state.lock().map_err(|_| {
            A11yError::internal("binding capture state mutex poisoned while adding binding")
        })?;
        if !state.active_names.insert(name.to_owned()) {
            return Ok(false);
        }
    }

    let mut builder = AddBindingParams::builder().name(name.to_owned());
    if let Some(execution_context_name) = execution_context_name {
        builder = builder.execution_context_name(execution_context_name.to_owned());
    }
    let params = builder.build().map_err(|err| A11yError::CdpAxtreeFailed {
        detail: format!("build Runtime.addBinding params: {err}"),
    })?;
    match slot.page.execute(params).await {
        Ok(_) => Ok(true),
        Err(err) => {
            if let Ok(mut state) = slot.state.lock() {
                state.active_names.remove(name);
            }
            Err(A11yError::CdpAxtreeFailed {
                detail: format!("Runtime.addBinding({name:?}): {err}"),
            })
        }
    }
}

fn status_from_slot(
    slot: &BindingSlot,
    name: &str,
    newly_armed: bool,
    binding_newly_added: bool,
    binding_removed: bool,
) -> A11yResult<CdpBindingCaptureStatus> {
    let state = slot.state.lock().map_err(|_| {
        A11yError::internal("binding capture state mutex poisoned while reading status")
    })?;
    Ok(CdpBindingCaptureStatus {
        newly_armed,
        binding_newly_added,
        binding_removed,
        endpoint: slot.endpoint.clone(),
        cdp_target_id: slot.target_id.clone(),
        name: name.to_owned(),
        armed_at_unix_ms: slot.armed_at_unix_ms,
        capacity: state.capacity,
        binding_active: state.active_names.contains(name),
        active_binding_count: state.active_names.len(),
        active_binding_names: state.active_binding_names(),
    })
}

fn push_binding_event(state: &Arc<Mutex<BindingState>>, event: &EventBindingCalled) {
    let Ok(mut state) = state.lock() else {
        return;
    };
    if !state.active_names.contains(&event.name) {
        return;
    }
    let (payload, payload_len, payload_truncated) = bounded_payload(&event.payload);
    let payload_json = if payload_truncated {
        None
    } else {
        serde_json::from_str(&payload).ok()
    };
    state.push(CdpBindingCall {
        seq: 0,
        name: event.name.clone(),
        payload,
        payload_len,
        payload_truncated,
        payload_json,
        execution_context_id: *event.execution_context_id.inner(),
        timestamp_ms: now_unix_ms(),
    });
}

fn bounded_payload(payload: &str) -> (String, usize, bool) {
    let payload_len = payload.chars().count();
    if payload_len <= MAX_PAYLOAD_CHARS {
        return (payload.to_owned(), payload_len, false);
    }
    (
        payload.chars().take(MAX_PAYLOAD_CHARS).collect(),
        payload_len,
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chromiumoxide::cdp::js_protocol::runtime::ExecutionContextId;

    #[test]
    fn binding_state_filters_by_name_and_cursor() {
        let mut state = BindingState::new(3);
        state.active_names.insert("alpha".to_owned());
        state.push(call("alpha", "one", 1));
        state.push(call("beta", "skip", 2));
        state.push(call("alpha", "two", 3));

        let filter = CdpBindingReadFilter {
            since_seq: Some(1),
            max: 10,
        };
        let calls: Vec<_> = state
            .entries
            .iter()
            .filter(|entry| entry.name == "alpha")
            .filter(|entry| filter.since_seq.is_none_or(|since| entry.seq >= since))
            .cloned()
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].payload, "two");
    }

    #[test]
    fn bounded_payload_preserves_character_boundaries() {
        let payload = "a".repeat(MAX_PAYLOAD_CHARS) + "b";
        let (bounded, len, truncated) = bounded_payload(&payload);
        assert_eq!(bounded.len(), MAX_PAYLOAD_CHARS);
        assert_eq!(len, MAX_PAYLOAD_CHARS + 1);
        assert!(truncated);
    }

    fn call(name: &str, payload: &str, context: i64) -> CdpBindingCall {
        CdpBindingCall {
            seq: context as u64 - 1,
            name: name.to_owned(),
            payload: payload.to_owned(),
            payload_len: payload.len(),
            payload_truncated: false,
            payload_json: None,
            execution_context_id: *ExecutionContextId::new(context).inner(),
            timestamp_ms: 0.0,
        }
    }
}
