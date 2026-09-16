//! Persistent page lifecycle + worker target event capture over raw CDP (#1199/#1200).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use chromiumoxide::Browser;
use chromiumoxide::cdp::browser_protocol::page::{
    EnableParams as PageEnableParams, EventDomContentEventFired, EventFrameNavigated,
    EventFrameStartedNavigating, EventFrameStoppedLoading, EventLifecycleEvent,
    EventLoadEventFired, EventNavigatedWithinDocument, Frame, SetLifecycleEventsEnabledParams,
};
use chromiumoxide::cdp::browser_protocol::target::{
    AutoAttachRelatedParams, EventAttachedToTarget, EventTargetCreated, EventTargetDestroyed,
    EventTargetInfoChanged, FilterEntry, TargetFilter, TargetId, TargetInfo,
};
use futures_util::StreamExt as _;
use serde::Serialize;
use tokio::task::JoinHandle;

use crate::{A11yError, A11yResult};

pub const DEFAULT_LIFECYCLE_BUFFER_CAPACITY: usize = 1000;
pub const MAX_LIFECYCLE_BUFFER_CAPACITY: usize = 10_000;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CdpPageEventEntry {
    pub seq: u64,
    pub event_kind: String,
    pub target_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_attached: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_target_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opener_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opener_frame_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_access_opener: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser_context_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_frame_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigation_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_s: Option<f64>,
    pub observed_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CdpPageEventsCaptureStatus {
    pub newly_armed: bool,
    pub armed_at_unix_ms: u64,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub capacity: usize,
}

#[derive(Clone, Debug, Default)]
pub struct CdpPageEventsReadFilter<'a> {
    pub since_seq: Option<u64>,
    pub event_kind: Option<&'a str>,
    pub worker_type: Option<&'a str>,
    pub max: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct CdpPageEventsReadResult {
    pub entries: Vec<CdpPageEventEntry>,
    pub pages: Vec<CdpPageTargetSnapshot>,
    pub workers: Vec<CdpWorkerSnapshot>,
    pub next_cursor: u64,
    pub returned: usize,
    pub total_buffered: usize,
    pub dropped: u64,
    pub armed_at_unix_ms: u64,
    pub capacity: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CdpPageTargetSnapshot {
    pub target_id: String,
    pub target_type: String,
    pub url: String,
    pub title: String,
    pub opener_id: Option<String>,
    pub opener_frame_id: Option<String>,
    pub can_access_opener: bool,
    pub browser_context_id: Option<String>,
    pub subtype: Option<String>,
    pub attached: bool,
    pub destroyed: bool,
    pub first_seen_seq: u64,
    pub last_seen_seq: u64,
    pub first_seen_unix_ms: u64,
    pub last_seen_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CdpWorkerSnapshot {
    pub worker_id: String,
    pub worker_type: String,
    pub url: String,
    pub title: String,
    pub attached: bool,
    pub destroyed: bool,
    pub first_seen_seq: u64,
    pub last_seen_seq: u64,
    pub first_seen_unix_ms: u64,
    pub last_seen_unix_ms: u64,
}

struct RingBuffer {
    entries: VecDeque<CdpPageEventEntry>,
    pages: HashMap<String, CdpPageTargetSnapshot>,
    workers: HashMap<String, CdpWorkerSnapshot>,
    capacity: usize,
    next_seq: u64,
    dropped: u64,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity.min(256)),
            pages: HashMap::new(),
            workers: HashMap::new(),
            capacity: capacity.max(1),
            next_seq: 0,
            dropped: 0,
        }
    }

    const fn cursor(&self) -> u64 {
        self.next_seq
    }

    fn push(&mut self, mut entry: CdpPageEventEntry) {
        entry.seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        if let Some(page_target_id) = entry.page_target_id.as_deref() {
            let page = self
                .pages
                .entry(page_target_id.to_owned())
                .or_insert_with(|| CdpPageTargetSnapshot {
                    target_id: page_target_id.to_owned(),
                    target_type: entry
                        .target_type
                        .clone()
                        .unwrap_or_else(|| "page".to_owned()),
                    url: entry.url.clone().unwrap_or_default(),
                    title: entry.title.clone().unwrap_or_default(),
                    opener_id: entry.opener_id.clone(),
                    opener_frame_id: entry.opener_frame_id.clone(),
                    can_access_opener: entry.can_access_opener.unwrap_or(false),
                    browser_context_id: entry.browser_context_id.clone(),
                    subtype: entry.subtype.clone(),
                    attached: false,
                    destroyed: false,
                    first_seen_seq: entry.seq,
                    last_seen_seq: entry.seq,
                    first_seen_unix_ms: entry.observed_at_unix_ms,
                    last_seen_unix_ms: entry.observed_at_unix_ms,
                });
            if let Some(target_type) = entry.target_type.as_ref()
                && !target_type.is_empty()
            {
                page.target_type = target_type.clone();
            }
            if let Some(url) = entry.url.as_ref()
                && !url.is_empty()
            {
                page.url = url.clone();
            }
            if let Some(title) = entry.title.as_ref()
                && !title.is_empty()
            {
                page.title = title.clone();
            }
            if entry.opener_id.is_some() {
                page.opener_id = entry.opener_id.clone();
            }
            if entry.opener_frame_id.is_some() {
                page.opener_frame_id = entry.opener_frame_id.clone();
            }
            if let Some(can_access_opener) = entry.can_access_opener {
                page.can_access_opener = can_access_opener;
            }
            if entry.browser_context_id.is_some() {
                page.browser_context_id = entry.browser_context_id.clone();
            }
            if entry.subtype.is_some() {
                page.subtype = entry.subtype.clone();
            }
            if let Some(target_attached) = entry.target_attached {
                page.attached = target_attached;
            }
            if entry.event_kind == "page_attached" {
                page.attached = true;
            }
            if entry.event_kind == "page_destroyed" {
                page.attached = false;
                page.destroyed = true;
            }
            page.last_seen_seq = entry.seq;
            page.last_seen_unix_ms = entry.observed_at_unix_ms;
        }
        if let Some(worker_id) = entry.worker_id.as_deref() {
            let worker =
                self.workers
                    .entry(worker_id.to_owned())
                    .or_insert_with(|| CdpWorkerSnapshot {
                        worker_id: worker_id.to_owned(),
                        worker_type: entry.worker_type.clone().unwrap_or_default(),
                        url: entry.worker_url.clone().unwrap_or_default(),
                        title: entry.title.clone().unwrap_or_default(),
                        attached: false,
                        destroyed: false,
                        first_seen_seq: entry.seq,
                        last_seen_seq: entry.seq,
                        first_seen_unix_ms: entry.observed_at_unix_ms,
                        last_seen_unix_ms: entry.observed_at_unix_ms,
                    });
            if let Some(worker_type) = entry.worker_type.as_ref()
                && !worker_type.is_empty()
            {
                worker.worker_type = worker_type.clone();
            }
            if let Some(url) = entry.worker_url.as_ref()
                && !url.is_empty()
            {
                worker.url = url.clone();
            }
            if let Some(title) = entry.title.as_ref()
                && !title.is_empty()
            {
                worker.title = title.clone();
            }
            worker.attached = matches!(
                entry.event_kind.as_str(),
                "worker_attached" | "worker_created" | "worker_info_changed"
            );
            if entry.event_kind == "worker_destroyed" {
                worker.attached = false;
                worker.destroyed = true;
            }
            worker.last_seen_seq = entry.seq;
            worker.last_seen_unix_ms = entry.observed_at_unix_ms;
        }
        while self.entries.len() >= self.capacity {
            self.entries.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.entries.push_back(entry);
    }
}

struct CaptureSlot {
    buffer: Arc<Mutex<RingBuffer>>,
    endpoint: String,
    armed_at_unix_ms: u64,
    capacity: usize,
    _browser: Browser,
    handler_task: JoinHandle<()>,
    listener_task: JoinHandle<()>,
}

impl Drop for CaptureSlot {
    fn drop(&mut self) {
        self.handler_task.abort();
        self.listener_task.abort();
    }
}

#[derive(Default)]
struct CaptureRegistry {
    slots: Mutex<HashMap<String, Arc<CaptureSlot>>>,
}

fn registry() -> &'static CaptureRegistry {
    static REGISTRY: OnceLock<CaptureRegistry> = OnceLock::new();
    REGISTRY.get_or_init(CaptureRegistry::default)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

pub async fn lifecycle_capture_ensure(
    endpoint: &str,
    target_id: &str,
    capacity: usize,
) -> A11yResult<CdpPageEventsCaptureStatus> {
    let target_id = target_id.trim();
    if target_id.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "page event capture target id must not be empty".to_owned(),
        });
    }

    if let Some(slot) = lookup_live(target_id) {
        return Ok(CdpPageEventsCaptureStatus {
            newly_armed: false,
            armed_at_unix_ms: slot.armed_at_unix_ms,
            endpoint: slot.endpoint.clone(),
            cdp_target_id: target_id.to_owned(),
            capacity: slot.capacity,
        });
    }

    let capacity = capacity.clamp(1, MAX_LIFECYCLE_BUFFER_CAPACITY);
    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("page event capture connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let armed = async {
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        page.execute(PageEnableParams::default())
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Page.enable for page event capture: {err}"),
            })?;
        page.execute(SetLifecycleEventsEnabledParams::new(true))
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Page.setLifecycleEventsEnabled for page event capture: {err}"),
            })?;

        let related_target_filter = TargetFilter::new(vec![
            FilterEntry::builder().r#type("page").exclude(false).build(),
            FilterEntry::builder()
                .r#type("worker")
                .exclude(false)
                .build(),
            FilterEntry::builder()
                .r#type("service_worker")
                .exclude(false)
                .build(),
            FilterEntry::builder()
                .r#type("shared_worker")
                .exclude(false)
                .build(),
        ]);
        browser
            .execute(
                AutoAttachRelatedParams::builder()
                    .target_id(TargetId::new(target_id.to_owned()))
                    .wait_for_debugger_on_start(false)
                    .filter(related_target_filter)
                    .build()
                    .map_err(|error| A11yError::CdpAxtreeFailed {
                        detail: format!("Target.autoAttachRelated related target params: {error}"),
                    })?,
            )
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!(
                    "Target.autoAttachRelated related targets for page event capture: {err}"
                ),
            })?;

        let dom_content_loaded = page
            .event_listener::<EventDomContentEventFired>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Page.domContentEventFired: {err}"),
            })?;
        let load = page
            .event_listener::<EventLoadEventFired>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Page.loadEventFired: {err}"),
            })?;
        let lifecycle = page
            .event_listener::<EventLifecycleEvent>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Page.lifecycleEvent: {err}"),
            })?;
        let frame_navigated =
            page.event_listener::<EventFrameNavigated>()
                .await
                .map_err(|err| A11yError::CdpAxtreeFailed {
                    detail: format!("subscribe Page.frameNavigated: {err}"),
                })?;
        let frame_started = page
            .event_listener::<EventFrameStartedNavigating>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Page.frameStartedNavigating: {err}"),
            })?;
        let frame_stopped = page
            .event_listener::<EventFrameStoppedLoading>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Page.frameStoppedLoading: {err}"),
            })?;
        let same_document = page
            .event_listener::<EventNavigatedWithinDocument>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Page.navigatedWithinDocument: {err}"),
            })?;
        let target_attached = browser
            .event_listener::<EventAttachedToTarget>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Target.attachedToTarget: {err}"),
            })?;
        let target_created = browser
            .event_listener::<EventTargetCreated>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Target.targetCreated: {err}"),
            })?;
        let target_destroyed = browser
            .event_listener::<EventTargetDestroyed>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Target.targetDestroyed: {err}"),
            })?;
        let target_info_changed = browser
            .event_listener::<EventTargetInfoChanged>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Target.targetInfoChanged: {err}"),
            })?;

        Ok::<_, A11yError>((
            page,
            dom_content_loaded,
            load,
            lifecycle,
            frame_navigated,
            frame_started,
            frame_stopped,
            same_document,
            target_attached,
            target_created,
            target_destroyed,
            target_info_changed,
        ))
    }
    .await;

    let (
        page,
        mut dom_content_loaded,
        mut load,
        mut lifecycle,
        mut frame_navigated,
        mut frame_started,
        mut frame_stopped,
        mut same_document,
        mut target_attached,
        mut target_created,
        mut target_destroyed,
        mut target_info_changed,
    ) = match armed {
        Ok(streams) => streams,
        Err(err) => {
            handler_task.abort();
            return Err(err);
        }
    };

    let buffer = Arc::new(Mutex::new(RingBuffer::new(capacity)));
    let pump_buffer = Arc::clone(&buffer);
    let root_target_id = target_id.to_owned();
    let listener_task = tokio::spawn(async move {
        let _page = page;
        let mut page_ids = HashSet::<String>::from([root_target_id.clone()]);
        let mut worker_ids = HashSet::<String>::new();
        loop {
            tokio::select! {
                Some(event) = dom_content_loaded.next() => {
                    push(&pump_buffer, dom_content_loaded_entry(&root_target_id, event.timestamp.inner().to_owned()));
                }
                Some(event) = load.next() => {
                    push(&pump_buffer, load_entry(&root_target_id, event.timestamp.inner().to_owned()));
                }
                Some(event) = lifecycle.next() => {
                    push(&pump_buffer, lifecycle_entry(&root_target_id, event.as_ref()));
                }
                Some(event) = frame_navigated.next() => {
                    push(&pump_buffer, frame_navigated_entry(&root_target_id, event.as_ref()));
                }
                Some(event) = frame_started.next() => {
                    push(&pump_buffer, frame_started_entry(&root_target_id, event.as_ref()));
                }
                Some(event) = frame_stopped.next() => {
                    push(&pump_buffer, frame_stopped_entry(&root_target_id, event.as_ref()));
                }
                Some(event) = same_document.next() => {
                    push(&pump_buffer, same_document_entry(&root_target_id, event.as_ref()));
                }
                Some(event) = target_attached.next() => {
                    let info = &event.target_info;
                    if is_related_page_target(&root_target_id, &page_ids, info) {
                        page_ids.insert(info.target_id.inner().clone());
                        push(&pump_buffer, page_entry("page_attached", &root_target_id, info));
                    } else if is_worker_type(&info.r#type) {
                        worker_ids.insert(info.target_id.inner().clone());
                        push(&pump_buffer, worker_entry("worker_attached", &root_target_id, info));
                    }
                }
                Some(event) = target_created.next() => {
                    let info = &event.target_info;
                    if is_related_page_target(&root_target_id, &page_ids, info) {
                        page_ids.insert(info.target_id.inner().clone());
                        push(&pump_buffer, page_entry("page_created", &root_target_id, info));
                    } else if is_worker_type(&info.r#type) {
                        worker_ids.insert(info.target_id.inner().clone());
                        push(&pump_buffer, worker_entry("worker_created", &root_target_id, info));
                    }
                }
                Some(event) = target_destroyed.next() => {
                    let target_id = event.target_id.inner().clone();
                    if target_id != root_target_id && page_ids.remove(&target_id) {
                        push(&pump_buffer, page_destroyed_entry(&root_target_id, &target_id));
                    } else if worker_ids.contains(&target_id) {
                        push(&pump_buffer, worker_destroyed_entry(&root_target_id, &target_id));
                    }
                }
                Some(event) = target_info_changed.next() => {
                    let info = &event.target_info;
                    if is_related_page_target(&root_target_id, &page_ids, info) {
                        page_ids.insert(info.target_id.inner().clone());
                        push(&pump_buffer, page_entry("page_info_changed", &root_target_id, info));
                    } else if is_worker_type(&info.r#type) {
                        worker_ids.insert(info.target_id.inner().clone());
                        push(&pump_buffer, worker_entry("worker_info_changed", &root_target_id, info));
                    }
                }
                else => break,
            }
        }
    });

    let armed_at_unix_ms = now_unix_ms();
    let slot = Arc::new(CaptureSlot {
        buffer,
        endpoint: endpoint.to_owned(),
        armed_at_unix_ms,
        capacity,
        _browser: browser,
        handler_task,
        listener_task,
    });
    if let Ok(mut slots) = registry().slots.lock() {
        if let Some(existing) = slots.get(target_id)
            && !existing.listener_task.is_finished()
        {
            return Ok(CdpPageEventsCaptureStatus {
                newly_armed: false,
                armed_at_unix_ms: existing.armed_at_unix_ms,
                endpoint: existing.endpoint.clone(),
                cdp_target_id: target_id.to_owned(),
                capacity: existing.capacity,
            });
        }
        slots.insert(target_id.to_owned(), slot);
    }

    Ok(CdpPageEventsCaptureStatus {
        newly_armed: true,
        armed_at_unix_ms,
        endpoint: endpoint.to_owned(),
        cdp_target_id: target_id.to_owned(),
        capacity,
    })
}

#[must_use]
pub fn lifecycle_capture_read(
    target_id: &str,
    filter: &CdpPageEventsReadFilter<'_>,
) -> Option<CdpPageEventsReadResult> {
    let slot = {
        let slots = registry().slots.lock().ok()?;
        Arc::clone(slots.get(target_id.trim())?)
    };
    let buffer = slot.buffer.lock().ok()?;
    let total_buffered = buffer.entries.len();
    let next_cursor = buffer.cursor();
    let dropped = buffer.dropped;
    let max = if filter.max == 0 {
        usize::MAX
    } else {
        filter.max
    };
    let entries: Vec<CdpPageEventEntry> = buffer
        .entries
        .iter()
        .filter(|entry| filter.since_seq.is_none_or(|since| entry.seq >= since))
        .filter(|entry| {
            filter
                .event_kind
                .is_none_or(|kind| entry.event_kind.eq_ignore_ascii_case(kind))
        })
        .filter(|entry| {
            filter.worker_type.is_none_or(|worker_type| {
                entry
                    .worker_type
                    .as_deref()
                    .is_some_and(|actual| actual.eq_ignore_ascii_case(worker_type))
            })
        })
        .take(max)
        .cloned()
        .collect();
    let mut pages = buffer.pages.values().cloned().collect::<Vec<_>>();
    pages.sort_by(|left, right| left.target_id.cmp(&right.target_id));
    let mut workers = buffer.workers.values().cloned().collect::<Vec<_>>();
    workers.sort_by(|left, right| left.worker_id.cmp(&right.worker_id));
    Some(CdpPageEventsReadResult {
        returned: entries.len(),
        entries,
        pages,
        workers,
        next_cursor,
        total_buffered,
        dropped,
        armed_at_unix_ms: slot.armed_at_unix_ms,
        capacity: slot.capacity,
    })
}

fn push(buffer: &Arc<Mutex<RingBuffer>>, entry: CdpPageEventEntry) {
    if let Ok(mut buffer) = buffer.lock() {
        buffer.push(entry);
    }
}

fn base_entry(event_kind: &str, target_id: &str) -> CdpPageEventEntry {
    CdpPageEventEntry {
        seq: 0,
        event_kind: event_kind.to_owned(),
        target_id: target_id.to_owned(),
        target_type: None,
        target_attached: None,
        page_target_id: None,
        opener_id: None,
        opener_frame_id: None,
        can_access_opener: None,
        browser_context_id: None,
        subtype: None,
        worker_id: None,
        worker_type: None,
        worker_url: None,
        frame_id: None,
        parent_frame_id: None,
        loader_id: None,
        name: None,
        url: None,
        title: None,
        navigation_type: None,
        timestamp_s: None,
        observed_at_unix_ms: now_unix_ms(),
    }
}

fn dom_content_loaded_entry(target_id: &str, timestamp_s: f64) -> CdpPageEventEntry {
    let mut entry = base_entry("domcontentloaded", target_id);
    entry.timestamp_s = Some(timestamp_s);
    entry
}

fn load_entry(target_id: &str, timestamp_s: f64) -> CdpPageEventEntry {
    let mut entry = base_entry("load", target_id);
    entry.timestamp_s = Some(timestamp_s);
    entry
}

fn lifecycle_entry(target_id: &str, event: &EventLifecycleEvent) -> CdpPageEventEntry {
    let mut entry = base_entry("lifecycle", target_id);
    entry.frame_id = Some(event.frame_id.inner().clone());
    entry.loader_id = Some(event.loader_id.inner().clone());
    entry.name = Some(event.name.clone());
    entry.timestamp_s = Some(*event.timestamp.inner());
    entry
}

fn frame_navigated_entry(target_id: &str, event: &EventFrameNavigated) -> CdpPageEventEntry {
    let mut entry = frame_entry("framenavigated", target_id, &event.frame);
    entry.navigation_type = Some(event.r#type.as_ref().to_owned());
    entry
}

fn frame_started_entry(target_id: &str, event: &EventFrameStartedNavigating) -> CdpPageEventEntry {
    let mut entry = base_entry("framestartednavigating", target_id);
    entry.frame_id = Some(event.frame_id.inner().clone());
    entry.loader_id = Some(event.loader_id.inner().clone());
    entry.url = Some(event.url.clone());
    entry.navigation_type = Some(event.navigation_type.as_ref().to_owned());
    entry
}

fn frame_stopped_entry(target_id: &str, event: &EventFrameStoppedLoading) -> CdpPageEventEntry {
    let mut entry = base_entry("framestoppedloading", target_id);
    entry.frame_id = Some(event.frame_id.inner().clone());
    entry
}

fn same_document_entry(target_id: &str, event: &EventNavigatedWithinDocument) -> CdpPageEventEntry {
    let mut entry = base_entry("framenavigated", target_id);
    entry.frame_id = Some(event.frame_id.inner().clone());
    entry.url = Some(event.url.clone());
    entry.navigation_type = Some(event.navigation_type.as_ref().to_owned());
    entry
}

fn frame_entry(event_kind: &str, target_id: &str, frame: &Frame) -> CdpPageEventEntry {
    let mut entry = base_entry(event_kind, target_id);
    entry.frame_id = Some(frame.id.inner().clone());
    entry.parent_frame_id = frame.parent_id.as_ref().map(|id| id.inner().clone());
    entry.loader_id = Some(frame.loader_id.inner().clone());
    entry.name = frame.name.clone();
    entry.url = Some(frame.url.clone());
    entry
}

fn page_entry(event_kind: &str, root_target_id: &str, info: &TargetInfo) -> CdpPageEventEntry {
    let mut entry = base_entry(event_kind, root_target_id);
    entry.target_type = Some(info.r#type.clone());
    entry.target_attached = Some(info.attached);
    entry.page_target_id = Some(info.target_id.inner().clone());
    entry.opener_id = info.opener_id.as_ref().map(|id| id.inner().clone());
    entry.opener_frame_id = info.opener_frame_id.as_ref().map(|id| id.inner().clone());
    entry.can_access_opener = Some(info.can_access_opener);
    entry.browser_context_id = info
        .browser_context_id
        .as_ref()
        .map(|id| id.inner().clone());
    entry.subtype = info.subtype.clone();
    entry.title = Some(info.title.clone());
    entry.url = Some(info.url.clone());
    entry
}

fn page_destroyed_entry(root_target_id: &str, page_target_id: &str) -> CdpPageEventEntry {
    let mut entry = base_entry("page_destroyed", root_target_id);
    entry.target_type = Some("page".to_owned());
    entry.page_target_id = Some(page_target_id.to_owned());
    entry
}

fn worker_entry(event_kind: &str, root_target_id: &str, info: &TargetInfo) -> CdpPageEventEntry {
    let mut entry = base_entry(event_kind, root_target_id);
    entry.target_type = Some(info.r#type.clone());
    entry.worker_id = Some(info.target_id.inner().clone());
    entry.worker_type = Some(info.r#type.clone());
    entry.worker_url = Some(info.url.clone());
    entry.title = Some(info.title.clone());
    entry.url = Some(info.url.clone());
    entry
}

fn worker_destroyed_entry(root_target_id: &str, worker_id: &str) -> CdpPageEventEntry {
    let mut entry = base_entry("worker_destroyed", root_target_id);
    entry.worker_id = Some(worker_id.to_owned());
    entry
}

fn is_related_page_target(
    root_target_id: &str,
    tracked_page_ids: &HashSet<String>,
    info: &TargetInfo,
) -> bool {
    if info.r#type != "page" {
        return false;
    }
    let target_id = info.target_id.inner();
    if target_id == root_target_id {
        return false;
    }
    tracked_page_ids.contains(target_id)
        || info
            .opener_id
            .as_ref()
            .is_some_and(|opener_id| tracked_page_ids.contains(opener_id.inner()))
}

fn is_worker_type(target_type: &str) -> bool {
    matches!(target_type, "worker" | "service_worker" | "shared_worker")
}

fn lookup_live(target_id: &str) -> Option<Arc<CaptureSlot>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_event_ring_buffer_tracks_workers_and_eviction() {
        let mut buffer = RingBuffer::new(2);
        buffer.push(worker_entry(
            "worker_created",
            "page-1",
            &TargetInfo::builder()
                .target_id(TargetId::new("worker-1"))
                .r#type("worker")
                .title("worker.js")
                .url("https://example.test/worker.js")
                .attached(true)
                .can_access_opener(false)
                .build()
                .expect("target info"),
        ));
        buffer.push(dom_content_loaded_entry("page-1", 1.0));
        buffer.push(load_entry("page-1", 2.0));

        assert_eq!(buffer.entries.len(), 2);
        assert_eq!(buffer.dropped, 1);
        let worker = buffer.workers.get("worker-1").expect("worker snapshot");
        assert_eq!(worker.worker_type, "worker");
        assert_eq!(worker.url, "https://example.test/worker.js");
    }

    #[test]
    fn page_event_ring_buffer_tracks_related_pages() {
        let mut buffer = RingBuffer::new(8);
        let popup = TargetInfo::builder()
            .target_id(TargetId::new("popup-1"))
            .r#type("page")
            .title("Popup")
            .url("https://example.test/popup")
            .attached(false)
            .opener_id(TargetId::new("page-1"))
            .can_access_opener(true)
            .build()
            .expect("target info");
        let unrelated = TargetInfo::builder()
            .target_id(TargetId::new("unrelated-1"))
            .r#type("page")
            .title("Unrelated")
            .url("https://elsewhere.test/")
            .attached(false)
            .can_access_opener(false)
            .build()
            .expect("target info");
        let mut page_ids = HashSet::<String>::from(["page-1".to_owned()]);

        assert!(is_related_page_target("page-1", &page_ids, &popup));
        assert!(!is_related_page_target("page-1", &page_ids, &unrelated));
        page_ids.insert(popup.target_id.inner().clone());

        buffer.push(page_entry("page_created", "page-1", &popup));
        buffer.push(page_destroyed_entry("page-1", "popup-1"));

        let page = buffer.pages.get("popup-1").expect("page snapshot");
        assert_eq!(page.target_type, "page");
        assert_eq!(page.url, "https://example.test/popup");
        assert_eq!(page.title, "Popup");
        assert_eq!(page.opener_id.as_deref(), Some("page-1"));
        assert!(!page.attached);
        assert!(page.destroyed);
        assert_eq!(page.first_seen_seq, 0);
        assert_eq!(page.last_seen_seq, 1);
    }

    #[test]
    fn worker_type_filter_is_strict() {
        assert!(is_worker_type("worker"));
        assert!(is_worker_type("service_worker"));
        assert!(is_worker_type("shared_worker"));
        assert!(!is_worker_type("page"));
        assert!(!is_worker_type("iframe"));
    }
}
