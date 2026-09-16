//! Persistent per-target console + page-error capture over CDP (#1091/#1092/#1093).
//!
//! Agents driving a web page through Synapse were blind to `console.*` output
//! and uncaught JS exceptions — the Playwright `page.on('console')` /
//! `page.on('pageerror')` surface. This module closes that gap.
//!
//! ## Why a persistent listener (and not connect-on-demand)
//!
//! The rest of the raw-CDP surface ([`crate::cdp_action::with_target_page`])
//! connects a fresh `chromiumoxide` client per call, runs one command, and drops
//! it. That model cannot capture console output, because Chrome does **not**
//! replay `Runtime.consoleAPICalled` history when a client sends
//! `Runtime.enable` — the events are delivered live, once, to whoever is
//! attached at the moment they fire. A connect-on-demand reader would only ever
//! see messages emitted during the few milliseconds it happened to be attached.
//!
//! So this module keeps **one long-lived CDP connection per captured target**.
//! When a target is first *armed* ([`console_capture_ensure`]) it connects,
//! enables `Runtime` + `Log`, subscribes to `Runtime.consoleAPICalled`,
//! `Runtime.exceptionThrown`, and `Log.entryAdded`, and pumps every event into a
//! bounded per-target ring buffer. [`console_capture_read`] returns a filtered,
//! cursor-delimited view of that buffer without consuming it (so multiple
//! readers and delta polling both work). The connection lives until the target
//! closes (the event streams end → the pump exits) or [`console_capture_stop`]
//! tears it down.
//!
//! Background-first: nothing here activates a tab or touches the OS foreground.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use chromiumoxide::Browser;
use chromiumoxide::cdp::browser_protocol::log::{
    EnableParams as LogEnableParams, EventEntryAdded, LogEntry,
};
use chromiumoxide::cdp::js_protocol::runtime::{
    EnableParams as RuntimeEnableParams, EventConsoleApiCalled, EventExceptionThrown,
    ExceptionDetails, ObjectPreview, ObjectPreviewSubtype, PropertyPreview, PropertyPreviewType,
    RemoteObject, RemoteObjectType, StackTrace,
};
use futures_util::StreamExt as _;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::task::JoinHandle;

use crate::cdp_value::{cdp_enum_str as enum_str, cdp_number_f64_or_zero};
use crate::{A11yError, A11yResult};

/// Default ring-buffer capacity (entries) per captured target. Bounded so a
/// chatty page cannot grow daemon memory without limit; oldest entries are
/// evicted first and the eviction count is reported back to callers.
pub const DEFAULT_CONSOLE_BUFFER_CAPACITY: usize = 1000;
/// Hard ceiling on a requested buffer capacity (defends against absurd values).
pub const MAX_CONSOLE_BUFFER_CAPACITY: usize = 10_000;
/// Per-entry display-text / stack truncation guard.
const MAX_TEXT_CHARS: usize = 16_384;

/// One captured console / page-error / browser-log record. `seq` is a
/// per-target monotonic cursor used for delta reads.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ConsoleEntry {
    /// Monotonic per-target sequence number (cursor for since-cursor reads).
    pub seq: u64,
    /// Origin class: `console-api`, `page-error`, `unhandled-rejection`, or
    /// `browser-log`. Lets a caller tell an uncaught throw from an unhandled
    /// promise rejection from a `console.error`.
    pub source: &'static str,
    /// Severity / call type: `log`, `info`, `warning`, `error`, `debug`,
    /// `trace`, `verbose`, … (the CDP type/level string).
    pub level: String,
    /// Rendered, space-joined display text (the args, or the exception message).
    pub text: String,
    /// Structured rendered arguments (primitives as JSON, objects/arrays from
    /// their CDP preview — never `[object Object]`). Empty for page errors.
    pub args: Vec<Value>,
    /// Source URL of the call / error, when CDP reports a location.
    pub url: Option<String>,
    /// 1-based source line, when known.
    pub line: Option<u32>,
    /// 1-based source column, when known.
    pub column: Option<u32>,
    /// Formatted stack trace (`    at fn (url:line:col)` per frame), when known.
    pub stack: Option<String>,
    /// `Log.entryAdded` sub-source (`network`, `security`, `deprecation`, …);
    /// `None` for `console-api` / exception entries.
    pub category: Option<String>,
    /// Event timestamp, milliseconds since the Unix epoch (CDP `Timestamp`).
    pub timestamp_ms: f64,
}

/// Result of [`console_capture_read`]: a filtered, cursor-delimited slice of a
/// target's console buffer plus the bookkeeping a delta poller needs.
#[derive(Clone, Debug, Serialize)]
pub struct ConsoleReadResult {
    pub entries: Vec<ConsoleEntry>,
    /// Exclusive upper bound of buffered sequence numbers (the seq the next
    /// captured entry will receive). Pass back as `since_seq` next call to
    /// receive only entries added since this read. Stable even when the returned
    /// slice is empty.
    pub next_cursor: u64,
    /// Entries returned after filtering + capping.
    pub returned: usize,
    /// Entries currently held in the ring buffer (pre-filter).
    pub total_buffered: usize,
    /// Entries evicted over the target's lifetime because the buffer was full.
    pub dropped: u64,
    /// When capture for this target was first armed (Unix ms).
    pub armed_at_unix_ms: f64,
}

/// Outcome of [`console_capture_ensure`].
#[derive(Clone, Debug, Serialize)]
pub struct ConsoleCaptureStatus {
    /// `true` if this call established the capture (it did not exist or had
    /// died and was re-armed); `false` if an existing live capture was reused.
    pub newly_armed: bool,
    /// When capture for this target was armed (Unix ms).
    pub armed_at_unix_ms: f64,
    pub endpoint: String,
    pub cdp_target_id: String,
}

/// Optional filters for [`console_capture_read`].
#[derive(Clone, Debug, Default)]
pub struct ConsoleReadFilter<'a> {
    /// Only entries with `seq >= since_seq` (delta semantics). Pass the prior
    /// read's `next_cursor` to receive only entries added since.
    pub since_seq: Option<u64>,
    /// Exact level match (case-insensitive), e.g. `error`, `warning`.
    pub level: Option<&'a str>,
    /// Exact source-class match, e.g. `page-error`, `unhandled-rejection`.
    pub source: Option<&'a str>,
    /// Substring match against the entry's display text (case-insensitive).
    pub text_contains: Option<&'a str>,
    /// Maximum entries to return (oldest-first after the cursor, capped here).
    pub max: usize,
}

struct RingBuffer {
    entries: VecDeque<ConsoleEntry>,
    capacity: usize,
    next_seq: u64,
    dropped: u64,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity.min(256)),
            capacity: capacity.max(1),
            next_seq: 0,
            dropped: 0,
        }
    }

    fn push(&mut self, mut entry: ConsoleEntry) {
        entry.seq = self.next_seq;
        self.next_seq += 1;
        while self.entries.len() >= self.capacity {
            self.entries.pop_front();
            self.dropped += 1;
        }
        self.entries.push_back(entry);
    }

    /// Highest assigned seq, or 0 when empty. Equal to `next_seq` (the cursor a
    /// caller should pass to receive only strictly-newer entries).
    const fn cursor(&self) -> u64 {
        self.next_seq
    }
}

struct CaptureSlot {
    buffer: Arc<Mutex<RingBuffer>>,
    endpoint: String,
    armed_at_unix_ms: f64,
    // Keep the connection + pumps alive for the capture's lifetime. Dropped /
    // aborted when the slot is removed (target closed / explicit stop).
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

fn now_unix_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}

/// Arms (or re-arms) persistent console capture for `target_id` over `endpoint`.
///
/// Idempotent: if a live capture already exists it is reused and
/// `newly_armed=false`. If the prior capture's pump has exited (target closed,
/// connection dropped) it is torn down and re-established. Capture begins from
/// the moment of arming — messages emitted before the first arm are not
/// retroactively captured (Chrome does not replay console history), so arm a
/// target as early as the session owns it for gap-free capture.
///
/// # Errors
///
/// `A11Y_CDP_ATTACH_FAILED` if the endpoint/target cannot be reached;
/// `A11Y_CDP_AXTREE_FAILED` if enabling the `Runtime`/`Log` domains or
/// subscribing to the event streams fails.
pub async fn console_capture_ensure(
    endpoint: &str,
    target_id: &str,
    capacity: usize,
) -> A11yResult<ConsoleCaptureStatus> {
    let target_id = target_id.trim();
    if target_id.is_empty() {
        return Err(A11yError::CdpAttachFailed {
            detail: "console capture target id must not be empty".to_owned(),
        });
    }

    // Reuse a live capture without reconnecting.
    if let Some(slot) = lookup_live(target_id) {
        return Ok(ConsoleCaptureStatus {
            newly_armed: false,
            armed_at_unix_ms: slot.armed_at_unix_ms,
            endpoint: slot.endpoint.clone(),
            cdp_target_id: target_id.to_owned(),
        });
    }

    let capacity = capacity.clamp(1, MAX_CONSOLE_BUFFER_CAPACITY);
    let (browser, mut handler) =
        Browser::connect(endpoint)
            .await
            .map_err(|err| A11yError::CdpAttachFailed {
                detail: format!("console capture connect {endpoint}: {err}"),
            })?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    // If anything below fails, abort the handler so we don't leak the
    // connection/task on the error path.
    let armed = async {
        let page = crate::cdp_action::get_target_page_with_discovery(&browser, target_id).await?;
        page.execute(RuntimeEnableParams::default())
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Runtime.enable for console capture: {err}"),
            })?;
        page.execute(LogEnableParams::default())
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("Log.enable for console capture: {err}"),
            })?;
        let console = page
            .event_listener::<EventConsoleApiCalled>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Runtime.consoleAPICalled: {err}"),
            })?;
        let exceptions = page
            .event_listener::<EventExceptionThrown>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Runtime.exceptionThrown: {err}"),
            })?;
        let logs = page
            .event_listener::<EventEntryAdded>()
            .await
            .map_err(|err| A11yError::CdpAxtreeFailed {
                detail: format!("subscribe Log.entryAdded: {err}"),
            })?;
        Ok::<_, A11yError>((page, console, exceptions, logs))
    }
    .await;

    let (page, mut console, mut exceptions, mut logs) = match armed {
        Ok(streams) => streams,
        Err(err) => {
            handler_task.abort();
            return Err(err);
        }
    };

    let buffer = Arc::new(Mutex::new(RingBuffer::new(capacity)));
    let pump_buffer = Arc::clone(&buffer);
    let listener_task = tokio::spawn(async move {
        // Hold the page for the pump's lifetime so the session (and its enabled
        // Runtime/Log domains) stays open while we drain the streams.
        let _page = page;
        loop {
            tokio::select! {
                Some(event) = console.next() => {
                    let entry = console_api_entry(event.as_ref());
                    push(&pump_buffer, entry);
                }
                Some(event) = exceptions.next() => {
                    let entry = exception_entry(&event.exception_details, ts_ms(&event.timestamp));
                    push(&pump_buffer, entry);
                }
                Some(event) = logs.next() => {
                    let entry = log_entry(&event.entry);
                    push(&pump_buffer, entry);
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
        _browser: browser,
        handler_task,
        listener_task,
    });
    if let Ok(mut slots) = registry().slots.lock() {
        // A racing arm may have inserted a live slot; if so, keep theirs and let
        // ours (`slot`) drop at return, aborting its tasks.
        if let Some(existing) = slots.get(target_id)
            && !existing.listener_task.is_finished()
        {
            return Ok(ConsoleCaptureStatus {
                newly_armed: false,
                armed_at_unix_ms: existing.armed_at_unix_ms,
                endpoint: existing.endpoint.clone(),
                cdp_target_id: target_id.to_owned(),
            });
        }
        slots.insert(target_id.to_owned(), slot);
    }
    Ok(ConsoleCaptureStatus {
        newly_armed: true,
        armed_at_unix_ms,
        endpoint: endpoint.to_owned(),
        cdp_target_id: target_id.to_owned(),
    })
}

/// Reads a filtered, cursor-delimited slice of a target's console buffer without
/// consuming it. Returns `None` if the target was never armed (so the caller can
/// surface a precise "not armed" error rather than an empty success).
#[must_use]
pub fn console_capture_read(
    target_id: &str,
    filter: &ConsoleReadFilter,
) -> Option<ConsoleReadResult> {
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
    let entries: Vec<ConsoleEntry> = buffer
        .entries
        .iter()
        .filter(|e| filter.since_seq.is_none_or(|since| e.seq >= since))
        .filter(|e| {
            filter
                .level
                .is_none_or(|lvl| e.level.eq_ignore_ascii_case(lvl))
        })
        .filter(|e| filter.source.is_none_or(|src| e.source == src))
        .filter(|e| {
            filter
                .text_contains
                .is_none_or(|needle| e.text.to_lowercase().contains(&needle.to_lowercase()))
        })
        .take(max)
        .cloned()
        .collect();
    Some(ConsoleReadResult {
        returned: entries.len(),
        entries,
        next_cursor,
        total_buffered,
        dropped,
        armed_at_unix_ms: slot.armed_at_unix_ms,
    })
}

/// Tears down capture for a target (e.g. when its tab is closed). Idempotent.
/// Returns `true` if a capture was removed.
#[must_use]
pub fn console_capture_stop(target_id: &str) -> bool {
    registry()
        .slots
        .lock()
        .ok()
        .and_then(|mut slots| slots.remove(target_id.trim()))
        .is_some()
}

/// Number of targets with a registered capture slot (live or not-yet-reaped).
#[must_use]
pub fn console_capture_active_count() -> usize {
    registry().slots.lock().map_or(0, |s| s.len())
}

fn lookup_live(target_id: &str) -> Option<Arc<CaptureSlot>> {
    let mut slots = registry().slots.lock().ok()?;
    match slots.get(target_id) {
        Some(slot) if !slot.listener_task.is_finished() => Some(Arc::clone(slot)),
        Some(_) => {
            // Pump exited (target closed / connection dropped) — reap so the
            // next ensure re-arms cleanly.
            slots.remove(target_id);
            None
        }
        None => None,
    }
}

fn push(buffer: &Arc<Mutex<RingBuffer>>, entry: ConsoleEntry) {
    if let Ok(mut buf) = buffer.lock() {
        buf.push(entry);
    }
}

// === Event → ConsoleEntry rendering ========================================

fn console_api_entry(event: &EventConsoleApiCalled) -> ConsoleEntry {
    let level = enum_str(&event.r#type);
    let args: Vec<Value> = event.args.iter().map(render_remote_object).collect();
    let text = truncate(&args.iter().map(value_to_text).collect::<Vec<_>>().join(" "));
    let (url, line, column) = top_frame_location(event.stack_trace.as_ref());
    let stack = event
        .stack_trace
        .as_ref()
        .map(format_stack)
        .filter(|s| !s.is_empty())
        .map(|s| truncate(&s));
    ConsoleEntry {
        seq: 0,
        source: "console-api",
        level,
        text,
        args,
        url,
        line,
        column,
        stack,
        category: None,
        timestamp_ms: ts_ms(&event.timestamp),
    }
}

fn exception_entry(details: &ExceptionDetails, timestamp_ms: f64) -> ConsoleEntry {
    let source = if details.text.contains("(in promise)") {
        "unhandled-rejection"
    } else {
        "page-error"
    };
    let stack = exception_stack(details).map(|s| truncate(&s));
    let (url, line, column) = (
        details.url.clone(),
        u32::try_from(details.line_number + 1).ok(),
        u32::try_from(details.column_number + 1).ok(),
    );
    ConsoleEntry {
        seq: 0,
        source,
        level: "error".to_owned(),
        text: truncate(&exception_message(details)),
        args: Vec::new(),
        url,
        line,
        column,
        stack,
        category: None,
        timestamp_ms,
    }
}

fn log_entry(entry: &LogEntry) -> ConsoleEntry {
    let level = enum_str(&entry.level);
    let category = Some(enum_str(&entry.source));
    let args: Vec<Value> = entry
        .args
        .as_ref()
        .map(|a| a.iter().map(render_remote_object).collect())
        .unwrap_or_default();
    let stack = entry
        .stack_trace
        .as_ref()
        .map(format_stack)
        .filter(|s| !s.is_empty())
        .map(|s| truncate(&s));
    ConsoleEntry {
        seq: 0,
        source: "browser-log",
        level,
        text: truncate(&entry.text),
        args,
        url: entry.url.clone(),
        line: entry.line_number.and_then(|n| u32::try_from(n + 1).ok()),
        column: None,
        stack,
        category,
        timestamp_ms: ts_ms(&entry.timestamp),
    }
}

/// First line of the exception's full description (e.g. `Error: boom`), falling
/// back to the CDP `text` prefix combined with a rendered primitive rejection
/// value (`throw "x"` / `Promise.reject("x")`).
fn exception_message(details: &ExceptionDetails) -> String {
    if let Some(exception) = &details.exception {
        if let Some(description) = &exception.description {
            return description.lines().next().unwrap_or(description).to_owned();
        }
        let rendered = value_to_text(&render_remote_object(exception));
        if !rendered.is_empty() {
            return format!("{}: {}", details.text, rendered);
        }
    }
    details.text.clone()
}

fn exception_stack(details: &ExceptionDetails) -> Option<String> {
    if let Some(stack) = &details.stack_trace {
        let formatted = format_stack(stack);
        if !formatted.is_empty() {
            return Some(formatted);
        }
    }
    // No structured frames — the description usually carries the textual stack.
    details
        .exception
        .as_ref()
        .and_then(|e| e.description.clone())
        .filter(|d| d.contains('\n'))
}

fn format_stack(stack: &StackTrace) -> String {
    stack
        .call_frames
        .iter()
        .map(|frame| {
            let name = if frame.function_name.is_empty() {
                "<anonymous>"
            } else {
                frame.function_name.as_str()
            };
            format!(
                "    at {} ({}:{}:{})",
                name,
                frame.url,
                frame.line_number + 1,
                frame.column_number + 1
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn top_frame_location(stack: Option<&StackTrace>) -> (Option<String>, Option<u32>, Option<u32>) {
    stack
        .and_then(|s| s.call_frames.first())
        .map_or((None, None, None), |f| {
            (
                Some(f.url.clone()),
                u32::try_from(f.line_number + 1).ok(),
                u32::try_from(f.column_number + 1).ok(),
            )
        })
}

/// Renders a CDP `RemoteObject` console argument to a structured JSON value.
/// Primitives use their literal value; objects/arrays are reconstructed from the
/// CDP `preview` so consumers get `{"a":1}` rather than `[object Object]`.
fn render_remote_object(object: &RemoteObject) -> Value {
    if let Some(value) = &object.value {
        return value.clone();
    }
    if let Some(unserializable) = &object.unserializable_value {
        return serde_json::to_value(unserializable).unwrap_or(Value::Null);
    }
    if matches!(object.r#type, RemoteObjectType::Undefined) {
        return Value::String("undefined".to_owned());
    }
    if let Some(preview) = &object.preview {
        return render_preview(preview);
    }
    if let Some(description) = &object.description {
        return Value::String(description.clone());
    }
    Value::Null
}

fn render_preview(preview: &ObjectPreview) -> Value {
    if matches!(preview.subtype, Some(ObjectPreviewSubtype::Array)) {
        let mut array: Vec<Value> = preview.properties.iter().map(render_property).collect();
        if preview.overflow {
            array.push(Value::String("…".to_owned()));
        }
        return Value::Array(array);
    }
    let mut map = Map::new();
    for property in &preview.properties {
        map.insert(property.name.clone(), render_property(property));
    }
    if preview.overflow {
        map.insert("…".to_owned(), Value::String("…".to_owned()));
    }
    Value::Object(map)
}

fn render_property(property: &PropertyPreview) -> Value {
    let raw = property.value.clone().unwrap_or_default();
    match property.r#type {
        PropertyPreviewType::Number => parse_number(&raw),
        PropertyPreviewType::Boolean => match raw.as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => Value::String(raw),
        },
        PropertyPreviewType::Undefined => Value::Null,
        PropertyPreviewType::String => Value::String(raw),
        _ => property
            .value_preview
            .as_ref()
            .map(render_preview)
            .unwrap_or(Value::String(raw)),
    }
}

/// Parses a CDP preview numeric string, preserving integer-ness (so `1` stays
/// the integer `1`, matching `JSON.stringify`, rather than becoming `1.0`).
fn parse_number(raw: &str) -> Value {
    if let Ok(int) = raw.parse::<i64>() {
        return Value::Number(int.into());
    }
    if let Ok(uint) = raw.parse::<u64>() {
        return Value::Number(uint.into());
    }
    serde_json::from_str::<f64>(raw)
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map_or_else(|| Value::String(raw.to_owned()), Value::Number)
}

fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// CDP `Timestamp` serializes as a JSON number (ms since epoch).
fn ts_ms<T: Serialize>(value: &T) -> f64 {
    cdp_number_f64_or_zero(value)
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_TEXT_CHARS {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(MAX_TEXT_CHARS).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(seq_text: &str, level: &str, source: &'static str) -> ConsoleEntry {
        ConsoleEntry {
            seq: 0,
            source,
            level: level.to_owned(),
            text: seq_text.to_owned(),
            args: vec![Value::String(seq_text.to_owned())],
            url: None,
            line: None,
            column: None,
            stack: None,
            category: None,
            timestamp_ms: 0.0,
        }
    }

    #[test]
    fn ring_buffer_assigns_monotonic_seq_and_evicts_oldest() {
        let mut buf = RingBuffer::new(3);
        for i in 0..5 {
            buf.push(entry(&format!("m{i}"), "log", "console-api"));
        }
        // capacity 3 → only the last three survive, seqs are monotonic
        let seqs: Vec<u64> = buf.entries.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![2, 3, 4]);
        assert_eq!(buf.dropped, 2);
        assert_eq!(buf.cursor(), 5);
        let texts: Vec<&str> = buf.entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["m2", "m3", "m4"]);
    }

    #[test]
    fn render_preview_reconstructs_object_not_object_object() {
        // Real CDP consoleAPICalled object-arg shape (preview with properties).
        let json = serde_json::json!({
            "type": "object",
            "className": "Object",
            "description": "Object",
            "objectId": "1.2.3",
            "preview": {
                "type": "object",
                "description": "Object",
                "overflow": false,
                "properties": [
                    {"name": "a", "type": "number", "value": "1"},
                    {"name": "ok", "type": "boolean", "value": "true"},
                    {"name": "name", "type": "string", "value": "synapse"}
                ]
            }
        });
        let object: RemoteObject = serde_json::from_value(json).expect("remote object");
        let rendered = render_remote_object(&object);
        assert_eq!(
            rendered,
            serde_json::json!({"a": 1, "ok": true, "name": "synapse"})
        );
        // never the useless coercion
        assert_ne!(value_to_text(&rendered), "[object Object]");
    }

    #[test]
    fn render_preview_reconstructs_array() {
        let json = serde_json::json!({
            "type": "object",
            "subtype": "array",
            "className": "Array",
            "description": "Array(3)",
            "objectId": "1.2.4",
            "preview": {
                "type": "object",
                "subtype": "array",
                "description": "Array(3)",
                "overflow": false,
                "properties": [
                    {"name": "0", "type": "number", "value": "10"},
                    {"name": "1", "type": "number", "value": "20"},
                    {"name": "2", "type": "string", "value": "z"}
                ]
            }
        });
        let object: RemoteObject = serde_json::from_value(json).expect("remote object");
        assert_eq!(
            render_remote_object(&object),
            serde_json::json!([10, 20, "z"])
        );
    }

    #[test]
    fn render_primitive_args_use_literal_values() {
        let string: RemoteObject =
            serde_json::from_value(serde_json::json!({"type": "string", "value": "hi"})).unwrap();
        assert_eq!(render_remote_object(&string), serde_json::json!("hi"));
        let number: RemoteObject =
            serde_json::from_value(serde_json::json!({"type": "number", "value": 42})).unwrap();
        assert_eq!(render_remote_object(&number), serde_json::json!(42));
        let undef: RemoteObject =
            serde_json::from_value(serde_json::json!({"type": "undefined"})).unwrap();
        assert_eq!(render_remote_object(&undef), serde_json::json!("undefined"));
    }

    #[test]
    fn exception_entry_distinguishes_throw_from_rejection() {
        let throw: ExceptionDetails = serde_json::from_value(serde_json::json!({
            "exceptionId": 1,
            "text": "Uncaught",
            "lineNumber": 0,
            "columnNumber": 6,
            "url": "https://x/app.js",
            "exception": {"type": "object", "subtype": "error", "className": "Error",
                "description": "Error: boom\n    at app (https://x/app.js:1:7)"}
        }))
        .unwrap();
        let e = exception_entry(&throw, 123.0);
        assert_eq!(e.source, "page-error");
        assert_eq!(e.level, "error");
        assert_eq!(e.text, "Error: boom");
        assert!(e.stack.as_deref().unwrap().contains("at app"));
        assert_eq!(e.line, Some(1));

        let rejection: ExceptionDetails = serde_json::from_value(serde_json::json!({
            "exceptionId": 2,
            "text": "Uncaught (in promise) Error: rejected",
            "lineNumber": 2,
            "columnNumber": 0,
            "exception": {"type": "object", "subtype": "error", "className": "Error",
                "description": "Error: rejected\n    at p (https://x/app.js:3:1)"}
        }))
        .unwrap();
        let r = exception_entry(&rejection, 124.0);
        assert_eq!(r.source, "unhandled-rejection");
        assert_eq!(r.text, "Error: rejected");
    }

    #[test]
    fn read_filter_cursor_level_and_source() {
        let mut buf = RingBuffer::new(10);
        buf.push(entry("first log", "log", "console-api"));
        buf.push(entry("an error", "error", "console-api"));
        buf.push(entry("boom", "error", "page-error"));
        // Manual filter pass mirroring console_capture_read's predicate chain:
        // since_seq=1 means "seq >= 1", returning the two entries after the
        // zeroth (delta cursor semantics).
        let after_first: Vec<&ConsoleEntry> = buf.entries.iter().filter(|e| e.seq >= 1).collect();
        assert_eq!(after_first.len(), 2);
        let only_errors: Vec<&ConsoleEntry> = buf
            .entries
            .iter()
            .filter(|e| e.level.eq_ignore_ascii_case("error"))
            .collect();
        assert_eq!(only_errors.len(), 2);
        let only_page_errors: Vec<&ConsoleEntry> = buf
            .entries
            .iter()
            .filter(|e| e.source == "page-error")
            .collect();
        assert_eq!(only_page_errors.len(), 1);
        assert_eq!(only_page_errors[0].text, "boom");
    }
}
