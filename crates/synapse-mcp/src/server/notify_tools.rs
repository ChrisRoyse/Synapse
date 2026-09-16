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
//!   truth: while a toast with the same key is still in history, repeats are
//!   suppressed (`deduped: true`); once the operator dismisses it, the next
//!   notify shows again.

use std::sync::Arc;

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

const MAX_TITLE_CHARS: usize = 200;
const MAX_BODY_CHARS: usize = 2000;
const MAX_DEDUPE_KEY_CHARS: usize = 256;
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
    /// Optional suppression key. While a toast with the same dedupe_key is
    /// still present in Action Center, repeat notifies are suppressed
    /// (response reports deduped=true, shown=false) instead of stacking.
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

#[cfg(not(windows))]
impl ToastRemovalOutcome {
    fn unsupported(tag: String) -> Self {
        Self {
            aumid: SYNAPSE_AUMID.to_owned(),
            tag,
            group: SYNAPSE_TOAST_GROUP.to_owned(),
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
            group: SYNAPSE_TOAST_GROUP.to_owned(),
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
    /// Real `ToastNotifier.Setting()` readback: "enabled", or
    /// "unavailable_first_use" when Windows has not yet materialized the
    /// per-app notification record (happens only before the first-ever toast
    /// of an unpackaged app; delivery is still proven via Action Center).
    notification_setting: String,
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
        if let Some(bad) = text
            .chars()
            .find(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        {
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

#[cfg(test)]
fn toast_xml(params: &NotifyHumanParams) -> String {
    toast_xml_with_actions(params, &[])
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

fn is_escalation_toast_payload(payload: &str) -> bool {
    payload.contains("<text>Synapse: Agent appears stuck and needs attention [")
        || payload.contains("<text>Synapse: Agent needs your input to continue [")
        || payload.contains("<text>Synapse: Agent is waiting for your approval [")
        || payload.contains("<text>Synapse: Agent finished and is ready for review [")
}

fn is_payload_empty_dedupe_toast_candidate(payload: &str, tag: &str) -> bool {
    payload.trim().is_empty() && tag.starts_with("dk-")
}

fn is_orphan_cleanup_candidate(payload: &str, tag: &str) -> bool {
    is_escalation_toast_payload(payload) || is_payload_empty_dedupe_toast_candidate(payload, tag)
}

#[cfg(windows)]
mod windows_toast {
    use super::{
        HISTORY_VERIFY_POLL_MS, HISTORY_VERIFY_TIMEOUT_MS, NotifyFailure, NotifyHumanParams,
        SYNAPSE_AUMID, SYNAPSE_NOTIFY_DISPLAY_NAME, SYNAPSE_TOAST_GROUP, ToastAction,
        ToastActivationCallback, ToastCleanupReport, ToastOutcome, ToastRemovalOutcome,
        error_codes, is_orphan_cleanup_candidate, toast_xml_with_actions,
    };
    use std::{
        collections::BTreeSet,
        sync::{Mutex, OnceLock, mpsc},
        time::{Duration, Instant},
    };
    use windows::{
        Data::Xml::Dom::XmlDocument,
        Foundation::TypedEventHandler,
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

    fn wide_null(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    enum NotifyCommand {
        Show(NotifyJob),
        Remove(RemoveJob),
        CleanupEscalationOrphans(CleanupJob),
    }

    struct NotifyJob {
        params: NotifyHumanParams,
        tag: String,
        actions: Vec<ToastAction>,
        activation_callback: Option<ToastActivationCallback>,
        reply: tokio::sync::oneshot::Sender<Result<ToastOutcome, NotifyFailure>>,
    }

    struct RemoveJob {
        tag: String,
        reply: tokio::sync::oneshot::Sender<ToastRemovalOutcome>,
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
    static NOTIFY_WORKER: OnceLock<Result<mpsc::Sender<NotifyCommand>, String>> = OnceLock::new();
    static LIVE_ACTIVATION_SUBSCRIPTIONS: OnceLock<Mutex<Vec<LiveActivationSubscription>>> =
        OnceLock::new();

    fn spawn_notify_worker() -> Result<mpsc::Sender<NotifyCommand>, String> {
        let (tx, rx) = mpsc::channel::<NotifyCommand>();
        std::thread::Builder::new()
            .name("synapse-notify".to_owned())
            .spawn(move || {
                let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                let com_error = com
                    .is_err()
                    .then(|| format!("CoInitializeEx(COINIT_MULTITHREADED) failed: {com:?}"));
                // COM stays initialized until the daemon exits; never
                // CoUninitialize, or cached WinRT factories dangle.
                for command in rx {
                    match command {
                        NotifyCommand::Show(job) => {
                            let result = match com_error.as_deref() {
                                Some(message) => Err(NotifyFailure::new(
                                    error_codes::NOTIFY_WORKER_FAILED,
                                    format!("notify worker thread has no COM apartment: {message}"),
                                )),
                                None => send_toast_blocking(
                                    &job.params,
                                    &job.tag,
                                    &job.actions,
                                    job.activation_callback,
                                ),
                            };
                            let _ = job.reply.send(result);
                        }
                        NotifyCommand::Remove(job) => {
                            let result = match com_error.as_deref() {
                                Some(message) => removal_failure(
                                    &job.tag,
                                    error_codes::NOTIFY_WORKER_FAILED,
                                    format!("notify worker thread has no COM apartment: {message}"),
                                ),
                                None => remove_toast_blocking(&job.tag),
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
            .map(|_handle| tx)
            .map_err(|error| format!("failed to spawn synapse-notify worker thread: {error}"))
    }

    pub(super) async fn send_toast(
        params: NotifyHumanParams,
        tag: String,
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
            .send(NotifyCommand::Show(NotifyJob {
                params,
                tag,
                actions,
                activation_callback,
                reply: reply_tx,
            }))
            .map_err(|_send_error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_WORKER_FAILED,
                    "synapse-notify worker thread terminated; toast job not accepted",
                )
            })?;
        reply_rx.await.map_err(|_recv_error| {
            NotifyFailure::new(
                error_codes::NOTIFY_WORKER_FAILED,
                "synapse-notify worker dropped the toast job without replying (worker panic?)",
            )
        })?
    }

    pub(super) async fn remove_toast(tag: String) -> ToastRemovalOutcome {
        let sender = match NOTIFY_WORKER.get_or_init(spawn_notify_worker).as_ref() {
            Ok(sender) => sender,
            Err(message) => {
                return removal_failure(&tag, error_codes::NOTIFY_WORKER_FAILED, message.clone());
            }
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if sender
            .send(NotifyCommand::Remove(RemoveJob {
                tag: tag.clone(),
                reply: reply_tx,
            }))
            .is_err()
        {
            return removal_failure(
                &tag,
                error_codes::NOTIFY_WORKER_FAILED,
                "synapse-notify worker thread terminated; toast removal job not accepted",
            );
        }
        reply_rx.await.unwrap_or_else(|_recv_error| {
            removal_failure(
                &tag,
                error_codes::NOTIFY_WORKER_FAILED,
                "synapse-notify worker dropped the toast removal job without replying",
            )
        })
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
        if sender
            .send(NotifyCommand::CleanupEscalationOrphans(CleanupJob {
                preserve_tags,
                reply: reply_tx,
            }))
            .is_err()
        {
            return cleanup_failure(
                error_codes::NOTIFY_WORKER_FAILED,
                "synapse-notify worker thread terminated; toast cleanup job not accepted",
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

    fn history_count_for_tag(tag: &str) -> Result<u32, NotifyFailure> {
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
        let mut count = 0_u32;
        for index in 0..size {
            let toast = toasts.GetAt(index).map_err(|error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_SHOW_FAILED,
                    format!("Action Center history GetAt({index}) failed: {error}"),
                )
            })?;
            let toast_tag = toast.Tag().map(|t| t.to_string_lossy()).unwrap_or_default();
            let toast_group = toast
                .Group()
                .map(|g| g.to_string_lossy())
                .unwrap_or_default();
            if toast_tag == tag && toast_group == SYNAPSE_TOAST_GROUP {
                count += 1;
            }
        }
        Ok(count)
    }

    fn removal_failure(
        tag: &str,
        code: &'static str,
        message: impl Into<String>,
    ) -> ToastRemovalOutcome {
        ToastRemovalOutcome {
            aumid: SYNAPSE_AUMID.to_owned(),
            tag: tag.to_owned(),
            group: SYNAPSE_TOAST_GROUP.to_owned(),
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
            group: SYNAPSE_TOAST_GROUP.to_owned(),
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
        let mut report = ToastCleanupReport {
            aumid: SYNAPSE_AUMID.to_owned(),
            group: SYNAPSE_TOAST_GROUP.to_owned(),
            status: "ok".to_owned(),
            scanned: size,
            candidates: 0,
            preserved_open: 0,
            removed: 0,
            already_absent: 0,
            failed: 0,
            outcomes: Vec::new(),
            error_code: None,
            error_message: None,
        };
        let mut remove_tags = BTreeSet::new();
        for index in 0..size {
            let toast = match toasts.GetAt(index) {
                Ok(toast) => toast,
                Err(error) => {
                    report.status = "error".to_owned();
                    report.failed = report.failed.saturating_add(1);
                    report.error_code = Some(error_codes::NOTIFY_SHOW_FAILED.to_owned());
                    report.error_message = Some(format!(
                        "Action Center history GetAt({index}) failed: {error}"
                    ));
                    continue;
                }
            };
            let toast_group = toast
                .Group()
                .map(|group| group.to_string_lossy())
                .unwrap_or_default();
            if toast_group != SYNAPSE_TOAST_GROUP {
                continue;
            }
            let tag = toast
                .Tag()
                .map(|tag| tag.to_string_lossy())
                .unwrap_or_default();
            let payload = toast
                .Content()
                .and_then(|document| document.GetXml())
                .map(|xml| xml.to_string_lossy())
                .unwrap_or_default();
            if !is_orphan_cleanup_candidate(&payload, &tag) {
                continue;
            }
            report.candidates = report.candidates.saturating_add(1);
            if tag.is_empty() {
                report.status = "error".to_owned();
                report.failed = report.failed.saturating_add(1);
                report.error_code = Some(error_codes::NOTIFY_DELIVERY_UNVERIFIED.to_owned());
                report.error_message =
                    Some("escalation-looking toast in Synapse group had an empty tag".to_owned());
                continue;
            }
            if preserve.contains(&tag) {
                report.preserved_open = report.preserved_open.saturating_add(1);
                continue;
            }
            remove_tags.insert(tag);
        }

        for tag in remove_tags {
            let outcome = remove_toast_blocking(&tag);
            if outcome.removed {
                report.removed = report.removed.saturating_add(1);
            } else if outcome.already_absent {
                report.already_absent = report.already_absent.saturating_add(1);
            } else {
                report.failed = report.failed.saturating_add(1);
            }
            report.outcomes.push(outcome);
        }
        if report.failed > 0 {
            report.status = "partial_error".to_owned();
        }
        report
    }

    fn remove_toast_blocking(tag: &str) -> ToastRemovalOutcome {
        if let Err(error) = ensure_aumid_registered() {
            return removal_failure(tag, error.code, error.message);
        }
        let before_count = match history_count_for_tag(tag) {
            Ok(count) => count,
            Err(error) => return removal_failure(tag, error.code, error.message),
        };
        if before_count == 0 {
            return ToastRemovalOutcome {
                aumid: SYNAPSE_AUMID.to_owned(),
                tag: tag.to_owned(),
                group: SYNAPSE_TOAST_GROUP.to_owned(),
                status: "not_present".to_owned(),
                removed: false,
                already_absent: true,
                before_count: Some(0),
                after_count: Some(0),
                error_code: None,
                error_message: None,
            };
        }

        let remove_result = ToastNotificationManager::History()
            .and_then(|history| {
                history.RemoveGroupedTagWithId(
                    &HSTRING::from(tag),
                    &HSTRING::from(SYNAPSE_TOAST_GROUP),
                    &HSTRING::from(SYNAPSE_AUMID),
                )
            })
            .map_err(|error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_SHOW_FAILED,
                    format!(
                        "ToastNotificationHistory.RemoveGroupedTagWithId(tag={tag}, group={SYNAPSE_TOAST_GROUP}, app_id={SYNAPSE_AUMID}) failed: {error}"
                    ),
                )
            });
        if let Err(error) = remove_result {
            return removal_failure(tag, error.code, error.message);
        }

        let after_count = match history_count_for_tag(tag) {
            Ok(count) => count,
            Err(error) => return removal_failure(tag, error.code, error.message),
        };
        ToastRemovalOutcome {
            aumid: SYNAPSE_AUMID.to_owned(),
            tag: tag.to_owned(),
            group: SYNAPSE_TOAST_GROUP.to_owned(),
            status: if after_count == 0 {
                "removed".to_owned()
            } else {
                "still_present".to_owned()
            },
            removed: after_count == 0,
            already_absent: false,
            before_count: Some(before_count),
            after_count: Some(after_count),
            error_code: (after_count != 0)
                .then_some(error_codes::NOTIFY_DELIVERY_UNVERIFIED.to_owned()),
            error_message: (after_count != 0).then(|| {
                format!(
                    "toast tag {tag} group {SYNAPSE_TOAST_GROUP} remained in Action Center after removal"
                )
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

    /// Runs on the dedicated `synapse-notify` COM worker thread only.
    fn send_toast_blocking(
        params: &NotifyHumanParams,
        tag: &str,
        actions: &[ToastAction],
        activation_callback: Option<ToastActivationCallback>,
    ) -> Result<ToastOutcome, NotifyFailure> {
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

        if params.dedupe_key.is_some() {
            let existing = history_count_for_tag(tag)?;
            if existing > 0 {
                return Ok(ToastOutcome {
                    shown: false,
                    deduped: true,
                    history_count: existing,
                    notification_setting,
                });
            }
        }

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
                        "toast XML payload rejected by XmlDocument.LoadXml: {error}; payload: {xml}"
                    ),
                )
            })?;
        let toast = ToastNotification::CreateToastNotification(&document).map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!("CreateToastNotification failed: {error}"),
            )
        })?;
        if let Some(callback) = activation_callback {
            register_activation_handler(&toast, tag, callback)?;
        }
        toast.SetTag(&HSTRING::from(tag)).map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!("ToastNotification.SetTag({tag}) failed: {error}"),
            )
        })?;
        toast
            .SetGroup(&HSTRING::from(SYNAPSE_TOAST_GROUP))
            .map_err(|error| {
                NotifyFailure::new(
                    error_codes::NOTIFY_SHOW_FAILED,
                    format!("ToastNotification.SetGroup({SYNAPSE_TOAST_GROUP}) failed: {error}"),
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
        notifier.Show(&toast).map_err(|error| {
            NotifyFailure::new(
                error_codes::NOTIFY_SHOW_FAILED,
                format!("ToastNotifier.Show failed: {error}"),
            )
        })?;

        // Show() succeeding proves nothing — verify the toast physically
        // landed in Action Center history before reporting success.
        let deadline = Instant::now() + Duration::from_millis(HISTORY_VERIFY_TIMEOUT_MS);
        loop {
            let count = history_count_for_tag(tag)?;
            if count > 0 {
                return Ok(ToastOutcome {
                    shown: true,
                    deduped: false,
                    history_count: count,
                    notification_setting,
                });
            }
            if Instant::now() >= deadline {
                return Err(NotifyFailure::new(
                    error_codes::NOTIFY_DELIVERY_UNVERIFIED,
                    format!(
                        "ToastNotifier.Show succeeded but no toast with tag {tag} group {SYNAPSE_TOAST_GROUP} appeared in Action Center history for {SYNAPSE_AUMID} within {HISTORY_VERIFY_TIMEOUT_MS}ms; \
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
async fn send_toast_for_platform(
    _params: NotifyHumanParams,
    _tag: String,
    _actions: Vec<ToastAction>,
    _activation_callback: Option<ToastActivationCallback>,
) -> Result<ToastOutcome, NotifyFailure> {
    Err(NotifyFailure::new(
        error_codes::NOTIFY_UNSUPPORTED_PLATFORM,
        "notify_human requires Windows toast notification support",
    ))
}

#[cfg(not(windows))]
async fn remove_toast_for_platform(tag: String) -> ToastRemovalOutcome {
    ToastRemovalOutcome::unsupported(tag)
}

#[cfg(not(windows))]
async fn cleanup_escalation_orphans_for_platform(
    _preserve_tags: Vec<String>,
) -> ToastCleanupReport {
    ToastCleanupReport::unsupported()
}

#[cfg(windows)]
async fn send_toast_for_platform(
    params: NotifyHumanParams,
    tag: String,
    actions: Vec<ToastAction>,
    activation_callback: Option<ToastActivationCallback>,
) -> Result<ToastOutcome, NotifyFailure> {
    windows_toast::send_toast(params, tag, actions, activation_callback).await
}

#[cfg(windows)]
async fn remove_toast_for_platform(tag: String) -> ToastRemovalOutcome {
    windows_toast::remove_toast(tag).await
}

#[cfg(windows)]
async fn cleanup_escalation_orphans_for_platform(preserve_tags: Vec<String>) -> ToastCleanupReport {
    windows_toast::cleanup_escalation_orphans(preserve_tags).await
}

async fn run_notify_human(params: NotifyHumanParams) -> Result<NotifyHumanResponse, ErrorData> {
    let tag = toast_tag_for(params.dedupe_key.as_deref());
    run_internal_toast(params, tag, Vec::new()).await
}

pub(crate) async fn remove_internal_toast(tag: String) -> ToastRemovalOutcome {
    let outcome = remove_toast_for_platform(tag.clone()).await;
    tracing::info!(
        code = "NOTIFY_TOAST_REMOVAL_RESULT",
        tag = %tag,
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
    run_internal_toast_with_tag(params, tag, actions, None).await
}

pub(crate) async fn run_internal_toast_with_activation(
    params: NotifyHumanParams,
    tag: String,
    actions: Vec<ToastAction>,
    activation_callback: ToastActivationCallback,
) -> Result<NotifyHumanResponse, ErrorData> {
    validate_params(&params)?;
    run_internal_toast_with_tag(params, tag, actions, Some(activation_callback)).await
}

async fn run_internal_toast_with_tag(
    params: NotifyHumanParams,
    tag: String,
    actions: Vec<ToastAction>,
    activation_callback: Option<ToastActivationCallback>,
) -> Result<NotifyHumanResponse, ErrorData> {
    let outcome = send_toast_for_platform(params, tag.clone(), actions, activation_callback)
        .await
        .map_err(|failure| {
            tracing::warn!(
                code = failure.code,
                tag = %tag,
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
        history_count = outcome.history_count,
        notification_setting = %outcome.notification_setting,
        "notify_human completed"
    );
    Ok(NotifyHumanResponse {
        shown: outcome.shown,
        deduped: outcome.deduped,
        aumid: SYNAPSE_AUMID.to_owned(),
        tag,
        group: SYNAPSE_TOAST_GROUP.to_owned(),
        notification_setting: outcome.notification_setting,
        verified_in_history: true,
        history_count: outcome.history_count,
    })
}

#[tool_router(router = notify_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Raise a Windows toast notification to the human operator (fire-and-forget). Registers the Synapse AUMID on first use, verifies delivery by reading the toast back from Action Center history (errors precisely instead of dropping silently), and while a toast with the same dedupe_key is still in Action Center, repeats are suppressed. suppress_popup delivers straight to Action Center without a banner."
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

#[cfg(test)]
mod tests {
    use super::*;

    fn params(title: &str, body: &str, dedupe_key: Option<&str>) -> NotifyHumanParams {
        NotifyHumanParams {
            title: title.to_owned(),
            body: body.to_owned(),
            kind: NotifyKind::Info,
            dedupe_key: dedupe_key.map(str::to_owned),
            suppress_popup: false,
        }
    }

    #[test]
    fn empty_title_rejected() {
        let error = validate_params(&params("   ", "body", None)).unwrap_err();
        assert!(error.message.contains("title must not be empty"));
    }

    #[test]
    fn oversized_title_rejected() {
        let long = "x".repeat(MAX_TITLE_CHARS + 1);
        let error = validate_params(&params(&long, "body", None)).unwrap_err();
        assert!(error.message.contains("max 200"));
    }

    #[test]
    fn oversized_body_rejected() {
        let long = "x".repeat(MAX_BODY_CHARS + 1);
        let error = validate_params(&params("title", &long, None)).unwrap_err();
        assert!(error.message.contains("max 2000"));
    }

    #[test]
    fn control_characters_rejected_but_whitespace_allowed() {
        let error = validate_params(&params("tit\u{0007}le", "body", None)).unwrap_err();
        assert!(error.message.contains("U+0007"));
        validate_params(&params("title", "line one\nline two\ttabbed", None)).unwrap();
    }

    #[test]
    fn empty_dedupe_key_rejected() {
        let error = validate_params(&params("title", "body", Some(" "))).unwrap_err();
        assert!(error.message.contains("dedupe_key must not be empty"));
    }

    #[test]
    fn dedupe_tags_are_stable_and_unique_tags_are_not() {
        let a = toast_tag_for(Some("build-failed"));
        let b = toast_tag_for(Some("build-failed"));
        let c = toast_tag_for(Some("build-ok"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("dk-"));
        assert!(a.len() <= 64, "platform caps toast tags at 64 chars");
        let unique_a = toast_tag_for(None);
        let unique_b = toast_tag_for(None);
        assert_ne!(unique_a, unique_b);
        assert!(unique_a.starts_with("id-"));
        assert!(unique_a.len() <= 64);
    }

    #[test]
    fn orphan_cleanup_candidates_include_empty_dedupe_payloads_only() {
        let escalation_payload = "<toast><visual><binding><text>Synapse: Agent appears stuck and needs attention [critical]</text></binding></visual></toast>";
        assert!(is_orphan_cleanup_candidate(escalation_payload, "id-legacy"));
        assert!(is_orphan_cleanup_candidate(
            "",
            "dk-0123456789abcdef0123456789abcdef"
        ));
        assert!(is_orphan_cleanup_candidate(
            "   \r\n",
            "dk-0123456789abcdef0123456789abcdef"
        ));
        assert!(!is_orphan_cleanup_candidate(
            "",
            "id-0123456789abcdef0123456789abcdef"
        ));
        assert!(!is_orphan_cleanup_candidate(
            "<toast><visual><binding><text>Regular Synapse update</text></binding></visual></toast>",
            "dk-0123456789abcdef0123456789abcdef",
        ));
    }

    #[test]
    fn toast_xml_escapes_markup() {
        let xml = toast_xml(&params(
            "alert <script> & \"quotes\"",
            "body with 'apostrophe' & <tag>",
            None,
        ));
        assert!(xml.contains("alert &lt;script&gt; &amp; &quot;quotes&quot;"));
        assert!(xml.contains("body with &apos;apostrophe&apos; &amp; &lt;tag&gt;"));
        assert!(!xml.contains("<script>"));
        assert!(xml.contains(r#"<toast duration="short">"#));
    }

    #[test]
    fn toast_xml_includes_protocol_actions_with_escaped_arguments() {
        let xml = toast_xml_with_actions(
            &params("approval", "body", None),
            &[
                ToastAction {
                    content: "Accept & run".to_owned(),
                    arguments: "synapse-approval://decide?approval_id=apr1-abc&decision=accept"
                        .to_owned(),
                    activation_type: ToastActionActivationType::Protocol,
                },
                ToastAction {
                    content: "Decline".to_owned(),
                    arguments: "synapse-approval://decide?decision=decline".to_owned(),
                    activation_type: ToastActionActivationType::Foreground,
                },
            ],
        );
        assert!(xml.contains("<actions>"));
        assert!(xml.contains("activationType=\"protocol\""));
        assert!(xml.contains("activationType=\"foreground\""));
        assert!(xml.contains("Accept &amp; run"));
        assert!(xml.contains("approval_id=apr1-abc&amp;decision=accept"));
    }

    #[test]
    fn warning_and_error_kinds_use_long_duration() {
        let mut p = params("title", "body", None);
        p.kind = NotifyKind::Error;
        assert!(toast_xml(&p).contains(r#"<toast duration="long">"#));
        p.kind = NotifyKind::Warning;
        assert!(toast_xml(&p).contains(r#"<toast duration="long">"#));
    }
}
