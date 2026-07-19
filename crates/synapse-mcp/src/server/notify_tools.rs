//! `notify_human` — fire-and-forget Windows toast notifications from the daemon
//! (issue #866, assist-surface epic #833).
//!
//! Design notes (no silent failures, verified delivery):
//! - `ToastNotifier::Show` reports success even when Windows drops the toast
//!   (e.g. unregistered AUMID), so this module never trusts the return value
//!   alone. It registers the Synapse AUMID under
//!   `HKCU\Software\Classes\AppUserModelId` with a registry readback, checks
//!   `ToastNotifier::Setting()` and maps every disabled state to a distinct
//!   error code, and after `Show` polls Action Center history until the toast
//!   (matched by tag+group) is physically present — erroring with
//!   `NOTIFY_DELIVERY_UNVERIFIED` if it never appears.
//! - `dedupe_key` suppression uses Action Center itself as the source of
//!   truth: while the exact same payload with that key is still in history,
//!   repeats are suppressed (`deduped: true`). A key collision with different
//!   content fails closed instead of misreporting another toast as delivery.

use std::{any::Any, sync::Arc};

use rmcp::{RoleServer, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synapse_core::error_codes;

use super::{ErrorData, Json, Parameters, SynapseService, mcp_error, tool, tool_router};

/// Application User Model ID registered for daemon-raised toasts.
pub const SYNAPSE_AUMID: &str = "Synapse.Daemon";
/// Display name shown on toasts and in Windows notification settings.
pub const SYNAPSE_NOTIFY_DISPLAY_NAME: &str = "Synapse";
/// Action Center group shared by all daemon toasts.
pub const SYNAPSE_TOAST_GROUP: &str = "synapse";
/// Reserved group used only by the escalation state machine. Public
/// `notify_human` and other M3 callers cannot select this namespace.
pub(crate) const SYNAPSE_ESCALATION_TOAST_GROUP: &str = "synapse-escalation-v1";

pub(crate) const MAX_TITLE_CHARS: usize = 200;
pub(crate) const MAX_BODY_CHARS: usize = 2000;
const MAX_DEDUPE_KEY_CHARS: usize = 256;
const TOAST_PAYLOAD_SCHEMA_VERSION: u32 = 1;
const MAX_FROZEN_TOAST_XML_BYTES: usize = 32 * 1024;
#[cfg(windows)]
const HISTORY_VERIFY_TIMEOUT_MS: u64 = 3_000;
#[cfg(windows)]
const HISTORY_VERIFY_POLL_MS: u64 = 100;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotifyKind {
    Info,
    Success,
    Warning,
    Error,
}

impl NotifyKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    /// Warnings and errors stay on screen longer.
    const fn toast_duration(self) -> &'static str {
        match self {
            Self::Info | Self::Success => "short",
            Self::Warning | Self::Error => "long",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotifyHumanParams {
    /// Toast headline. Required, non-empty, at most 200 characters.
    pub title: String,
    /// Toast body text. May be empty; at most 2000 characters.
    pub body: String,
    /// Severity of the notification: info, success, warning, or error.
    /// warning/error toasts use the long display duration.
    pub kind: NotifyKind,
    /// Optional suppression key. While the exact same payload with this key is
    /// present in Action Center, repeats are suppressed (deduped=true,
    /// shown=false). Reusing a live key for different content is an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    /// Deliver straight to Action Center without a popup banner. The toast is
    /// still verified in Action Center history. Default false.
    #[serde(default)]
    pub suppress_popup: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotifyHumanResponse {
    /// True when a new toast was raised; false when deduped.
    pub shown: bool,
    /// True when an existing toast with the same dedupe_key suppressed this one.
    pub deduped: bool,
    /// AUMID the toast was raised under.
    pub aumid: String,
    /// Platform tag identifying this toast in Action Center (derived from
    /// dedupe_key when given, otherwise unique per call).
    pub tag: String,
    /// Action Center group shared by Synapse toasts.
    pub group: String,
    /// Windows notification setting at send time: "enabled", or
    /// "unavailable_first_use" when the per-app notification record did not
    /// exist yet (only before the first-ever Synapse toast; Windows creates
    /// it on first Show). Every disabled state is a distinct error instead.
    pub notification_setting: String,
    /// True when the toast was read back from Action Center history after
    /// Show — physical delivery proof, not an assumption.
    pub verified_in_history: bool,
    /// Toasts with this tag+group present in Action Center history after the
    /// operation.
    pub history_count: u32,
    /// SHA-256 of the domain-separated payload envelope: schema marker,
    /// suppress-popup byte, and canonical WinRT XML read from the one matching
    /// Action Center row.
    pub payload_sha256: String,
    /// Physical Action Center expiration in Unix milliseconds. Escalation
    /// toasts bind this to their durable TTL; ordinary toasts have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiration_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToastRemovalOutcome {
    pub aumid: String,
    pub tag: String,
    pub group: String,
    pub status: String,
    pub removed: bool,
    pub already_absent: bool,
    pub before_count: Option<u32>,
    pub after_count: Option<u32>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToastHistoryReadback {
    pub aumid: String,
    pub tag: String,
    pub group: String,
    pub history_count: u32,
    pub present: bool,
    /// Exact payload digests for every matching Tag+Group row, in history order.
    pub payload_sha256s: Vec<String>,
    /// Expiration for each matching row, in the same history order.
    #[serde(default)]
    pub expiration_unix_ms: Vec<Option<u64>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToastCleanupReport {
    pub aumid: String,
    pub group: String,
    pub status: String,
    pub scanned: u32,
    pub candidates: u32,
    pub preserved_open: u32,
    pub removed: u32,
    pub already_absent: u32,
    pub failed: u32,
    pub outcomes: Vec<ToastRemovalOutcome>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedToastPayload {
    pub schema_version: u32,
    /// Exact canonical WinRT LoadXml/GetXml output frozen before authorization.
    pub canonical_xml: String,
    pub suppress_popup: bool,
    /// Domain-separated digest over schema marker, suppress-popup, and XML.
    pub payload_sha256: String,
}

#[cfg(not(windows))]
impl ToastRemovalOutcome {
    fn unsupported(tag: String, group: String) -> Self {
        Self {
            aumid: SYNAPSE_AUMID.to_owned(),
            tag,
            group,
            status: "unsupported_platform".to_owned(),
            removed: false,
            already_absent: false,
            before_count: None,
            after_count: None,
            error_code: Some(error_codes::NOTIFY_UNSUPPORTED_PLATFORM.to_owned()),
            error_message: Some(
                "toast history removal requires Windows notification support".to_owned(),
            ),
        }
    }
}

#[cfg(not(windows))]
impl ToastCleanupReport {
    fn unsupported() -> Self {
        Self {
            aumid: SYNAPSE_AUMID.to_owned(),
            group: SYNAPSE_ESCALATION_TOAST_GROUP.to_owned(),
            status: "unsupported_platform".to_owned(),
            scanned: 0,
            candidates: 0,
            preserved_open: 0,
            removed: 0,
            already_absent: 0,
            failed: 0,
            outcomes: Vec::new(),
            error_code: Some(error_codes::NOTIFY_UNSUPPORTED_PLATFORM.to_owned()),
            error_message: Some(
                "toast history cleanup requires Windows notification support".to_owned(),
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToastAction {
    pub content: String,
    pub arguments: String,
    pub activation_type: ToastActionActivationType,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum ToastActionActivationType {
    Foreground,
    Protocol,
}

impl ToastActionActivationType {
    const fn as_xml_value(self) -> &'static str {
        match self {
            Self::Foreground => "foreground",
            Self::Protocol => "protocol",
        }
    }
}

pub(crate) type ToastActivationCallback = Arc<dyn Fn(String) + Send + Sync + 'static>;

pub(crate) struct ToastPreShowFailure {
    pub code: &'static str,
    pub message: String,
}

/// Opaque ownership token returned by an escalation authorizer on the COM
/// worker. Dropping it releases the agent-transition boundary. Pre-Show
/// cancellation reconciliation runs while that boundary is still owned.
pub(crate) struct ToastShowAuthority {
    _owner: Box<dyn Any>,
    on_pre_show_cancel: Option<ToastPreShowCancel>,
}

type ToastPreShowCancel = Box<dyn FnOnce(&str) -> Result<(), ToastPreShowFailure>>;

impl ToastShowAuthority {
    pub(crate) fn new<T: 'static>(
        owner: T,
        on_pre_show_cancel: impl FnOnce(&str) -> Result<(), ToastPreShowFailure> + 'static,
    ) -> Self {
        Self {
            _owner: Box::new(owner),
            on_pre_show_cancel: Some(Box::new(on_pre_show_cancel)),
        }
    }

    fn reconcile_before_show(&mut self, reason: &str) -> Result<(), ToastPreShowFailure> {
        self.on_pre_show_cancel
            .take()
            .ok_or_else(|| ToastPreShowFailure {
                code: error_codes::NOTIFY_WORKER_FAILED,
                message: "escalation Show authority has no pre-Show reconciler".to_owned(),
            })?(reason)
    }
}

pub(crate) type ToastPreShowAuthorizer = Box<
    dyn FnOnce(&PreparedToastPayload) -> Result<Option<ToastShowAuthority>, ToastPreShowFailure>
        + Send
        + 'static,
>;

/// Failure raised from the toast worker; carries a precise error code.
#[derive(Clone, Debug)]
struct NotifyFailure {
    code: &'static str,
    message: String,
}

impl NotifyFailure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

struct ToastOutcome {
    shown: bool,
    deduped: bool,
    history_count: u32,
    payload_sha256: String,
    expiration_unix_ms: Option<u64>,
    /// Real `ToastNotifier.Setting()` readback: "enabled", or
    /// "unavailable_first_use" when Windows has not yet materialized the
    /// per-app notification record (happens only before the first-ever toast
    /// of an unpackaged app; delivery is still proven via Action Center).
    notification_setting: String,
}

#[derive(Clone, Debug)]
struct HistoryInspection {
    count: u32,
    payload_sha256s: Vec<String>,
    expiration_unix_ms: Vec<Option<u64>>,
}

pub(crate) fn toast_text_char_allowed(character: char) -> bool {
    let scalar = character as u32;
    matches!(character, '\n' | '\r' | '\t')
        || (!character.is_control()
            && ((0x20..=0xD7FF).contains(&scalar)
                || (0xE000..=0xFFFD).contains(&scalar)
                || (0x1_0000..=0x10_FFFF).contains(&scalar)))
}

fn validate_params(params: &NotifyHumanParams) -> Result<(), ErrorData> {
    if params.title.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "notify_human title must not be empty or whitespace-only",
        ));
    }
    let title_chars = params.title.chars().count();
    if title_chars > MAX_TITLE_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("notify_human title is {title_chars} characters; max {MAX_TITLE_CHARS}"),
        ));
    }
    let body_chars = params.body.chars().count();
    if body_chars > MAX_BODY_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("notify_human body is {body_chars} characters; max {MAX_BODY_CHARS}"),
        ));
    }
    for (field, text) in [("title", &params.title), ("body", &params.body)] {
        if let Some(bad) = text.chars().find(|c| !toast_text_char_allowed(*c)) {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "notify_human {field} contains control character U+{:04X}, which the Windows toast XML payload cannot carry",
                    bad as u32
                ),
            ));
        }
    }
    if let Some(dedupe_key) = params.dedupe_key.as_deref() {
        if dedupe_key.trim().is_empty() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "notify_human dedupe_key must not be empty or whitespace-only when provided",
            ));
        }
        let key_chars = dedupe_key.chars().count();
        if key_chars > MAX_DEDUPE_KEY_CHARS {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "notify_human dedupe_key is {key_chars} characters; max {MAX_DEDUPE_KEY_CHARS}"
                ),
            ));
        }
    }
    Ok(())
}

/// Tag is capped at 64 chars by the platform, so dedupe keys are hashed.
#[must_use]
pub fn toast_tag_for(dedupe_key: Option<&str>) -> String {
    match dedupe_key {
        Some(key) => {
            let digest = Sha256::digest(key.as_bytes());
            let mut tag = String::with_capacity(35);
            tag.push_str("dk-");
            for byte in &digest[..16] {
                use std::fmt::Write as _;
                let _ = write!(tag, "{byte:02x}");
            }
            tag
        }
        None => format!("id-{}", uuid::Uuid::new_v4().simple()),
    }
}

fn escape_xml_text(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn toast_xml_with_actions(params: &NotifyHumanParams, actions: &[ToastAction]) -> String {
    let actions_xml = if actions.is_empty() {
        String::new()
    } else {
        let mut xml = String::from("<actions>");
        for action in actions {
            xml.push_str(&format!(
                r#"<action content="{content}" activationType="{activation_type}" arguments="{arguments}"/>"#,
                content = escape_xml_text(&action.content),
                activation_type = action.activation_type.as_xml_value(),
                arguments = escape_xml_text(&action.arguments),
            ));
        }
        xml.push_str("</actions>");
        xml
    };
    format!(
        concat!(
            r#"<toast duration="{duration}">"#,
            "<visual>",
            r#"<binding template="ToastGeneric">"#,
            "<text>{title}</text>",
            "<text>{body}</text>",
            r#"<text placement="attribution">Synapse - {kind}</text>"#,
            "</binding>",
            "</visual>",
            "{actions}",
            "</toast>",
        ),
        duration = params.kind.toast_duration(),
        title = escape_xml_text(&params.title),
        body = escape_xml_text(&params.body),
        kind = params.kind.as_str(),
        actions = actions_xml,
    )
}

fn toast_payload_digest(canonical_xml: &str, suppress_popup: bool) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-toast-payload-v1\0");
    hasher.update([u8::from(suppress_popup)]);
    hasher.update(canonical_xml.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn prepared_toast_payload_valid(payload: &PreparedToastPayload) -> bool {
    payload.schema_version == TOAST_PAYLOAD_SCHEMA_VERSION
        && !payload.canonical_xml.is_empty()
        && payload.canonical_xml.len() <= MAX_FROZEN_TOAST_XML_BYTES
        && toast_payload_digest(&payload.canonical_xml, payload.suppress_popup)
            == payload.payload_sha256
}

fn notify_request_details(params: &NotifyHumanParams, tag: &str) -> Value {
    json!({
        "title": params.title,
        "body": params.body,
        "kind": params.kind.as_str(),
        "dedupe_key": params.dedupe_key,
        "suppress_popup": params.suppress_popup,
        "aumid": SYNAPSE_AUMID,
        "tag": tag,
        "group": SYNAPSE_TOAST_GROUP,
    })
}

fn is_escalation_toast_tag(tag: &str) -> bool {
    tag.strip_prefix("et1-").is_some_and(|hex| {
        hex.len() == 32
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
}

#[cfg(windows)]
mod windows_toast {
    use super::{
        HISTORY_VERIFY_POLL_MS, HISTORY_VERIFY_TIMEOUT_MS, HistoryInspection,
        MAX_FROZEN_TOAST_XML_BYTES, NotifyFailure, NotifyHumanParams, PreparedToastPayload,
        SYNAPSE_AUMID, SYNAPSE_ESCALATION_TOAST_GROUP, SYNAPSE_NOTIFY_DISPLAY_NAME,
        TOAST_PAYLOAD_SCHEMA_VERSION, ToastAction, ToastActivationCallback, ToastCleanupReport,
        ToastOutcome, ToastPreShowAuthorizer, ToastRemovalOutcome, error_codes,
        is_escalation_toast_tag, toast_payload_digest, toast_xml_with_actions,
    };
    use std::{
        collections::{BTreeSet, VecDeque},
        sync::{Arc, Condvar, Mutex, OnceLock},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    use windows::{
        Data::Xml::Dom::XmlDocument,
        Foundation::{DateTime, IReference, PropertyValue, TypedEventHandler},
        UI::Notifications::{
            NotificationSetting, ToastActivatedEventArgs, ToastNotification,
            ToastNotificationManager, ToastNotifier,
        },
        Win32::{
            Foundation::ERROR_SUCCESS,
            System::{
                Com::{COINIT_MULTITHREADED, CoInitializeEx},
                Registry::{
                    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE,
                    REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE, RRF_RT_REG_SZ, RegCloseKey,
                    RegCreateKeyExW, RegGetValueW, RegSetValueExW,
                },
            },
        },
        core::{HSTRING, IInspectable, Interface as _, PCWSTR},
    };

    const AUMID_SUBKEY: &str = "Software\\Classes\\AppUserModelId\\Synapse.Daemon";
    const DISPLAY_NAME_VALUE: &str = "DisplayName";
    const MAX_LIVE_ACTIVATION_SUBSCRIPTIONS: usize = 64;
    /// E_NOT_FOUND / ERROR_NOT_FOUND as an HRESULT (0x80070490): what
    /// `ToastNotifier.Setting()` throws before the app's first-ever toast.
    #[allow(clippy::cast_possible_wrap)]
    const E_NOT_FOUND_HRESULT: windows::core::HRESULT =
        windows::core::HRESULT(0x8007_0490_u32 as i32);
    #[allow(clippy::cast_possible_wrap)]
    const E_POINTER_HRESULT: windows::core::HRESULT =
        windows::core::HRESULT(0x8000_4003_u32 as i32);
    const UNIX_EPOCH_OFFSET_MS: u64 = 11_644_473_600_000;
    const HUNDRED_NS_PER_MS: u64 = 10_000;

    fn wide_null(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    enum NotifyCommand {
        Prepare(PrepareJob),
        Show(NotifyJob),
        Inspect(InspectJob),
        Remove(RemoveJob),
        CleanupEscalationOrphans(CleanupJob),
    }

    #[derive(Clone, Copy)]
    enum NotifyPriority {
        Escalation,
        Control,
        Ordinary,
    }

    const ESCALATION_QUEUE_CAPACITY: usize = 64;
    const CONTROL_QUEUE_CAPACITY: usize = 128;
    const ORDINARY_QUEUE_CAPACITY: usize = 128;

    struct NotifyQueueState {
        escalation: VecDeque<NotifyCommand>,
        control: VecDeque<NotifyCommand>,
        ordinary: VecDeque<NotifyCommand>,
        worker_alive: bool,
    }

    struct NotifyQueue {
        state: Mutex<NotifyQueueState>,
        available: Condvar,
    }

    #[derive(Clone)]
    struct NotifySender {
        queue: Arc<NotifyQueue>,
    }

    impl NotifySender {
        fn send(&self, priority: NotifyPriority, command: NotifyCommand) -> Result<(), String> {
            let mut state =
                self.queue.state.lock().map_err(|poisoned| {
                    format!("notify admission queue is poisoned: {poisoned}")
                })?;
            if !state.worker_alive {
                return Err("synapse-notify worker is not alive".to_owned());
            }
            let (queue, capacity, class) = match priority {
                NotifyPriority::Escalation => (
                    &mut state.escalation,
                    ESCALATION_QUEUE_CAPACITY,
                    "escalation",
                ),
                NotifyPriority::Control => (&mut state.control, CONTROL_QUEUE_CAPACITY, "control"),
                NotifyPriority::Ordinary => {
                    (&mut state.ordinary, ORDINARY_QUEUE_CAPACITY, "ordinary")
                }
            };
            if queue.len() >= capacity {
                return Err(format!(
                    "notify {class} admission queue is full: depth={} capacity={capacity}; job rejected before side effects",
                    queue.len()
                ));
            }
            queue.push_back(command);
            drop(state);
            self.queue.available.notify_one();
            Ok(())
        }
    }

    impl NotifyQueue {
        fn recv(&self) -> Result<NotifyCommand, String> {
            let mut state = self
                .state
                .lock()
                .map_err(|poisoned| format!("notify worker queue is poisoned: {poisoned}"))?;
            loop {
                if let Some(command) = state.escalation.pop_front() {
                    return Ok(command);
                }
                if let Some(command) = state.control.pop_front() {
                    return Ok(command);
                }
                if let Some(command) = state.ordinary.pop_front() {
                    return Ok(command);
                }
                state = self
                    .available
                    .wait(state)
                    .map_err(|poisoned| format!("notify worker wait is poisoned: {poisoned}"))?;
            }
        }
    }

    struct NotifyWorkerLease {
        queue: Arc<NotifyQueue>,
    }

    impl Drop for NotifyWorkerLease {
        fn drop(&mut self) {
            let mut state = match self.queue.state.lock() {
                Ok(state) => state,
                Err(poisoned) => {
                    tracing::error!(
                        code = "NOTIFY_WORKER_QUEUE_POISONED_ON_EXIT",
                        detail = %poisoned,
                        "recovering queue ownership only to drop queued reply senders after notify worker termination"
                    );
                    poisoned.into_inner()
                }
            };
            state.worker_alive = false;
            // Dropping queued commands drops their oneshot senders, so every
            // waiter receives an explicit worker-failed error instead of
            // hanging behind a dead global queue forever.
            state.escalation.clear();
            state.control.clear();
            state.ordinary.clear();
            self.queue.available.notify_all();
        }
    }

    struct PrepareJob {
        params: NotifyHumanParams,
        actions: Vec<ToastAction>,
        reply: tokio::sync::oneshot::Sender<Result<PreparedToastPayload, NotifyFailure>>,
    }

    struct NotifyJob {
        params: NotifyHumanParams,
        tag: String,
        group: String,
        actions: Vec<ToastAction>,
        frozen_payload: Option<PreparedToastPayload>,
        not_after_unix_ms: Option<u64>,
        pre_show_authorizer: Option<ToastPreShowAuthorizer>,
        activation_callback: Option<ToastActivationCallback>,
        reply: tokio::sync::oneshot::Sender<Result<Option<ToastOutcome>, NotifyFailure>>,
    }

    struct RemoveJob {
        tag: String,
        group: String,
        expected_payload_sha256: Option<String>,
        expected_expiration_unix_ms: Option<u64>,
        allow_reserved_orphan: bool,
        reply: tokio::sync::oneshot::Sender<ToastRemovalOutcome>,
    }

    struct InspectJob {
        tag: String,
        group: String,
        reply: tokio::sync::oneshot::Sender<Result<HistoryInspection, NotifyFailure>>,
    }

    struct CleanupJob {
        preserve_tags: Vec<String>,
        reply: tokio::sync::oneshot::Sender<ToastCleanupReport>,
    }

    struct LiveActivationSubscription {
        toast: ToastNotification,
        token: i64,
    }

    /// Single long-lived worker thread that owns COM (MTA) for the daemon's
    /// lifetime and serializes every WinRT notification-platform call.
    ///
    /// Per-call CoInitializeEx/CoUninitialize on pooled threads is NOT safe
    /// here: tearing down the last MTA invalidates windows-rs's process-wide
    /// cached activation factories, and the next toast call then dies with an
    /// access violation that kills the daemon (observed during manual FSV; same reason
    /// synapse-a11y routes UIA through a dedicated COM worker thread).
    static NOTIFY_WORKER: OnceLock<Result<NotifySender, String>> = OnceLock::new();
    static LIVE_ACTIVATION_SUBSCRIPTIONS: OnceLock<Mutex<Vec<LiveActivationSubscription>>> =
        OnceLock::new();

    fn spawn_notify_worker() -> Result<NotifySender, String> {
        let queue = Arc::new(NotifyQueue {
            state: Mutex::new(NotifyQueueState {
                escalation: VecDeque::new(),
                control: VecDeque::new(),
                ordinary: VecDeque::new(),
                worker_alive: true,
            }),
            available: Condvar::new(),
        });
        let worker_queue = Arc::clone(&queue);
        std::thread::Builder::new()
            .name("synapse-notify".to_owned())
            .spawn(move || {
                let _lease = NotifyWorkerLease {
                    queue: Arc::clone(&worker_queue),
                };
                let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                let com_error = com
                    .is_err()
                    .then(|| format!("CoInitializeEx(COINIT_MULTITHREADED) failed: {com:?}"));
                // COM stays initialized until the daemon exits; never
                // CoUninitialize, or cached WinRT factories dangle.
                loop {
                    let command = match worker_queue.recv() {
                        Ok(command) => command,
                        Err(error) => {
                            tracing::error!(
                                code = "NOTIFY_WORKER_QUEUE_FAILED",
                                detail = %error,
                                "synapse-notify worker terminated because its bounded admission queue failed"
                            );
                            break;
                        }
                    };
                    match command {
                        NotifyCommand::Prepare(job) => {
                            let result = match com_error.as_deref() {
                                Some(message) => Err(NotifyFailure::new(
                                    error_codes::NOTIFY_WORKER_FAILED,
                                    format!("notify worker thread has no COM apartment: {message}"),
                                )),
                                None => prepare_toast_payload_blocking(&job.params, &job.actions),
                            };
                            let _ = job.reply.send(result);
                        }
                        NotifyCommand::Show(job) => {
                            let result = match com_error.as_deref() {
                                Some(message) => Err(NotifyFailure::new(
                                    error_codes::NOTIFY_WORKER_FAILED,
                                    format!("notify worker thread has no COM apartment: {message}"),
                                )),
                                None => send_toast_blocking(
                                    &job.params,
                                    &job.tag,
                                    &job.group,
                                    &job.actions,
                                    job.frozen_payload.as_ref(),
                                    job.not_after_unix_ms,
                                    job.pre_show_authorizer,
                                    job.activation_callback,
                                ),
                            };
                            let _ = job.reply.send(result);
                        }
                        NotifyCommand::Inspect(job) => {
                            let result = match com_error.as_deref() {
                                Some(message) => Err(NotifyFailure::new(
                                    error_codes::NOTIFY_WORKER_FAILED,
                                    format!("notify worker thread has no COM apartment: {message}"),
                                )),
                                None => inspect_history_for_tag(&job.tag, &job.group),
                            };
                            let _ = job.reply.send(result);
                        }
                        NotifyCommand::Remove(job) => {
                            let result = match com_error.as_deref() {
                                Some(message) => removal_failure(
                                    &job.tag,
                                    &job.group,
                                    error_codes::NOTIFY_WORKER_FAILED,
                                    format!("notify worker thread has no COM apartment: {message}"),
                                ),
                                None => remove_toast_blocking(
                                    &job.tag,
                                    &job.group,
                                    job.expected_payload_sha256.as_deref(),
                                    job.expected_expiration_unix_ms,
                                    job.allow_reserved_orphan,
                                ),
                            };
                            let _ = job.reply.send(result);
                        }
                        NotifyCommand::CleanupEscalationOrphans(job) => {
                            let result = match com_error.as_deref() {
                                Some(message) => cleanup_failure(
                                    error_codes::NOTIFY_WORKER_FAILED,
                                    format!("notify worker thread has no COM apartment: {message}"),
                                ),
                                None => cleanup_escalation_orphans_blocking(&job.preserve_tags),
                            };
                            let _ = job.reply.send(result);
                        }
                    }
                }
            })
            .map(|_handle| NotifySender { queue })
            .map_err(|error| format!("failed to spawn synapse-notify worker thread: {error}"))
    }

    pub(super) async fn send_toast(
        params: NotifyHumanParams,
        tag: String,
        group: String,
        actions: Vec<ToastAction>,
        activation_callback: Option<ToastActivationCallback>,
    ) -> Result<ToastOutcome, NotifyFailure> {
        let sender = NOTIFY_WORKER
            .get_or_init(spawn_notify_worker)
            .as_ref()
            .map_err(|message| {
                NotifyFailure::new(error_codes::NOTIFY_WORKER_FAILED, message.clone())
            })?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        sender
            .send(
                NotifyPriority::Ordinary,
                NotifyCommand::Show(NotifyJob {
                    params,
                    tag,
                    group,
                    actions,
                    frozen_payload: None,
                    not_after_unix_ms: None,
                    pre_show_authorizer: None,
                    activation_callback,
                    reply: reply_tx,
                }),
            )
            .map_err(|send_error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_WORKER_FAILED,
                    format!("toast job was not admitted: {send_error}"),
                )
            })?;
        reply_rx
            .await
            .map_err(|_recv_error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_WORKER_FAILED,
                    "synapse-notify worker dropped the toast job without replying (worker panic?)",
                )
            })??
            .ok_or_else(|| {
                NotifyFailure::new(
                    error_codes::NOTIFY_WORKER_FAILED,
                    "ordinary toast job was skipped without an escalation pre-Show authorizer",
                )
            })
    }

    pub(super) fn send_escalation_toast_synchronously(
        params: NotifyHumanParams,
        tag: String,
        group: String,
        actions: Vec<ToastAction>,
        frozen_payload: PreparedToastPayload,
        not_after_unix_ms: u64,
        pre_show_authorizer: ToastPreShowAuthorizer,
    ) -> Result<Option<ToastOutcome>, NotifyFailure> {
        let sender = NOTIFY_WORKER
            .get_or_init(spawn_notify_worker)
            .as_ref()
            .map_err(|message| {
                NotifyFailure::new(error_codes::NOTIFY_WORKER_FAILED, message.clone())
            })?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        sender
            .send(
                NotifyPriority::Escalation,
                NotifyCommand::Show(NotifyJob {
                    params,
                    tag,
                    group,
                    actions,
                    frozen_payload: Some(frozen_payload),
                    not_after_unix_ms: Some(not_after_unix_ms),
                    pre_show_authorizer: Some(pre_show_authorizer),
                    activation_callback: None,
                    reply: reply_tx,
                }),
            )
            .map_err(|send_error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_WORKER_FAILED,
                    format!("synchronous escalation toast job was not admitted: {send_error}"),
                )
            })?;
        reply_rx.blocking_recv().map_err(|_recv_error| {
            NotifyFailure::new(
                error_codes::NOTIFY_WORKER_FAILED,
                "synapse-notify worker dropped the synchronous escalation toast job without replying",
            )
        })?
    }

    pub(super) async fn prepare_toast_payload(
        params: NotifyHumanParams,
        actions: Vec<ToastAction>,
    ) -> Result<PreparedToastPayload, NotifyFailure> {
        let sender = NOTIFY_WORKER
            .get_or_init(spawn_notify_worker)
            .as_ref()
            .map_err(|message| {
                NotifyFailure::new(error_codes::NOTIFY_WORKER_FAILED, message.clone())
            })?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        sender
            .send(
                NotifyPriority::Escalation,
                NotifyCommand::Prepare(PrepareJob {
                    params,
                    actions,
                    reply: reply_tx,
                }),
            )
            .map_err(|send_error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_WORKER_FAILED,
                    format!("toast preparation job was not admitted: {send_error}"),
                )
            })?;
        reply_rx.await.map_err(|_recv_error| {
            NotifyFailure::new(
                error_codes::NOTIFY_WORKER_FAILED,
                "synapse-notify worker dropped the toast preparation job without replying",
            )
        })?
    }

    pub(super) async fn remove_toast(
        tag: String,
        group: String,
        expected_payload_sha256: String,
        expected_expiration_unix_ms: Option<u64>,
    ) -> ToastRemovalOutcome {
        let sender = match NOTIFY_WORKER.get_or_init(spawn_notify_worker).as_ref() {
            Ok(sender) => sender,
            Err(message) => {
                return removal_failure(
                    &tag,
                    &group,
                    error_codes::NOTIFY_WORKER_FAILED,
                    message.clone(),
                );
            }
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if let Err(send_error) = sender.send(
            NotifyPriority::Control,
            NotifyCommand::Remove(RemoveJob {
                tag: tag.clone(),
                group: group.clone(),
                expected_payload_sha256: Some(expected_payload_sha256),
                expected_expiration_unix_ms,
                allow_reserved_orphan: false,
                reply: reply_tx,
            }),
        ) {
            return removal_failure(
                &tag,
                &group,
                error_codes::NOTIFY_WORKER_FAILED,
                format!("toast removal job was not admitted: {send_error}"),
            );
        }
        reply_rx.await.unwrap_or_else(|_recv_error| {
            removal_failure(
                &tag,
                &group,
                error_codes::NOTIFY_WORKER_FAILED,
                "synapse-notify worker dropped the toast removal job without replying",
            )
        })
    }

    pub(super) async fn inspect_toast(
        tag: String,
        group: String,
    ) -> Result<HistoryInspection, NotifyFailure> {
        let sender = NOTIFY_WORKER
            .get_or_init(spawn_notify_worker)
            .as_ref()
            .map_err(|message| {
                NotifyFailure::new(error_codes::NOTIFY_WORKER_FAILED, message.clone())
            })?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        sender
            .send(
                NotifyPriority::Control,
                NotifyCommand::Inspect(InspectJob {
                    tag,
                    group,
                    reply: reply_tx,
                }),
            )
            .map_err(|send_error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_WORKER_FAILED,
                    format!("toast inspection job was not admitted: {send_error}"),
                )
            })?;
        reply_rx.await.map_err(|_recv_error| {
            NotifyFailure::new(
                error_codes::NOTIFY_WORKER_FAILED,
                "synapse-notify worker dropped the toast inspection job without replying",
            )
        })?
    }

    pub(super) async fn cleanup_escalation_orphans(
        preserve_tags: Vec<String>,
    ) -> ToastCleanupReport {
        let sender = match NOTIFY_WORKER.get_or_init(spawn_notify_worker).as_ref() {
            Ok(sender) => sender,
            Err(message) => {
                return cleanup_failure(error_codes::NOTIFY_WORKER_FAILED, message.clone());
            }
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if let Err(send_error) = sender.send(
            NotifyPriority::Ordinary,
            NotifyCommand::CleanupEscalationOrphans(CleanupJob {
                preserve_tags,
                reply: reply_tx,
            }),
        ) {
            return cleanup_failure(
                error_codes::NOTIFY_WORKER_FAILED,
                format!("toast cleanup job was not admitted: {send_error}"),
            );
        }
        reply_rx.await.unwrap_or_else(|_recv_error| {
            cleanup_failure(
                error_codes::NOTIFY_WORKER_FAILED,
                "synapse-notify worker dropped the toast cleanup job without replying",
            )
        })
    }

    /// Idempotently registers the Synapse AUMID for toast display and proves
    /// it with a registry readback. Without this key Windows drops toasts
    /// silently, which is exactly the failure mode this tool must never have.
    pub(super) fn ensure_aumid_registered() -> Result<(), NotifyFailure> {
        let subkey_wide = wide_null(AUMID_SUBKEY);
        let value_wide = wide_null(DISPLAY_NAME_VALUE);
        let mut key = HKEY::default();
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey_wide.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE | KEY_QUERY_VALUE,
                None,
                &raw mut key,
                None,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(NotifyFailure::new(
                error_codes::NOTIFY_AUMID_REGISTRATION_FAILED,
                format!(
                    "RegCreateKeyExW(HKCU\\{AUMID_SUBKEY}) failed with status {}",
                    status.0
                ),
            ));
        }
        let display_wide = wide_null(SYNAPSE_NOTIFY_DISPLAY_NAME);
        let display_bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(display_wide.as_ptr().cast::<u8>(), display_wide.len() * 2)
        };
        let status = unsafe {
            RegSetValueExW(
                key,
                PCWSTR(value_wide.as_ptr()),
                None,
                REG_SZ,
                Some(display_bytes),
            )
        };
        let close_status = unsafe { RegCloseKey(key) };
        if status != ERROR_SUCCESS {
            return Err(NotifyFailure::new(
                error_codes::NOTIFY_AUMID_REGISTRATION_FAILED,
                format!(
                    "RegSetValueExW(HKCU\\{AUMID_SUBKEY}\\{DISPLAY_NAME_VALUE}) failed with status {}",
                    status.0
                ),
            ));
        }
        if close_status != ERROR_SUCCESS {
            tracing::warn!(
                code = "NOTIFY_REGISTRY_CLOSE_FAILED",
                status = close_status.0,
                "RegCloseKey after AUMID registration failed"
            );
        }

        // Readback: the registration only counts if the value is physically
        // in the registry with the expected content.
        let readback = read_aumid_display_name().ok_or_else(|| {
            NotifyFailure::new(
                error_codes::NOTIFY_AUMID_REGISTRATION_FAILED,
                format!(
                    "AUMID DisplayName readback found nothing at HKCU\\{AUMID_SUBKEY} immediately after write"
                ),
            )
        })?;
        if readback != SYNAPSE_NOTIFY_DISPLAY_NAME {
            return Err(NotifyFailure::new(
                error_codes::NOTIFY_AUMID_REGISTRATION_FAILED,
                format!(
                    "AUMID DisplayName readback mismatch: expected {SYNAPSE_NOTIFY_DISPLAY_NAME:?}, found {readback:?}"
                ),
            ));
        }
        Ok(())
    }

    pub(super) fn read_aumid_display_name() -> Option<String> {
        let subkey_wide = wide_null(AUMID_SUBKEY);
        let value_wide = wide_null(DISPLAY_NAME_VALUE);
        let mut value_type = REG_VALUE_TYPE::default();
        let mut byte_len = 0_u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey_wide.as_ptr()),
                PCWSTR(value_wide.as_ptr()),
                RRF_RT_REG_SZ,
                Some(&raw mut value_type),
                None,
                Some(&raw mut byte_len),
            )
        };
        if status != ERROR_SUCCESS || byte_len == 0 {
            return None;
        }
        let mut buffer = vec![0_u16; (byte_len as usize).div_ceil(2)];
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey_wide.as_ptr()),
                PCWSTR(value_wide.as_ptr()),
                RRF_RT_REG_SZ,
                Some(&raw mut value_type),
                Some(buffer.as_mut_ptr().cast()),
                Some(&raw mut byte_len),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let units = (byte_len as usize).div_ceil(2).min(buffer.len());
        buffer.truncate(units);
        let nul = buffer
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..nul]))
    }

    fn map_setting_error(setting: NotificationSetting) -> Option<NotifyFailure> {
        if setting == NotificationSetting::Enabled {
            return None;
        }
        let (code, reason) = if setting == NotificationSetting::DisabledForApplication {
            (
                error_codes::NOTIFY_DISABLED_FOR_APPLICATION,
                "notifications for the Synapse app are turned off in Windows Settings > System > Notifications",
            )
        } else if setting == NotificationSetting::DisabledForUser {
            (
                error_codes::NOTIFY_DISABLED_FOR_USER,
                "notifications are turned off for this user in Windows Settings > System > Notifications",
            )
        } else if setting == NotificationSetting::DisabledByGroupPolicy {
            (
                error_codes::NOTIFY_DISABLED_BY_GROUP_POLICY,
                "notifications are disabled by group policy",
            )
        } else if setting == NotificationSetting::DisabledByManifest {
            (
                error_codes::NOTIFY_DISABLED_BY_MANIFEST,
                "notifications are disabled by app manifest",
            )
        } else {
            (
                error_codes::NOTIFY_SHOW_FAILED,
                "ToastNotifier reported an unknown NotificationSetting",
            )
        };
        Some(NotifyFailure::new(
            code,
            format!("{reason} (NotificationSetting={})", setting.0),
        ))
    }

    fn inspect_history_for_tag(tag: &str, group: &str) -> Result<HistoryInspection, NotifyFailure> {
        let history = ToastNotificationManager::History().map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!("ToastNotificationManager.History() failed: {error}"),
            )
        })?;
        let toasts = history
            .GetHistoryWithId(&HSTRING::from(SYNAPSE_AUMID))
            .map_err(|error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_SHOW_FAILED,
                    format!(
                        "ToastNotificationHistory.GetHistoryWithId({SYNAPSE_AUMID}) failed: {error}"
                    ),
                )
            })?;
        let size = toasts.Size().map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!("Action Center history Size() failed: {error}"),
            )
        })?;
        let mut payload_sha256s = Vec::new();
        let mut expiration_unix_ms = Vec::new();
        for index in 0..size {
            let toast = toasts.GetAt(index).map_err(|error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_SHOW_FAILED,
                    format!("Action Center history GetAt({index}) failed: {error}"),
                )
            })?;
            let toast_tag = toast
                .Tag()
                .map(|value| value.to_string_lossy())
                .map_err(|error| {
                    NotifyFailure::new(
                        error_codes::NOTIFY_SHOW_FAILED,
                        format!("Action Center history Tag() failed at index {index}: {error}"),
                    )
                })?;
            let toast_group =
                toast
                    .Group()
                    .map(|value| value.to_string_lossy())
                    .map_err(|error| {
                        NotifyFailure::new(
                            error_codes::NOTIFY_SHOW_FAILED,
                            format!(
                                "Action Center history Group() failed at index {index}: {error}"
                            ),
                        )
                    })?;
            if toast_tag == tag && toast_group == group {
                let payload = toast
                    .Content()
                    .and_then(|document| document.GetXml())
                    .map(|xml| xml.to_string_lossy())
                    .map_err(|error| {
                        NotifyFailure::new(
                            error_codes::NOTIFY_SHOW_FAILED,
                            format!(
                                "Action Center history Content().GetXml() failed for tag={tag} group={group} at index {index}: {error}"
                            ),
                        )
                    })?;
                let suppress_popup = toast.SuppressPopup().map_err(|error| {
                    NotifyFailure::new(
                        error_codes::NOTIFY_SHOW_FAILED,
                        format!(
                            "Action Center history SuppressPopup() failed for tag={tag} group={group} at index {index}: {error}"
                        ),
                    )
                })?;
                payload_sha256s.push(toast_payload_digest(&payload, suppress_popup));
                expiration_unix_ms.push(read_expiration_unix_ms(&toast, tag, group, index)?);
            }
        }
        let count = u32::try_from(payload_sha256s.len()).map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                format!(
                    "Action Center matching history count does not fit u32 for tag={tag} group={group}: {error}"
                ),
            )
        })?;
        Ok(HistoryInspection {
            count,
            payload_sha256s,
            expiration_unix_ms,
        })
    }

    fn removal_failure(
        tag: &str,
        group: &str,
        code: &'static str,
        message: impl Into<String>,
    ) -> ToastRemovalOutcome {
        ToastRemovalOutcome {
            aumid: SYNAPSE_AUMID.to_owned(),
            tag: tag.to_owned(),
            group: group.to_owned(),
            status: "error".to_owned(),
            removed: false,
            already_absent: false,
            before_count: None,
            after_count: None,
            error_code: Some(code.to_owned()),
            error_message: Some(message.into()),
        }
    }

    fn cleanup_failure(code: &'static str, message: impl Into<String>) -> ToastCleanupReport {
        ToastCleanupReport {
            aumid: SYNAPSE_AUMID.to_owned(),
            group: SYNAPSE_ESCALATION_TOAST_GROUP.to_owned(),
            status: "error".to_owned(),
            scanned: 0,
            candidates: 0,
            preserved_open: 0,
            removed: 0,
            already_absent: 0,
            failed: 0,
            outcomes: Vec::new(),
            error_code: Some(code.to_owned()),
            error_message: Some(message.into()),
        }
    }

    fn cleanup_escalation_orphans_blocking(preserve_tags: &[String]) -> ToastCleanupReport {
        if let Err(error) = ensure_aumid_registered() {
            return cleanup_failure(error.code, error.message);
        }
        let history = match ToastNotificationManager::History().map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!("ToastNotificationManager.History() failed: {error}"),
            )
        }) {
            Ok(history) => history,
            Err(error) => return cleanup_failure(error.code, error.message),
        };
        let toasts = match history.GetHistoryWithId(&HSTRING::from(SYNAPSE_AUMID)) {
            Ok(toasts) => toasts,
            Err(error) => {
                return cleanup_failure(
                    error_codes::NOTIFY_SHOW_FAILED,
                    format!(
                        "ToastNotificationHistory.GetHistoryWithId({SYNAPSE_AUMID}) failed: {error}"
                    ),
                );
            }
        };
        let size = match toasts.Size() {
            Ok(size) => size,
            Err(error) => {
                return cleanup_failure(
                    error_codes::NOTIFY_SHOW_FAILED,
                    format!("Action Center history Size() failed: {error}"),
                );
            }
        };

        let preserve = preserve_tags.iter().cloned().collect::<BTreeSet<_>>();
        if let Some(invalid) = preserve.iter().find(|tag| !is_escalation_toast_tag(tag)) {
            return cleanup_failure(
                error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                format!(
                    "refusing escalation cleanup because durable preserve tag {invalid:?} is outside the reserved et1 namespace"
                ),
            );
        }

        // Finish a complete, fail-closed read of Action Center before the
        // first mutation. A partial scan can never authorize deletion.
        let mut remove_tags = BTreeSet::new();
        let mut observed_tags = BTreeSet::new();
        let mut preserved_open = 0_u32;
        let mut candidates = 0_u32;
        for index in 0..size {
            let toast = match toasts.GetAt(index) {
                Ok(toast) => toast,
                Err(error) => {
                    return cleanup_failure(
                        error_codes::NOTIFY_SHOW_FAILED,
                        format!("Action Center history GetAt({index}) failed: {error}"),
                    );
                }
            };
            let toast_group = toast
                .Group()
                .map(|group| group.to_string_lossy())
                .map_err(|error| {
                    NotifyFailure::new(
                        error_codes::NOTIFY_SHOW_FAILED,
                        format!("Action Center history Group() failed at index {index}: {error}"),
                    )
                });
            let toast_group = match toast_group {
                Ok(group) => group,
                Err(error) => return cleanup_failure(error.code, error.message),
            };
            if toast_group != SYNAPSE_ESCALATION_TOAST_GROUP {
                continue;
            }
            let tag = toast
                .Tag()
                .map(|tag| tag.to_string_lossy())
                .map_err(|error| {
                    NotifyFailure::new(
                        error_codes::NOTIFY_SHOW_FAILED,
                        format!("Action Center history Tag() failed at index {index}: {error}"),
                    )
                });
            let tag = match tag {
                Ok(tag) => tag,
                Err(error) => return cleanup_failure(error.code, error.message),
            };
            if !is_escalation_toast_tag(&tag) {
                return cleanup_failure(
                    error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                    format!(
                        "reserved escalation group contains invalid tag {tag:?} at Action Center index {index}; refusing all cleanup"
                    ),
                );
            }
            if !observed_tags.insert(tag.clone()) {
                return cleanup_failure(
                    error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                    format!(
                        "reserved escalation group contains duplicate tag {tag:?}; refusing ambiguous cleanup"
                    ),
                );
            }
            candidates += 1;
            if preserve.contains(&tag) {
                preserved_open += 1;
                continue;
            }
            remove_tags.insert(tag);
        }

        let mut report = ToastCleanupReport {
            aumid: SYNAPSE_AUMID.to_owned(),
            group: SYNAPSE_ESCALATION_TOAST_GROUP.to_owned(),
            status: "ok".to_owned(),
            scanned: size,
            candidates,
            preserved_open,
            removed: 0,
            already_absent: 0,
            failed: 0,
            outcomes: Vec::new(),
            error_code: None,
            error_message: None,
        };
        for tag in remove_tags {
            let outcome =
                remove_toast_blocking(&tag, SYNAPSE_ESCALATION_TOAST_GROUP, None, None, true);
            if outcome.removed {
                report.removed += 1;
            } else if outcome.already_absent {
                report.already_absent += 1;
            } else {
                report.failed += 1;
            }
            report.outcomes.push(outcome);
        }
        if report.failed > 0 {
            report.status = "partial_error".to_owned();
        }
        report
    }

    fn removal_precondition_failure(
        tag: &str,
        group: &str,
        before_count: u32,
        message: impl Into<String>,
    ) -> ToastRemovalOutcome {
        ToastRemovalOutcome {
            aumid: SYNAPSE_AUMID.to_owned(),
            tag: tag.to_owned(),
            group: group.to_owned(),
            status: "precondition_failed".to_owned(),
            removed: false,
            already_absent: false,
            before_count: Some(before_count),
            after_count: Some(before_count),
            error_code: Some(error_codes::NOTIFY_DELIVERY_UNVERIFIED.to_owned()),
            error_message: Some(message.into()),
        }
    }

    fn remove_toast_blocking(
        tag: &str,
        group: &str,
        expected_payload_sha256: Option<&str>,
        expected_expiration_unix_ms: Option<u64>,
        allow_reserved_orphan: bool,
    ) -> ToastRemovalOutcome {
        if let Err(error) = ensure_aumid_registered() {
            return removal_failure(tag, group, error.code, error.message);
        }
        let before = match inspect_history_for_tag(tag, group) {
            Ok(inspection) => inspection,
            Err(error) => return removal_failure(tag, group, error.code, error.message),
        };
        if before.count == 0 {
            return ToastRemovalOutcome {
                aumid: SYNAPSE_AUMID.to_owned(),
                tag: tag.to_owned(),
                group: group.to_owned(),
                status: "not_present".to_owned(),
                removed: false,
                already_absent: true,
                before_count: Some(0),
                after_count: Some(0),
                error_code: None,
                error_message: None,
            };
        }

        if before.count != 1
            || before.payload_sha256s.len() != 1
            || before.expiration_unix_ms.len() != 1
        {
            return removal_precondition_failure(
                tag,
                group,
                before.count,
                format!(
                    "refusing ambiguous toast removal: expected exactly one Action Center row for tag={tag} group={group}; count={} payload_rows={} expiration_rows={}",
                    before.count,
                    before.payload_sha256s.len(),
                    before.expiration_unix_ms.len()
                ),
            );
        }
        match expected_payload_sha256 {
            Some(expected) if before.payload_sha256s[0] != expected => {
                return removal_precondition_failure(
                    tag,
                    group,
                    before.count,
                    format!(
                        "refusing toast removal because the physical payload is not the durable escalation payload: tag={tag} group={group} expected_sha256={expected} actual_sha256={}",
                        before.payload_sha256s[0]
                    ),
                );
            }
            Some(_) if before.expiration_unix_ms[0] != expected_expiration_unix_ms => {
                return removal_precondition_failure(
                    tag,
                    group,
                    before.count,
                    format!(
                        "refusing toast removal because the physical expiration is not the durable escalation expiration: tag={tag} group={group} expected_expiration_unix_ms={expected_expiration_unix_ms:?} actual_expiration_unix_ms={:?}",
                        before.expiration_unix_ms[0]
                    ),
                );
            }
            Some(_) => {}
            None if allow_reserved_orphan
                && group == SYNAPSE_ESCALATION_TOAST_GROUP
                && is_escalation_toast_tag(tag) => {}
            None => {
                return removal_precondition_failure(
                    tag,
                    group,
                    before.count,
                    format!(
                        "refusing unbound toast removal for tag={tag} group={group}; an exact durable payload digest is required"
                    ),
                );
            }
        }

        let remove_result = ToastNotificationManager::History()
            .and_then(|history| {
                history.RemoveGroupedTagWithId(
                    &HSTRING::from(tag),
                    &HSTRING::from(group),
                    &HSTRING::from(SYNAPSE_AUMID),
                )
            })
            .map_err(|error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_SHOW_FAILED,
                    format!(
                        "ToastNotificationHistory.RemoveGroupedTagWithId(tag={tag}, group={group}, app_id={SYNAPSE_AUMID}) failed: {error}"
                    ),
                )
            });
        if let Err(error) = remove_result {
            return removal_failure(tag, group, error.code, error.message);
        }

        let after_count = match inspect_history_for_tag(tag, group) {
            Ok(inspection) => inspection.count,
            Err(error) => return removal_failure(tag, group, error.code, error.message),
        };
        ToastRemovalOutcome {
            aumid: SYNAPSE_AUMID.to_owned(),
            tag: tag.to_owned(),
            group: group.to_owned(),
            status: if after_count == 0 {
                "removed".to_owned()
            } else {
                "still_present".to_owned()
            },
            removed: after_count == 0,
            already_absent: false,
            before_count: Some(before.count),
            after_count: Some(after_count),
            error_code: (after_count != 0)
                .then_some(error_codes::NOTIFY_DELIVERY_UNVERIFIED.to_owned()),
            error_message: (after_count != 0).then(|| {
                format!("toast tag {tag} group {group} remained in Action Center after removal")
            }),
        }
    }

    fn create_notifier() -> Result<ToastNotifier, NotifyFailure> {
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(SYNAPSE_AUMID)).map_err(
            |error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_SHOW_FAILED,
                    format!("CreateToastNotifierWithId({SYNAPSE_AUMID}) failed: {error}"),
                )
            },
        )
    }

    fn prepare_toast_document(
        params: &NotifyHumanParams,
        actions: &[ToastAction],
    ) -> Result<(XmlDocument, PreparedToastPayload), NotifyFailure> {
        let xml = toast_xml_with_actions(params, actions);
        let document = XmlDocument::new().map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_XML_PAYLOAD_INVALID,
                format!("XmlDocument creation failed: {error}"),
            )
        })?;
        document
            .LoadXml(&HSTRING::from(xml.as_str()))
            .map_err(|error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_XML_PAYLOAD_INVALID,
                    format!(
                        "toast XML payload rejected by XmlDocument.LoadXml: {error}; source_xml_bytes={}",
                        xml.len()
                    ),
                )
            })?;
        let canonical_xml = document.GetXml().map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_XML_PAYLOAD_INVALID,
                format!("XmlDocument.GetXml canonical readback failed: {error}"),
            )
        })?;
        let prepared = PreparedToastPayload {
            schema_version: TOAST_PAYLOAD_SCHEMA_VERSION,
            canonical_xml: canonical_xml.to_string_lossy(),
            suppress_popup: params.suppress_popup,
            payload_sha256: toast_payload_digest(
                &canonical_xml.to_string_lossy(),
                params.suppress_popup,
            ),
        };
        Ok((document, prepared))
    }

    fn prepare_toast_payload_blocking(
        params: &NotifyHumanParams,
        actions: &[ToastAction],
    ) -> Result<PreparedToastPayload, NotifyFailure> {
        prepare_toast_document(params, actions).map(|(_document, prepared)| prepared)
    }

    fn prepare_frozen_toast_document(
        params: &NotifyHumanParams,
        frozen: &PreparedToastPayload,
    ) -> Result<(XmlDocument, PreparedToastPayload), NotifyFailure> {
        if frozen.schema_version != TOAST_PAYLOAD_SCHEMA_VERSION
            || frozen.canonical_xml.is_empty()
            || frozen.canonical_xml.len() > MAX_FROZEN_TOAST_XML_BYTES
            || frozen.suppress_popup != params.suppress_popup
        {
            return Err(NotifyFailure::new(
                error_codes::NOTIFY_XML_PAYLOAD_INVALID,
                format!(
                    "frozen toast payload metadata is invalid: schema_version={} expected_schema_version={TOAST_PAYLOAD_SCHEMA_VERSION} xml_bytes={} max_xml_bytes={MAX_FROZEN_TOAST_XML_BYTES} frozen_suppress_popup={} params_suppress_popup={}",
                    frozen.schema_version,
                    frozen.canonical_xml.len(),
                    frozen.suppress_popup,
                    params.suppress_popup
                ),
            ));
        }
        let expected_digest = toast_payload_digest(&frozen.canonical_xml, frozen.suppress_popup);
        if expected_digest != frozen.payload_sha256 {
            return Err(NotifyFailure::new(
                error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                format!(
                    "frozen toast payload digest is corrupt: expected_sha256={expected_digest} durable_sha256={}",
                    frozen.payload_sha256
                ),
            ));
        }
        let document = XmlDocument::new().map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_XML_PAYLOAD_INVALID,
                format!("XmlDocument creation for frozen toast failed: {error}"),
            )
        })?;
        document
            .LoadXml(&HSTRING::from(frozen.canonical_xml.as_str()))
            .map_err(|error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_XML_PAYLOAD_INVALID,
                    format!(
                        "frozen toast XML rejected by XmlDocument.LoadXml: {error}; xml_bytes={}",
                        frozen.canonical_xml.len()
                    ),
                )
            })?;
        let canonical_readback = document.GetXml().map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_XML_PAYLOAD_INVALID,
                format!("frozen toast XmlDocument.GetXml readback failed: {error}"),
            )
        })?;
        if canonical_readback.to_string_lossy() != frozen.canonical_xml {
            return Err(NotifyFailure::new(
                error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                format!(
                    "WinRT canonicalization changed a durably frozen toast payload; refusing template substitution or replay: durable_sha256={}",
                    frozen.payload_sha256
                ),
            ));
        }
        Ok((document, frozen.clone()))
    }

    fn set_and_verify_expiration(
        toast: &ToastNotification,
        not_after_unix_ms: u64,
    ) -> Result<(), NotifyFailure> {
        let universal_time = not_after_unix_ms
            .checked_add(UNIX_EPOCH_OFFSET_MS)
            .and_then(|milliseconds| milliseconds.checked_mul(HUNDRED_NS_PER_MS))
            .and_then(|ticks| i64::try_from(ticks).ok())
            .ok_or_else(|| {
                NotifyFailure::new(
                    error_codes::NOTIFY_SHOW_FAILED,
                    format!(
                        "escalation deadline cannot be represented as WinRT DateTime: not_after_unix_ms={not_after_unix_ms}"
                    ),
                )
            })?;
        let boxed = PropertyValue::CreateDateTime(DateTime {
            UniversalTime: universal_time,
        })
        .and_then(|value| value.cast::<IReference<DateTime>>())
        .map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!(
                    "WinRT expiration DateTime boxing failed for not_after_unix_ms={not_after_unix_ms}: {error}"
                ),
            )
        })?;
        toast.SetExpirationTime(&boxed).map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!(
                    "ToastNotification.SetExpirationTime failed for not_after_unix_ms={not_after_unix_ms}: {error}"
                ),
            )
        })?;
        let readback = toast
            .ExpirationTime()
            .and_then(|value| value.Value())
            .map_err(|error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_SHOW_FAILED,
                    format!(
                        "ToastNotification.ExpirationTime readback failed for not_after_unix_ms={not_after_unix_ms}: {error}"
                    ),
                )
            })?;
        if readback.UniversalTime != universal_time {
            return Err(NotifyFailure::new(
                error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                format!(
                    "ToastNotification expiration readback differed: not_after_unix_ms={not_after_unix_ms} expected_universal_time={universal_time} actual_universal_time={}",
                    readback.UniversalTime
                ),
            ));
        }
        Ok(())
    }

    fn read_expiration_unix_ms(
        toast: &ToastNotification,
        tag: &str,
        group: &str,
        index: u32,
    ) -> Result<Option<u64>, NotifyFailure> {
        let reference = match toast.ExpirationTime() {
            Ok(reference) => reference,
            Err(error) if error.code() == E_POINTER_HRESULT => return Ok(None),
            Err(error) => {
                return Err(NotifyFailure::new(
                    error_codes::NOTIFY_SHOW_FAILED,
                    format!(
                        "Action Center history ExpirationTime() failed for tag={tag} group={group} at index {index}: {error}"
                    ),
                ));
            }
        };
        let value = reference.Value().map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!(
                    "Action Center history expiration Value() failed for tag={tag} group={group} at index {index}: {error}"
                ),
            )
        })?;
        let universal_time = u64::try_from(value.UniversalTime).map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                format!(
                    "Action Center history expiration is negative for tag={tag} group={group} at index {index}: universal_time={} error={error}",
                    value.UniversalTime
                ),
            )
        })?;
        let ticks_since_unix = universal_time
            .checked_sub(UNIX_EPOCH_OFFSET_MS.saturating_mul(HUNDRED_NS_PER_MS))
            .ok_or_else(|| {
                NotifyFailure::new(
                    error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                    format!(
                        "Action Center history expiration predates Unix epoch for tag={tag} group={group} at index {index}: universal_time={universal_time}"
                    ),
                )
            })?;
        if ticks_since_unix % HUNDRED_NS_PER_MS != 0 {
            return Err(NotifyFailure::new(
                error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                format!(
                    "Action Center history expiration is not millisecond-aligned for tag={tag} group={group} at index {index}: universal_time={universal_time}"
                ),
            ));
        }
        Ok(Some(ticks_since_unix / HUNDRED_NS_PER_MS))
    }

    fn verify_unique_payload(
        inspection: &HistoryInspection,
        expected_payload_sha256: &str,
        expected_expiration_unix_ms: Option<u64>,
        tag: &str,
        group: &str,
        operation: &str,
    ) -> Result<(), NotifyFailure> {
        if inspection.count != 1
            || inspection.payload_sha256s.len() != 1
            || inspection.expiration_unix_ms.len() != 1
        {
            return Err(NotifyFailure::new(
                error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                format!(
                    "{operation} requires exactly one Action Center row for tag={tag} group={group}; found count={} payload_rows={} expiration_rows={}",
                    inspection.count,
                    inspection.payload_sha256s.len(),
                    inspection.expiration_unix_ms.len()
                ),
            ));
        }
        if inspection.payload_sha256s[0] != expected_payload_sha256 {
            return Err(NotifyFailure::new(
                error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                format!(
                    "{operation} found a payload mismatch for tag={tag} group={group}; expected_sha256={expected_payload_sha256}, actual_sha256={}",
                    inspection.payload_sha256s[0]
                ),
            ));
        }
        if inspection.expiration_unix_ms[0] != expected_expiration_unix_ms {
            return Err(NotifyFailure::new(
                error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                format!(
                    "{operation} found an expiration mismatch for tag={tag} group={group}; expected_expiration_unix_ms={expected_expiration_unix_ms:?} actual_expiration_unix_ms={:?}",
                    inspection.expiration_unix_ms[0]
                ),
            ));
        }
        Ok(())
    }

    /// Runs on the dedicated `synapse-notify` COM worker thread only.
    fn send_toast_blocking(
        params: &NotifyHumanParams,
        tag: &str,
        group: &str,
        actions: &[ToastAction],
        frozen_payload: Option<&PreparedToastPayload>,
        not_after_unix_ms: Option<u64>,
        pre_show_authorizer: Option<ToastPreShowAuthorizer>,
        activation_callback: Option<ToastActivationCallback>,
    ) -> Result<Option<ToastOutcome>, NotifyFailure> {
        ensure_aumid_registered()?;
        let notifier = create_notifier()?;
        // Windows only materializes the per-app notification record when an
        // unpackaged app shows its first toast; until then Setting() throws
        // E_NOT_FOUND (0x80070490) — see CommunityToolkit#3626. That exact
        // failure is not a delivery error (delivery is proven below via the
        // Action Center readback); every other state is mapped precisely.
        let notification_setting = match notifier.Setting() {
            Ok(setting) => {
                if let Some(failure) = map_setting_error(setting) {
                    return Err(failure);
                }
                "enabled".to_owned()
            }
            Err(error) if error.code() == E_NOT_FOUND_HRESULT => {
                tracing::info!(
                    code = "NOTIFY_SETTING_RECORD_MISSING",
                    aumid = SYNAPSE_AUMID,
                    "ToastNotifier.Setting() has no per-app record yet (first toast for this AUMID); relying on Action Center delivery verification"
                );
                "unavailable_first_use".to_owned()
            }
            Err(error) => {
                return Err(NotifyFailure::new(
                    error_codes::NOTIFY_SHOW_FAILED,
                    format!("ToastNotifier.Setting() failed: {error}"),
                ));
            }
        };

        let (document, prepared) = match frozen_payload {
            Some(frozen) => prepare_frozen_toast_document(params, frozen)?,
            None => prepare_toast_document(params, actions)?,
        };
        let existing = if params.dedupe_key.is_some() {
            let existing = inspect_history_for_tag(tag, group)?;
            if existing.count > 0 {
                verify_unique_payload(
                    &existing,
                    &prepared.payload_sha256,
                    not_after_unix_ms,
                    tag,
                    group,
                    "toast deduplication",
                )?;
                Some(existing)
            } else {
                None
            }
        } else {
            None
        };

        let toast = ToastNotification::CreateToastNotification(&document).map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!("CreateToastNotification failed: {error}"),
            )
        })?;
        toast.SetTag(&HSTRING::from(tag)).map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!("ToastNotification.SetTag({tag}) failed: {error}"),
            )
        })?;
        toast.SetGroup(&HSTRING::from(group)).map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!("ToastNotification.SetGroup({group}) failed: {error}"),
            )
        })?;
        if params.suppress_popup {
            toast.SetSuppressPopup(true).map_err(|error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_SHOW_FAILED,
                    format!("ToastNotification.SetSuppressPopup(true) failed: {error}"),
                )
            })?;
        }
        if let Some(not_after_unix_ms) = not_after_unix_ms {
            set_and_verify_expiration(&toast, not_after_unix_ms)?;
        }
        if let Some(callback) = activation_callback {
            register_activation_handler(&toast, tag, callback)?;
        }

        // Every fallible operation that is provably before Show completes
        // above. Only now may an escalation commit StartedUnknown and retain
        // its transition guard across the actual side-effect invocation.
        let requires_authorization = pre_show_authorizer.is_some();
        let mut authority = match pre_show_authorizer {
            Some(authorize) => authorize(&prepared)
                .map_err(|failure| NotifyFailure::new(failure.code, failure.message))?,
            None => None,
        };
        if requires_authorization && authority.is_none() {
            return Ok(None);
        }

        if let Some(not_after_unix_ms) = not_after_unix_ms {
            let now_unix_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
                Ok(duration) => duration.as_millis(),
                Err(error) => {
                    let reconcile_result = authority
                        .as_mut()
                        .ok_or_else(|| {
                            NotifyFailure::new(
                                error_codes::NOTIFY_WORKER_FAILED,
                                "escalation Show job lost its authorization token before clock validation",
                            )
                        })?
                        .reconcile_before_show("system_clock_invalid_before_show");
                    drop(authority);
                    if let Err(failure) = reconcile_result {
                        return Err(NotifyFailure::new(
                            failure.code,
                            format!("{}; original_clock_error={error}", failure.message),
                        ));
                    }
                    return Err(NotifyFailure::new(
                        error_codes::NOTIFY_SHOW_FAILED,
                        format!(
                            "system clock precedes Unix epoch before ToastNotifier.Show; durable known-unsent state restored: {error}"
                        ),
                    ));
                }
            };
            if now_unix_ms >= u128::from(not_after_unix_ms) {
                authority
                    .as_mut()
                    .ok_or_else(|| {
                        NotifyFailure::new(
                            error_codes::NOTIFY_WORKER_FAILED,
                            "expired escalation Show job lost its authorization token",
                        )
                    })?
                    .reconcile_before_show("deadline_expired_before_show")
                    .map_err(|failure| NotifyFailure::new(failure.code, failure.message))?;
                drop(authority);
                tracing::info!(
                    code = error_codes::NOTIFY_DELIVERY_EXPIRED,
                    now_unix_ms,
                    not_after_unix_ms,
                    tag,
                    group,
                    "escalation authority expired at the COM queue head; durable intent was reconciled before skipping Show"
                );
                return Ok(None);
            }
        }

        // Expired authority is rejected above even when a physically matching
        // row already exists; stale work cannot report dedupe success.
        if let Some(existing) = existing {
            let outcome = ToastOutcome {
                shown: false,
                deduped: true,
                history_count: existing.count,
                payload_sha256: prepared.payload_sha256,
                expiration_unix_ms: existing.expiration_unix_ms[0],
                notification_setting,
            };
            drop(authority);
            return Ok(Some(outcome));
        }

        notifier.Show(&toast).map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!("ToastNotifier.Show failed: {error}"),
            )
        })?;

        // The durable transition boundary protects authorization through the
        // actual Show side effect. Release it before history polling: the
        // latter is an independent physical readback and may wait for Windows
        // to materialize the row, while acknowledgements must remain live.
        drop(authority);

        // Show() succeeding proves nothing — verify the toast physically
        // landed in Action Center history before reporting success.
        let deadline = Instant::now() + Duration::from_millis(HISTORY_VERIFY_TIMEOUT_MS);
        loop {
            let inspection = inspect_history_for_tag(tag, group)?;
            if inspection.count > 0 {
                verify_unique_payload(
                    &inspection,
                    &prepared.payload_sha256,
                    not_after_unix_ms,
                    tag,
                    group,
                    "post-Show delivery verification",
                )?;
                let outcome = ToastOutcome {
                    shown: true,
                    deduped: false,
                    history_count: inspection.count,
                    payload_sha256: prepared.payload_sha256,
                    expiration_unix_ms: inspection.expiration_unix_ms[0],
                    notification_setting,
                };
                return Ok(Some(outcome));
            }
            if Instant::now() >= deadline {
                return Err(NotifyFailure::new(
                    error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                    format!(
                        "ToastNotifier.Show succeeded but no toast with tag {tag} group {group} appeared in Action Center history for {SYNAPSE_AUMID} within {HISTORY_VERIFY_TIMEOUT_MS}ms; \
                         likely causes: AUMID registration not honored yet, or 'show in notification center' disabled for Synapse in Windows Settings"
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(HISTORY_VERIFY_POLL_MS));
        }
    }

    fn register_activation_handler(
        toast: &ToastNotification,
        tag: &str,
        callback: ToastActivationCallback,
    ) -> Result<(), NotifyFailure> {
        let tag_for_handler = tag.to_owned();
        let handler =
            TypedEventHandler::<ToastNotification, IInspectable>::new(move |_sender, args| {
                let args = match args.ok() {
                    Ok(args) => args,
                    Err(error) => {
                        tracing::warn!(
                            code = "NOTIFY_TOAST_ACTIVATION_ARGS_MISSING",
                            tag = %tag_for_handler,
                            "toast activation delivered no arguments: {error}"
                        );
                        return Ok(());
                    }
                };
                let arguments = match args
                    .cast::<ToastActivatedEventArgs>()
                    .and_then(|args| args.Arguments())
                {
                    Ok(arguments) => arguments.to_string_lossy(),
                    Err(error) => {
                        tracing::warn!(
                            code = "NOTIFY_TOAST_ACTIVATION_ARGS_INVALID",
                            tag = %tag_for_handler,
                            "toast activation arguments could not be read: {error}"
                        );
                        return Ok(());
                    }
                };
                tracing::info!(
                    code = "NOTIFY_TOAST_ACTIVATED",
                    tag = %tag_for_handler,
                    arguments_len = arguments.len(),
                    "toast activation callback received operator action"
                );
                callback(arguments);
                Ok(())
            });
        let token = toast.Activated(&handler).map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!("ToastNotification.Activated handler registration failed: {error}"),
            )
        })?;
        retain_activation_subscription(toast, token)
    }

    fn retain_activation_subscription(
        toast: &ToastNotification,
        token: i64,
    ) -> Result<(), NotifyFailure> {
        let subscriptions = LIVE_ACTIVATION_SUBSCRIPTIONS.get_or_init(|| Mutex::new(Vec::new()));
        let mut guard = subscriptions.lock().map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!("toast activation subscription registry is poisoned: {error}"),
            )
        })?;
        while guard.len() >= MAX_LIVE_ACTIVATION_SUBSCRIPTIONS {
            let stale = guard.remove(0);
            let _ = stale.toast.RemoveActivated(stale.token);
        }
        guard.push(LiveActivationSubscription {
            toast: toast.clone(),
            token,
        });
        Ok(())
    }
}

#[cfg(not(windows))]
async fn prepare_toast_payload_for_platform(
    _params: NotifyHumanParams,
    _actions: Vec<ToastAction>,
) -> Result<PreparedToastPayload, NotifyFailure> {
    Err(NotifyFailure::new(
        error_codes::NOTIFY_UNSUPPORTED_PLATFORM,
        "toast payload preparation requires Windows notification support",
    ))
}

#[cfg(not(windows))]
async fn send_toast_for_platform(
    _params: NotifyHumanParams,
    _tag: String,
    _group: String,
    _actions: Vec<ToastAction>,
    _activation_callback: Option<ToastActivationCallback>,
) -> Result<ToastOutcome, NotifyFailure> {
    Err(NotifyFailure::new(
        error_codes::NOTIFY_UNSUPPORTED_PLATFORM,
        "notify_human requires Windows toast notification support",
    ))
}

#[cfg(not(windows))]
fn send_escalation_toast_synchronously_for_platform(
    _params: NotifyHumanParams,
    _tag: String,
    _group: String,
    _actions: Vec<ToastAction>,
    _frozen_payload: PreparedToastPayload,
    _not_after_unix_ms: u64,
    _pre_show_authorizer: ToastPreShowAuthorizer,
) -> Result<Option<ToastOutcome>, NotifyFailure> {
    Err(NotifyFailure::new(
        error_codes::NOTIFY_UNSUPPORTED_PLATFORM,
        "escalation toast delivery requires Windows notification support",
    ))
}

#[cfg(not(windows))]
async fn remove_toast_for_platform(
    tag: String,
    group: String,
    _expected_payload_sha256: String,
    _expected_expiration_unix_ms: Option<u64>,
) -> ToastRemovalOutcome {
    ToastRemovalOutcome::unsupported(tag, group)
}

#[cfg(not(windows))]
async fn inspect_toast_for_platform(
    _tag: String,
    _group: String,
) -> Result<HistoryInspection, NotifyFailure> {
    Err(NotifyFailure::new(
        error_codes::NOTIFY_UNSUPPORTED_PLATFORM,
        "toast history inspection requires Windows notification support",
    ))
}

#[cfg(not(windows))]
async fn cleanup_escalation_orphans_for_platform(
    _preserve_tags: Vec<String>,
) -> ToastCleanupReport {
    ToastCleanupReport::unsupported()
}

#[cfg(windows)]
async fn prepare_toast_payload_for_platform(
    params: NotifyHumanParams,
    actions: Vec<ToastAction>,
) -> Result<PreparedToastPayload, NotifyFailure> {
    windows_toast::prepare_toast_payload(params, actions).await
}

#[cfg(windows)]
async fn send_toast_for_platform(
    params: NotifyHumanParams,
    tag: String,
    group: String,
    actions: Vec<ToastAction>,
    activation_callback: Option<ToastActivationCallback>,
) -> Result<ToastOutcome, NotifyFailure> {
    windows_toast::send_toast(params, tag, group, actions, activation_callback).await
}

#[cfg(windows)]
fn send_escalation_toast_synchronously_for_platform(
    params: NotifyHumanParams,
    tag: String,
    group: String,
    actions: Vec<ToastAction>,
    frozen_payload: PreparedToastPayload,
    not_after_unix_ms: u64,
    pre_show_authorizer: ToastPreShowAuthorizer,
) -> Result<Option<ToastOutcome>, NotifyFailure> {
    windows_toast::send_escalation_toast_synchronously(
        params,
        tag,
        group,
        actions,
        frozen_payload,
        not_after_unix_ms,
        pre_show_authorizer,
    )
}

#[cfg(windows)]
async fn remove_toast_for_platform(
    tag: String,
    group: String,
    expected_payload_sha256: String,
    expected_expiration_unix_ms: Option<u64>,
) -> ToastRemovalOutcome {
    windows_toast::remove_toast(
        tag,
        group,
        expected_payload_sha256,
        expected_expiration_unix_ms,
    )
    .await
}

#[cfg(windows)]
async fn inspect_toast_for_platform(
    tag: String,
    group: String,
) -> Result<HistoryInspection, NotifyFailure> {
    windows_toast::inspect_toast(tag, group).await
}

#[cfg(windows)]
async fn cleanup_escalation_orphans_for_platform(preserve_tags: Vec<String>) -> ToastCleanupReport {
    windows_toast::cleanup_escalation_orphans(preserve_tags).await
}

async fn run_notify_human(params: NotifyHumanParams) -> Result<NotifyHumanResponse, ErrorData> {
    let tag = toast_tag_for(params.dedupe_key.as_deref());
    run_internal_toast(params, tag, Vec::new()).await
}

pub(crate) async fn remove_internal_toast(
    tag: String,
    expected_payload_sha256: String,
) -> ToastRemovalOutcome {
    remove_internal_toast_in_group(tag, SYNAPSE_TOAST_GROUP, expected_payload_sha256, None).await
}

pub(crate) async fn remove_internal_escalation_toast(
    tag: String,
    expected_payload_sha256: String,
    expected_expiration_unix_ms: u64,
) -> ToastRemovalOutcome {
    remove_internal_toast_in_group(
        tag,
        SYNAPSE_ESCALATION_TOAST_GROUP,
        expected_payload_sha256,
        Some(expected_expiration_unix_ms),
    )
    .await
}

async fn remove_internal_toast_in_group(
    tag: String,
    group: &str,
    expected_payload_sha256: String,
    expected_expiration_unix_ms: Option<u64>,
) -> ToastRemovalOutcome {
    let outcome = remove_toast_for_platform(
        tag.clone(),
        group.to_owned(),
        expected_payload_sha256.clone(),
        expected_expiration_unix_ms,
    )
    .await;
    tracing::info!(
        code = "NOTIFY_TOAST_REMOVAL_RESULT",
        tag = %tag,
        group,
        expected_payload_sha256 = %expected_payload_sha256,
        expected_expiration_unix_ms,
        status = %outcome.status,
        removed = outcome.removed,
        already_absent = outcome.already_absent,
        before_count = outcome.before_count,
        after_count = outcome.after_count,
        error_code = outcome.error_code.as_deref().unwrap_or(""),
        "toast history removal completed"
    );
    outcome
}

pub(crate) async fn inspect_internal_toast(tag: String) -> Result<ToastHistoryReadback, ErrorData> {
    inspect_internal_toast_in_group(tag, SYNAPSE_TOAST_GROUP).await
}

pub(crate) async fn inspect_internal_escalation_toast(
    tag: String,
) -> Result<ToastHistoryReadback, ErrorData> {
    inspect_internal_toast_in_group(tag, SYNAPSE_ESCALATION_TOAST_GROUP).await
}

async fn inspect_internal_toast_in_group(
    tag: String,
    group: &str,
) -> Result<ToastHistoryReadback, ErrorData> {
    let inspection = inspect_toast_for_platform(tag.clone(), group.to_owned())
        .await
        .map_err(|failure| {
            tracing::warn!(
                code = failure.code,
                tag = %tag,
                "Action Center history inspection failed: {}",
                failure.message
            );
            mcp_error(failure.code, failure.message)
        })?;
    let readback = ToastHistoryReadback {
        aumid: SYNAPSE_AUMID.to_owned(),
        tag: tag.clone(),
        group: group.to_owned(),
        history_count: inspection.count,
        present: inspection.count > 0,
        payload_sha256s: inspection.payload_sha256s,
        expiration_unix_ms: inspection.expiration_unix_ms,
    };
    tracing::info!(
        code = "NOTIFY_TOAST_HISTORY_READBACK",
        tag = %tag,
        group,
        history_count = readback.history_count,
        present = readback.present,
        expiration_unix_ms = ?readback.expiration_unix_ms,
        "readback=Action Center exact Tag+Group history state"
    );
    Ok(readback)
}

pub(crate) async fn remove_orphaned_escalation_toasts(
    preserve_tags: Vec<String>,
) -> ToastCleanupReport {
    let report = cleanup_escalation_orphans_for_platform(preserve_tags).await;
    tracing::info!(
        code = "NOTIFY_ORPHAN_ESCALATION_TOAST_CLEANUP",
        status = %report.status,
        scanned = report.scanned,
        candidates = report.candidates,
        preserved_open = report.preserved_open,
        removed = report.removed,
        already_absent = report.already_absent,
        failed = report.failed,
        error_code = report.error_code.as_deref().unwrap_or(""),
        "orphan escalation toast cleanup completed"
    );
    report
}

pub(crate) async fn run_internal_toast(
    params: NotifyHumanParams,
    tag: String,
    actions: Vec<ToastAction>,
) -> Result<NotifyHumanResponse, ErrorData> {
    validate_params(&params)?;
    run_internal_toast_with_tag(params, tag, SYNAPSE_TOAST_GROUP, actions, None).await
}

pub(crate) async fn run_internal_toast_with_activation(
    params: NotifyHumanParams,
    tag: String,
    actions: Vec<ToastAction>,
    activation_callback: ToastActivationCallback,
) -> Result<NotifyHumanResponse, ErrorData> {
    validate_params(&params)?;
    run_internal_toast_with_tag(
        params,
        tag,
        SYNAPSE_TOAST_GROUP,
        actions,
        Some(activation_callback),
    )
    .await
}

pub(crate) async fn prepare_internal_escalation_toast(
    params: NotifyHumanParams,
    actions: Vec<ToastAction>,
) -> Result<PreparedToastPayload, ErrorData> {
    validate_params(&params)?;
    prepare_toast_payload_for_platform(params, actions)
        .await
        .map_err(|failure| {
            tracing::warn!(
                code = failure.code,
                group = SYNAPSE_ESCALATION_TOAST_GROUP,
                "escalation toast payload preparation failed: {}",
                failure.message
            );
            mcp_error(failure.code, failure.message)
        })
}

/// Synchronous bridge used only from a Tokio blocking thread. The supplied
/// authorizer runs at the COM queue head immediately before dedupe/Show and
/// retains the transition linearization boundary through dedupe or the actual
/// `Show` invocation. Physical history verification is a separate read after
/// releasing the boundary. `None` means authorization or pre-Show
/// reconciliation skipped the side effect.
pub(crate) fn run_internal_escalation_toast_blocking(
    params: NotifyHumanParams,
    tag: String,
    actions: Vec<ToastAction>,
    frozen_payload: PreparedToastPayload,
    not_after_unix_ms: u64,
    pre_show_authorizer: ToastPreShowAuthorizer,
) -> Result<Option<NotifyHumanResponse>, ErrorData> {
    validate_params(&params)?;
    if !is_escalation_toast_tag(&tag) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("reserved escalation toast tag is not canonical: {tag:?}"),
        ));
    }
    let outcome = send_escalation_toast_synchronously_for_platform(
        params,
        tag.clone(),
        SYNAPSE_ESCALATION_TOAST_GROUP.to_owned(),
        actions,
        frozen_payload,
        not_after_unix_ms,
        pre_show_authorizer,
    )
    .map_err(|failure| {
        tracing::warn!(
            code = failure.code,
            tag = %tag,
            group = SYNAPSE_ESCALATION_TOAST_GROUP,
            not_after_unix_ms,
            "synchronous escalation toast failed: {}",
            failure.message
        );
        mcp_error(failure.code, failure.message)
    })?;
    let Some(outcome) = outcome else {
        tracing::info!(
            code = "NOTIFY_ESCALATION_TOAST_SKIPPED",
            tag = %tag,
            group = SYNAPSE_ESCALATION_TOAST_GROUP,
            not_after_unix_ms,
            "COM queue-head authorization skipped escalation toast before dedupe/Show"
        );
        return Ok(None);
    };
    tracing::info!(
        code = "NOTIFY_ESCALATION_TOAST_RESULT",
        shown = outcome.shown,
        deduped = outcome.deduped,
        tag = %tag,
        group = SYNAPSE_ESCALATION_TOAST_GROUP,
        history_count = outcome.history_count,
        notification_setting = %outcome.notification_setting,
        not_after_unix_ms,
        "synchronous escalation toast completed; Show was authorized inside the transition boundary and history was verified afterward"
    );
    Ok(Some(NotifyHumanResponse {
        shown: outcome.shown,
        deduped: outcome.deduped,
        aumid: SYNAPSE_AUMID.to_owned(),
        tag,
        group: SYNAPSE_ESCALATION_TOAST_GROUP.to_owned(),
        notification_setting: outcome.notification_setting,
        verified_in_history: true,
        history_count: outcome.history_count,
        payload_sha256: outcome.payload_sha256,
        expiration_unix_ms: outcome.expiration_unix_ms,
    }))
}

async fn run_internal_toast_with_tag(
    params: NotifyHumanParams,
    tag: String,
    group: &str,
    actions: Vec<ToastAction>,
    activation_callback: Option<ToastActivationCallback>,
) -> Result<NotifyHumanResponse, ErrorData> {
    let outcome = send_toast_for_platform(
        params,
        tag.clone(),
        group.to_owned(),
        actions,
        activation_callback,
    )
    .await
    .map_err(|failure| {
        tracing::warn!(
            code = failure.code,
            tag = %tag,
            group,
            "notify_human failed: {}",
            failure.message
        );
        mcp_error(failure.code, failure.message)
    })?;

    tracing::info!(
        code = "NOTIFY_TOAST_RESULT",
        shown = outcome.shown,
        deduped = outcome.deduped,
        tag = %tag,
        group,
        history_count = outcome.history_count,
        notification_setting = %outcome.notification_setting,
        "notify_human completed"
    );
    Ok(NotifyHumanResponse {
        shown: outcome.shown,
        deduped: outcome.deduped,
        aumid: SYNAPSE_AUMID.to_owned(),
        tag,
        group: group.to_owned(),
        notification_setting: outcome.notification_setting,
        verified_in_history: true,
        history_count: outcome.history_count,
        payload_sha256: outcome.payload_sha256,
        expiration_unix_ms: outcome.expiration_unix_ms,
    })
}

#[tool_router(router = notify_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Raise a Windows toast notification to the human operator (fire-and-forget). Registers the Synapse AUMID on first use and verifies exact payload delivery by reading the toast back from Action Center history. While an identical toast with the same dedupe_key remains in history, repeats are suppressed; reusing a live key for different content fails closed. suppress_popup delivers straight to Action Center without a banner."
    )]
    pub async fn notify_human(
        &self,
        params: Parameters<NotifyHumanParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<NotifyHumanResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "notify_human",
            notify_kind = params.kind.as_str(),
            dedupe_key = params.dedupe_key.as_deref().unwrap_or(""),
            suppress_popup = params.suppress_popup,
            "tool.invocation kind=notify_human"
        );
        validate_params(&params)?;
        let tag = toast_tag_for(params.dedupe_key.as_deref());
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?;
        let details = notify_request_details(&params, &tag);
        if let Some(session_id) = session_id.as_deref() {
            self.audit_action_started_with_details_for_session(
                "notify_human",
                &details,
                session_id,
            )?;
        } else {
            self.audit_action_started_with_details("notify_human", &details)?;
        }
        let result = run_notify_human(params).await;
        match session_id.as_deref() {
            Some(session_id) => {
                self.audit_action_result_for_session("notify_human", &result, session_id)?;
            }
            None => self.audit_action_result("notify_human", &result)?,
        }
        result.map(Json)
    }
}
